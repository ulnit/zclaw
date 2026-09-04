//! ulnclaw CLI — port of the hermes CLI core (chat REPL, one-shot runs,
//! session/skill/cron/tool management).

use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use ulnclaw::agent::{Agent, AgentConfig};
use ulnclaw::config::UlncLawConfig;
use ulnclaw::provider::openai::OpenAiProvider;
use ulnclaw::provider::{Message, Role};
use ulnclaw::session::sqlite::SqliteSessionStore;
use ulnclaw::session::SessionStore;
use ulnclaw::tools::builtin::register_builtin_tools;
use ulnclaw::tools::context::ToolContext;
use ulnclaw::tools::ToolRegistry;
use ulnclaw::toolsets;

#[derive(Parser)]
#[command(
    name = "ulnclaw",
    version,
    about = "ulnclaw — Rust AI agent engine (hermes-agent port)"
)]
struct Cli {
    /// Config file path (default: ~/.ulnclaw/config.toml)
    #[arg(long, global = true)]
    config: Option<String>,
    /// Named profile from config.toml [profiles]
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Resume an existing session by ID or unique prefix (hermes --resume)
    #[arg(short = 'r', long, global = true)]
    resume: Option<String>,
    /// Continue the most recent session; with a value, continue the
    /// session matching that title or id (hermes --continue [NAME])
    #[arg(short = 'c', long, global = true, num_args = 0..=1, default_missing_value = "")]
    continue_last: Option<String>,
    /// Model override for this invocation (hermes -m): wins over the
    /// profile's configured model.
    #[arg(short = 'm', long, global = true)]
    model: Option<String>,
    /// Provider override for this invocation (hermes --provider):
    /// resolves the model against this backend instead of the
    /// configured one.
    #[arg(long, global = true)]
    provider: Option<String>,
    /// Reasoning effort for this invocation (hermes --reasoning):
    /// none, minimal, low, medium, high, xhigh, max, or ultra.
    /// Overrides agent.reasoning_effort from config.toml for this run
    /// only; kanban workers pin it per task.
    #[arg(long, global = true, value_name = "LEVEL")]
    reasoning: Option<String>,
    /// Priority Processing for this invocation (hermes --fast toggle):
    /// `--fast` sends service_tier=priority on OpenAI-compatible
    /// requests. Overrides agent.service_tier from config.toml for this
    /// run only.
    #[arg(long, global = true)]
    fast: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Interactive chat REPL (default)
    Chat,
    /// One-shot run: ulnclaw run "your prompt"
    Run { prompt: Vec<String> },
    /// Session management
    Sessions {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Kanban task board (hermes kanban engine)
    Kanban {
        #[command(subcommand)]
        action: KanbanAction,
    },
    /// First-class projects — named multi-folder workspaces (hermes
    /// project registry)
    #[command(name = "project")]
    Projects {
        #[command(subcommand)]
        action: ProjectsAction,
    },
    /// Petdex mascot pets — browse/install/animate (hermes pets)
    Pets {
        #[command(subcommand)]
        action: PetsAction,
    },
    /// List registered tools and toolsets; enable/disable toolsets (hermes tools)
    Tools {
        /// Action: list (default) | enable | disable
        action: Option<String>,
        /// Toolset name for enable/disable
        name: Option<String>,
        /// Emit the inventory as JSON
        #[arg(long)]
        json: bool,
    },
    /// Skill management
    Skills {
        #[command(subcommand)]
        action: Option<SkillAction>,
    },
    /// Skill bundles — load multiple skills under one /command (hermes bundles)
    Bundles {
        #[command(subcommand)]
        action: Option<BundlesAction>,
    },
    /// Cron job management
    Cron {
        #[command(subcommand)]
        action: Option<CronAction>,
    },
    /// Start the HTTP gateway (OpenAI-compatible API server)
    Gateway {
        /// Bind host (overrides [gateway] host / ULNCLAW_GATEWAY_HOST)
        #[arg(long)]
        host: Option<String>,
        /// Bind port (overrides [gateway] port / ULNCLAW_GATEWAY_PORT)
        #[arg(long)]
        port: Option<u16>,
        /// Terminate the gateway instance recorded in gateway.pid and
        /// take over (hermes `gateway run --replace`).
        #[arg(long)]
        replace: bool,
        /// Start even if another gateway instance is already running
        /// (not recommended: two dispatchers race for the same board).
        #[arg(long)]
        force: bool,
    },
    /// Headless gateway alias (hermes `serve` parity) — desktop shells
    /// spawn `ulnclaw serve --host 127.0.0.1 --port 0`.
    Serve {
        /// Bind host (overrides [gateway] host / ULNCLAW_GATEWAY_HOST)
        #[arg(long)]
        host: Option<String>,
        /// Bind port (overrides [gateway] port / ULNCLAW_GATEWAY_PORT)
        #[arg(long)]
        port: Option<u16>,
        /// Terminate the recorded gateway instance and take over
        #[arg(long)]
        replace: bool,
        /// Start even if another gateway instance is already running
        #[arg(long)]
        force: bool,
    },
    /// Web dashboard server management — run/status/stop over the gateway
    /// process that serves the dashboard (hermes dashboard/serve)
    Dashboard {
        /// Action: run (default) | status | stop
        action: Option<String>,
        /// Bind host (run action)
        #[arg(long)]
        host: Option<String>,
        /// Bind port (run action)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Weixin (WeChat iLink bot) account management
    Weixin {
        #[command(subcommand)]
        action: WeixinAction,
    },
    /// QQ Bot scan-to-configure onboarding (hermes qqbot `qr_register`
    /// gateway-setup wizard)
    Qq {
        #[command(subcommand)]
        action: QqAction,
    },
    /// Google Chat per-user OAuth for native attachment delivery —
    /// client-secret install / auth-url / code exchange / revoke
    /// (hermes `plugins.platforms.google_chat.oauth` helper)
    GoogleChatOauth {
        #[command(subcommand)]
        action: GoogleChatOauthAction,
    },
    /// Spotify PKCE OAuth login/status/logout (hermes `hermes auth spotify`) —
    /// tokens persist under providers.spotify in auth.json and power the
    /// `spotify_*` tools
    SpotifyAuth {
        #[command(subcommand)]
        action: SpotifyAuthAction,
    },
    /// Filesystem checkpoint management (snapshot list/restore/prune)
    Checkpoints {
        #[command(subcommand)]
        action: CheckpointAction,
    },
    /// Git working-tree diff (hermes working_diff): what changed here?
    Diff {
        /// Show staged changes only (`git diff --cached`)
        #[arg(long)]
        staged: bool,
        /// Show everything since HEAD (staged + unstaged + untracked)
        #[arg(long)]
        all: bool,
        /// Directory to inspect (default: cwd)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Optional pathspecs to restrict the diff
        paths: Vec<String>,
    },
    /// Mixture-of-Agents: fan out reference models + aggregator synthesis
    /// (hermes `moa` presets / `/moa` one-shot)
    Moa {
        #[command(subcommand)]
        action: Option<MoaAction>,
    },
    /// Multi-provider model catalog (models.dev registry)
    Models {
        #[command(subcommand)]
        action: Option<ModelsAction>,
    },
    /// Persistent memory: status, or `reset` to erase (hermes memory)
    Memory {
        /// Omit for status; `reset [all|memory|user]` erases memory files
        args: Vec<String>,
        /// Skip the interactive confirmation (reset)
        #[arg(long)]
        yes: bool,
    },
    /// Import Claude Code / Codex CLI setups into ulnclaw (hermes import-agent)
    ImportAgent {
        /// Agent to import from: claude-code | codex (default: auto-detect)
        agent: Option<String>,
        /// Source directory (default: ~/.claude or ~/.codex)
        #[arg(long)]
        source: Option<PathBuf>,
        /// Preview only — write nothing
        #[arg(long)]
        dry_run: bool,
        /// Replace skills that already exist in the target
        #[arg(long)]
        overwrite: bool,
    },
    /// Supply-chain security checks (hermes security)
    Security {
        #[command(subcommand)]
        action: SecurityAction,
    },
    /// External secret sources: status / sync (hermes secrets)
    Secrets {
        #[command(subcommand)]
        action: SecretsAction,
    },
    /// Computer Use (cua-driver): status / doctor / install (hermes computer-use)
    ComputerUse {
        #[command(subcommand)]
        action: ComputerUseAction,
    },
    /// Plugin management: list/enable/disable/accept-hooks (hermes plugins)
    Plugins {
        #[command(subcommand)]
        action: Option<PluginsAction>,
    },
    /// Shell-hook inspection: list/test/revoke/doctor (hermes hooks)
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// DM pairing codes: list/approve/revoke/clear-pending (hermes pairing)
    Pairing {
        #[command(subcommand)]
        action: PairingAction,
    },
    /// Manage named profiles — `[profiles.*]` overrides in config.toml
    /// (hermes profile management, lean port)
    Profiles {
        #[command(subcommand)]
        action: Option<ProfilesAction>,
    },
    /// OAuth device-flow login: login/status/refresh/logout (hermes portal auth)
    Auth {
        #[command(subcommand)]
        action: Option<AuthAction>,
    },
    /// Skill sync across devices: status/pull/push/now/enable/disable/device (hermes sync)
    Sync {
        #[command(subcommand)]
        action: Option<SyncAction>,
    },
    /// Local OpenAI-compatible proxy attaching stored OAuth credentials:
    /// start/status/providers (hermes proxy)
    Proxy {
        #[command(subcommand)]
        action: Option<ProxyAction>,
    },
    /// Sandbox egress via managed iron-proxy: install/setup/start/stop/
    /// restart/reload/status/disable/config (hermes `hermes egress`)
    Egress {
        #[command(subcommand)]
        action: EgressAction,
    },
    /// Skill library curation — pin/archive/restore/prune/usage reports
    /// (hermes `hermes curator`)
    Curator {
        #[command(subcommand)]
        action: Option<CuratorAction>,
    },
    /// What Hermes has learned, on a timeline — learned skills & memories
    /// (hermes `hermes journey`; hermes aliases `learning` / `memory-graph`)
    #[command(visible_alias = "learning", visible_alias = "memory-graph")]
    Journey {
        #[command(subcommand)]
        action: Option<JourneyAction>,
        /// Render the timeline built up to this point (0=oldest, 1=now)
        #[arg(long, default_value = "1.0")]
        reveal: f64,
        /// Animate the build-up over time (Ctrl-C to stop)
        #[arg(long)]
        play: bool,
        /// Animation frames per second for --play (default 12)
        #[arg(long, default_value = "12")]
        fps: u32,
        /// Override render width in columns
        #[arg(long)]
        width: Option<usize>,
        /// Override render height in rows
        #[arg(long)]
        height: Option<usize>,
        /// Disable color output
        #[arg(long)]
        no_color: bool,
        /// Print the raw graph payload as JSON and exit
        #[arg(long)]
        json: bool,
    },
    /// Write a default config.toml
    Init,
    /// List available skins/themes (hermes skin engine)
    Skins,
    /// Suggested automations — list/accept/dismiss pending cron proposals
    /// (hermes /suggestions)
    Suggestions {
        /// Subcommand args (accept N | dismiss N | catalog | clear)
        args: Vec<String>,
    },
    /// Usage insights and analytics over session history (hermes insights)
    Insights {
        /// Number of days to analyze
        #[arg(long, default_value = "30")]
        days: u32,
        /// Filter by source (cli, gateway, cron, …)
        #[arg(long)]
        source: Option<String>,
        /// Output the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Terminal approval mode: show, or persist manual|smart|off (hermes approvals)
    Approvals {
        /// Mode to persist: manual | smart | off (omit to show current)
        mode: Option<String>,
    },
    /// Measure the system prompt + tool-schema payload (hermes prompt-size)
    PromptSize {
        /// Emit the breakdown as JSON
        #[arg(long)]
        json: bool,
    },
    /// Diagnostics: collect a redacted share bundle (hermes debug)
    Debug {
        /// Bundle subcommand (currently: report)
        #[arg(default_value = "report")]
        action: String,
        /// Lines of recent log to include per file
        #[arg(long, default_value = "200")]
        lines: usize,
        /// Include raw (unredacted) log content — handle with care
        #[arg(long)]
        no_redact: bool,
        /// Output directory (default: ulnclaw-debug-<timestamp>)
        #[arg(long)]
        output: Option<String>,
    },
    /// Diagnose configuration and dependencies (hermes doctor)
    Doctor {
        /// Attempt to fix issues automatically
        #[arg(long)]
        fix: bool,
        /// Probe the configured provider API endpoint
        #[arg(long)]
        online: bool,
        /// Output the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show status of all components (hermes status)
    Status {
        /// Show all details (redacted for sharing; default rendering already redacts)
        #[arg(long)]
        all: bool,
        /// Run deep checks (may take longer)
        #[arg(long)]
        deep: bool,
    },
    /// Inspect gateway monitoring — health & diagnostics export posture (hermes monitoring)
    Monitoring {
        /// Action: status (default)
        action: Option<String>,
    },
    /// WhatsApp (Baileys bridge) status & diagnostics (hermes whatsapp surface)
    Whatsapp {
        /// Action: status (default)
        action: Option<String>,
    },
    /// WhatsApp Business Cloud API setup wizard (hermes whatsapp-cloud)
    WhatsappCloud,
    /// Dynamic webhook subscriptions: subscribe/list/remove/test (hermes webhook)
    Webhook {
        #[command(subcommand)]
        action: Option<WebhookAction>,
    },
    /// Launch the desktop GUI — Electron "ulnclaw desktop" (hermes gui/desktop)
    #[command(visible_alias = "desktop")]
    Gui {
        /// Explicit path to the ulnclaw-desktop executable
        #[arg(long)]
        binary: Option<std::path::PathBuf>,
        /// Run the unpackaged app (`npm start` in desktop-electron) instead
        #[arg(long)]
        dev: bool,
    },
    /// OAuth device-flow login (alias for `auth login`; hermes login)
    Login,
    /// Remove stored OAuth tokens (alias for `auth logout`; hermes logout)
    Logout,
    /// Interactive provider + model picker (hermes model)
    Model {
        /// Clear the models.dev picker cache before selecting
        #[arg(long)]
        refresh: bool,
    },
    /// Interactive onboarding wizard (hermes setup)
    Setup {
        /// Section to configure: model, terminal, gateway, tools, agent
        section: Option<String>,
        /// Fill only missing items (existing installs)
        #[arg(long)]
        quick: bool,
        /// Reset configuration to defaults before running
        #[arg(long)]
        reset: bool,
        /// Print non-interactive guidance and exit
        #[arg(long)]
        non_interactive: bool,
    },
    /// Generate shell completion scripts (hermes completion)
    Completion {
        /// Shell to generate completions for (bash, zsh, fish, elvish, powershell)
        shell: clap_complete::Shell,
    },
    /// Dump a compact, copy-pasteable setup summary (hermes dump)
    Dump {
        /// Show redacted key values instead of set/not set
        #[arg(long)]
        show_keys: bool,
    },
    /// Show version, install info and update status (hermes version)
    Version {
        /// Skip the online update check
        #[arg(long)]
        no_update_check: bool,
    },
    /// View and edit configuration (hermes config)
    Config {
        /// Subcommand: show (default) | get <key> | set <key> <value> | unset <key> | path | env-path | edit
        args: Vec<String>,
        /// Print the value as JSON (config get)
        #[arg(long)]
        json: bool,
        /// Skip the unknown-key notice (config set)
        #[arg(long)]
        force: bool,
    },
    /// Send a message to a configured messaging platform from scripts/cron/CI (hermes send)
    Send {
        /// Delivery target: 'platform' (home channel), 'platform:chat_id', 'platform:#channel-name'
        #[arg(short = 't', long)]
        to: Option<String>,
        /// Message text — with --list it acts as the platform filter. Omitted: read from --file or stdin
        message: Option<String>,
        /// Read the message body from PATH ('-' forces stdin)
        #[arg(short = 'f', long)]
        file: Option<String>,
        /// Prepend a subject/header line before the message body
        #[arg(short = 's', long)]
        subject: Option<String>,
        /// List available targets instead of sending
        #[arg(short = 'l', long)]
        list: bool,
        /// Suppress stdout on success (exit code only)
        #[arg(short = 'q', long)]
        quiet: bool,
        /// Emit the raw JSON result
        #[arg(long)]
        json: bool,
    },
    /// Slack integration helpers (hermes slack)
    Slack {
        /// Subcommand: manifest (generate a Slack app manifest with the gateway commands as native slashes)
        subcommand: Option<String>,
        /// Write the manifest to PATH instead of stdout (no path: <home>/slack-manifest.json)
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        write: Option<String>,
        /// Override the bot display name (default: Ulnclaw)
        #[arg(long)]
        name: Option<String>,
        /// Override the bot description
        #[arg(long)]
        description: Option<String>,
        /// Override the long app description (175-4000 characters)
        #[arg(long)]
        long_description: Option<String>,
        /// Read the long app description from a UTF-8 file
        #[arg(long)]
        long_description_file: Option<String>,
        /// Emit only the features.slash_commands array (for merging into an existing manifest)
        #[arg(long)]
        slashes_only: bool,
        /// Omit Slack AI Assistant mode (flat DM surface, inline slash commands)
        #[arg(long)]
        no_assistant: bool,
        /// Use Slack's Agent messaging experience instead of the legacy Assistant one
        #[arg(long)]
        agent_view: bool,
    },
    /// Run the agent across a JSONL dataset of prompts in parallel with checkpointing (hermes batch_runner.py)
    Batch {
        /// Dataset file — JSONL, each line {"prompt": "...", ...}
        #[arg(long)]
        dataset_file: String,
        /// Prompts per batch file (default 10)
        #[arg(long, default_value_t = 10)]
        batch_size: usize,
        /// Run name — outputs to batch_runs/<run_name>/
        #[arg(long)]
        run_name: String,
        /// Parallel batch workers (default: CPU count)
        #[arg(long)]
        num_workers: Option<usize>,
        /// Resume an interrupted run (skip completed prompts)
        #[arg(long)]
        resume: bool,
        /// Verbose per-prompt logging
        #[arg(long)]
        verbose: bool,
        /// Max agent iterations per prompt
        #[arg(long)]
        max_iterations: Option<usize>,
        /// Model override for this run
        #[arg(long)]
        model: Option<String>,
    },
    /// Run as an ACP (Agent Client Protocol) stdio server for editors like Zed (hermes acp)
    Acp {
        /// Verbose request/response logging on stderr
        #[arg(long)]
        verbose: bool,
    },
    /// MCP channel bridge — expose messaging conversations to MCP clients (hermes mcp)
    Mcp {
        /// Subcommand: serve (stdio MCP server)
        args: Vec<String>,
        /// Verbose request/response logging on stderr
        #[arg(long)]
        verbose: bool,
    },
    /// Remove ulnclaw: code, PATH entries, wrappers (hermes uninstall)
    Uninstall {
        /// Full uninstall — also wipe ~/.ulnclaw configs/sessions/logs
        #[arg(long)]
        full: bool,
        /// Print the uninstall plan without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Manage the fallback provider chain (hermes fallback)
    Fallback {
        /// Subcommand: list (default) | add <provider:model> | remove <N|provider:model> | clear
        args: Vec<String>,
        /// Assume yes (skip the clear confirmation)
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Back up the ulnclaw home directory to a zip (hermes backup)
    Backup {
        /// Snapshot management: list | restore <id> | prune [keep]
        action: Vec<String>,
        /// Output path for the zip (default: ~/ulnclaw-backup-<timestamp>.zip)
        #[arg(short, long)]
        output: Option<String>,
        /// Quick snapshot: only critical state files (config, state.db, .env, cron)
        #[arg(short, long)]
        quick: bool,
        /// Label for the snapshot (only used with --quick)
        #[arg(short, long)]
        label: Option<String>,
    },
    /// Restore from a backup zip, overlaying onto the current home (hermes import)
    Import {
        /// Path to the backup zip
        zip: String,
    },
    /// Update ulnclaw to the latest version (hermes update)
    Update {
        /// Check whether an update is available without installing anything
        #[arg(long)]
        check: bool,
        /// Update against this branch instead of the current one
        #[arg(long)]
        branch: Option<String>,
        /// Assume yes for interactive prompts (ulnclaw flow is non-interactive)
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// View and filter ulnclaw log files (hermes logs)
    Logs {
        /// Log to view: agent (default), errors, gateway, or 'list'
        log_name: Option<String>,
        /// Number of lines to show
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,
        /// Follow the log in real time (like tail -f)
        #[arg(short = 'f', long)]
        follow: bool,
        /// Minimum log level (DEBUG, INFO, WARNING, ERROR, CRITICAL)
        #[arg(long)]
        level: Option<String>,
        /// Filter lines containing this session ID substring
        #[arg(long)]
        session: Option<String>,
        /// Show lines since TIME ago (e.g. 1h, 30m, 2d)
        #[arg(long)]
        since: Option<String>,
        /// Filter by component: gateway, agent, tools, cli, cron, browser
        #[arg(long)]
        component: Option<String>,
    },
    /// Safe non-LLM command console — REPL over the CLI commands (hermes console)
    Console,
    /// Migrate config for retired models / deprecated settings (hermes migrate)
    Migrate {
        /// Migration to run: xai
        migration: Option<String>,
        /// Rewrite config.toml in place (default: dry run)
        #[arg(long)]
        apply: bool,
        /// Skip the automatic config backup on --apply
        #[arg(long)]
        no_backup: bool,
    },
}

#[derive(Subcommand)]
enum KanbanBoardsAction {
    /// List boards with task counts
    List,
    /// Create a board
    Create {
        slug: String,
        /// Display name (defaults to the slug)
        #[arg(long)]
        name: Option<String>,
        /// Default working directory for tasks on this board
        #[arg(long)]
        workdir: Option<String>,
    },
    /// Remove an empty board
    Rm { slug: String },
    /// Switch the current board
    Switch { slug: String },
    /// Show the current board
    Show,
    /// Rename a board's display name
    Rename { slug: String, name: Vec<String> },
    /// Set (or clear with --workdir "") a board's default working directory
    SetWorkdir {
        slug: String,
        #[arg(long)]
        workdir: Option<String>,
    },
}

#[derive(Subcommand)]
enum PetsAction {
    /// Browse the petdex gallery
    List {
        /// Filter by slug/name substring
        query: Vec<String>,
        /// Only show installed pets
        #[arg(long)]
        installed: bool,
        /// Max rows (0 = all)
        #[arg(long, default_value = "40")]
        limit: usize,
    },
    /// Install a pet from the gallery
    Install {
        /// Pet slug (e.g. boba)
        slug: String,
        /// Re-download even if present
        #[arg(long)]
        force: bool,
        /// Make it the active pet
        #[arg(long)]
        select: bool,
    },
    /// Set the active pet (writes display.pet.*)
    Select {
        /// Pet slug (omit for interactive picker)
        slug: Option<String>,
    },
    /// Animate the active pet in the terminal
    Show {
        /// Pet slug (default: active)
        slug: Option<String>,
        /// Single state: idle/run/review/failed/wave/jump/waiting
        #[arg(long)]
        state: Option<String>,
        /// Cycle through all states
        #[arg(long)]
        cycle: bool,
        /// Play once instead of looping
        #[arg(long)]
        once: bool,
        /// Override render mode (kitty/iterm/sixel/unicode/auto)
        #[arg(long)]
        mode: Option<String>,
        /// Override scale (0 = config)
        #[arg(long, default_value = "0")]
        scale: f64,
    },
    /// Disable the pet display
    Off,
    /// Resize the pet everywhere (display.pet.scale)
    Scale {
        /// Scale factor, e.g. 0.5 (clamped 0.1-3.0)
        factor: String,
    },
    /// Delete an installed pet
    Remove {
        /// Pet slug
        slug: String,
    },
    /// Check pet setup + terminal graphics support
    Doctor,
    /// Generate ("hatch") a brand-new pet from a description
    Hatch {
        /// What the pet is (e.g. "a tiny cyber fox")
        description: Vec<String>,
        /// Style: auto/pixel/plush/clay/sticker/flat-vector/3d-toy/painterly
        #[arg(long)]
        style: Option<String>,
        /// Display name (default: first words of the description)
        #[arg(long)]
        name: Option<String>,
        /// Use an existing image as the base look (skip draft generation)
        #[arg(long)]
        base: Option<String>,
        /// Generate N base drafts, save them, and stop (no hatch)
        #[arg(long, default_value = "0")]
        drafts: usize,
    },
}

/// First-class project registry actions (hermes `project` command).
/// Projects are human-named workspaces spanning one or more folders;
/// the primary repo anchors kanban worktrees + deterministic branches.
#[derive(Subcommand)]
enum ProjectsAction {
    /// Create a new project (first folder = primary)
    Create {
        /// Human name, e.g. 'Hermes Agent'
        name: String,
        /// Folder paths to include (first = primary)
        folders: Vec<String>,
        /// Explicit slug override
        #[arg(long)]
        slug: Option<String>,
        /// Primary repo path
        #[arg(long)]
        primary: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        icon: Option<String>,
        #[arg(long)]
        color: Option<String>,
        /// Bind a kanban board slug
        #[arg(long)]
        board: Option<String>,
        /// Set as the active project
        #[arg(long = "use")]
        r#use: bool,
    },
    /// List projects
    #[command(visible_alias = "ls")]
    List {
        /// Include archived projects
        #[arg(long)]
        all: bool,
    },
    /// Show a project's details
    Show {
        /// Project id or slug
        project: String,
    },
    /// Add a folder to a project
    AddFolder {
        /// Project id or slug
        project: String,
        /// Folder path
        path: String,
        #[arg(long)]
        label: Option<String>,
        /// Mark as primary repo
        #[arg(long)]
        primary: bool,
    },
    /// Remove a folder from a project
    RemoveFolder {
        /// Project id or slug
        project: String,
        /// Folder path
        path: String,
    },
    /// Rename a project
    Rename {
        /// Project id or slug
        project: String,
        /// New name
        name: String,
    },
    /// Set the primary folder (must already be in the project)
    SetPrimary {
        /// Project id or slug
        project: String,
        /// Folder path
        path: String,
    },
    /// Set the active project (omit to clear)
    Use {
        /// Project id or slug
        project: Option<String>,
    },
    /// Archive a project
    Archive {
        /// Project id or slug
        project: String,
    },
    /// Restore an archived project
    Restore {
        /// Project id or slug
        project: String,
    },
    /// Bind a kanban board to a project (omit board to unbind)
    BindBoard {
        /// Project id or slug
        project: String,
        /// Board slug
        board: Option<String>,
    },
    /// Scan the filesystem for git repos into the discovery cache
    Scan {
        /// Roots to scan (repeatable; default: home directory)
        #[arg(long = "root")]
        roots: Vec<String>,
        /// Max levels below each root to descend
        #[arg(long, default_value_t = ulnclaw::projects_scan::DEFAULT_MAX_DEPTH)]
        max_depth: usize,
    },
    /// List the cached discovered repos (or clear the cache)
    Repos {
        /// Clear the cache instead of listing
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum KanbanAction {
    /// Initialize the board store (idempotent)
    Init,
    /// Manage boards
    Boards {
        #[command(subcommand)]
        action: KanbanBoardsAction,
    },
    /// Create a task on the current board
    #[command(visible_alias = "new")]
    Create {
        title: Vec<String>,
        /// Task body / description
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long, default_value = "0")]
        priority: i64,
        #[arg(long)]
        tenant: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// Provider the pinned model belongs to; requires --model
        /// (hermes create --provider)
        #[arg(long)]
        provider: Option<String>,
        /// Link to a project (id or slug): anchors the task's worktree
        /// under the project's primary repo with a deterministic branch
        /// (hermes create --project)
        #[arg(long)]
        project: Option<String>,
        /// Skill force-loaded into the dispatcher worker (repeatable)
        #[arg(long = "skill")]
        skills: Vec<String>,
        /// Per-attempt worker runtime cap: seconds (300) or duration
        /// (90s, 30m, 2h, 1d) — hermes create --max-runtime
        #[arg(long)]
        max_runtime: Option<String>,
        /// Dedup key (hermes idempotency_key)
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Park in triage — the specifier/decomposer fleshes out the spec
        /// and promotes the task to todo (hermes create --triage)
        #[arg(long)]
        triage: bool,
        /// Circuit breaker: block the task on the Nth failed attempt
        /// (hermes create --max-retries; 1 trips on the first failure)
        #[arg(long)]
        max_retries: Option<i64>,
        /// Workspace: scratch | worktree | worktree:<path> | dir:<path>
        /// (hermes create --workspace)
        #[arg(long)]
        workspace: Option<String>,
        /// Worktree branch name (requires --workspace worktree; hermes
        /// create --branch)
        #[arg(long)]
        branch: Option<String>,
        /// Park the card directly in a column: running (default flow)
        /// or blocked for human-ops review (hermes create
        /// --initial-status)
        #[arg(long)]
        initial_status: Option<String>,
        /// Goal-loop mode: the spawned worker keeps working in its
        /// session until the auxiliary judge agrees the card is done
        /// (hermes create --goal)
        #[arg(long)]
        goal: bool,
        /// Goal-loop turn budget (hermes create --goal-max-turns)
        #[arg(long)]
        goal_max_turns: Option<i64>,
        /// Per-task reasoning effort pin: none, minimal, low, medium,
        /// high, xhigh, max, or ultra (hermes reasoning_effort). The
        /// dispatched worker runs at this depth regardless of the
        /// profile's agent.reasoning_effort.
        #[arg(long)]
        reasoning: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List tasks
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        board: Option<String>,
        /// Only tasks belonging to this workflow template (hermes list
        /// --workflow-template-id)
        #[arg(long)]
        workflow_template_id: Option<String>,
        #[arg(long, default_value = "200")]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show a task with comments and events
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Move a task to ready
    Ready { id: String },
    /// Assign a task
    Assign { id: String, assignee: String },
    /// Claim a ready task (ready → running)
    Claim {
        id: String,
        /// Claim TTL in seconds (default 1800)
        #[arg(long)]
        ttl: Option<i64>,
        /// Claimer identity (default host:pid)
        #[arg(long)]
        claimer: Option<String>,
    },
    /// Extend a live claim
    Heartbeat { id: String },
    /// Mark a task done
    Done {
        /// One or more task ids (--result/--summary/--metadata apply
        /// to all of them; hermes complete)
        id: Vec<String>,
        /// Result summary
        #[arg(long)]
        result: Option<String>,
        /// Structured handoff summary for downstream tasks (falls back
        /// to --result; hermes complete --summary)
        #[arg(long)]
        summary: Option<String>,
        /// JSON dict of structured facts stored on the closing run
        /// (hermes complete --metadata)
        #[arg(long)]
        metadata: Option<String>,
        /// Completion artifact path (repeatable). Files inside a
        /// managed scratch workspace are staged into
        /// <home>/kanban/attachments/<task>/ so workspace cleanup
        /// cannot erase them (hermes complete artifacts=[...])
        #[arg(long = "artifact", value_name = "PATH")]
        artifact: Vec<String>,
        /// Task id this worker created (repeatable). Verified against
        /// the board before completion; phantom ids block it (hermes
        /// created_cards anti-hallucination gate)
        #[arg(long = "created-card", value_name = "TASK_ID")]
        created_card: Vec<String>,
    },
    /// Park running task(s) in the review column after opening a PR;
    /// the dispatcher spawns a review agent to verify and merge
    /// (hermes review lifecycle)
    Review {
        /// One or more task ids
        id: Vec<String>,
        /// Short note, e.g. the PR URL
        #[arg(long)]
        reason: Option<String>,
    },
    /// Block a task with a reason
    Block {
        id: String,
        reason: Vec<String>,
        /// Typed block: dependency | needs_input | capability |
        /// transient (hermes block --kind)
        #[arg(long)]
        kind: Option<String>,
        /// Additional task ids to block with the same reason (hermes
        /// block --ids bulk mode)
        #[arg(long = "ids", num_args = 1..)]
        extra_ids: Vec<String>,
    },
    /// Unblock one or more tasks (blocked/scheduled → ready or todo)
    Unblock {
        id: Vec<String>,
        /// Recorded as a comment before unblocking (hermes unblock
        /// --reason)
        #[arg(long)]
        reason: Option<String>,
    },
    /// Archive one or more tasks (or purge archived ones with --rm)
    Archive {
        /// Task ids to archive
        id: Vec<String>,
        /// Permanently delete already-archived task ids (hermes
        /// archive --rm)
        #[arg(long = "rm", num_args = 1..)]
        purge: Vec<String>,
    },
    /// Comment on a task
    Comment { id: String, text: Vec<String> },
    /// Add a parent→child dependency (hermes `kanban link <parent> <child>`)
    Link { parent: String, child: String },
    /// Remove a parent→child dependency (hermes `kanban unlink`)
    Unlink { parent: String, child: String },
    /// Create a Kanban Swarm v1 graph: parallel workers → verifier →
    /// synthesizer (hermes `kanban swarm`)
    Swarm {
        /// Swarm goal / final outcome
        goal: Vec<String>,
        /// Parallel worker card ASSIGNEE:TITLE[:skill,skill] (repeatable)
        #[arg(long = "worker")]
        workers: Vec<String>,
        /// Verifier assignee
        #[arg(long)]
        verifier: String,
        /// Synthesizer/writer assignee
        #[arg(long)]
        synthesizer: String,
        /// Dedup key — rerunning with the same key recovers the swarm
        /// instead of duplicating it (hermes idempotency_key)
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Emit JSON output
        #[arg(long)]
        json: bool,
    },
    /// Flesh out a triage-column task into a concrete spec via the
    /// auxiliary LLM and promote it triage→todo (hermes `kanban specify`)
    Specify {
        /// Task id (omit with --all)
        id: Option<String>,
        /// Specify every task currently in the triage column
        #[arg(long)]
        all: bool,
    },
    /// Decompose a triage-column task into a graph of child tasks routed
    /// to profiles via the auxiliary LLM (hermes `kanban decompose`)
    Decompose {
        /// Task id (omit with --all)
        id: Option<String>,
        /// Decompose every task currently in the triage column
        #[arg(long)]
        all: bool,
    },
    /// Structured distress signals for tasks (hermes `kanban diagnostics`)
    Diagnostics {
        /// Task id (default: every open task on the board)
        id: Option<String>,
        /// Only show diagnostics at/above this severity (warning|error|critical)
        #[arg(long)]
        min_severity: Option<String>,
        /// Emit JSON per task
        #[arg(long)]
        json: bool,
    },
    /// Park a task in Scheduled — waiting on time, not human input
    /// (hermes `kanban schedule`)
    Schedule {
        id: String,
        reason: Vec<String>,
        /// Additional task ids to schedule with the same reason
        /// (hermes schedule --ids bulk mode)
        #[arg(long = "ids", num_args = 1..)]
        extra_ids: Vec<String>,
    },
    Promote {
        id: String,
        reason: Vec<String>,
        /// Promote even if parent dependencies are not done yet
        #[arg(long)]
        force: bool,
        /// Additional task ids to promote with the same reason (hermes
        /// promote --ids bulk mode)
        #[arg(long = "ids", num_args = 1..)]
        extra_ids: Vec<String>,
        /// Validate the promotion without mutating state
        #[arg(long)]
        dry_run: bool,
        /// Machine-readable result
        #[arg(long)]
        json: bool,
    },
    /// Release an active worker claim on a running task (hermes `kanban reclaim`)
    Reclaim {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Reassign a task to another profile ('none' clears), optionally
    /// reclaiming first (hermes `kanban reassign`)
    Reassign {
        id: String,
        profile: String,
        /// Release any active claim before reassigning
        #[arg(long)]
        reclaim: bool,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Edit a task's title/body (hermes `kanban edit`)
    Edit {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        /// Recovery edit on a DONE task: backfill the result text
        /// (hermes edit --result)
        #[arg(long)]
        result: Option<String>,
        /// Structured handoff summary, falls back to --result (hermes
        /// edit --summary)
        #[arg(long)]
        summary: Option<String>,
        /// JSON dict of structured facts for the latest completed run
        /// (hermes edit --metadata)
        #[arg(long)]
        metadata: Option<String>,
    },
    /// Set a per-task model override; omit the model to clear it
    /// (hermes `kanban set-model [--provider]`)
    SetModel {
        id: String,
        model: Option<String>,
        /// Provider the model belongs to; cleared together with the
        /// model (hermes set-model --provider)
        #[arg(long)]
        provider: Option<String>,
    },
    /// Set a per-task reasoning-effort pin; omit the level to clear it
    /// (hermes `set_reasoning_effort`). `none` is a real value (thinking
    /// off), not a clear.
    SetReasoning {
        id: String,
        /// none | minimal | low | medium | high | xhigh | max | ultra
        effort: Option<String>,
    },
    /// Attach a local file to a task (hermes `kanban attach`)
    Attach { id: String, path: PathBuf },
    /// List a task's attachments (hermes `kanban attachments`)
    Attachments { id: String },
    /// Delete an attachment by id (hermes `kanban attach-rm`)
    AttachRm { attachment_id: i64 },
    /// Show a task's event trail; --follow keeps watching (hermes `kanban tail`)
    Tail {
        id: String,
        /// Keep watching for new events
        #[arg(long)]
        follow: bool,
        /// How many trailing events to show first
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Print the worker log for a task (hermes `kanban log`)
    Log {
        id: String,
        /// Only print the last N bytes
        #[arg(long)]
        tail: Option<u64>,
    },
    /// Attempt history for a task — one row per run (hermes `kanban runs`)
    Runs {
        id: String,
        #[arg(long)]
        json: bool,
        /// With --state-name: filter runs by this task_runs column
        #[arg(long, value_parser = ["status", "outcome"])]
        state_type: Option<String>,
        /// With --state-type: keep runs whose column equals this value
        #[arg(long)]
        state_name: Option<String>,
    },
    /// Print the full worker context for a task — brief, prior
    /// attempts, parent handoffs, comments (hermes `kanban context`)
    Context { id: String },
    /// Integrity-check kanban.db and auto-repair index-scoped damage
    /// (hermes `kanban repair`)
    Repair {
        #[arg(long)]
        json: bool,
    },
    /// Known assignees (config roster) with per-status task counts
    /// (hermes `kanban assignees`)
    Assignees {
        #[arg(long)]
        json: bool,
    },
    /// DEPRECATED — the dispatcher runs inside the gateway. `--force`
    /// keeps the old standalone loop alive (hermes `kanban daemon`)
    Daemon {
        /// Tick interval in seconds
        #[arg(long, default_value = "60")]
        interval: u64,
        /// Write the dispatcher pid here
        #[arg(long)]
        pidfile: Option<PathBuf>,
        /// Run the standalone loop anyway (hidden escape hatch)
        #[arg(long, hide = true)]
        force: bool,
    },
    /// Subscribe a gateway chat to a task's terminal events (hermes
    /// `kanban notify-subscribe`)
    NotifySubscribe {
        id: String,
        #[arg(long)]
        platform: String,
        #[arg(long)]
        chat_id: String,
        /// dm / group / channel (used by wake routing)
        #[arg(long)]
        chat_type: Option<String>,
        #[arg(long)]
        thread_id: Option<String>,
        #[arg(long)]
        user_id: Option<String>,
        /// Profile gateway that owns/delivers this subscription
        #[arg(long)]
        notifier_profile: Option<String>,
    },
    /// List notification subscriptions, optionally for one task (hermes
    /// `kanban notify-list`)
    NotifyList {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove a gateway subscription from a task (hermes
    /// `kanban notify-unsubscribe`)
    NotifyUnsubscribe {
        id: String,
        #[arg(long)]
        platform: String,
        #[arg(long)]
        chat_id: String,
        #[arg(long)]
        thread_id: Option<String>,
    },
    /// Per-status + per-assignee counts + oldest-ready age (hermes
    /// `kanban stats`)
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Live-stream board task_events to the terminal, Ctrl+C to exit
    /// (hermes `kanban watch`)
    Watch {
        /// Only show events for tasks assigned to this profile
        #[arg(long)]
        assignee: Option<String>,
        /// Only show events from tasks in this tenant (hermes watch
        /// --tenant)
        #[arg(long)]
        tenant: Option<String>,
        /// Comma-separated event kinds to include
        #[arg(long)]
        kinds: Option<String>,
        /// Poll interval in seconds (default 0.5)
        #[arg(long, default_value = "0.5")]
        interval: f64,
    },
    /// Remove git worktrees of done/archived tasks (hermes dispatcher gc)
    Gc,
    /// One dispatcher pass: reclaim stale claims, promote parent-done
    /// todos, spawn workers for ready tasks (hermes `kanban dispatch`)
    Dispatch {
        /// Max concurrent workers (counts already-running tasks)
        #[arg(long, default_value = "2")]
        max_spawn: usize,
        /// Show what would spawn without spawning
        #[arg(long)]
        dry_run: bool,
        /// Consecutive spawn failures before a task is auto-blocked
        #[arg(long, default_value = "2")]
        failure_limit: usize,
        /// Emit the tick result as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    /// List recent sessions
    List {
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Show a session's messages
    Show {
        id: String,
        /// Raw mode: every message with timestamp, tool name/id and
        /// tool_calls — no decoration, grep-friendly (hermes trace
        /// display parity)
        #[arg(long)]
        raw: bool,
        /// Emit the transcript as a JSON array (implies raw detail)
        #[arg(long)]
        json: bool,
        /// Prefix each message with its timestamp in the pretty view
        #[arg(long)]
        timestamps: bool,
    },
    /// Full-text search across sessions
    Search { query: Vec<String> },
    /// Export a session as verifiable Markdown (hermes session export)
    Export {
        id: String,
        /// Output directory (default: <home>/exports)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Export format: md or html
        #[arg(long, default_value = "md")]
        format: String,
        /// Skip the SHA256 verification footer
        #[arg(long)]
        no_verification: bool,
    },
    /// Recap recent activity in a session (local, no LLM call)
    Recap { id: String },
    /// Offline non-destructive recovery of a damaged state.db
    /// (hermes session_recovery)
    Recover {
        /// Damaged database file (copied first; never opened in place)
        source: PathBuf,
        /// Output path (must not exist; default: <source>.recovered.db)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Prune (delete) ended sessions matching filters (hermes sessions prune)
    Prune {
        #[command(flatten)]
        filters: SessionFilterArgs,
        /// Also prune archived sessions (default: skip them)
        #[arg(long)]
        include_archived: bool,
        /// Show candidates without deleting anything
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Archive (soft-hide, recoverable) ended sessions matching filters
    /// (hermes sessions archive)
    Archive {
        #[command(flatten)]
        filters: SessionFilterArgs,
        /// Show candidates without archiving anything
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Session stats: totals, per-source counts, database size
    /// (hermes sessions stats)
    Stats,
    /// Delete a specific session and all its messages (hermes sessions delete)
    Delete {
        /// Session ID (or unique prefix) to delete
        id: String,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Set or change a session's title (hermes sessions rename)
    Rename {
        /// Session ID (or unique prefix) to rename
        id: String,
        /// New title for the session
        #[arg(required = true)]
        title: Vec<String>,
    },
    /// Reclaim disk space: merge FTS5 segments + VACUUM, no data change
    /// (hermes sessions optimize)
    Optimize,
    /// Interactive session browser — browse, filter, and resume sessions
    /// (hermes sessions browse)
    Browse {
        /// Filter by source (cli, cron, gateway, ...)
        #[arg(long)]
        source: Option<String>,
        /// Max sessions to load
        #[arg(long, default_value = "500")]
        limit: usize,
    },
    /// Re-title sessions whose auto-title came from a /skill's own text
    /// (hermes sessions retitle-skills)
    #[command(
        long_about = "Sessions opened with a /skill were auto-titled from the expanded \
message, which embeds the whole skill body \u{2014} so the title describes the SKILL, not the \
request. This regenerates those titles from what the user actually typed. Lists what it would \
change unless --apply is passed."
    )]
    RetitleSkills {
        /// Max sessions to scan
        #[arg(long, default_value = "200")]
        limit: usize,
        /// Write the new titles (default: dry run)
        #[arg(long)]
        apply: bool,
    },
    /// Re-title one session from its first exchange via the LLM
    /// (HTTP twin: POST /api/sessions/:id/retitle; P569)
    Retitle {
        /// Session id (prefix ok)
        id: String,
        /// Write the new title (default: dry run)
        #[arg(long)]
        apply: bool,
    },
    /// Import sessions from a portable JSON export
    /// (`GET /api/sessions/:id/export?format=json`; P577)
    Import {
        /// Path to the export JSON file
        file: std::path::PathBuf,
        /// Preview what would be imported without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Repair a malformed state.db schema so hidden sessions reappear
    /// (hermes sessions repair)
    Repair {
        /// Only report whether the database opens cleanly; do not modify it
        #[arg(long)]
        check_only: bool,
        /// Skip the timestamped backup copy (not recommended)
        #[arg(long)]
        no_backup: bool,
    },
}

/// Shared time/filter flags for `sessions prune` / `sessions archive`
/// (hermes session_filters surface). Time values accept durations (`5h`,
/// `30m`, `2d`, `1w`, bare number = days) or ISO timestamps
/// (`2026-07-05`, `2026-07-05 14:30`).
#[derive(clap::Args)]
struct SessionFilterArgs {
    /// Last activity older than this (duration or ISO timestamp)
    #[arg(long)]
    older_than: Option<String>,
    /// Last activity newer than this (duration or ISO timestamp)
    #[arg(long)]
    newer_than: Option<String>,
    /// Session started before this (duration or ISO timestamp)
    #[arg(long)]
    before: Option<String>,
    /// Session started after this (duration or ISO timestamp)
    #[arg(long)]
    after: Option<String>,
    /// Exact source match (cli, cron, gateway, ...)
    #[arg(long)]
    source: Option<String>,
    /// Case-insensitive substring match on the session title
    #[arg(long)]
    title: Option<String>,
    /// Exact end_reason match (e.g. compression, ended)
    #[arg(long)]
    end_reason: Option<String>,
    /// Session cwd under this directory prefix
    #[arg(long)]
    cwd: Option<String>,
    /// Minimum message count
    #[arg(long)]
    min_messages: Option<i64>,
    /// Maximum message count
    #[arg(long)]
    max_messages: Option<i64>,
    /// Case-insensitive substring match on the model id
    #[arg(long)]
    model: Option<String>,
    /// Minimum total tokens (input + output)
    #[arg(long)]
    min_tokens: Option<i64>,
    /// Maximum total tokens (input + output)
    #[arg(long)]
    max_tokens: Option<i64>,
}

impl SessionFilterArgs {
    /// Translate CLI flags into prune filters (hermes `build_prune_filters`).
    fn build(&self) -> Result<ulnclaw::session::filters::PruneFilters, String> {
        use ulnclaw::session::filters::{parse_point_in_time, PruneFilters};
        let mut filters = PruneFilters::default();
        if let Some(value) = &self.older_than {
            let bound = parse_point_in_time(value, "--older-than")?;
            filters.last_active_before = Some(match filters.last_active_before {
                Some(current) => current.min(bound),
                None => bound,
            });
        }
        if let Some(value) = &self.newer_than {
            let bound = parse_point_in_time(value, "--newer-than")?;
            filters.last_active_after = Some(match filters.last_active_after {
                Some(current) => current.max(bound),
                None => bound,
            });
        }
        if let Some(value) = &self.before {
            let bound = parse_point_in_time(value, "--before")?;
            filters.started_before = Some(match filters.started_before {
                Some(current) => current.min(bound),
                None => bound,
            });
        }
        if let Some(value) = &self.after {
            let bound = parse_point_in_time(value, "--after")?;
            filters.started_after = Some(match filters.started_after {
                Some(current) => current.max(bound),
                None => bound,
            });
        }
        if let Some(started_after) = filters.started_after {
            if let Some(started_before) = filters.started_before {
                if started_after >= started_before {
                    return Err(format!(
                        "Empty start-time window: the --after bound ({}) is not earlier than the --before bound ({})",
                        ulnclaw::session::filters::format_epoch(Some(started_after)),
                        ulnclaw::session::filters::format_epoch(Some(started_before))
                    ));
                }
            }
        }
        filters.source = self.source.clone();
        filters.title_like = self.title.clone();
        filters.end_reason = self.end_reason.clone();
        filters.cwd_prefix = self.cwd.clone();
        filters.min_messages = self.min_messages;
        filters.max_messages = self.max_messages;
        filters.model_like = self.model.clone();
        filters.min_tokens = self.min_tokens;
        filters.max_tokens = self.max_tokens;
        Ok(filters)
    }
}

#[derive(Subcommand)]
enum MoaAction {
    /// Run one prompt through a MoA preset and print the synthesis
    Run {
        prompt: Vec<String>,
        /// Preset name (default: `moa.default_preset` or "default")
        #[arg(long)]
        preset: Option<String>,
    },
    /// Show configured presets
    List,
    /// Delete a preset from config.toml
    Delete { name: String },
}

#[derive(Subcommand)]
enum ModelsAction {
    /// List catalog providers (id, name, model count, env vars)
    Providers,
    /// List models for a provider from the catalog (agentic by default)
    List {
        provider: String,
        /// Include non-agentic models (TTS, embeddings, image, ...)
        #[arg(long)]
        all: bool,
        /// Force a catalog refresh before listing
        #[arg(long)]
        refresh: bool,
    },
    /// Show metadata for one model (limits, cost, capabilities)
    Info { provider: String, model: String },
    /// Force-refresh the local models.dev cache
    Refresh,
}

#[derive(Subcommand)]
enum SkillAction {
    List,
    View {
        name: String,
    },
    /// List installed skills that declare a blueprint schedule
    Blueprints,
    /// Schedule a blueprint skill as a cron job (hermes blueprint jobs)
    Schedule {
        name: String,
        /// Custom job name (default: `blueprint:<skill>`)
        #[arg(long)]
        job_name: Option<String>,
    },
    /// Remove the cron job created for a blueprint skill
    Unschedule {
        name: String,
    },
    /// Security-scan an installed skill (hermes skills_guard)
    Scan {
        name: String,
        /// Source identifier for trust resolution (e.g. openai/skills)
        #[arg(long, default_value = "community")]
        source: String,
        /// Emit the scan result as JSON
        #[arg(long)]
        json: bool,
        /// Evaluate the install policy with force override
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum BundlesAction {
    /// Show all bundles (default)
    List,
    /// Dump one bundle's contents
    Show { name: String },
    /// Build a new bundle
    Create {
        name: String,
        /// Skills to include (repeatable)
        #[arg(long = "skill", required = true)]
        skills: Vec<String>,
        /// Bundle description
        #[arg(long)]
        description: Option<String>,
        /// Extra guidance injected above the skill bodies
        #[arg(long)]
        instruction: Option<String>,
        /// Overwrite an existing bundle of the same name
        #[arg(long)]
        overwrite: bool,
    },
    /// Remove a bundle
    Delete { name: String },
    /// Re-scan the bundles directory
    Reload,
}

#[derive(Subcommand)]
enum SecurityAction {
    /// Audit pinned MCP server packages against OSV.dev (hermes security audit)
    Audit {
        /// Emit findings as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SecretsAction {
    /// Show configured sources, helper/bws availability, and token presence
    Status,
    /// Fetch secrets now and report what would change (dry-run by default)
    Sync {
        /// Actually export the winners into the process environment
        #[arg(long)]
        apply: bool,
    },
    /// Bitwarden Secrets Manager backend (hermes `secrets bitwarden`)
    Bitwarden {
        #[command(subcommand)]
        action: BitwardenSecretsAction,
    },
    /// 1Password backend (hermes `secrets onepassword`)
    #[command(name = "onepassword")]
    OnePassword {
        #[command(subcommand)]
        action: OnePasswordSecretsAction,
    },
}

#[derive(Subcommand)]
enum BitwardenSecretsAction {
    /// Interactive wizard: install bws, store access token, pick project
    Setup {
        /// Provide the access token non-interactively (stored in .env)
        #[arg(long)]
        access_token: Option<String>,
        /// Bitwarden region / self-hosted endpoint (skips the prompt)
        #[arg(long)]
        server_url: Option<String>,
        /// Pre-select a project UUID instead of prompting
        #[arg(long)]
        project_id: Option<String>,
    },
    /// Download and verify the pinned bws binary
    Install {
        /// Re-download even if a managed copy already exists
        #[arg(long)]
        force: bool,
    },
    /// Show config + binary + token validation status
    Status,
    /// Rotate the access token: validate a new one, then store it in .env
    Token {
        /// Provide the new token non-interactively (default: masked prompt)
        #[arg(long)]
        access_token: Option<String>,
        /// Store without probing Bitwarden first (not recommended)
        #[arg(long)]
        no_verify: bool,
    },
    /// Turn off the Bitwarden integration
    Disable,
}

#[derive(Subcommand)]
enum OnePasswordSecretsAction {
    /// Wizard: locate op, store service-account token, enable the source
    Setup {
        /// Absolute path to the op binary (default: resolve via PATH)
        #[arg(long)]
        binary_path: Option<String>,
        /// op account shorthand passed as --account
        #[arg(long)]
        account: Option<String>,
        /// Service-account token to store in .env (default: prompt/env)
        #[arg(long)]
        token: Option<String>,
    },
    /// Show config + op binary + references
    Status,
    /// Map an env var to an op:// reference
    Set {
        /// Environment variable name
        name: String,
        /// op://vault/item/field reference
        reference: String,
    },
    /// Remove an env-var → reference mapping
    Remove {
        /// Environment variable name
        name: String,
    },
    /// Turn off the 1Password integration
    Disable,
}

#[derive(Subcommand)]
enum WebhookAction {
    /// Create or update a dynamic webhook subscription
    Subscribe {
        /// Subscription name (lowercase alphanumeric, hyphens/underscores)
        name: String,
        /// Human-readable description
        #[arg(long)]
        description: Option<String>,
        /// Comma-separated event-type filter (empty = all events)
        #[arg(long)]
        events: Option<String>,
        /// HMAC secret (random when omitted)
        #[arg(long)]
        secret: Option<String>,
        /// Prompt template ({body}/{event} substituted)
        #[arg(long)]
        prompt: Option<String>,
        /// Comma-separated skills to attach to the route
        #[arg(long)]
        skills: Option<String>,
        /// Delivery target: log, telegram, discord, slack, whatsapp_cloud
        #[arg(long)]
        deliver: Option<String>,
        /// Chat/channel id for the delivery target
        #[arg(long)]
        deliver_chat_id: Option<String>,
        /// Deliver the rendered prompt directly (no agent turn)
        #[arg(long)]
        deliver_only: bool,
        /// Script command recorded on the route
        #[arg(long)]
        script: Option<String>,
    },
    /// List dynamic webhook subscriptions
    List,
    /// Remove a dynamic webhook subscription
    Remove {
        /// Subscription name
        name: String,
    },
    /// POST a signed test payload to a subscription
    Test {
        /// Subscription name
        name: String,
        /// JSON payload (default: built-in test event)
        #[arg(long)]
        payload: Option<String>,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Start the RFC 8628 device flow and wait for authorization
    Login,
    /// Show current login state
    Status,
    /// Refresh the access token using the stored refresh_token
    Refresh,
    /// Remove stored tokens
    Logout,
    /// Print the portal URL (open it manually)
    Open,
}

#[derive(Subcommand)]
enum SyncAction {
    /// Show sync state: gate, opt-ins, device, manifest summary
    Status,
    /// Pull remote skills into the local skills directory
    Pull,
    /// Push opted-in skills to the sync endpoint
    Push,
    /// Reconcile now: pull then push
    Now,
    /// Opt a skill into sync
    Enable { skill: String },
    /// Opt a skill out of sync
    Disable { skill: String },
    /// Show or set this device's label
    Device {
        /// New device label
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum PluginsAction {
    /// List discovered plugins, their hooks and tools
    List,
    /// Enable a disabled plugin
    Enable { name: String },
    /// Disable a plugin
    Disable { name: String },
    /// Add every pending [hooks] command to the consent allowlist
    AcceptHooks,
    /// Install a plugin from a Git URL or owner/repo shorthand (hermes plugins install)
    Install {
        /// Git URL, owner/repo, or owner/repo/path/to/plugin
        identifier: String,
        /// Reinstall over an existing plugin directory
        #[arg(long)]
        force: bool,
        /// Enable immediately without prompting
        #[arg(long, conflicts_with = "no_enable")]
        enable: bool,
        /// Install disabled (skip the enable prompt)
        #[arg(long)]
        no_enable: bool,
    },
    /// Update an installed plugin from its git remote (hermes plugins update)
    Update { name: String },
    /// Remove an installed plugin directory (hermes plugins remove)
    Remove { name: String },
}

#[derive(Subcommand)]
enum ProxyAction {
    /// Run the proxy in the foreground (Ctrl+C stops)
    Start {
        /// Bind host (default 127.0.0.1, [proxy] host)
        #[arg(long)]
        host: Option<String>,
        /// Bind port (default 8645, [proxy] port)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Show whether the OAuth upstream adapter is ready
    Status,
    /// List available upstream providers
    Providers,
}

/// `ulnclaw egress` subcommands (hermes `hermes_cli/proxy_cli.py`).
#[derive(Subcommand)]
enum EgressAction {
    /// Download the pinned iron-proxy binary
    Install {
        /// Re-download even if a managed copy already exists
        #[arg(long)]
        force: bool,
    },
    /// Wizard: install + CA + mint tokens + write config
    Setup {
        /// Override the tunnel port (default 9090)
        #[arg(long)]
        tunnel_port: Option<u16>,
        /// Discover provider keys from secrets.bitwarden instead of env
        #[arg(long)]
        from_bitwarden: bool,
        /// Explicitly switch credential_source back to env on re-setup
        #[arg(long)]
        no_bitwarden: bool,
        /// Mint fresh proxy tokens for every provider
        #[arg(long)]
        rotate_tokens: bool,
        /// Restart a running daemon automatically after writing config
        #[arg(long, conflicts_with = "no_restart")]
        restart: bool,
        /// Do not restart a running daemon after setup
        #[arg(long)]
        no_restart: bool,
    },
    /// Start the managed iron-proxy
    Start,
    /// Stop the managed iron-proxy
    Stop,
    /// Restart the managed iron-proxy
    Restart,
    /// Hot-reload the running daemon's ruleset from proxy.yaml
    Reload,
    /// Show proxy state and mappings
    Status {
        /// Print the proxy tokens (default: redacted prefix only)
        #[arg(long)]
        show_tokens: bool,
    },
    /// Turn off the proxy integration
    Disable,
    /// Print the generated proxy.yaml path
    Config,
}

#[derive(Subcommand)]
enum PairingAction {
    /// Show pending pairing requests and approved users
    List,
    /// Approve a pairing request by the code the bot DM'd or its request id
    Approve { platform: String, code: String },
    /// Revoke a paired user's access
    Revoke { platform: String, user_id: String },
    /// Clear pending pairing codes (all platforms unless one is given)
    ClearPending { platform: Option<String> },
}

#[derive(Subcommand)]
enum ProfilesAction {
    /// List configured profiles (default)
    List,
    /// Show one profile's override as TOML
    Show { name: String },
    /// Create or replace a profile override
    Set {
        name: String,
        /// Model provider (openai, anthropic, …) — set together with --model
        #[arg(long)]
        provider: Option<String>,
        /// Model name — set together with --provider
        #[arg(long)]
        model: Option<String>,
        /// Custom base URL for the provider
        #[arg(long)]
        base_url: Option<String>,
        /// Sampling temperature
        #[arg(long)]
        temperature: Option<f32>,
        /// Comma-separated toolsets to enable (empty string clears the override)
        #[arg(long)]
        enable: Option<String>,
        /// Comma-separated toolsets to disable (empty string clears the override)
        #[arg(long)]
        disable: Option<String>,
    },
    /// Rename a profile
    Rename { name: String, new_name: String },
    /// Delete a profile
    Delete { name: String },
}

#[derive(Subcommand)]
enum HooksAction {
    /// List configured shell hooks and their consent state
    List,
    /// Fire one event with its default payload (or --payload-file) and print responses
    Test {
        event: String,
        /// JSON file to feed as the payload instead of the built-in default
        #[arg(long)]
        payload_file: Option<std::path::PathBuf>,
    },
    /// Revoke consent for every hook entry whose command equals COMMAND
    Revoke { command: String },
    /// Run every consented hook with its default payload and report failures
    Doctor,
}

#[derive(Subcommand)]
enum ComputerUseAction {
    /// Print whether cua-driver is installed, its version, and the config
    Status,
    /// Run cua-driver's health_report and render the check matrix
    Doctor {
        /// Emit the raw structured payload as JSON
        #[arg(long)]
        json: bool,
    },
    /// Install or upgrade cua-driver via the upstream installer script
    Install {
        /// Re-run the installer even when cua-driver is already found
        #[arg(long)]
        upgrade: bool,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum CuratorAction {
    /// Skill usage summary (states, provenance, pins, unmanaged)
    Status,
    /// Run the deterministic auto-transition pass now (stale/archive/reactivate)
    Run {
        /// Preview without applying transitions
        #[arg(long)]
        dry_run: bool,
    },
    /// Pause the curator (auto-transition passes refuse to apply)
    Pause,
    /// Resume a paused curator
    Resume,
    /// Pin a skill so auto-transitions never touch it
    Pin { skill: String },
    /// Unpin a skill
    Unpin { skill: String },
    /// Archive a skill now (recoverable via restore)
    Archive { skill: String },
    /// Restore an archived skill
    Restore { skill: String },
    /// List archived (recoverable) skills
    ListArchived,
    /// Usage telemetry table for every skill on disk
    Usage {
        /// Sort order: activity (default), name, or recent
        #[arg(long, default_value = "activity")]
        sort: String,
        /// Emit rows as JSON
        #[arg(long)]
        json: bool,
    },
    /// Bulk-archive unpinned agent-created skills idle for >= N days
    Prune {
        /// Idle threshold in days
        #[arg(long, default_value = "90")]
        days: u64,
        /// Preview without archiving
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Hand unmanaged skills to the curator (stamps provenance)
    Adopt {
        /// Skill name(s) to adopt
        skill: Vec<String>,
        /// Adopt every unmanaged skill
        #[arg(long)]
        all_unmanaged: bool,
        /// Preview without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// List skills with no provenance marker
    ListUnmanaged,
}

#[derive(Subcommand)]
enum JourneyAction {
    /// List node ids (for delete/edit)
    List {
        #[arg(long)]
        no_color: bool,
    },
    /// Delete a learned skill (archived) or memory by node id
    Delete {
        /// Node id (skill name or memory:<source>:<index>; see `journey list`)
        node: String,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Edit a learned skill or memory by node id in $EDITOR
    Edit {
        /// Node id (skill name or memory:<source>:<index>; see `journey list`)
        node: String,
    },
}

#[derive(Subcommand)]
enum CronAction {
    List,
    Remove {
        id: String,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    /// Run a job once, immediately (unattended: cron approval mode applies)
    Run {
        id: String,
    },
    /// Create a cron job (hermes `cron create`)
    Create {
        /// Job name
        #[arg(long)]
        name: String,
        /// Schedule: cron expression, `@every 30m`, or `@at <unix-ts>`
        #[arg(long)]
        schedule: String,
        /// Prompt the agent runs each fire
        #[arg(long)]
        prompt: String,
        /// Delivery target: origin/local/platform/platform:chat[:thread]/all
        #[arg(long)]
        deliver: Option<String>,
        /// Skills to load, comma-separated
        #[arg(long)]
        skills: Option<String>,
        /// Runs remaining (omit = forever)
        #[arg(long)]
        repeat: Option<i64>,
    },
    /// Show one job in full (hermes `cron show`)
    Show {
        id: String,
    },
    /// Automation blueprints: list the catalog, or fill + create one
    /// inline (hermes `cron blueprints`)
    Blueprints {
        /// Blueprint name (omit to list the catalog)
        name: Option<String>,
        /// Inline slot values (slot=val …)
        values: Vec<String>,
    },
}

#[derive(Subcommand)]
enum CheckpointAction {
    /// List checkpoints for a directory (default: cwd)
    List {
        /// Directory to inspect (default: current directory)
        dir: Option<String>,
    },
    /// Show the shared checkpoint store status (projects, sizes)
    Status,
    /// Restore a directory (or a single file) to a checkpoint
    Restore {
        /// Checkpoint hash (short or full)
        hash: String,
        /// Optional single file to restore
        file: Option<String>,
        /// Directory the checkpoint belongs to (default: cwd)
        #[arg(long)]
        dir: Option<String>,
    },
    /// Preview the diff between the working tree and a checkpoint
    Diff {
        /// Checkpoint hash (short or full)
        hash: String,
        /// Directory the checkpoint belongs to (default: cwd)
        #[arg(long)]
        dir: Option<String>,
    },
    /// Delete orphan/stale checkpoints and reclaim store space
    Prune {
        /// Retention window in days (default: [checkpoints] retention_days)
        #[arg(long)]
        days: Option<u64>,
    },
}

fn load_config(cli: &Cli) -> UlncLawConfig {
    let mut config =
        UlncLawConfig::load(cli.config.as_ref().map(std::path::Path::new)).unwrap_or_default();
    if let Some(ref profile) = cli.profile {
        config = config.with_profile(profile);
    }
    // CLI -m / --provider win over config AND profile (hermes -m
    // semantics): kanban workers pinned via task overrides ride these
    // same flags.
    if let Some(model) = cli
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        config.model.model = model.to_string();
    }
    if let Some(provider) = cli
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        config.model.provider = provider.to_string();
    }
    // CLI --reasoning wins over config AND profile (hermes --reasoning
    // semantics): kanban workers pinned via task overrides ride this
    // same flag. A typo'd level is a hard error, not a silent fallback.
    if cli.reasoning.is_some() {
        match ulnclaw::kanban::normalize_reasoning_effort(cli.reasoning.as_deref()) {
            Ok(Some(level)) => config.agent.reasoning_effort = level,
            Ok(None) => config.agent.reasoning_effort = String::new(),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }
    }
    // CLI --fast wins over config (hermes Priority Processing toggle):
    // only meaningful for OpenAI-flagship models; other providers ignore
    // the pin.
    if cli.fast {
        config.agent.service_tier = "fast".to_string();
    }
    // Resolve the active theme once per process (hermes init_skin_from_config).
    ulnclaw::skin::init_skin_from_config(&config);
    config
}

fn build_provider(
    config: &UlncLawConfig,
    session_id: Option<&str>,
) -> Result<Arc<dyn ulnclaw::provider::Provider>, String> {
    build_provider_with(config, session_id, false)
}

/// Like [`build_provider`] with an explicit keyless policy. The gateway
/// stack passes `keyless_ok = true` so a fresh install with no provider
/// key still boots — the UI surfaces setup guidance (onboarding, Models
/// view) and runs fail with the provider's own auth error until a key
/// lands in `[model] api_key`, the credentials pool or the environment
/// and the gateway restarts. CLI entry points stay strict.
fn build_provider_with(
    config: &UlncLawConfig,
    session_id: Option<&str>,
    keyless_ok: bool,
) -> Result<Arc<dyn ulnclaw::provider::Provider>, String> {
    // Persistent MoA facade (hermes build_moa_facade): `[model] provider
    // = "moa"` runs the whole agent loop on the preset — `model` selects
    // the preset. Slot keys resolve per preset, no main key required.
    if config.model.provider.trim().eq_ignore_ascii_case("moa") {
        let facade = ulnclaw::moa::MoaProvider::new(config.clone(), session_id.map(str::to_string))
            .map_err(|e| e.to_string())?;
        eprintln!(
            "🤖 AI Agent initialized with MoA preset: {}",
            facade.preset_name()
        );
        return Ok(Arc::new(facade));
    }
    let api_key = config.resolve_api_key();
    let keyless = matches!(
        config.model.provider.as_str(),
        "ollama" | "llamacpp" | "llama_cpp" | "local"
    );
    if api_key.is_none() && !keyless {
        if keyless_ok {
            eprintln!(
                "gateway: no API key configured — starting keyless; add a provider key \
                 (Models view, credentials pool, or OPENAI_API_KEY/ANTHROPIC_API_KEY) \
                 and restart the gateway to enable chat."
            );
        } else {
            return Err(
                "No API key found. Set OPENAI_API_KEY / ANTHROPIC_API_KEY (or api_key in config.toml)."
                    .into(),
            );
        }
    }
    if config.model.provider == "anthropic" {
        let mut builder = ulnclaw::provider::anthropic::AnthropicProvider::builder()
            .endpoint(&config.resolve_base_url())
            .model(&config.model.model)
            .name(&config.model.provider)
            .max_retries(config.model.max_retries);
        if let Some(ref key) = api_key {
            builder = builder.api_key(key);
        }
        return Ok(Arc::new(builder.build().map_err(|e| e.to_string())?));
    }
    let mut builder = OpenAiProvider::builder()
        .endpoint(&config.resolve_base_url())
        .model(&config.model.model)
        .name(&config.model.provider)
        .max_retries(config.model.max_retries);
    if let Some(ref key) = api_key {
        builder = builder.api_key(key);
    }
    // Pinned reasoning effort (hermes agent.reasoning_effort): empty =
    // endpoint default.
    let effort = config.agent.reasoning_effort.trim();
    if !effort.is_empty() {
        builder = builder.reasoning_effort(effort);
    }
    // Priority Processing pin (hermes agent.service_tier): "fast" maps
    // to service_tier=priority; anything else keeps the endpoint default.
    if config
        .agent
        .service_tier
        .trim()
        .eq_ignore_ascii_case("fast")
    {
        builder = builder.service_tier("priority");
    }
    let provider = builder.build().map_err(|e| e.to_string())?;
    Ok(Arc::new(provider))
}

#[tokio::main]
async fn main() {
    // Rust blocks SIGPIPE by default; restore the default handler so piping
    // into `head` (e.g. `ulnclaw completion bash | head`) exits silently
    // instead of panicking on a closed stdout.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let config = load_config(&cli);

    // Apply external secret sources before anything runs (hermes env_loader
    // behavior): fetch enabled sources, merge with process env ∪ .env, and
    // export the winners. Single-threaded pre-dispatch, never fatal.
    let secrets_report =
        ulnclaw::secrets::apply_all(&config.secrets, &ulnclaw::config::ulnclaw_home());
    for err in &secrets_report.errors {
        eprintln!("[secrets] warning: {err}");
    }

    // Discover plugins + consented shell hooks (hermes plugin manager +
    // shell_hooks registration at startup).
    let plugin_warnings = ulnclaw::plugins::init(&ulnclaw::config::ulnclaw_home(), &config).await;
    for warning in &plugin_warnings {
        eprintln!("[plugins] {warning}");
    }

    let result = dispatch(cli, config).await;
    // Hermes atexit parity: release any cloud browser session so paid
    // backends do not leak orphaned sessions after a clean exit.
    ulnclaw::browser::cloud::shutdown_cloud_sessions().await;
    if let Err(e) = result {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

/// Build one full gateway stack (approval router + agent + state + cron)
/// rooted at `home`. Used for the default gateway and for each multiplex
/// `/p/<profile>` mirror (profile-scoped home).
async fn build_gateway_stack(
    config: &UlncLawConfig,
    home: &std::path::Path,
    gateway_key: Option<String>,
) -> Result<Arc<ulnclaw::gateway::GatewayState>, String> {
    std::fs::create_dir_all(home).ok();
    let router = ulnclaw::gateway::ApprovalRouter::with_options(
        std::time::Duration::from_secs(config.approvals.timeout),
        Some(home.join("approvals.json")),
    );
    let state_holder: Arc<tokio::sync::OnceCell<Arc<ulnclaw::gateway::GatewayState>>> =
        Arc::new(tokio::sync::OnceCell::new());
    let approve = ulnclaw::gateway::gateway_approve_fn(router.clone(), state_holder.clone());
    // Messaging turns render approvals in-chat (adaptive cards on Teams,
    // /approve text elsewhere) instead of the run-based HTTP flow.
    let approve = ulnclaw::messaging::messaging_aware_approve_fn(approve);
    let agent = make_agent_in(config, false, Some(approve), home, None, true).await?;
    agent.context().set_async_delivery(true);
    // P723: placeholder-aware terminal cwd (hermes cwd_placeholder) —
    // messaging turns start in the resolved cwd when one applies.
    if let Some(cwd) = ulnclaw::cwd_placeholder::resolve_messaging_cwd(config) {
        agent.context().set_cwd(cwd);
    }
    let state = ulnclaw::gateway::GatewayState::new(
        agent,
        config.model.model.clone(),
        config.model.provider.clone(),
        gateway_key,
        router,
    )
    .map_err(|e| e.to_string())?;
    state_holder.set(state.clone()).ok();
    let cron_store =
        ulnclaw::cron::CronStore::open(&home.join("state.db")).map_err(|e| e.to_string())?;
    state.cron.set(std::sync::Arc::new(cron_store)).ok();
    state.skills_dir.set(home.join("skills")).ok();
    // Cron scheduler: dispatch due jobs as tracked cron runs (hermes
    // scheduler loop). 30s polling matches the job-timing granularity.
    ulnclaw::gateway::spawn_cron_scheduler(state.clone(), 30);
    // Gateway monitoring: OTLP health/diagnostics export + heartbeat
    // sampler (hermes agent/monitoring streamer). No-op unless
    // [monitoring] is enabled with an OTLP endpoint.
    {
        let platform_count = [
            config.messaging.telegram.enabled,
            config.messaging.discord.enabled,
            config.messaging.slack.enabled,
            config.messaging.signal.enabled,
            config.messaging.weixin.enabled,
            config.messaging.qq.enabled,
            config.messaging.yuanbao.enabled,
            config.messaging.email.enabled,
            config.messaging.mattermost.enabled,
            config.messaging.matrix.enabled,
            config.messaging.dingtalk.enabled,
            config.messaging.wecom.enabled,
            config.messaging.feishu.enabled,
            config.messaging.homeassistant.enabled,
            config.messaging.sms.enabled,
            config.messaging.whatsapp.enabled,
            config.messaging.irc.enabled,
            config.messaging.ntfy.enabled,
            config.messaging.simplex.enabled,
            config.messaging.teams.enabled,
            config.messaging.line.enabled,
            config.messaging.google_chat.enabled,
            config.messaging.buzz.enabled,
            config.messaging.photon.enabled,
            config.messaging.raft.enabled,
            config.messaging.a2a.enabled,
        ]
        .iter()
        .filter(|e| **e)
        .count() as u64;
        ulnclaw::gateway::spawn_monitoring(state.clone(), &config, platform_count);
    }
    if config.kanban.dispatch_in_gateway {
        // Provider factory for the auto-decompose tick: builds from LIVE
        // config each call so [auxiliary] / model edits take effect
        // without a gateway restart (hermes #49638 semantics).
        let provider_factory: ulnclaw::gateway::DispatcherProviderFactory =
            std::sync::Arc::new(|| {
                let live = ulnclaw::config::UlncLawConfig::load(None).unwrap_or_default();
                build_provider(&live, None)
            });
        ulnclaw::gateway::spawn_kanban_dispatcher(
            config.kanban.dispatch_interval_secs,
            config.kanban.max_spawn,
            config.kanban.worktrees,
            Some(provider_factory),
        );
    }
    Ok(state)
}

/// P709: one profile's messaging stack for inbound profile routing
/// (hermes multiplexer per-profile runtime): the same isolation as a
/// `/p/<name>` HTTP mirror — `[profiles.<name>]` override + a
/// profile-scoped home — without any background loops (the cron
/// scheduler, monitoring and kanban dispatchers run once per process,
/// owned by the default stack).
async fn build_profile_messaging_stack(
    config: &UlncLawConfig,
    name: &str,
    home: &std::path::Path,
    gateway_key: Option<String>,
) -> std::result::Result<std::sync::Arc<ulnclaw::messaging::Dispatcher>, String> {
    let profile_config = config.clone().with_profile(name);
    let profile_home = home.join("profiles").join(name);
    std::fs::create_dir_all(&profile_home).ok();
    let router = ulnclaw::gateway::ApprovalRouter::with_options(
        std::time::Duration::from_secs(profile_config.approvals.timeout),
        Some(profile_home.join("approvals.json")),
    );
    let state_holder: std::sync::Arc<
        tokio::sync::OnceCell<std::sync::Arc<ulnclaw::gateway::GatewayState>>,
    > = std::sync::Arc::new(tokio::sync::OnceCell::new());
    let approve = ulnclaw::gateway::gateway_approve_fn(router.clone(), state_holder.clone());
    let approve = ulnclaw::messaging::messaging_aware_approve_fn(approve);
    let agent = make_agent_in(
        &profile_config,
        false,
        Some(approve),
        &profile_home,
        None,
        true,
    )
    .await?;
    agent.context().set_async_delivery(true);
    // P723: placeholder-aware terminal cwd for profile gateways too.
    if let Some(cwd) = ulnclaw::cwd_placeholder::resolve_messaging_cwd(&profile_config) {
        agent.context().set_cwd(cwd);
    }
    let state = ulnclaw::gateway::GatewayState::new(
        agent.clone(),
        profile_config.model.model.clone(),
        profile_config.model.provider.clone(),
        gateway_key,
        router,
    )
    .map_err(|e| e.to_string())?;
    state_holder.set(state.clone()).ok();
    Ok(ulnclaw::messaging::Dispatcher::new(
        agent,
        state.store.clone(),
    ))
}

/// `ulnclaw dashboard [run|status|stop]` — hermes `dashboard`/`serve`
/// parity. The gateway process serves the dashboard, so `run` delegates to
/// `gateway_cmd`; `status`/`stop` inspect/terminate the recorded pid.
async fn dashboard_cmd(
    config: &UlncLawConfig,
    action: &str,
    host: Option<String>,
    port: Option<u16>,
) -> Result<(), String> {
    match action {
        "run" | "start" => gateway_cmd(config, host, port, false, false).await,
        "status" => {
            let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
            let gateway = config.gateway.resolved();
            match ulnclaw::gateway_pidfile::running_gateway_pid(&home) {
                Some(pid) => {
                    println!("dashboard: running (pid {pid})");
                    println!("  url:     http://{}:{}", gateway.host, gateway.port);
                    println!(
                        "  pidfile: {}",
                        ulnclaw::gateway_pidfile::pidfile_path(&home).display()
                    );
                }
                None => println!("dashboard: not running"),
            }
            Ok(())
        }
        "stop" => {
            let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
            match ulnclaw::gateway_pidfile::running_gateway_pid(&home) {
                Some(pid) => {
                    ulnclaw::gateway_pidfile::replace_running(
                        pid,
                        std::time::Duration::from_secs(10),
                    )?;
                    println!("dashboard: stopped (pid {pid})");
                }
                None => println!("dashboard: not running"),
            }
            Ok(())
        }
        other => Err(format!(
            "Unknown dashboard action: {other} (expected run, status, or stop)"
        )),
    }
}

/// Quote-aware whitespace tokenizer for console lines.
fn console_tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';
    for ch in line.chars() {
        if in_quotes {
            if ch == quote_char {
                in_quotes = false;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quotes = true;
            quote_char = ch;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// `ulnclaw console` — safe non-LLM command console (hermes console
/// parity): an interactive REPL that re-parses each line as CLI arguments
/// and dispatches it through the normal command handlers.
async fn console_cmd(config: &UlncLawConfig) -> Result<(), String> {
    println!("ulnclaw console — safe command console (no LLM).");
    println!(
        "Type any subcommand without the 'ulnclaw' prefix (e.g. 'sessions list', 'status', 'tools')."
    );
    println!("'help' shows the command list; 'exit' or Ctrl-D leaves.");
    let stdin = std::io::stdin();
    loop {
        use std::io::Write;
        print!("> ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            println!();
            break;
        }
        let tokens = console_tokenize(line.trim());
        if tokens.is_empty() {
            continue;
        }
        match tokens[0].as_str() {
            "exit" | "quit" => break,
            "help" => {
                if let Err(e) = Cli::try_parse_from(["ulnclaw", "--help"]) {
                    print!("{e}");
                }
                continue;
            }
            "chat" | "console" => {
                eprintln!(
                    "console: '{}' is not available inside the console.",
                    tokens[0]
                );
                continue;
            }
            _ => {}
        }
        let argv: Vec<String> = std::iter::once("ulnclaw".to_string())
            .chain(tokens)
            .collect();
        match Cli::try_parse_from(&argv) {
            Ok(cli) => {
                if let Err(e) = dispatch(cli, config.clone()).await {
                    eprintln!("error: {e}");
                }
            }
            Err(e) => eprint!("{e}"),
        }
    }
    Ok(())
}

async fn gateway_cmd(
    config: &UlncLawConfig,
    host: Option<String>,
    port: Option<u16>,
    replace: bool,
    force: bool,
) -> Result<(), String> {
    let mut gateway = config.gateway.resolved();
    if let Some(host) = host {
        gateway.host = host;
    }
    if let Some(port) = port {
        gateway.port = port;
    }
    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
    // P701: lifecycle ledger — report any unclean previous death
    // (SIGKILL / OOM / host death) before claiming the sentinel for
    // this life (hermes lifecycle_ledger record_startup).
    ulnclaw::lifecycle_ledger::record_startup(&home);
    // P698: restart-loop breaker — a stale pidfile (file present but the
    // recorded process dead) means the previous instance died without a
    // clean shutdown. Captured before the --replace kill so a deliberate
    // takeover never counts as a crash boot (hermes restart_loop_guard).
    let crash_boot = ulnclaw::gateway_pidfile::pidfile_path(&home).exists()
        && ulnclaw::gateway_pidfile::running_gateway_pid(&home).is_none();
    // Single-instance guard (hermes `gateway run [--replace]` contract):
    // the pidfile carries pid + start token, so a recycled pid never
    // blocks a fresh start.
    if let Some(pid) = ulnclaw::gateway_pidfile::running_gateway_pid(&home) {
        if replace {
            println!("gateway: replacing running instance (pid {pid})...");
            ulnclaw::gateway_pidfile::replace_running(pid, std::time::Duration::from_secs(10))?;
        } else if !force {
            return Err(format!(
                "Another gateway instance is already running (PID {pid}).\n                   Use 'ulnclaw gateway --replace' to take over,\n                   or '--force' to run alongside it (two dispatchers will race)."
            ));
        }
    }
    let state = build_gateway_stack(config, &home, gateway.key.clone()).await?;
    if crash_boot {
        ulnclaw::restart_loop_guard::check_and_record(&home, None);
    }
    if config.logging.memory_monitor {
        ulnclaw::memory_monitor::start(std::time::Duration::from_secs(
            config.logging.memory_monitor_interval_secs,
        ));
    }
    // One kanban notification delivery loop per gateway process (the
    // kanban store is shared; multiplex profile stacks must not spawn
    // their own notifiers or deliveries would duplicate).
    ulnclaw::gateway::spawn_kanban_notifier(Some(ulnclaw::gateway::WakeEndpoint {
        host: gateway.host.clone(),
        port: gateway.port,
        key: gateway.key.clone(),
    }));

    // `/p/<profile>` multiplexing (hermes api_server parity): every route
    // is mirrored under `/p/<profile>/...`. With `[gateway]
    // multiplex_profiles = true` each mirror is backed by its own stack
    // built from the `[profiles.<name>]` override; otherwise the prefix is
    // accepted and served by the default profile.
    let hub = {
        let multiplex = gateway.multiplex_profiles;
        let profiles: std::collections::HashSet<String> = config.profiles.keys().cloned().collect();
        let base_config = config.clone();
        let base_home = home.clone();
        let gateway_key = gateway.key.clone();
        let builder: ulnclaw::gateway::ProfileRouterBuilder = Arc::new(move |name: String| {
            let config = base_config.clone();
            let home = base_home.clone();
            let key = gateway_key.clone();
            Box::pin(async move {
                let profile_config = config.with_profile(&name);
                let profile_home = home.join("profiles").join(&name);
                let state = build_gateway_stack(&profile_config, &profile_home, key)
                    .await
                    .map_err(|e| {
                        ulnclaw::error::AgentError::config(format!(
                            "profile '{}' gateway stack: {}",
                            name, e
                        ))
                    })?;
                Ok(ulnclaw::gateway::router(state))
            })
        });
        let scope_base = home.clone();
        let scope_builder: ulnclaw::gateway::ProfileScopeBuilder = Arc::new(move |name: &str| {
            // Profile secret scope (hermes `_profile_runtime_scope`):
            // `<home>/profiles/<name>/.env` + hydrated external sources,
            // installed around every `/p/<name>/...` request.
            ulnclaw::secret_scope::build_profile_secret_scope(
                &scope_base.join("profiles").join(name),
            )
        });
        ulnclaw::gateway::ProfileHub::new(
            multiplex,
            profiles,
            ulnclaw::gateway::router(state.clone()),
            builder,
            Some(scope_builder),
        )
    };

    // Messaging platform gateways (hermes gateway/platforms): platforms
    // live in the gateway process alongside the HTTP API. The gate
    // covers every `[messaging.*]` section — `run_messaging` starts
    // each enabled platform loop (senders register on startup, which
    // cron delivery and clarify routing depend on).
    //
    // P709: inbound profile routing (hermes `gateway.profile_routes`):
    // with multiplexing on and routes configured, matching inbound
    // chats run under the routed profile's agent/home stack instead of
    // the default profile's.
    let profile_routing_hub: Option<std::sync::Arc<ulnclaw::messaging::ProfileRoutingHub>> = {
        if gateway.multiplex_profiles && !gateway.profile_routes.is_empty() {
            let known_profiles: std::collections::HashSet<String> =
                config.profiles.keys().cloned().collect();
            let specs: Vec<ulnclaw::profile_routing::ProfileRouteSpec> = gateway
                .profile_routes
                .iter()
                .filter(|spec| {
                    let profile = spec.profile.trim();
                    let known = known_profiles.contains(profile);
                    if !known {
                        eprintln!(
                            "[gateway] profile route '{}' targets unknown profile '{}' — skipped",
                            spec.name, profile
                        );
                    }
                    known
                })
                .cloned()
                .collect();
            let routes = ulnclaw::profile_routing::parse_profile_routes(&specs);
            if routes.is_empty() {
                None
            } else {
                let base_config = config.clone();
                let base_home = home.clone();
                let gateway_key = gateway.key.clone();
                let factory: std::sync::Arc<dyn ulnclaw::messaging::ProfileDispatcherFactory> =
                    std::sync::Arc::new(move |name: String| {
                        let config = base_config.clone();
                        let home = base_home.clone();
                        let key = gateway_key.clone();
                        async move { build_profile_messaging_stack(&config, &name, &home, key).await }
                    });
                eprintln!(
                    "[gateway] profile routing active: {} route(s), most-specific-first",
                    routes.len()
                );
                Some(ulnclaw::messaging::ProfileRoutingHub::new(routes, factory))
            }
        } else {
            None
        }
    };
    let msg = &config.messaging;
    if msg.any_platform_enabled() {
        let messaging_config = config.clone();
        let agent = state.agent.clone();
        let store = state.store.clone();
        let routing = profile_routing_hub.clone();
        tokio::spawn(async move {
            ulnclaw::messaging::run_messaging(&messaging_config, agent, store, routing).await;
        });
    }
    if msg.bluebubbles.enabled {
        let bluebubbles_config = msg.bluebubbles.clone();
        tokio::spawn(async move {
            ulnclaw::webhook_platforms::bluebubbles_startup(bluebubbles_config).await;
        });
    }

    // Gateway restart notification (hermes
    // `gateway_restart_notification` parity): ping each platform's
    // most recent channel so messaging users see the bot came back.
    if config.messaging.gateway_restart_notification {
        tokio::spawn(async move {
            ulnclaw::messaging::announce_gateway_restart(std::time::Duration::from_secs(5)).await;
        });
    }

    // P700: delivery-ledger recovery — redeliver orphaned final replies
    // once platform senders have registered (hermes delivery_ledger
    // sweep_recoverable; delayed so the connect loops beat us to it).
    {
        let ledger_store = state.store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let redelivered = ulnclaw::delivery_ledger::sweep_and_redeliver(&ledger_store).await;
            if redelivered > 0 {
                tracing::info!(
                    "[delivery_ledger] redelivered {redelivered} orphaned reply(ies) after restart"
                );
            }
        });
    }

    // P701: lifecycle heartbeat — the sentinel carries a memory sample
    // so a later unclean-death report can classify OOM cycles.
    ulnclaw::lifecycle_ledger::start_heartbeat(home.clone());

    // P712: systemd sd_notify watchdog (hermes systemd_notify): armed
    // when `[gateway] systemd_watchdog_seconds` > 0 and systemd
    // stamped WATCHDOG_USEC; READY=1 fires once the listener binds
    // (inside serve_multiplex), STOPPING=1 on the way down.
    ulnclaw::systemd_notify::arm_watchdog(gateway.systemd_watchdog_seconds);

    // P717: snapshot the running binary so health surfaces can detect
    // an in-place update underneath the gateway (hermes code_skew).
    ulnclaw::code_skew::record_boot_fingerprint();

    // Register this instance in the pidfile; a startup failure or a
    // clean shutdown removes it (best-effort — a stale file only
    // weakens the guard, and the liveness check self-heals it).
    let pidfile = ulnclaw::gateway_pidfile::pidfile_path(&home);
    let pidfile_written = ulnclaw::gateway_pidfile::write_pidfile(&home).is_ok();
    let result =
        ulnclaw::gateway::serve_multiplex(state, Some(hub), &gateway.host, gateway.port).await;
    if pidfile_written {
        let _ = std::fs::remove_file(&pidfile);
    }
    ulnclaw::systemd_notify::stop_watchdog().await;
    ulnclaw::memory_monitor::stop();
    if result.is_ok() {
        // Clean shutdown: clear the crash-boot history so only genuine
        // crashes accumulate in the breaker window (hermes clear()).
        ulnclaw::restart_loop_guard::clear(&home);
        ulnclaw::lifecycle_ledger::mark_exited(&home, Some(0), "graceful_shutdown");
    } else {
        ulnclaw::lifecycle_ledger::mark_exited(&home, Some(1), "serve_error");
    }
    // P733: cgroup orphan reaper (hermes cgroup_cleanup, which systemd
    // runs as ExecStopPost after the gateway exits): SIGKILL long-lived
    // helpers the gateway doesn't track (adb, platform bridges, ...) so
    // they don't block Restart=always. Runs last, when nothing of ours
    // should still be draining.
    let reaped = ulnclaw::cgroup_cleanup::reap_cgroup(None);
    if reaped > 0 {
        tracing::info!(
            "[cgroup_cleanup] reaped {reaped} orphaned process(es) from the unit cgroup"
        );
    }
    result.map_err(|e| e.to_string())
}

async fn checkpoints_cmd(config: &UlncLawConfig, action: CheckpointAction) -> Result<(), String> {
    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
    let manager =
        ulnclaw::checkpoint::CheckpointManager::new(home.join("checkpoints"), &config.checkpoints);
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());
    match action {
        CheckpointAction::List { dir } => {
            let dir = dir.unwrap_or(cwd);
            let checkpoints = manager.list_checkpoints(&dir).await;
            println!(
                "{}",
                ulnclaw::checkpoint::format_checkpoint_list(&checkpoints, &dir)
            );
        }
        CheckpointAction::Status => {
            let status = manager.status().await;
            println!(
                "{}",
                serde_json::to_string_pretty(&status).map_err(|e| e.to_string())?
            );
        }
        CheckpointAction::Restore { hash, file, dir } => {
            let dir = dir.unwrap_or(cwd);
            let result = manager
                .restore(&dir, &hash, file.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "✅ Restored {} to {} ({})",
                dir,
                result["restored_to"].as_str().unwrap_or("?"),
                result["reason"].as_str().unwrap_or("?")
            );
        }
        CheckpointAction::Diff { hash, dir } => {
            let dir = dir.unwrap_or(cwd);
            let result = manager.diff(&dir, &hash).await.map_err(|e| e.to_string())?;
            let stat = result["stat"].as_str().unwrap_or("");
            if !stat.is_empty() {
                println!("{}", stat);
            }
            println!("{}", result["diff"].as_str().unwrap_or(""));
        }
        CheckpointAction::Prune { days } => {
            let days = days.unwrap_or(config.checkpoints.retention_days);
            let stats = manager.prune(days, true).await;
            println!(
                "scanned: {}, deleted orphan: {}, deleted stale: {}, freed: {} bytes",
                stats.scanned, stats.deleted_orphan, stats.deleted_stale, stats.bytes_freed
            );
        }
    }
    Ok(())
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;
    let filter = if verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
    };
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter);
    // hermes_logging parity: rotating file handlers under <home>/logs/
    // (agent.log INFO+, errors.log WARNING+, gateway.log gateway targets).
    let file_layers = ulnclaw::logs::file_layers();
    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layers)
        .init();
}

async fn dispatch(cli: Cli, config: UlncLawConfig) -> Result<(), String> {
    match cli.command.unwrap_or(Commands::Chat) {
        Commands::Chat => chat_repl(&config, cli.resume.clone(), cli.continue_last).await,
        Commands::Run { prompt } => {
            let prompt = prompt.join(" ");
            if prompt.is_empty() {
                return Err("usage: ulnclaw run \"your prompt\"".into());
            }
            one_shot(&config, &prompt, cli.resume.clone(), cli.continue_last).await
        }
        Commands::Sessions { action } => sessions_cmd(action, &config).await,
        Commands::Kanban { action } => kanban_cmd(action).await,
        Commands::Projects { action } => projects_cmd(action),
        Commands::Pets { action } => pets_cmd(action).await,
        Commands::Tools { action, name, json } => tools_cmd(
            &config,
            action.as_deref().unwrap_or("list"),
            name.as_deref(),
            json,
        ),
        Commands::Skills { action } => skills_cmd(action.unwrap_or(SkillAction::List)).await,
        Commands::Bundles { action } => bundles_cmd(action.unwrap_or(BundlesAction::List)),
        Commands::Security { action } => {
            let SecurityAction::Audit { json } = action;
            let components = ulnclaw::security_audit::discover_mcp_components(&config);
            let total = components.len();
            if total == 0 {
                println!(
                    "No auditable components found. Only MCP servers that pin a package \
                     version (npx pkg@ver / uvx pkg==ver) are scanned."
                );
                return Ok(());
            }
            // reqwest's blocking client builds its own runtime — run it off
            // the async main context.
            let findings =
                tokio::task::spawn_blocking(move || ulnclaw::security_audit::run_audit(components))
                    .await
                    .map_err(|e| e.to_string())??;
            if json {
                println!("{}", ulnclaw::security_audit::render_json(&findings, total));
            } else {
                println!(
                    "{}",
                    ulnclaw::security_audit::render_human(&findings, total)
                );
            }
            Ok(())
        }
        Commands::Secrets { action } => secrets_cmd(&config, action),
        Commands::ComputerUse { action } => computer_use_cmd(&config, action).await,
        Commands::Plugins { action } => {
            plugins_cmd(&config, action.unwrap_or(PluginsAction::List)).await
        }
        Commands::Hooks { action } => hooks_cmd(&config, action).await,
        Commands::Pairing { action } => pairing_cmd(action).await,
        Commands::Profiles { action } => profiles_cmd(action.unwrap_or(ProfilesAction::List)),
        Commands::Auth { action } => auth_cmd(&config, action.unwrap_or(AuthAction::Status)).await,
        Commands::Sync { action } => sync_cmd(&config, action.unwrap_or(SyncAction::Status)).await,
        Commands::Proxy { action } => proxy_cmd(&config, action).await,
        Commands::Egress { action } => egress_cmd(action).await,
        Commands::Cron { action } => cron_cmd(&config, action.unwrap_or(CronAction::List)).await,
        Commands::Gateway {
            host,
            port,
            replace,
            force,
        } => gateway_cmd(&config, host, port, replace, force).await,
        Commands::Serve {
            host,
            port,
            replace,
            force,
        } => gateway_cmd(&config, host, port, replace, force).await,
        Commands::Dashboard { action, host, port } => {
            dashboard_cmd(&config, action.as_deref().unwrap_or("run"), host, port).await
        }
        Commands::Console => {
            // Boxed: console_cmd dispatches back into this function (async
            // recursion needs an explicit boxed future).
            let fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>>>> =
                Box::pin(console_cmd(&config));
            fut.await
        }
        Commands::Migrate {
            migration,
            apply,
            no_backup,
        } => migrate_cmd(migration.as_deref(), apply, no_backup, cli.config.clone()),
        Commands::Weixin { action } => weixin_cmd(action).await,
        Commands::Qq { action } => qq_cmd(action).await,
        Commands::GoogleChatOauth { action } => google_chat_oauth_cmd(action).await,
        Commands::SpotifyAuth { action } => spotify_auth_cmd(action).await,
        Commands::Checkpoints { action } => checkpoints_cmd(&config, action).await,
        Commands::Diff {
            staged,
            all,
            dir,
            paths,
        } => {
            let mode = if all {
                ulnclaw::git_diff::DiffMode::All
            } else if staged {
                ulnclaw::git_diff::DiffMode::Staged
            } else {
                ulnclaw::git_diff::DiffMode::Working
            };
            let cwd = dir
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            match ulnclaw::git_diff::collect_working_diff(&cwd, mode, &paths) {
                Ok(result) if result.empty => {
                    println!("No changes ({} mode).", mode.as_str());
                }
                Ok(result) => {
                    if !result.stat.is_empty() {
                        println!("{}", result.stat);
                        println!();
                    }
                    if !result.diff.is_empty() {
                        println!("{}", result.diff);
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
            Ok(())
        }
        Commands::Moa { action } => {
            moa_cmd(
                &config,
                action.unwrap_or(MoaAction::List),
                cli.config.as_deref(),
            )
            .await
        }
        Commands::Models { action } => {
            // reqwest's blocking client builds its own runtime, so catalog
            // network fetches must run off the async main context.
            tokio::task::spawn_blocking(move || {
                models_cmd(action.unwrap_or(ModelsAction::Providers))
            })
            .await
            .map_err(|e| e.to_string())?
        }
        Commands::Memory { args, yes } => {
            let home = ulnclaw::config::ulnclaw_home();
            ulnclaw::memory_cmd::handle_memory_command(&home, &args, yes)
        }
        Commands::ImportAgent {
            agent,
            source,
            dry_run,
            overwrite,
        } => {
            let user_home = dirs::home_dir().ok_or("cannot resolve home directory")?;
            let agents: Vec<String> = match agent {
                Some(name) => vec![name],
                None => ulnclaw::agent_import::detect_agents(&user_home),
            };
            if agents.is_empty() {
                return Err(
                    "No supported agent installs found (~/.claude, ~/.codex). \
                     Specify one explicitly: ulnclaw import-agent <claude-code|codex> [--source DIR]"
                        .to_string(),
                );
            }
            for name in agents {
                let default_dir = match name.as_str() {
                    "claude-code" => user_home.join(".claude"),
                    "codex" => user_home.join(".codex"),
                    _ => {
                        return Err(format!(
                            "Unsupported agent: {name:?} (expected claude-code|codex)"
                        ))
                    }
                };
                let source_root = source.clone().unwrap_or(default_dir);
                let target_root = ulnclaw::config::ulnclaw_home();
                let importer = ulnclaw::agent_import::AgentImporter::new(
                    &name,
                    source_root,
                    target_root,
                    !dry_run,
                    overwrite,
                )
                .map_err(|e| e)?;
                let report = importer.run();
                print!("{}", ulnclaw::agent_import::format_import_report(&report));
            }
            Ok(())
        }
        Commands::Curator { action } => curator_cmd(action.unwrap_or(CuratorAction::Status)),
        Commands::Journey {
            action,
            reveal,
            play,
            fps,
            width,
            height,
            no_color,
            json,
        } => journey_cmd(action, reveal, play, fps, width, height, no_color, json),
        Commands::Init => {
            let path = UlncLawConfig::write_default_if_missing().map_err(|e| e.to_string())?;
            println!("config written to {}", path.display());
            Ok(())
        }
        Commands::Skins => {
            let active = ulnclaw::skin::get_active_skin_name();
            for info in ulnclaw::skin::list_skins() {
                let marker = if info.name == active { "*" } else { " " };
                println!(
                    "{} {:<16} {:<8} {}",
                    marker, info.name, info.source, info.description
                );
            }
            println!(
                "Active skin: {} (set [display] skin in config.toml)",
                active
            );
            Ok(())
        }
        Commands::Suggestions { args } => {
            println!(
                "{}",
                ulnclaw::cron::suggestions::handle_suggestions_command(&args.join(" "))
            );
            Ok(())
        }
        Commands::Insights { days, source, json } => {
            let engine =
                ulnclaw::insights::InsightsEngine::open_default().map_err(|e| e.to_string())?;
            let report = engine
                .generate(
                    days,
                    source.as_deref(),
                    Some(config.model.provider.as_str()),
                )
                .map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                print!("{}", ulnclaw::insights::format_terminal(&report));
            }
            Ok(())
        }
        Commands::Approvals { mode } => {
            let result = ulnclaw::approvals_cmd::run_approval_mode_command(mode.as_deref());
            if result.ok {
                println!("{}", result.message);
                Ok(())
            } else {
                Err(result.message)
            }
        }
        Commands::Debug {
            action,
            lines,
            no_redact,
            output,
        } => {
            if action != "report" {
                return Err(format!(
                    "unknown debug action '{action}' (expected: report)"
                ));
            }
            let report = ulnclaw::debug_cmd::handle_debug_command(
                &config,
                cli.profile.as_deref(),
                lines,
                !no_redact,
                output.as_deref(),
            )?;
            print!("{report}");
            Ok(())
        }
        Commands::PromptSize { json } => {
            let home = ulnclaw::config::ulnclaw_home();
            let mut registry = ulnclaw::tools::ToolRegistry::new();
            ulnclaw::tools::builtin::register_builtin_tools(&mut registry);
            ulnclaw::toolsets::apply_toolset_policy(
                &mut registry,
                &config.enabled_toolsets,
                &config.disabled_toolsets,
            );
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let data =
                ulnclaw::prompt_size::compute_prompt_breakdown(&config, &home, &cwd, &registry);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?
                );
            } else {
                print!("{}", ulnclaw::prompt_size::render_breakdown(&data));
            }
            Ok(())
        }
        Commands::Doctor { fix, online, json } => {
            let opts = ulnclaw::doctor::DoctorOptions { fix, online, json };
            let report = ulnclaw::doctor::run_doctor(&config, &opts);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                print!("{}", report.render());
            }
            Ok(())
        }
        Commands::Monitoring { action } => {
            match action
                .as_deref()
                .map(|a| a.trim())
                .filter(|a| !a.is_empty())
            {
                None | Some("status") => {
                    print!("{}", ulnclaw::monitoring::render_status(&config.monitoring));
                    Ok(())
                }
                Some(other) => {
                    eprintln!("Unknown monitoring action: {other}");
                    std::process::exit(2);
                }
            }
        }
        Commands::Whatsapp { action } => {
            match action.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                None | Some("status") => {
                    print!("{}", ulnclaw::whatsapp::whatsapp_status(&config).await);
                    Ok(())
                }
                Some(other) => {
                    eprintln!("Unknown whatsapp action: {other}");
                    std::process::exit(2);
                }
            }
        }
        Commands::WhatsappCloud => {
            let code = ulnclaw::whatsapp_cloud_setup::run_wizard()?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Commands::Webhook { action } => {
            use ulnclaw::webhook_subscriptions as whs;
            match action {
                None => {
                    println!("Usage: ulnclaw webhook {{subscribe|list|remove|test}}");
                    println!("Run 'ulnclaw webhook --help' for details.");
                    Ok(())
                }
                Some(WebhookAction::Subscribe {
                    name,
                    description,
                    events,
                    secret,
                    prompt,
                    skills,
                    deliver,
                    deliver_chat_id,
                    deliver_only,
                    script,
                }) => {
                    if !config.messaging.webhook.enabled {
                        println!("  Webhook platform is not enabled. Enable it with:");
                        println!("    ulnclaw config set messaging.webhook.enabled true");
                        println!("  Then start the gateway: ulnclaw gateway");
                        return Ok(());
                    }
                    let opts = whs::SubscribeOptions {
                        description,
                        events,
                        secret,
                        prompt,
                        skills,
                        deliver,
                        deliver_chat_id,
                        deliver_only,
                        script,
                    };
                    print!("{}", whs::cmd_subscribe(&name, &opts)?);
                    Ok(())
                }
                Some(WebhookAction::List) => {
                    print!("{}", whs::cmd_list()?);
                    Ok(())
                }
                Some(WebhookAction::Remove { name }) => {
                    print!("{}", whs::cmd_remove(&name)?);
                    Ok(())
                }
                Some(WebhookAction::Test { name, payload }) => {
                    print!("{}", whs::cmd_test(&name, payload.as_deref())?);
                    Ok(())
                }
            }
        }
        Commands::Gui { binary, dev } => {
            if dev {
                let pid = ulnclaw::gui_cmd::launch_dev(&ulnclaw::gui_cmd::repo_root())?;
                println!("✓ Desktop dev server starting (pid {pid}).");
                Ok(())
            } else {
                let bin = ulnclaw::gui_cmd::resolve_binary(
                    binary.as_deref(),
                    &ulnclaw::gui_cmd::repo_root(),
                )?;
                let pid = ulnclaw::gui_cmd::launch(&bin)?;
                println!("✓ Desktop GUI launched: {} (pid {pid})", bin.display());
                Ok(())
            }
        }
        Commands::Login => auth_cmd(&config, AuthAction::Login).await,
        Commands::Logout => auth_cmd(&config, AuthAction::Logout).await,
        Commands::Model { refresh } => ulnclaw::model_cmd::run_model_picker(refresh),
        Commands::Setup {
            section,
            quick,
            reset,
            non_interactive,
        } => ulnclaw::setup_cmd::run_setup(section.as_deref(), quick, reset, non_interactive),
        Commands::Status { all: _, deep } => {
            let opts = ulnclaw::status::StatusOptions { deep };
            print!("{}", ulnclaw::status::show_status(&config, &opts));
            Ok(())
        }
        Commands::Completion { shell } => {
            let mut cmd = <Cli as clap::CommandFactory>::command();
            clap_complete::generate(shell, &mut cmd, "ulnclaw", &mut std::io::stdout());
            Ok(())
        }
        Commands::Dump { show_keys } => {
            print!(
                "{}",
                ulnclaw::dump::build_dump(&config, cli.profile.as_deref(), show_keys)
            );
            Ok(())
        }
        Commands::Version { no_update_check } => {
            let root = ulnclaw::update::find_repo_root();
            print!(
                "{}",
                ulnclaw::dump::build_version_report(root.as_deref(), !no_update_check)
            );
            Ok(())
        }
        Commands::Config { args, json, force } => {
            let out = ulnclaw::config_cmd::handle_config_command(&args, json, force)?;
            if !out.is_empty() {
                println!("{out}");
            }
            Ok(())
        }
        Commands::Send {
            to,
            message,
            file,
            subject,
            list,
            quiet,
            json,
        } => send_cmd(to, message, file, subject, list, quiet, json).await,
        Commands::Slack {
            subcommand,
            write,
            name,
            description,
            long_description,
            long_description_file,
            slashes_only,
            no_assistant,
            agent_view,
        } => match subcommand.as_deref() {
            Some("manifest") => {
                let opts = ulnclaw::slack_cli::ManifestOptions {
                    name,
                    description,
                    long_description,
                    long_description_file,
                    slashes_only,
                    no_assistant,
                    agent_view,
                    write: write.map(|p| if p.is_empty() { None } else { Some(p) }),
                };
                let code = ulnclaw::slack_cli::run_manifest_command(
                    &opts,
                    &ulnclaw::config::ulnclaw_home(),
                );
                std::process::exit(code);
            }
            Some(other) => {
                eprintln!("Unknown slack subcommand: {other}");
                std::process::exit(1);
            }
            None => {
                eprintln!(
                        "usage: ulnclaw slack <subcommand>\n\nsubcommands:\n  manifest   Generate a Slack app manifest with every gateway\n             command registered as a native slash\n\nRun `ulnclaw slack manifest -h` for details."
                    );
                std::process::exit(1);
            }
        },
        Commands::Batch {
            dataset_file,
            batch_size,
            run_name,
            num_workers,
            resume,
            verbose,
            max_iterations,
            model,
        } => {
            ulnclaw::batch_runner::run(ulnclaw::batch_runner::BatchOptions {
                dataset_file: dataset_file.into(),
                batch_size,
                run_name,
                num_workers: num_workers.unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(4)
                }),
                resume,
                verbose,
                max_iterations,
                model,
            })
            .await
        }
        Commands::Acp { verbose } => ulnclaw::acp_adapter::run_stdio(verbose)
            .await
            .map_err(|e| e.to_string()),
        Commands::Mcp { args, verbose } => match args.first().map(String::as_str) {
            Some("serve") => ulnclaw::mcp_serve::run_stdio(verbose)
                .await
                .map_err(|e| e.to_string()),
            Some(other) => Err(format!("unknown mcp subcommand: {other} (expected: serve)")),
            None => Err("usage: ulnclaw mcp serve [--verbose]".to_string()),
        },
        Commands::Uninstall { full, dry_run, yes } => uninstall_cmd(full, dry_run, yes),
        Commands::Fallback { args, yes } => {
            let home = ulnclaw::config::ulnclaw_home();
            print!(
                "{}",
                ulnclaw::fallback::handle_fallback_command(&home, &args, yes)?
            );
            Ok(())
        }
        Commands::Backup {
            action,
            output,
            quick,
            label,
        } => {
            let home = ulnclaw::config::ulnclaw_home();
            match action.first().map(|a| a.as_str()) {
                Some("list") => {
                    let snapshots = ulnclaw::backup::list_quick_snapshots(&home);
                    if snapshots.is_empty() {
                        println!(
                            "No quick snapshots yet. Create one with 'ulnclaw backup --quick'."
                        );
                    } else {
                        println!(
                            "Quick snapshots in {}/{}:",
                            home.display(),
                            ulnclaw::backup::QUICK_SNAPSHOTS_DIR
                        );
                        for snapshot in &snapshots {
                            println!(
                                "  {:<32} {:>4} file(s)  {:>10}",
                                snapshot.id,
                                snapshot.files,
                                ulnclaw::backup::format_size(snapshot.bytes as f64)
                            );
                        }
                    }
                    Ok(())
                }
                Some("restore") => {
                    let Some(id) = action.get(1) else {
                        return Err("usage: ulnclaw backup restore <snapshot-id>".into());
                    };
                    match ulnclaw::backup::restore_quick_snapshot(&home, id) {
                        Ok(true) => println!("✓ Restored state from snapshot {id}."),
                        Ok(false) => println!("Snapshot '{id}' not found or empty."),
                        Err(e) => return Err(e),
                    }
                    Ok(())
                }
                Some("prune") => {
                    let keep: usize = action
                        .get(1)
                        .and_then(|k| k.parse().ok())
                        .unwrap_or(ulnclaw::backup::QUICK_DEFAULT_KEEP);
                    let removed = ulnclaw::backup::prune_quick_snapshots(&home, keep);
                    println!("Pruned {removed} snapshot(s) (keeping {keep}).");
                    Ok(())
                }
                Some(unknown) => Err(format!(
                    "Unknown backup action: '{unknown}'. Use list, restore <id>, or prune [keep]."
                )),
                None => {
                    if quick {
                        match ulnclaw::backup::create_quick_snapshot(
                            &home,
                            label.as_deref(),
                            None,
                            None,
                        )? {
                            Some(id) => {
                                println!("✓ Quick snapshot created: {id}");
                                println!("  Restore with: ulnclaw backup restore {id}");
                            }
                            None => println!("No state files found to snapshot."),
                        }
                        Ok(())
                    } else {
                        println!("Scanning {} ...", home.display());
                        let summary = ulnclaw::backup::create_backup(
                            &home,
                            output.as_ref().map(std::path::Path::new),
                        )?;
                        println!("Backing up {} files ...", summary.file_count);
                        print!("{}", ulnclaw::backup::format_backup_summary(&summary));
                        Ok(())
                    }
                }
            }
        }
        Commands::Import { zip } => {
            let home = ulnclaw::config::ulnclaw_home();
            let zip_path = std::path::PathBuf::from(&zip);
            // Safety net snapshot of current state before overlaying.
            if let Some(id) =
                ulnclaw::backup::create_quick_snapshot(&home, Some("pre-import"), None, None)?
            {
                println!("→ Current state snapshotted as {id} before import.");
            }
            let report = ulnclaw::backup::import_backup(&home, &zip_path)?;
            print!("{}", ulnclaw::backup::format_import_report(&report));
            if let Some(message) = ulnclaw::backup::restore_cron_jobs_if_emptied(&home) {
                println!("{message}");
            }
            Ok(())
        }
        Commands::Update { check, branch, yes } => {
            let opts = ulnclaw::update::UpdateOptions { check, branch, yes };
            let root = ulnclaw::update::find_repo_root()
                .ok_or("✗ Not a git repository — cannot check for updates.")?;
            if check {
                let (outcome, log_lines) = ulnclaw::update::check_update(&root, &opts)?;
                for line in log_lines {
                    println!("{line}");
                }
                print!("{}", ulnclaw::update::format_check_report(&outcome));
            } else {
                // hermes _run_pre_update_backup: quick state snapshot first.
                let home = ulnclaw::config::ulnclaw_home();
                if let Some(id) = ulnclaw::backup::create_pre_update_backup(&home) {
                    println!("→ Pre-update state snapshot: {id}");
                }
                let report = ulnclaw::update::apply_update(&root, &opts)?;
                print!("{}", ulnclaw::update::format_update_report(&report));
                if let Some(message) = ulnclaw::backup::restore_cron_jobs_if_emptied(&home) {
                    println!("{message}");
                }
            }
            Ok(())
        }
        Commands::Logs {
            log_name,
            lines,
            follow,
            level,
            session,
            since,
            component,
        } => {
            let name = log_name.unwrap_or_else(|| "agent".to_string());
            if name == "list" {
                print!("{}", ulnclaw::logs::list_logs());
                return Ok(());
            }
            let opts = ulnclaw::logs::TailOptions {
                num_lines: lines,
                follow,
                level,
                session: session.clone(),
                since: since.clone(),
                component: component.clone(),
            };
            print!("{}", ulnclaw::logs::tail_log(&name, &opts)?);
            if follow {
                ulnclaw::logs::follow_log(&name, &opts)?;
            }
            Ok(())
        }
    }
}

async fn make_agent(
    config: &UlncLawConfig,
    interactive: bool,
    approve_override: Option<ulnclaw::tools::context::ApproveFn>,
    session_id: Option<String>,
) -> Result<Arc<Agent>, String> {
    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
    make_agent_in(
        config,
        interactive,
        approve_override,
        &home,
        session_id,
        false,
    )
    .await
}

/// Build an agent rooted at an explicit home directory — the multiplex
/// gateway uses this to scope each `/p/<profile>` agent to
/// `<home>/profiles/<name>` (hermes profile home scoping).
async fn make_agent_in(
    config: &UlncLawConfig,
    interactive: bool,
    approve_override: Option<ulnclaw::tools::context::ApproveFn>,
    home: &std::path::Path,
    session_id: Option<String>,
    keyless_ok: bool,
) -> Result<Arc<Agent>, String> {
    let provider = build_provider_with(config, session_id.as_deref(), keyless_ok)?;
    // Tirith scanner availability (hermes `_ensure_tirith_security`):
    // quick PATH/local checks now, network download in a background
    // thread so startup never blocks.
    ulnclaw::tirith::ensure_installed(&config.security);
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);

    // MCP servers: connect and register their tools (hermes mcp_tool.py).
    for server in config.mcp.servers.iter().filter(|server| server.enabled) {
        match ulnclaw::mcp::register_mcp_server_lazy(&mut registry, server).await {
            Ok(count) => eprintln!("[mcp] {}: {} tools registered", server.name, count),
            Err(e) => eprintln!("[mcp] {}: unavailable ({})", server.name, e),
        }
    }

    // Directory plugins: register their tools (hermes register_tool).
    let plugin_tool_count = ulnclaw::plugins::register_plugin_tools(&mut registry);
    if plugin_tool_count > 0 {
        eprintln!("[plugins] {plugin_tool_count} plugin tool(s) registered");
    }

    toolsets::apply_toolset_policy(
        &mut registry,
        &config.enabled_toolsets,
        &config.disabled_toolsets,
    );

    std::fs::create_dir_all(home).ok();
    let store =
        Arc::new(SqliteSessionStore::open(home.join("state.db")).map_err(|e| e.to_string())?);
    // Crash recovery for background delegations (hermes durable registry):
    // rows still running from a previous process become terminal
    // "outcome unknown" results delivered on the next drain.
    if ulnclaw::async_delegation::recover_from_store(&store) > 0 {
        eprintln!("[delegation] recovered abandoned background delegation(s) from a previous run");
    }

    let mut context = ToolContext::new()
        .with_home(home.to_path_buf())
        .with_config(config.clone());
    if let Some(sid) = session_id {
        context = context.with_session_id(sid);
    }
    context = context
        .with_async_delivery(interactive)
        .with_env_passthrough(&config.terminal.env_passthrough)
        .with_store(store.clone())
        .with_provider(provider.clone());

    if interactive {
        context = context.with_clarify(Arc::new(|question, choices, multi| {
            Box::pin(async move {
                println!("\n❓ {}", question);
                if !choices.is_empty() {
                    for (i, choice) in choices.iter().enumerate() {
                        println!("  {}. {}", i + 1, choice);
                    }
                }
                if multi {
                    println!("(multiple allowed)");
                }
                print!("> ");
                std::io::stdout().flush().ok();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).ok();
                let answer = line.trim().to_string();
                if let Ok(idx) = answer.parse::<usize>() {
                    if idx >= 1 && idx <= choices.len() {
                        return Ok(choices[idx - 1].clone());
                    }
                }
                Ok(answer)
            })
        }));
        if context.approve.is_none() {
            context = context.with_approve(Arc::new(|reason, command| {
                Box::pin(async move {
                    println!(
                        "\n⚠️  Approve dangerous command? [{}]\n{}\nApprove? [y/N]",
                        reason, command
                    );
                    print!("> ");
                    std::io::stdout().flush().ok();
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line).ok();
                    matches!(line.trim(), "y" | "Y" | "yes" | "YES")
                })
            }));
        }
    }
    if let Some(approve) = approve_override {
        context = context.with_approve(approve);
    }
    if !interactive && context.clarify.is_none() {
        // Messaging-aware clarify (hermes clarify_gateway): renders the
        // prompt on the current platform chat and blocks until the user
        // taps a button or replies. Non-messaging runs (plain /api/chat,
        // cron, one-shot) get the standard non-interactive error.
        context = context.with_clarify(ulnclaw::messaging::messaging_clarify_fn());
    }

    // Checkpoint auto-maintenance in the background (hermes startup hook).
    if config.checkpoints.enabled {
        let manager = context.checkpoint_manager();
        tokio::spawn(async move {
            manager.maybe_auto_prune().await;
        });
    }

    // Capture the tool catalog snapshot for tool_search (before registry moves).
    context.set_tool_definitions(registry.definitions());
    let agent = Agent::new(provider.clone(), registry).with_config(AgentConfig {
        max_iterations: config.agent.max_iterations,
        // P619: agent.system_prompt (written by /personality) replaces
        // the built-in prompt when non-empty (hermes semantics).
        system_prompt: {
            let prompt = config.agent.system_prompt.trim();
            if prompt.is_empty() {
                None
            } else {
                Some(prompt.to_string())
            }
        },
        concurrent_tool_execution: config.agent.concurrent_tool_execution,
        max_concurrent_tools: config.agent.max_concurrent_tools,
        approval: config.agent.approval,
        context_budget_tokens: config.agent.context_budget_tokens,
        persist: true,
        source: "cli".to_string(),
        environment_probe: config.agent.environment_probe,
        terminal_backend: config
            .terminal
            .backend
            .clone()
            .unwrap_or_else(|| "local".to_string()),
        ..Default::default()
    });
    let agent = agent
        .with_store(store)
        .with_tool_context(context)
        .with_fallback_specs(&config.model.fallbacks);
    let agent = Arc::new(agent);
    // Wire the runners now that the agent is in an Arc.
    agent.wire_runners();
    Ok(agent)
}

/// `ulnclaw send` — pipe text from shell scripts to any configured
/// messaging platform (hermes `hermes_cli/send_cmd.py`): no LLM, no agent
/// loop, bot-token platforms need no running gateway. Exit codes: 0 ok,
/// 1 delivery/backend error, 2 usage error.
async fn send_cmd(
    to: Option<String>,
    message: Option<String>,
    file: Option<String>,
    subject: Option<String>,
    list: bool,
    quiet: bool,
    json_mode: bool,
) -> Result<(), String> {
    const USAGE_EXIT: i32 = 2;
    const FAILURE_EXIT: i32 = 1;

    // --list short-circuits everything else (the positional doubles as the
    // platform filter, hermes argparse behavior).
    if list {
        let filter = message.as_deref().map(str::trim).filter(|f| !f.is_empty());
        let code = send_list_targets(filter, json_mode).await;
        std::process::exit(code);
    }

    let Some(target) = to.filter(|t| !t.trim().is_empty()) else {
        eprintln!(
            "ulnclaw send: --to PLATFORM[:channel[:thread]] is required\n\
             Examples:\n\
             \x20 ulnclaw send --to telegram \"hello\"\n\
             \x20 ulnclaw send --to discord:#ops --file report.md\n\
             \x20 ulnclaw send --list      # list available targets"
        );
        std::process::exit(USAGE_EXIT);
    };

    // Message body: positional → --file (or '-' for stdin) → piped stdin.
    let mut body: Option<String> = message.clone();
    if body.is_none() {
        if let Some(path) = &file {
            body = Some(if path == "-" {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .map_err(|e| e.to_string())?;
                buf
            } else {
                std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read --file {}: {e}", path))?
            });
        } else if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .map_err(|e| e.to_string())?;
            if !buf.trim().is_empty() {
                body = Some(buf);
            }
        }
    }
    let Some(mut body) = body.filter(|body| !body.trim().is_empty()) else {
        eprintln!(
            "ulnclaw send: no message provided. Pass text as a positional \
             argument, use --file PATH, or pipe data via stdin."
        );
        std::process::exit(USAGE_EXIT);
    };
    if let Some(subject) = subject.filter(|s| !s.is_empty()) {
        body = format!("{subject}\n\n{}", body.trim_start());
    }

    let result = ulnclaw::send_message_tool::run_send_message(serde_json::json!({
        "action": "send",
        "target": target,
        "message": body,
    }))
    .await;

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        );
    } else if !quiet {
        if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
            eprintln!("ulnclaw send: {error}");
        } else if result
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            match result.get("note").and_then(serde_json::Value::as_str) {
                Some(note) => println!("{note}"),
                None => println!("sent"),
            }
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
        }
    }

    let failed = result.get("error").is_some()
        || !(result
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || result.get("skipped").is_some());
    std::process::exit(if failed { FAILURE_EXIT } else { 0 });
}

/// hermes `_list_targets`: channel directory ∪ configured-but-undiscovered
/// platforms, optional platform filter, human or --json rendering.
async fn send_list_targets(filter: Option<&str>, json_mode: bool) -> i32 {
    let mut platforms: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    for (platform, entry) in ulnclaw::channel_directory::list_channels(None) {
        platforms
            .entry(platform)
            .or_default()
            .push(serde_json::json!({
                "id": entry.id,
                "name": entry.name,
                "type": entry.chat_type,
            }));
    }
    // Merge configured-but-undiscovered platforms (hermes
    // get_connected_platforms merge) so --list never hides a working target.
    if let Ok(config) = ulnclaw::config::UlncLawConfig::load(None) {
        let configured: Vec<(&str, bool)> = vec![
            ("telegram", config.messaging.telegram.enabled),
            ("discord", config.messaging.discord.enabled),
            ("slack", config.messaging.slack.enabled),
            ("signal", config.messaging.signal.enabled),
            ("weixin", config.messaging.weixin.enabled),
            ("qq", config.messaging.qq.enabled),
            ("yuanbao", config.messaging.yuanbao.enabled),
            ("email", config.messaging.email.enabled),
            ("mattermost", config.messaging.mattermost.enabled),
            ("matrix", config.messaging.matrix.enabled),
            ("dingtalk", config.messaging.dingtalk.enabled),
            ("wecom", config.messaging.wecom.enabled),
            ("feishu", config.messaging.feishu.enabled),
            ("homeassistant", config.messaging.homeassistant.enabled),
            ("sms", config.messaging.sms.enabled),
            ("whatsapp", config.messaging.whatsapp.enabled),
            ("irc", config.messaging.irc.enabled),
            ("ntfy", config.messaging.ntfy.enabled),
            ("simplex", config.messaging.simplex.enabled),
            ("teams", config.messaging.teams.enabled),
            ("line", config.messaging.line.enabled),
            ("google_chat", config.messaging.google_chat.enabled),
            ("buzz", config.messaging.buzz.enabled),
            ("photon", config.messaging.photon.enabled),
            ("raft", config.messaging.raft.enabled),
            ("a2a", config.messaging.a2a.enabled),
        ];
        for (name, enabled) in configured {
            if enabled {
                platforms.entry(name.to_string()).or_default();
            }
        }
    }

    if let Some(filter) = filter {
        let key = filter.to_lowercase();
        let filtered: std::collections::BTreeMap<String, Vec<serde_json::Value>> = platforms
            .into_iter()
            .filter(|(name, _)| name.to_lowercase() == key)
            .collect();
        if filtered.is_empty() {
            eprintln!(
                "ulnclaw send: no targets found for platform '{filter}'. \
                 Configured: (none matched)"
            );
            return 1;
        }
        platforms = filtered;
    }

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"platforms": platforms}))
                .unwrap_or_default()
        );
        return 0;
    }
    if platforms.is_empty() {
        println!("No messaging platforms connected or no channels discovered yet.");
        return 0;
    }
    println!("Available messaging targets:\n");
    for (platform, entries) in &platforms {
        let title = {
            let mut chars = platform.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        };
        if entries.is_empty() {
            println!("{title}:");
            println!(
                "  (no channels discovered yet — send directly with {platform}:<chat_id>, \
                 or bare '{platform}' for the home channel)"
            );
            println!();
            continue;
        }
        println!("{title}:");
        for entry in entries {
            let id = entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let chat_type = entry
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let label = if platform == "discord" {
                format!("#{name}")
            } else if !chat_type.is_empty() {
                format!("{name} ({chat_type})")
            } else {
                name.to_string()
            };
            println!("  {platform}:{label}");
            let _ = id;
        }
        println!();
    }
    println!("Use these as the \"target\" parameter when sending.");
    println!("Bare platform name (e.g. \"telegram\") sends to home channel.");
    0
}

async fn one_shot(
    config: &UlncLawConfig,
    prompt: &str,
    resume: Option<String>,
    continue_last: Option<String>,
) -> Result<(), String> {
    let target = resolve_resume_target(resume.as_deref(), continue_last.as_deref())?;
    let agent = make_agent(config, false, None, target.clone()).await?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    ulnclaw::plugins::fire_session_event(
        "on_session_start",
        &agent.context().session_id,
        &cwd,
        serde_json::json!({"source": "cli", "mode": "one_shot"}),
    )
    .await;
    let result = agent
        .run_with_session(prompt, None, target.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    let content =
        ulnclaw::plugins::transform_llm_output(&agent.context().session_id, &cwd, &result.content)
            .await;
    println!("{}", content);

    // Kanban goal-loop mode (hermes cli.py quiet path): a worker spawned
    // for a goal_mode card keeps working in THIS session until the
    // auxiliary judge agrees the card is done, the worker terminates the
    // task itself, or the turn budget runs out (sticky block). No-op for
    // every normal worker and every non-kanban run.
    if ulnclaw::kanban::worker_goal_mode_env() {
        if let Some(task_id) = ulnclaw::kanban::worker_task_env() {
            kanban_goal_loop_for_worker(&agent, &task_id, &content, target.as_deref()).await;
        }
    }

    // Kanban stop-guard (hermes agent/kanban_stop.py): a dispatcher-spawned
    // worker must end on kanban_complete/kanban_block. If the run finished
    // without moving the task to a terminal status, nudge and continue
    // (bounded by STOP_NUDGE_MAX_ATTEMPTS).
    if let Some(task_id) = ulnclaw::kanban::worker_task_env() {
        let mut attempts = 0usize;
        loop {
            let needs_nudge = {
                match ulnclaw::kanban::KanbanStore::open_default() {
                    Ok(store) => match store.get_task(&task_id) {
                        Ok(Some(task)) => !matches!(task.status.as_str(), "done" | "blocked"),
                        _ => false,
                    },
                    Err(_) => false,
                }
            };
            if !needs_nudge {
                break;
            }
            let Some(nudge) = ulnclaw::kanban::build_kanban_stop_nudge(&task_id, attempts) else {
                eprintln!(
                    "kanban: worker for {task_id} ended without kanban_complete/kanban_block \
                     after {attempts} nudge(s) — protocol violation"
                );
                break;
            };
            attempts += 1;
            let result = agent
                .run_with_session(&nudge, None, target.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            let content = ulnclaw::plugins::transform_llm_output(
                &agent.context().session_id,
                &cwd,
                &result.content,
            )
            .await;
            println!("{}", content);
        }
    }
    ulnclaw::plugins::fire_session_event(
        "on_session_end",
        &agent.context().session_id,
        &cwd,
        serde_json::json!({"source": "cli", "mode": "one_shot"}),
    )
    .await;
    Ok(())
}

/// Kanban goal-mode worker loop (hermes cli.py
/// `_run_kanban_goal_loop_q`): drive a goal_mode worker through the
/// Ralph-style judge loop IN THIS SESSION until the judge agrees the
/// card is done, the worker terminates the task itself, or the turn
/// budget runs out (sticky block for human review). All errors are
/// swallowed — a broken goal loop must never wedge a worker; the
/// dispatcher's claim TTL / crash detection is the backstop.
async fn kanban_goal_loop_for_worker(
    agent: &Agent,
    task_id: &str,
    first_response: &str,
    target: Option<&str>,
) {
    let store = match ulnclaw::kanban::KanbanStore::open_default() {
        Ok(store) => store,
        Err(e) => {
            eprintln!("kanban goal loop: open store failed: {e}");
            return;
        }
    };
    let task = match store.get_task(task_id) {
        Ok(Some(task)) => task,
        _ => return,
    };
    let mut goal_parts = vec![task.title.clone()];
    if !task.body.trim().is_empty() {
        goal_parts.push(task.body.clone());
    }
    let goal_text = goal_parts.join("\n\n");
    if goal_text.trim().is_empty() {
        return;
    }
    let max_turns = task
        .goal_max_turns
        .filter(|turns| *turns > 0)
        .map(|turns| turns as u32)
        .or_else(ulnclaw::kanban::worker_goal_max_turns_env)
        .unwrap_or(ulnclaw::goals::DEFAULT_MAX_TURNS as u32);
    let store_ref = &store;
    let ctx = agent.context();
    let Some(provider) = ctx.provider.clone() else {
        eprintln!("kanban goal loop: no provider available for the judge; skipping the loop");
        return;
    };
    let config = ctx.config.clone();
    let result = ulnclaw::goals::run_kanban_goal_loop(
        task_id,
        |prompt| {
            Box::pin(async move {
                match agent.run_with_session(&prompt, None, target).await {
                    Ok(result) => {
                        if !result.content.is_empty() {
                            println!("{}", result.content);
                        }
                        Ok(result.content)
                    }
                    Err(e) => Err(e.to_string()),
                }
            })
        },
        || {
            let store = store_ref;
            Box::pin(async move { store.get_task(task_id).ok().flatten().map(|t| t.status) })
        },
        |reason| {
            let store = store_ref;
            Box::pin(async move {
                if let Err(e) = store.block_task_guarded(
                    task_id,
                    &reason,
                    None,
                    ulnclaw::kanban::worker_run_id_for(task_id),
                ) {
                    eprintln!("kanban goal loop: block failed: {e}");
                }
            })
        },
        |response| {
            let config = config.clone();
            let provider = provider.clone();
            let goal = goal_text.clone();
            Box::pin(async move {
                let (verdict, _transport_failed) =
                    ulnclaw::goals::judge_goal(&config, provider, &goal, &response, &[], &[], None)
                        .await;
                verdict
            })
        },
        |msg| println!("{msg}"),
        max_turns,
        first_response,
    )
    .await;
    println!(
        "kanban goal loop: task {task_id} outcome={:?} after {} turn(s) — {}",
        result.outcome, result.turns_used, result.reason
    );
}

/// Resolve `--resume <id|prefix>` / `--continue` to a concrete session id
/// (hermes startup resume). Returns `Ok(None)` when neither flag is set.
fn resolve_resume_target(
    resume: Option<&str>,
    continue_last: Option<&str>,
) -> Result<Option<String>, String> {
    if resume.is_none() && continue_last.is_none() {
        return Ok(None);
    }
    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
    let store = SqliteSessionStore::open(home.join("state.db")).map_err(|e| e.to_string())?;
    if let Some(id) = resume {
        return store
            .resolve_session_id(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("Session '{}' not found.", id))
            .map(Some);
    }
    let name = continue_last.unwrap_or("");
    if name.is_empty() {
        // `-c` with no value: continue the most recent session.
        return store
            .latest_session_id()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "No previous session to continue.".to_string())
            .map(Some);
    }
    // `-c NAME` (hermes _resolve_session_by_name_or_id): try exact id /
    // unique prefix first, then title; project the result forward through
    // any compression chain so resumes land on the live tip.
    let resolved = store
        .resolve_session_id(name)
        .map_err(|e| e.to_string())?
        .or_else(|| store.resolve_session_by_title(name).ok().flatten());
    let Some(resolved) = resolved else {
        return Err(format!(
            "No session found matching '{name}'.\nUse 'ulnclaw sessions list' to see available sessions."
        ));
    };
    let tip = store
        .compression_tip(&resolved)
        .map_err(|e| e.to_string())?
        .unwrap_or(resolved);
    Ok(Some(tip))
}

/// Print a random feature tip tinted with the active skin's banner_dim
/// (hermes `✦ Tip:` line). Color only applies on a TTY without NO_COLOR.
fn print_tip() {
    let tip = ulnclaw::tips::format_tip(ulnclaw::tips::get_random_tip());
    let skin = ulnclaw::skin::get_active_skin();
    let color = skin.get_color("banner_dim", "#B8860B");
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("{}", ulnclaw::skin::colorize(&color, &tip));
    } else {
        println!("{}", tip);
    }
}

/// Hermes-style welcome banner: wordmark + skin-colored panel with model,
/// toolsets, skills, and update status. Degrades to plain summary lines
/// when stdout is not a TTY.
async fn print_welcome_banner(config: &UlncLawConfig, agent: &Arc<Agent>) {
    let toolsets: Vec<(String, Vec<String>)> = agent
        .toolset_names()
        .into_iter()
        .map(|toolset| {
            let display = ulnclaw::banner::display_toolset_name(&toolset);
            let tools = agent.toolset_tool_names(&toolset);
            (display, tools)
        })
        .collect();
    let total_tools: usize = toolsets.iter().map(|(_, tools)| tools.len()).sum();
    // The models.dev lookup uses a blocking reqwest client, which must not
    // run (or be dropped) inside the async runtime; spawn_blocking keeps it
    // on a blocking thread. The 2s cap keeps startup snappy when the
    // registry is unreachable — the task keeps running and warms the cache.
    let lookup_provider = config.model.provider.clone();
    let lookup_model = config.model.model.clone();
    let context_length = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            ulnclaw::models_dev::lookup_models_dev_context(&lookup_provider, &lookup_model)
        }),
    )
    .await
    .ok()
    .and_then(|joined| joined.ok())
    .flatten();
    let info = ulnclaw::banner::BannerInfo {
        model: config.model.model.clone(),
        provider: config.model.provider.clone(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        session_id: Some(agent.context().session_id.clone()),
        context_length,
        toolsets,
        skills: ulnclaw::banner::get_available_skills(),
        total_tools,
        yolo: config.approvals.mode == "off",
        update_behind: ulnclaw::banner::get_update_result(std::time::Duration::from_millis(500)),
    };
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let term_width = ulnclaw::banner::terminal_width();
        println!(
            "{}",
            ulnclaw::banner::build_startup_display(&info, term_width)
        );
    } else {
        println!(
            "ulnclaw {} — model: {} ({})",
            ulnclaw::VERSION,
            config.model.model,
            config.model.provider
        );
        println!("Type /help for commands, /quit to exit.");
    }
}

async fn chat_repl(
    config: &UlncLawConfig,
    resume: Option<String>,
    continue_last: Option<String>,
) -> Result<(), String> {
    // Kick off the git update check while the agent is being constructed
    // (hermes prefetch_update_check on the startup path).
    ulnclaw::banner::prefetch_update_check();
    // Resolve --resume/--continue before the agent exists (hermes startup
    // resume): the whole REPL conversation lives in ONE session row.
    let mut session_id = uuid::Uuid::new_v4().to_string();
    let mut history: Vec<Message> = Vec::new();
    let mut resumed_from: Option<String> = None;
    if resume.is_some() || continue_last.is_some() {
        let target = resolve_resume_target(resume.as_deref(), continue_last.as_deref())?.unwrap();
        let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
        let pre_store =
            SqliteSessionStore::open(home.join("state.db")).map_err(|e| e.to_string())?;
        let messages = pre_store
            .load_messages(&target)
            .map_err(|e| e.to_string())?;
        history = messages
            .into_iter()
            .filter(|m| m.role != Role::System)
            .collect();
        let title = pre_store
            .get_session_title(&target)
            .map_err(|e| e.to_string())?;
        resumed_from = Some(match title {
            Some(t) => format!("{} ({})", target, t),
            None => target.clone(),
        });
        session_id = target;
    }
    let agent = make_agent(config, true, None, Some(session_id)).await?;
    // Display-only tool-progress rendering wired through agent callbacks
    // (hermes CLI tool-progress scrollback); /focus and /verbose compose
    // on top of it (hermes focus_view + tool_progress_mode).
    let display = Arc::new(std::sync::Mutex::new(
        ulnclaw::focus_view::DisplayState::default(),
    ));
    {
        let progress_display = display.clone();
        let mut callbacks = ulnclaw::agent::AgentCallbacks::default();
        callbacks.on_tool_start = Some(Box::new(move |name, _args| {
            let show = {
                let mut state = progress_display.lock().unwrap();
                state.on_tool_call(name)
            };
            if show {
                println!("\n⚙ {name}");
            }
        }));
        agent.set_callbacks(callbacks).await;
    }
    // Session-scoped prompt stash (hermes Ctrl+S gesture → /stash).
    let mut stash = ulnclaw::prompt_stash::PromptStash::new();
    // Cross-process active-session cap (hermes active_sessions): the lease
    // is released automatically when the REPL exits.
    let (_session_lease, session_limit_error) =
        ulnclaw::active_sessions::try_acquire_active_session(
            &agent.context().session_id,
            "cli",
            config,
        );
    if let Some(message) = session_limit_error {
        return Err(message);
    }
    print_welcome_banner(config, &agent).await;
    // Random feature tip (hermes startup tip), tinted with the active
    // skin's banner_dim.
    print_tip();
    let repl_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    ulnclaw::plugins::fire_session_event(
        "on_session_start",
        &agent.context().session_id,
        &repl_cwd,
        serde_json::json!({"source": "cli", "mode": "repl"}),
    )
    .await;

    if let Some(label) = &resumed_from {
        println!("Resuming session: {}", label);
    }
    let stdin = std::io::stdin();
    let mut session_key = agent.context().session_id.clone();
    let store = agent.context().store.clone();
    // Standing-goal (Ralph loop) manager for this session — state persists
    // in state.db keyed by session id (hermes GoalManager per live session).
    let mut goal_manager = ulnclaw::goals::GoalManager::new(
        session_key.clone(),
        store.clone(),
        ulnclaw::goals::DEFAULT_MAX_TURNS,
    );
    // Input queued by slash commands (e.g. /goal kicks the loop with the
    // goal text; the judge feeds continuation prompts through here too).
    let mut pending: Option<String> = None;
    loop {
        // Drain finished background delegations into the conversation
        // (hermes CLI completion drain, positive-ownership by session key).
        for completion in
            ulnclaw::async_delegation::drain_completions(store.as_deref(), &session_key)
        {
            println!(
                "\n[delegation {} finished — consolidated result injected]",
                completion.delegation_id
            );
            history.push(Message {
                role: Role::User,
                content: Some(completion.message),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        let input = if let Some(next) = pending.take() {
            next
        } else {
            let stash_indicator = stash.indicator();
            if stash_indicator.is_empty() {
                print!("\n> ");
            } else {
                print!("\n{stash_indicator} > ");
            }
            std::io::stdout().flush().map_err(|e| e.to_string())?;
            let mut line = String::new();
            stdin.read_line(&mut line).map_err(|e| e.to_string())?;
            line.trim().to_string()
        };
        if input.is_empty() {
            continue;
        }
        if input.starts_with('/') {
            match handle_slash(
                &input,
                &agent,
                &mut history,
                &mut goal_manager,
                &mut pending,
                &mut session_key,
                &mut stash,
                &display,
            )
            .await
            {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    println!("error: {}", e);
                    continue;
                }
            }
        }

        match agent
            .run_with_session(&input, Some(history.clone()), Some(&session_key))
            .await
        {
            Ok(result) => {
                let content = ulnclaw::plugins::transform_llm_output(
                    &session_key,
                    &repl_cwd,
                    &result.content,
                )
                .await;
                println!("\n{}", content);
                // Focus view post-turn recovery line: how many tool lines
                // were hidden and how to get them back (hermes format_hidden_line).
                let hidden_line = {
                    let mut state = display.lock().unwrap();
                    ulnclaw::focus_view::format_hidden_line(state.take_hidden_count())
                };
                if let Some(hidden_line) = hidden_line {
                    println!("{hidden_line}");
                }
                // Keep the conversation going (drop system prompt from history).
                history = result
                    .conversation
                    .into_iter()
                    .filter(|m| m.role != Role::System)
                    .collect();
                // The Ralph loop: when a standing goal is active, judge the
                // turn and feed the continuation prompt back as the next
                // user message until the goal is done/paused/cleared.
                if goal_manager.is_active() {
                    if let Some(provider) = agent.tool_context().provider.clone() {
                        let background = ulnclaw::goals::gather_background_processes();
                        let decision = goal_manager
                            .evaluate_after_turn(config, provider, &result.content, &background)
                            .await;
                        if !decision.message.is_empty() {
                            println!("\n{}", decision.message);
                        }
                        if decision.should_continue {
                            if let Some(prompt) = decision.continuation_prompt {
                                pending = Some(prompt);
                            }
                        }
                    }
                }
            }
            Err(e) => println!("error: {}", e),
        }
    }
    ulnclaw::plugins::fire_session_event(
        "on_session_end",
        &session_key,
        &repl_cwd,
        serde_json::json!({"source": "cli", "mode": "repl"}),
    )
    .await;
    // Plugin hook: on_session_finalize — the REPL session is done and its
    // state can be archived/flushed (hermes session-boundary event).
    ulnclaw::plugins::fire_session_event(
        "on_session_finalize",
        &session_key,
        &repl_cwd,
        serde_json::json!({"source": "cli", "mode": "repl"}),
    )
    .await;
    Ok(())
}

/// System note injected into the conversation when `/browser connect`
/// succeeds (hermes `_pending_input` context injection).
const BROWSER_CONNECT_NOTE: &str = "[System note: The user invoked /browser connect and connected your browser tools to a Chromium-family dev/debug browser via Chrome DevTools Protocol. Your browser_navigate, browser_snapshot, browser_click, and other browser tools now control that CDP browser. The command itself is a signal that using browser tools for their current browser-related request is expected; do not wait for separate permission just because CDP is connected. This is typically an isolated debug profile, not the user's main everyday browser. It is still user-visible and may contain pages, logged-in sessions, or cookies in that debug profile, so avoid destructive actions, closing tabs, or navigating away unless the user's task calls for it.]";

async fn handle_slash(
    input: &str,
    agent: &Arc<Agent>,
    history: &mut Vec<Message>,
    goals: &mut ulnclaw::goals::GoalManager,
    pending: &mut Option<String>,
    session_key: &mut String,
    stash: &mut ulnclaw::prompt_stash::PromptStash,
    display: &Arc<std::sync::Mutex<ulnclaw::focus_view::DisplayState>>,
) -> Result<bool, String> {
    let mut parts = input.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    match cmd {
        "/focus" => {
            let (enabled, configured) = {
                let state = display.lock().unwrap();
                (state.focus_enabled, state.configured_mode.clone())
            };
            match ulnclaw::focus_view::resolve_focus_arg(rest, enabled) {
                ulnclaw::focus_view::FocusArg::Set(target) => {
                    let message = {
                        let mut state = display.lock().unwrap();
                        state.focus_enabled = target;
                        ulnclaw::focus_view::format_focus_toggle_message(
                            target,
                            Some(&state.configured_mode),
                        )
                    };
                    println!("{message}");
                }
                ulnclaw::focus_view::FocusArg::Status => {
                    println!(
                        "{}",
                        ulnclaw::focus_view::format_focus_status(enabled, Some(&configured))
                    );
                }
                ulnclaw::focus_view::FocusArg::Usage => {
                    println!("{}", ulnclaw::focus_view::FOCUS_USAGE);
                }
            }
        }
        "/verbose" => {
            let mut state = display.lock().unwrap();
            if rest.is_empty() {
                println!(
                    "tool progress: {} (modes: off | new | all | verbose)",
                    state.configured_mode
                );
            } else {
                let wanted = rest.trim().to_ascii_lowercase();
                if ulnclaw::focus_view::TOOL_PROGRESS_MODES.contains(&wanted.as_str()) {
                    state.configured_mode = wanted.clone();
                    println!("tool progress: {}", wanted.to_uppercase());
                } else {
                    println!("unknown mode '{rest}' — modes: off | new | all | verbose");
                }
            }
        }
        "/stash" => {
            // hermes Ctrl+S gesture mapped onto the line REPL: content →
            // park; empty + 1 item → pop; empty + 2+ → browse list.
            let mut parts = rest.splitn(2, ' ');
            let head = parts.next().unwrap_or("");
            let tail = parts.next().unwrap_or("").trim();
            match head {
                "list" => {
                    if stash.is_empty() {
                        println!("nothing stashed.");
                    } else {
                        println!("stashed drafts (newest first):");
                        for (idx, entry) in stash.items().iter().enumerate() {
                            println!("  {}. {}", idx + 1, entry.preview);
                        }
                    }
                }
                "pop" => {
                    let index: usize = tail
                        .parse::<usize>()
                        .map(|n| n.saturating_sub(1))
                        .unwrap_or(0);
                    match stash.pop(index) {
                        Some((text, _images)) => println!("restored draft:\n{text}"),
                        None => println!("nothing to pop at that position."),
                    }
                }
                "drop" => {
                    let index: usize = tail
                        .parse::<usize>()
                        .map(|n| n.saturating_sub(1))
                        .unwrap_or(usize::MAX);
                    if index == usize::MAX || index >= stash.len() {
                        println!("usage: /stash drop <n> (1 = newest)");
                    } else if stash.pop(index).is_some() {
                        println!("dropped draft {} ({} remain).", index + 1, stash.len());
                    }
                }
                "clear" => {
                    stash.clear();
                    println!("stash cleared.");
                }
                "" => match ulnclaw::prompt_stash::resolve_ctrl_s(stash, "", &[]) {
                    (ulnclaw::prompt_stash::StashAction::Restored, Some((text, _images))) => {
                        println!("restored draft:\n{text}");
                    }
                    (ulnclaw::prompt_stash::StashAction::OpenPanel, None) => {
                        println!("stashed drafts (newest first):");
                        for (idx, entry) in stash.items().iter().enumerate() {
                            println!("  {}. {}", idx + 1, entry.preview);
                        }
                        println!("/stash pop <n> restores · /stash drop <n> deletes · /stash clear wipes");
                    }
                    _ => println!("nothing stashed."),
                },
                _ => {
                    // Anything else is draft text to park (hermes gesture).
                    if stash.stash(rest, &[]) {
                        println!(
                            "stashed ({} parked). {}",
                            stash.len(),
                            stash.placeholder_hint()
                        );
                    }
                }
            }
        }
        "/pet" => {
            // Toggle, browse, or adopt a petdex mascot (hermes `/pet`).
            // Gallery/install calls use reqwest::blocking, so run the whole
            // thing off the async REPL task.
            let home = ulnclaw::config::ulnclaw_home();
            let rest = rest.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                let low = rest.to_lowercase();
                if rest.is_empty() || low == "toggle" {
                    let (enabled, name, err) = ulnclaw::pets::toggle_pet_display(&home);
                    if let Some(err) = err {
                        println!("(x_x) {err}");
                    } else if enabled {
                        let name = name.unwrap_or_else(|| "your pet".to_string());
                        println!("(^_^)b {name} is out — it'll pop in shortly.");
                    } else {
                        match name {
                            Some(name) => println!("(-_-)zzZ {name} put away."),
                            None => println!("(-_-)zzZ Pet put away."),
                        }
                    }
                } else if matches!(low.as_str(), "list" | "gallery" | "browse" | "all") {
                    let _ = ulnclaw::pets::cmd_list(&home, "", false, 40);
                } else if low == "scale" || low.starts_with("scale ") {
                    let value = rest["scale".len()..].trim();
                    if value.is_empty() {
                        println!("(o_o) Usage: /pet scale <factor>  (e.g. /pet scale 0.5)");
                    } else {
                        let _ = ulnclaw::pets::cmd_scale(value);
                    }
                } else if low == "off" {
                    let _ = ulnclaw::pets::cmd_off();
                } else {
                    println!("(o_o) Fetching '{rest}' from petdex…");
                    let code = ulnclaw::pets::cmd_install(&home, &rest, false, true);
                    if code != 0 {
                        println!("(x_x) Couldn't adopt '{rest}'.");
                    }
                }
            })
            .await
            .map_err(|e| e.to_string())?;
        }
        "/kanban" => {
            // Board ops inline (hermes `/kanban` → kanban.run_slash).
            let home = ulnclaw::config::ulnclaw_home();
            let rest = rest.to_string();
            let output =
                tokio::task::spawn_blocking(move || ulnclaw::kanban::run_slash(&home, &rest))
                    .await
                    .map_err(|e| e.to_string())?;
            print!("{output}");
        }
        "/egress" => {
            // Docker egress proxy status (hermes `/egress` →
            // proxy_cli.format_status_text).
            let show_tokens = rest.trim() == "--show-tokens";
            let text = tokio::task::spawn_blocking(move || {
                ulnclaw::egress_cmd::format_status_text(show_tokens)
            })
            .await
            .map_err(|e| e.to_string())?;
            println!("{text}");
        }
        "/hatch" => {
            // Generate a brand-new pet from a description (hermes `/hatch`).
            // Runs the full image-model pipeline, so keep the blocking calls
            // off the async REPL task.
            let home = ulnclaw::config::ulnclaw_home();
            let concept = rest.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                ulnclaw::pets_generate::cmd_hatch(&home, &concept, None, None, None, 0)
            })
            .await
            .map_err(|e| e.to_string())?;
        }
        "/paste" => {
            // hermes clipboard: save the clipboard image as PNG under the
            // ulnclaw home and hand the agent a path reference.
            if ulnclaw::clipboard::is_remote_shell_session() {
                println!("note: SSH session detected — native clipboard tools write the REMOTE machine's clipboard; your terminal's OSC 52 paste reaches the local one.");
            }
            if !ulnclaw::clipboard::has_clipboard_image() {
                println!("no image on the clipboard.");
            } else {
                let dir = ulnclaw::clipboard::clipboard_dir();
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let dest = dir.join(format!("clipboard-{ts}.png"));
                if ulnclaw::clipboard::save_clipboard_image(&dest) {
                    println!("saved clipboard image: {}", dest.display());
                    println!("reference it in your next prompt (vision_analyze / read_file).");
                } else {
                    println!(
                        "clipboard image extraction failed (is wl-paste/xclip/pngpaste installed?)"
                    );
                }
            }
        }
        "/quit" | "/exit" | "/q" => return Ok(false),
        "/new" => {
            history.clear();
            // Plugin hook: on_session_reset — the old conversation is being
            // abandoned for a fresh one (hermes session-boundary event).
            ulnclaw::plugins::fire_session_event(
                "on_session_reset",
                session_key,
                &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                serde_json::json!({"source": "cli", "mode": "repl"}),
            )
            .await;
            // Fresh conversation = fresh session row (hermes /new); rotate
            // the live key and reset the per-session goal manager.
            *session_key = uuid::Uuid::new_v4().to_string();
            *goals = ulnclaw::goals::GoalManager::new(
                session_key.clone(),
                agent.context().store.clone(),
                ulnclaw::goals::DEFAULT_MAX_TURNS,
            );
            println!("New conversation started.");
            print_tip();
        }
        "/help" => {
            println!(
                "Commands:\n  /new            start a fresh conversation\n  /history        show turn count\n  /recap          recap recent activity in this conversation\n  /moa <prompt>   one-shot Mixture-of-Agents synthesis (default preset)\n  /reload           reload <home>/.env vars into the running session\n  /search <text>  search past sessions\n  /tools          list enabled tools\n  /browser <status|connect [url]|disconnect>   browser CDP endpoint\n  /skills         list skills\n  /<bundle>       invoke a skill bundle (ulnclaw bundles)\n  /memory         show persistent memory\n  /goal [text|status|show|draft|pause|resume|clear|wait|unwait]   standing goal (Ralph loop)\n  /subgoal [text|remove <n>|clear]   extra criteria on the active goal\n  /blueprint [name] [slot=val…]   automation blueprints: catalog, guided setup, or direct create\n  /suggestions [accept N|dismiss N|catalog|clear]   suggested automations\n  /cron [list|show|pause|resume|run|remove|status]   manage scheduled jobs (hermes /cron)\n  /init [notes]        generate or update AGENTS.md from a project scan (hermes /init)\n  /agents              active agents + recent delegations (hermes /agents)\n  /journey             learning timeline: learned skills + memory (hermes /journey)\n  /snapshot [create [label]|list|restore <id>|prune [keep]]   state snapshots (hermes /snapshot)\n  /platforms           messaging-platform connection posture (hermes /platforms)\n  /platform [list|pause|resume] [name]   gateway platform retry controls (hermes /platform)\n  /curator [status|run|pause|resume|pin|unpin|archive|restore|list-archived]   skill maintenance (hermes /curator)\n  /bundles [list|show <name>]   skill bundles; /<slug> invokes one (hermes /bundles)\n  /toolsets            toolset catalog, registered ones marked (hermes /toolsets)\n  /sessions       list recent sessions\n  /usage          token usage of this conversation\n  /insights [days]  usage analytics across sessions (hermes insights)\n  /rollback [N|hash] [file]   list/restore checkpoints (hermes-style)\n  /rollback diff <N|hash>     preview changes since a checkpoint\n  /diff [N|hash|session]      cumulative session diff / vs a checkpoint\n  /gitdiff [staged|all]     git working-tree diff (what changed here?)\n  /focus [on|off|status]    focus view: just prompt + answer, hidden-line count (hermes /focus)\n  /verbose [off|new|all|verbose]   tool-progress mode (hermes /verbose)\n  /stash [text|list|pop [n]|drop <n>|clear]   park/restore draft prompts (hermes Ctrl+S stash)\n  /kanban [list|show|create|done|block|unblock|comment|boards|promote|reclaim|assign|runs|stats]   coordination board (hermes /kanban)\n  /egress [--show-tokens]   Docker egress proxy status (hermes /egress)\n  /pet [toggle|list|scale <n>|off|<slug>]   petdex mascot (hermes /pet)\n  /hatch <description>   generate a brand-new pet (hermes /hatch)\n  /paste            save the clipboard image to the ulnclaw home (hermes clipboard)\n  /reload-mcp     reload MCP servers from config (prompt-cache warning + confirm)\n  /quit           exit"
            );
        }
        "/reload-mcp" => {
            // hermes /reload-mcp: rebuilds the MCP tool surface. That
            // invalidates the provider prompt cache, so confirm first
            // unless approvals.mcp_reload_confirm = false (persisted by
            // the "always" choice).
            let confirm_required = agent.tool_context().config.approvals.mcp_reload_confirm;
            if confirm_required {
                println!("⚠️  /reload-mcp — prompt cache invalidation warning");
                println!("   Reloading MCP servers rebuilds the tool set for this session and");
                println!("   invalidates the provider prompt cache; the next message re-sends");
                println!(
                    "   full input tokens (expensive on long-context / high-reasoning models)."
                );
                print!("   Approve [o]nce / [a]lways / [c]ancel? ");
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer).ok();
                let choice = match answer.trim().to_lowercase().as_str() {
                    "o" | "once" | "y" | "yes" => "once",
                    "a" | "always" => "always",
                    _ => "cancel",
                };
                if choice == "cancel" {
                    println!("🟡 /reload-mcp cancelled. MCP tools unchanged.");
                    return Ok(true);
                }
                if choice == "always" {
                    match ulnclaw::config_cmd::set_config_value(
                        "approvals.mcp_reload_confirm",
                        "false",
                        false,
                    ) {
                        Ok(_) => {
                            println!("🔒 Future /reload-mcp calls will run without confirmation.")
                        }
                        Err(e) => println!("⚠️  Couldn't persist opt-out ({}); reloading once.", e),
                    }
                }
            }
            println!("🔄 Reloading MCP servers...");
            // Re-read config.toml fresh so newly added/edited servers apply.
            match ulnclaw::config::UlncLawConfig::load(None) {
                Ok(fresh_config) => {
                    let report = agent.reload_mcp(&fresh_config).await;
                    println!("{}", ulnclaw::mcp::format_reload_report(&report));
                    // Inject a note at the END of the conversation so the
                    // model knows tools changed (appended last to preserve
                    // the prompt-cache prefix).
                    let mut change_parts: Vec<String> = Vec::new();
                    if !report.added.is_empty() {
                        change_parts.push(format!("Added servers: {}", report.added.join(", ")));
                    }
                    if !report.removed.is_empty() {
                        change_parts
                            .push(format!("Removed servers: {}", report.removed.join(", ")));
                    }
                    if !report.reconnected.is_empty() {
                        change_parts.push(format!(
                            "Reconnected servers: {}",
                            report.reconnected.join(", ")
                        ));
                    }
                    if change_parts.is_empty() {
                        change_parts.push("server list unchanged".to_string());
                    }
                    history.push(Message {
                        role: Role::User,
                        content: Some(format!(
                            "[system note] MCP tools were just reloaded ({}). {} tool(s) now available.",
                            change_parts.join("; "),
                            report.tool_count
                        )),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                }
                Err(e) => println!("reload failed: {}", e),
            }
        }
        "/history" => {
            println!("{} messages in current conversation.", history.len());
        }
        "/recap" => {
            println!(
                "{}",
                ulnclaw::session::recap::build_recap(history, None, None)
            );
        }
        "/reload" => {
            let count = ulnclaw::config::reload_env();
            println!("\u{2713} Reloaded .env — {count} variable(s) updated.");
        }
        "/moa" => {
            if rest.is_empty() {
                println!("usage: /moa <prompt>  (runs one prompt through the default MoA preset)");
            } else {
                let config = agent.tool_context().config.clone();
                match ulnclaw::moa::run_moa(&config, rest, None).await {
                    Ok(outcome) => {
                        for reference in &outcome.references {
                            if reference.failed() {
                                eprintln!("  ✗ {} failed", reference.label);
                            } else {
                                eprintln!("  ✓ {}", reference.label);
                            }
                        }
                        println!("{}", outcome.synthesis);
                    }
                    Err(e) => println!("moa failed: {}", e),
                }
            }
        }
        "/usage" => {
            println!("(usage is tracked per session in state.db; see `ulnclaw sessions list`)");
        }
        "/insights" => {
            let days: u32 = rest.parse().unwrap_or(30);
            let provider = agent.tool_context().config.model.provider.clone();
            match ulnclaw::insights::InsightsEngine::open(&ulnclaw::insights::default_store_path())
            {
                Ok(engine) => match engine.generate(days, None, Some(provider.as_str())) {
                    Ok(report) => print!("{}", ulnclaw::insights::format_terminal(&report)),
                    Err(e) => println!("insights failed: {e}"),
                },
                Err(e) => println!("insights failed: {e}"),
            }
        }
        "/search" => {
            if rest.is_empty() {
                println!("usage: /search <text>");
            } else if let Some(store) = agent.tool_context().store.clone() {
                match store.search_messages(rest, 10) {
                    Ok(hits) => {
                        if hits.is_empty() {
                            println!("No matches.");
                        }
                        for (session_id, snippet) in hits {
                            println!("[{}] {}", &session_id[..session_id.len().min(8)], snippet);
                        }
                    }
                    Err(e) => println!("search failed: {}", e),
                }
            }
        }
        "/tools" => {
            println!("(use `ulnclaw tools` outside the REPL)");
        }
        "/browser" => {
            // hermes `/browser` UX: live CDP endpoint control.
            let mut parts = rest.splitn(2, ' ');
            match parts.next().unwrap_or("") {
                "status" | "" => {
                    if ulnclaw::browser::camofox::is_camofox_mode() {
                        let url = ulnclaw::browser::camofox::camofox_url().unwrap_or_default();
                        let available = ulnclaw::browser::camofox::check_available().await;
                        let vnc = ulnclaw::browser::camofox::vnc_url().await;
                        println!(
                            "browser: camofox backend — {url} (available: {available}{})",
                            vnc.map(|v| format!(", vnc: {v}")).unwrap_or_default()
                        );
                    } else {
                        match ulnclaw::browser::endpoint_with_source() {
                            Some((source, raw)) => {
                                let mode = if ulnclaw::browser::is_auto_mode(&raw) { "managed" } else { "endpoint" };
                                println!("browser: configured via {source} — {raw} (mode: {mode})");
                            }
                            None => println!("browser: not configured (set ULNCLAW_BROWSER_CDP or /browser connect <url>, or CAMOFOX_URL for the Camofox backend)"),
                        }
                    }
                }
                "connect" => {
                    let url = parts.next().unwrap_or("").trim().to_string();
                    if url.is_empty() {
                        // Hermes default flow: probe both loopbacks, arbitrate
                        // a squatted port, auto-launch a visible debug
                        // browser with per-candidate diagnostics.
                        let outcome = ulnclaw::browser::connect::connect_local_default(
                            ulnclaw::browser::connect::DEFAULT_BROWSER_CDP_PORT,
                        )
                        .await;
                        println!();
                        for line in &outcome.messages {
                            println!("   {line}");
                        }
                        if let Some(found) = outcome.url {
                            match ulnclaw::browser::set_cdp_override(&found) {
                                Ok(()) => {
                                    println!();
                                    println!("🌐 Browser connected to live Chromium-family browser via CDP");
                                    println!("   Endpoint: {found}");
                                    println!();
                                    history.push(Message {
                                        role: Role::User,
                                        content: Some(BROWSER_CONNECT_NOTE.to_string()),
                                        tool_calls: None,
                                        tool_call_id: None,
                                        name: None,
                                    });
                                }
                                Err(e) => println!("connect failed: {e}"),
                            }
                        }
                    } else {
                        match ulnclaw::browser::set_cdp_override(&url) {
                            Ok(()) => {
                                println!(
                                    "browser endpoint set to {url} (live until disconnect/exit)"
                                );
                                history.push(Message {
                                    role: Role::User,
                                    content: Some(BROWSER_CONNECT_NOTE.to_string()),
                                    tool_calls: None,
                                    tool_call_id: None,
                                    name: None,
                                });
                            }
                            Err(e) => println!("connect failed: {e}"),
                        }
                    }
                }
                "disconnect" => {
                    ulnclaw::browser::clear_cdp_override();
                    println!("browser endpoint override cleared");
                    history.push(Message {
                        role: Role::User,
                        content: Some(
                            "[System note: The user has disconnected the browser tools from their                              live Chromium-family browser. Browser tools are back to default mode                              (managed local browser or configured endpoint).]"
                                .to_string(),
                        ),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                }
                other => {
                    println!("unknown /browser subcommand: {other} (status|connect|disconnect)")
                }
            }
        }
        "/gitdiff" => {
            let (mode, rest) = match rest.split_whitespace().next() {
                Some("staged") => (ulnclaw::git_diff::DiffMode::Staged, ""),
                Some("all") => (ulnclaw::git_diff::DiffMode::All, ""),
                _ => (ulnclaw::git_diff::DiffMode::Working, rest),
            };
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            match ulnclaw::git_diff::collect_working_diff(&cwd, mode, &[]) {
                Ok(result) if result.empty => println!("No changes ({} mode).", mode.as_str()),
                Ok(result) => {
                    if !result.stat.is_empty() {
                        println!("{}", result.stat);
                        println!();
                    }
                    println!("{}", result.diff);
                    let _ = rest;
                }
                Err(e) => println!("gitdiff failed: {}", e),
            }
        }
        "/skills" => {
            let dir = agent.tool_context().home.join("skills");
            for skill in ulnclaw::skills::list_skills(&dir) {
                println!("  {} — {}", skill.name, skill.description);
            }
        }
        "/memory" => {
            match ulnclaw::tools::builtin::memory::load_memory_for_prompt(
                &agent.tool_context().home,
            ) {
                Some(memory) => println!("{}", memory),
                None => println!("(memory is empty)"),
            }
        }
        "/goal" => {
            // Standing goal — the Ralph loop (hermes /goal). Subcommands:
            // status/show/draft/pause/resume/clear/wait/unwait; anything
            // else is treated as the goal text (inline `field: value` lines
            // become a completion contract).
            let lower = rest.to_ascii_lowercase();
            if rest.is_empty() || lower == "status" {
                println!("  {}", goals.status_line());
            } else if lower == "show" {
                println!("  {}", goals.status_line());
                println!("  {}", goals.render_contract());
            } else if lower.starts_with("draft") {
                let objective = rest["draft".len()..].trim();
                if objective.is_empty() {
                    println!("  Usage: /goal draft <objective in plain language>");
                } else {
                    println!("  Drafting completion contract…");
                    let contract = match agent.tool_context().provider.clone() {
                        Some(provider) => {
                            let config = agent.tool_context().config.clone();
                            ulnclaw::goals::draft_contract(&config, provider, objective).await
                        }
                        None => None,
                    };
                    match goals.set(objective, None, contract) {
                        Ok(state) => {
                            println!(
                                "  ⊙ Goal set ({}-turn budget): {}",
                                state.max_turns, state.goal
                            );
                            if state.has_contract() {
                                println!("  Drafted completion contract:");
                                for line in state.contract.render_block().lines() {
                                    println!("    {}", line);
                                }
                                println!(
                                    "  Tighten any field by re-setting the goal with inline lines (e.g. verify: <command>). Use /goal show to review."
                                );
                            } else {
                                println!(
                                    "  Couldn't draft a contract (aux model unavailable) — running as a free-form goal. The per-turn judge still applies."
                                );
                            }
                            pending.replace(state.goal.clone());
                        }
                        Err(e) => println!("  Invalid goal: {}", e),
                    }
                }
            } else if lower == "pause" {
                match goals.pause("user-paused") {
                    Some(state) => println!("  ⏸ Goal paused: {}", state.goal),
                    None => println!("  No goal set."),
                }
            } else if lower == "resume" {
                match goals.resume(true) {
                    Some(state) => {
                        println!("  ▶ Goal resumed: {}", state.goal);
                        println!("  Send any message (or type 'continue') to kick it off.");
                    }
                    None => println!("  No goal to resume."),
                }
            } else if matches!(lower.as_str(), "clear" | "stop" | "done") {
                let had = goals.has_goal();
                goals.clear();
                if had {
                    println!("  ✓ Goal cleared.");
                } else {
                    println!("  No active goal.");
                }
            } else if lower == "wait" || lower.starts_with("wait ") {
                let wait_arg = rest["wait".len()..].trim();
                if wait_arg.is_empty() {
                    println!("  Usage: /goal wait <pid> [reason]");
                } else {
                    let mut wtokens = wait_arg.splitn(2, char::is_whitespace);
                    let pid_str = wtokens.next().unwrap_or("");
                    match pid_str.parse::<u32>() {
                        Ok(pid) => {
                            let reason = wtokens.next().unwrap_or("").trim();
                            match goals.wait_on(pid, reason) {
                                Ok(_) => {
                                    let suffix = if reason.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" ({})", reason)
                                    };
                                    println!(
                                        "  ⏳ Goal parked on pid {}{}. Loop pauses until it exits.",
                                        pid, suffix
                                    );
                                }
                                Err(e) => println!("  /goal wait: {}", e),
                            }
                        }
                        Err(_) => println!("  /goal wait: <pid> must be an integer process id."),
                    }
                }
            } else if lower == "unwait" {
                if goals.stop_waiting() {
                    println!("  ▶ Wait barrier cleared — goal loop resumes.");
                } else {
                    println!("  No wait barrier set.");
                }
            } else {
                let (headline, contract) = ulnclaw::goals::parse_contract(rest);
                let goal_text = if headline.is_empty() {
                    rest.to_string()
                } else {
                    headline
                };
                let contract = if contract.is_empty() {
                    None
                } else {
                    Some(contract)
                };
                match goals.set(&goal_text, None, contract) {
                    Ok(state) => {
                        println!(
                            "  ⊙ Goal set ({}-turn budget): {}",
                            state.max_turns, state.goal
                        );
                        if state.has_contract() {
                            println!("  Completion contract:");
                            for line in state.contract.render_block().lines() {
                                println!("    {}", line);
                            }
                        }
                        println!(
                            "  After each turn, a judge model checks if the goal is done{}. The agent keeps working until it is, you pause/clear it, or the budget is exhausted. Use /goal status, /goal show, /goal pause, /goal resume, /goal clear.",
                            if state.has_contract() { " against the contract above" } else { "" }
                        );
                        pending.replace(state.goal.clone());
                    }
                    Err(e) => println!("  Invalid goal: {}", e),
                }
            }
        }
        "/subgoal" => {
            // Extra criteria on the active goal (hermes /subgoal).
            if rest.is_empty() {
                println!("  {}", goals.render_subgoals());
            } else {
                let mut sub_parts = rest.splitn(2, ' ');
                match sub_parts.next().unwrap_or("") {
                    "remove" => {
                        let idx_str = sub_parts.next().unwrap_or("").trim();
                        if idx_str.is_empty() {
                            println!("  Usage: /subgoal remove <n>");
                        } else {
                            match idx_str.parse::<usize>() {
                                Ok(n) => match goals.remove_subgoal(n) {
                                    Ok(removed) => {
                                        println!("  ✓ Removed subgoal {}: {}", n, removed)
                                    }
                                    Err(e) => println!("  /subgoal remove: {}", e),
                                },
                                Err(_) => println!(
                                    "  /subgoal remove: <n> must be an integer (1-based index)."
                                ),
                            }
                        }
                    }
                    "clear" => match goals.clear_subgoals() {
                        Ok(count) => println!("  ✓ Cleared {} subgoal(s).", count),
                        Err(e) => println!("  /subgoal clear: {}", e),
                    },
                    _ => match goals.add_subgoal(rest) {
                        Ok(text) => println!("  ✓ Added subgoal: {}", text),
                        Err(e) => println!("  /subgoal: {}", e),
                    },
                }
            }
        }
        "/blueprint" => {
            // Hermes /blueprint CLI parity: catalog listing, guided setup
            // (the seed runs as the next user turn via `pending`), or
            // direct inline create with slot=val pairs.
            let args = rest.to_string();
            let result = tokio::task::spawn_blocking(move || {
                ulnclaw::cron::blueprints::handle_blueprint_command(&args)
            })
            .await
            .map_err(|e| e.to_string())?;
            println!("{}", result.text);
            if let Some(seed) = result.agent_seed {
                pending.replace(seed);
            }
        }
        "/suggestions" => {
            println!(
                "{}",
                ulnclaw::cron::suggestions::handle_suggestions_command(rest)
            );
        }
        "/toolsets" => {
            // Hermes /toolsets parity (P672): toolset catalog with the
            // agent's registered toolsets marked.
            let enabled = agent.toolset_names();
            print!("{}", ulnclaw::toolsets::format_toolsets_digest(&enabled));
        }
        "/bundles" => {
            // Hermes /bundles parity (P671): list/show skill bundles.
            print!("{}", ulnclaw::bundles::run_slash(rest));
        }
        "/curator" => {
            // Hermes /curator parity (P670): skill maintenance from
            // the chat prompt (the CLI twin is `ulnclaw curator`).
            let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
            print!("{}", ulnclaw::curator::run_slash(&home, rest));
        }
        "/platforms" => {
            // Hermes /platforms parity (P669): messaging-platform
            // posture digest.
            let rows = ulnclaw::messaging::platform_state_rows();
            print!("{}", ulnclaw::messaging::format_platforms_digest(&rows));
        }
        "/platform" => {
            // Hermes /platform parity (P694): retry-queue controls.
            println!("{}", ulnclaw::messaging::run_platform_slash(rest));
        }
        "/snapshot" => {
            // Hermes /snapshot parity (P668): quick state snapshots
            // (create/list/restore/prune) from the chat prompt.
            let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
            print!("{}", ulnclaw::backup::snapshot_slash(&home, rest));
        }
        "/journey" => {
            // Hermes /journey parity (P667): the learning timeline as
            // a digest (the TUI renderer is `ulnclaw journey`).
            let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
            print!("{}", ulnclaw::learning_graph::journey_digest(&home, 15));
        }
        "/agents" => {
            // Hermes /agents parity (P666): active agents + recent
            // delegations digest from the live registry.
            let records = ulnclaw::async_delegation::list_delegations();
            print!(
                "{}",
                ulnclaw::async_delegation::format_agents_digest(&records)
            );
        }
        "/init" => {
            // Hermes /init parity (P665): generate or update AGENTS.md
            // from a project scan — a prompt-injection turn seeded via
            // `pending`, like /blueprint.
            let extra = rest.trim().to_string();
            let msg = tokio::task::spawn_blocking(move || {
                ulnclaw::init_command::build_init_prompt_for_cwd(None, &extra)
            })
            .await
            .map_err(|e| e.to_string())?;
            if ulnclaw::init_command::is_update_prompt(&msg) {
                println!("⚡ Updating AGENTS.md from a project scan...");
            } else {
                println!("⚡ Generating AGENTS.md from a project scan...");
            }
            pending.replace(msg);
        }
        "/cron" => {
            // Hermes /cron parity (P663): manage scheduled jobs inline;
            // `run <id>` seeds the job's prompt as the next user turn
            // via `pending`.
            let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
            let result = ulnclaw::cron::run_slash(&home, rest);
            print!("{}", result.text);
            if let Some(seed) = result.run_prompt {
                pending.replace(seed);
            }
        }
        "/sessions" => {
            if let Some(store) = agent.tool_context().store.clone() {
                match store.list_sessions(10) {
                    Ok(sessions) => {
                        for session in sessions {
                            println!(
                                "[{}] {} messages, model={:?}",
                                &session.id[..session.id.len().min(8)],
                                session.messages.len(),
                                session.metadata.model
                            );
                        }
                    }
                    Err(e) => println!("list failed: {}", e),
                }
            }
        }
        "/rollback" => {
            let context = agent.tool_context();
            let manager = context.checkpoint_manager();
            let dir_str = context.cwd().to_string_lossy().to_string();
            let mut sub = rest.splitn(2, ' ');
            let first = sub.next().unwrap_or("").trim();
            let second = sub.next().unwrap_or("").trim();
            let checkpoints = manager.list_checkpoints(&dir_str).await;
            if first.is_empty() {
                println!(
                    "{}",
                    ulnclaw::checkpoint::format_checkpoint_list(&checkpoints, &dir_str)
                );
            } else if first == "diff" {
                match resolve_checkpoint(&checkpoints, second) {
                    Some(hash) => match manager.diff(&dir_str, &hash).await {
                        Ok(result) => print_diff_result(&result),
                        Err(e) => println!("diff failed: {}", e),
                    },
                    None => println!("no such checkpoint: {}", second),
                }
            } else {
                match resolve_checkpoint(&checkpoints, first) {
                    Some(hash) => {
                        let file = if second.is_empty() {
                            None
                        } else {
                            Some(second)
                        };
                        match manager.restore(&dir_str, &hash, file).await {
                            Ok(result) => println!(
                                "✅ Restored to {} ({})",
                                result["restored_to"].as_str().unwrap_or("?"),
                                result["reason"].as_str().unwrap_or("?")
                            ),
                            Err(e) => println!("restore failed: {}", e),
                        }
                    }
                    None => println!("no such checkpoint: {}", first),
                }
            }
        }
        "/diff" => {
            let context = agent.tool_context();
            let manager = context.checkpoint_manager();
            let dir_str = context.cwd().to_string_lossy().to_string();
            if rest.is_empty() || rest == "session" {
                print_diff_result(&manager.session_diff(&dir_str).await);
            } else {
                let checkpoints = manager.list_checkpoints(&dir_str).await;
                match resolve_checkpoint(&checkpoints, rest) {
                    Some(hash) => match manager.diff(&dir_str, &hash).await {
                        Ok(result) => print_diff_result(&result),
                        Err(e) => println!("diff failed: {}", e),
                    },
                    None => println!("no such checkpoint: {}", rest),
                }
            }
        }
        other => {
            // Skill bundles win over the unknown-command fallback (hermes
            // bundle-over-skill slash precedence): /<bundle> loads every
            // member skill into one turn.
            let cmd_name = other.trim_start_matches('/');
            match ulnclaw::bundles::resolve_bundle_command_key(cmd_name) {
                Some(key) => {
                    let skills_dir = agent.tool_context().home.join("skills");
                    match ulnclaw::bundles::build_bundle_invocation_message(&key, rest, &skills_dir)
                    {
                        Some((message, loaded, missing)) => {
                            let mut note = format!(
                                "bundle {}: loaded {} skill(s)",
                                key.trim_start_matches('/'),
                                loaded.len()
                            );
                            if !missing.is_empty() {
                                note.push_str(&format!("; missing: {}", missing.join(", ")));
                            }
                            println!("{note}");
                            match agent
                                .run_with_session(
                                    &message,
                                    Some(history.clone()),
                                    Some(session_key),
                                )
                                .await
                            {
                                Ok(result) => {
                                    println!("\n{}", result.content);
                                    *history = result
                                        .conversation
                                        .into_iter()
                                        .filter(|m| m.role != Role::System)
                                        .collect();
                                }
                                Err(e) => println!("bundle run failed: {}", e),
                            }
                        }
                        None => println!(
                            "bundle {} has no loadable skills",
                            key.trim_start_matches('/')
                        ),
                    }
                }
                None => println!("unknown command: {} (/help for a list)", other),
            }
        }
    }
    Ok(true)
}

/// Resolve a checkpoint selector: 1-based list index or (prefix of a) hash.
fn resolve_checkpoint(
    checkpoints: &[ulnclaw::checkpoint::CheckpointEntry],
    selector: &str,
) -> Option<String> {
    if selector.is_empty() {
        return None;
    }
    if let Ok(n) = selector.parse::<usize>() {
        return checkpoints.get(n.saturating_sub(1)).map(|c| c.hash.clone());
    }
    checkpoints
        .iter()
        .find(|c| c.hash.starts_with(selector) || c.short_hash == selector)
        .map(|c| c.hash.clone())
}

fn print_diff_result(result: &serde_json::Value) {
    if result
        .get("empty")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        println!("No changes recorded.");
        return;
    }
    let stat = result["stat"].as_str().unwrap_or("");
    let diff = result["diff"].as_str().unwrap_or("");
    if stat.is_empty() && diff.is_empty() {
        println!("No changes.");
    } else {
        if !stat.is_empty() {
            println!("{}", stat);
        }
        if !diff.is_empty() {
            println!("{}", diff);
        }
    }
}

async fn auth_cmd(config: &UlncLawConfig, action: AuthAction) -> Result<(), String> {
    use ulnclaw::oauth;
    let home = ulnclaw::config::ulnclaw_home();
    let cfg = &config.oauth;
    match action {
        AuthAction::Login => {
            let auth = oauth::device_authorize(cfg)
                .await
                .map_err(|e| e.to_string())?;
            for line in oauth::login_instructions(&auth) {
                println!("{line}");
            }
            let tokens = oauth::poll_for_token(cfg, &auth)
                .await
                .map_err(|e| e.to_string())?;
            oauth::save_tokens(&home, &tokens).map_err(|e| e.to_string())?;
            println!("✓ Logged in.");
        }
        AuthAction::Status => {
            let tokens = oauth::load_tokens(&home);
            if tokens.logged_in() {
                println!("Auth: ✓ logged in");
                if tokens.expires_at > 0 {
                    let expired = tokens.expired();
                    println!(
                        "Access token expires: {} ({})",
                        tokens.expires_at,
                        if expired {
                            "EXPIRED — run `ulnclaw auth refresh`"
                        } else {
                            "valid"
                        }
                    );
                }
                if !tokens.scope.is_empty() {
                    println!("Scope: {}", tokens.scope);
                }
                println!(
                    "Refresh token: {}",
                    if tokens.refresh_token.is_empty() {
                        "none"
                    } else {
                        "stored"
                    }
                );
            } else {
                println!("Auth: not logged in (ulnclaw auth login)");
            }
            if cfg.token_url.is_empty() {
                println!("Note: [oauth] is not configured — set device_authorization_url/token_url/client_id.");
            }
        }
        AuthAction::Refresh => {
            oauth::refresh(cfg, &home)
                .await
                .map_err(|e| e.to_string())?;
            println!("✓ Access token refreshed.");
        }
        AuthAction::Logout => {
            oauth::clear_tokens(&home).map_err(|e| e.to_string())?;
            println!("✓ Tokens removed.");
        }
        AuthAction::Open => {
            if cfg.portal_url.is_empty() {
                return Err("no [oauth] portal_url configured".to_string());
            }
            println!("{}", cfg.portal_url);
        }
    }
    Ok(())
}

async fn proxy_cmd(config: &UlncLawConfig, action: Option<ProxyAction>) -> Result<(), String> {
    use ulnclaw::proxy_cmd;
    let home = ulnclaw::config::ulnclaw_home();
    match action {
        None => {
            println!("ulnclaw proxy — local OpenAI-compatible proxy that attaches your");
            println!("stored OAuth credentials to outbound requests.");
            println!();
            println!("Subcommands:");
            println!("  ulnclaw proxy start [--host 127.0.0.1] [--port 8645]");
            println!("      Run the proxy in the foreground.");
            println!("  ulnclaw proxy status");
            println!("      Show whether the upstream adapter is ready.");
            println!("  ulnclaw proxy providers");
            println!("      List available upstream providers.");
        }
        Some(ProxyAction::Status) => {
            let logged_in = proxy_cmd::is_authenticated(&home);
            let tokens = ulnclaw::oauth::load_tokens(&home);
            let expiry = if tokens.expires_at > 0 {
                chrono::DateTime::from_timestamp(tokens.expires_at as i64, 0)
                    .map(|dt| format!(" (bearer expires {})", dt.to_rfc3339()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            println!("ulnclaw proxy upstream adapters\n");
            if logged_in {
                println!("  [oauth   ] Stored OAuth token — ready{expiry}");
            } else {
                println!("  [oauth   ] Stored OAuth token — not logged in");
            }
            let upstream = config.proxy.upstream_url.trim();
            if upstream.is_empty() {
                println!("\n  upstream: not configured — set [proxy] upstream_url");
            } else {
                println!("\n  upstream: {upstream}");
            }
            println!("\nStart the proxy with: ulnclaw proxy start");
        }
        Some(ProxyAction::Providers) => {
            println!("Available proxy upstream providers:");
            println!("  oauth  — stored OAuth device-flow token ([oauth] config)");
        }
        Some(ProxyAction::Start { host, port }) => {
            let mut proxy_cfg = config.proxy.clone();
            if let Some(h) = host {
                proxy_cfg.host = h;
            }
            if let Some(p) = port {
                proxy_cfg.port = p;
            }
            if proxy_cfg.upstream_url.trim().is_empty() {
                return Err(
                    "no upstream configured — set [proxy] upstream_url in config.toml".to_string(),
                );
            }
            if !proxy_cmd::is_authenticated(&home) {
                return Err(
                    "not logged into the OAuth provider. Run `ulnclaw auth login` first."
                        .to_string(),
                );
            }
            println!(
                "Starting ulnclaw proxy for stored OAuth credentials\n  Listening on:  http://{}:{}/v1\n  Forwarding to: {}\n  Use any bearer token in the client — the proxy attaches your real credential.\n\nPress Ctrl+C to stop.",
                proxy_cfg.host, proxy_cfg.port, proxy_cfg.upstream_url
            );
            proxy_cmd::run_server(config.oauth.clone(), proxy_cfg, home)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// `ulnclaw egress` — managed iron-proxy sandbox egress (hermes
/// `hermes egress`). Handlers are synchronous (subprocess + fs), so
/// they run on the blocking pool.
async fn egress_cmd(action: EgressAction) -> Result<(), String> {
    use ulnclaw::egress_cmd;
    tokio::task::spawn_blocking(move || match action {
        EgressAction::Install { force } => egress_cmd::handle_install(force),
        EgressAction::Setup {
            tunnel_port,
            from_bitwarden,
            no_bitwarden,
            rotate_tokens,
            restart,
            no_restart,
        } => egress_cmd::handle_setup(egress_cmd::SetupOptions {
            tunnel_port,
            from_bitwarden,
            no_bitwarden,
            rotate_tokens,
            restart: if restart {
                Some(true)
            } else if no_restart {
                Some(false)
            } else {
                None
            },
        }),
        EgressAction::Start => egress_cmd::handle_start(),
        EgressAction::Stop => egress_cmd::handle_stop(),
        EgressAction::Restart => egress_cmd::handle_restart(),
        EgressAction::Reload => egress_cmd::handle_reload(),
        EgressAction::Status { show_tokens } => egress_cmd::handle_status(show_tokens),
        EgressAction::Disable => egress_cmd::handle_disable(),
        EgressAction::Config => egress_cmd::handle_config(),
    })
    .await
    .map_err(|e| format!("egress task join: {e}"))?
}

async fn sync_cmd(config: &UlncLawConfig, action: SyncAction) -> Result<(), String> {
    use ulnclaw::skills_sync;
    let home = ulnclaw::config::ulnclaw_home();
    let cfg = &config.sync;
    let oauth_cfg = &config.oauth;
    match action {
        SyncAction::Status => {
            let state = skills_sync::load_state(&home);
            println!("device id:   {}", state.device_id);
            let label = if !state.device_name.is_empty() {
                state.device_name.clone()
            } else if !cfg.device_name.is_empty() {
                cfg.device_name.clone()
            } else {
                "(unset)".to_string()
            };
            println!("device name: {label}");
            if let Some(reason) = skills_sync::inert_reason(cfg) {
                println!("gate:        INERT — {reason}");
            } else {
                println!("gate:        active ({})", cfg.base_url);
                match skills_sync::read_manifest(cfg, oauth_cfg, &home).await {
                    Ok(manifest) => {
                        println!("remote skills: {}", manifest.skills.len());
                        for (name, skill) in &manifest.skills {
                            println!(
                                "  {name} (from {}, {} file(s))",
                                skill.device,
                                skill.files.len()
                            );
                        }
                    }
                    Err(e) => println!("remote manifest: unavailable ({e})"),
                }
            }
            if state.enabled.is_empty() {
                println!("opted-in:    none (ulnclaw sync enable <skill>)");
            } else {
                println!("opted-in:    {}", state.enabled.join(", "));
            }
        }
        SyncAction::Pull => match skills_sync::pull(cfg, oauth_cfg, &home).await {
            Ok(names) if names.is_empty() => println!("Nothing new to pull."),
            Ok(names) => println!("✓ Pulled: {}", names.join(", ")),
            Err(e) => println!("{e}"),
        },
        SyncAction::Push => match skills_sync::push(cfg, oauth_cfg, &home).await {
            Ok(names) if names.is_empty() => {
                println!("Nothing to push (no skills opted in — ulnclaw sync enable <skill>).")
            }
            Ok(names) => println!("✓ Pushed: {}", names.join(", ")),
            Err(e) => println!("{e}"),
        },
        SyncAction::Now => {
            match skills_sync::pull(cfg, oauth_cfg, &home).await {
                Ok(names) if !names.is_empty() => println!("✓ Pulled: {}", names.join(", ")),
                Ok(_) => println!("Nothing new to pull."),
                Err(e) => println!("{e}"),
            }
            match skills_sync::push(cfg, oauth_cfg, &home).await {
                Ok(names) if !names.is_empty() => println!("✓ Pushed: {}", names.join(", ")),
                Ok(_) => println!("Nothing to push."),
                Err(e) => println!("{e}"),
            }
        }
        SyncAction::Enable { skill } => {
            let skills_dir = home.join("skills");
            if !skills_dir.join(&skill).join("SKILL.md").is_file() {
                return Err(format!(
                    "skill {skill:?} not found in {}",
                    skills_dir.display()
                ));
            }
            let mut state = skills_sync::load_state(&home);
            if !state.enabled.iter().any(|s| s == &skill) {
                state.enabled.push(skill.clone());
                skills_sync::save_state(&home, &state).map_err(|e| e.to_string())?;
            }
            println!("✓ {skill} opted into sync.");
        }
        SyncAction::Disable { skill } => {
            let mut state = skills_sync::load_state(&home);
            state.enabled.retain(|s| s != &skill);
            skills_sync::save_state(&home, &state).map_err(|e| e.to_string())?;
            println!("✓ {skill} opted out of sync.");
        }
        SyncAction::Device { name } => {
            if let Some(name) = name {
                let stored =
                    skills_sync::set_device_name(&home, &name).map_err(|e| e.to_string())?;
                println!("device label set to '{stored}'.");
                println!("New pushes from this device will use this label.");
            } else {
                println!("{}", skills_sync::stable_device_id(&home));
            }
        }
    }
    Ok(())
}

async fn plugins_cmd(config: &UlncLawConfig, action: PluginsAction) -> Result<(), String> {
    use ulnclaw::plugins;
    let home = ulnclaw::config::ulnclaw_home();
    match action {
        PluginsAction::List => {
            // Ensure the runtime is populated (plugins init happens in main).
            let _ = plugins::init(&home, config).await;
            let loaded = plugins::loaded_plugins();
            if loaded.is_empty() {
                println!("No plugins found in {}.", home.join("plugins").display());
                println!("Install a plugin directory with a plugin.toml manifest there.");
            } else {
                println!(
                    "{:<20} {:<8} {:<6} {:<6} {}",
                    "NAME", "VERSION", "HOOKS", "TOOLS", "DESCRIPTION"
                );
                for plugin in &loaded {
                    let disabled = plugin.disabled;
                    let name = if disabled {
                        format!("{} (disabled)", plugin.manifest.name)
                    } else {
                        plugin.manifest.name.clone()
                    };
                    println!(
                        "{:<20} {:<8} {:<6} {:<6} {}",
                        name,
                        plugin.manifest.version,
                        plugin.manifest.hooks.len(),
                        plugin.manifest.tools.len(),
                        plugin.manifest.description
                    );
                }
            }
            if !config.hooks.events.is_empty() {
                println!("\nConfig shell hooks ([hooks]):");
                let mut events: Vec<(&String, &Vec<String>)> = config.hooks.events.iter().collect();
                events.sort_by(|a, b| a.0.cmp(b.0));
                for (event, commands) in events {
                    for command in commands {
                        println!("  {event}: {command}");
                    }
                }
            }
        }
        PluginsAction::Enable { name } => {
            println!("{}", plugins::enable_plugin(&home, &name)?);
        }
        PluginsAction::Disable { name } => {
            println!("{}", plugins::disable_plugin(&home, &name)?);
        }
        PluginsAction::AcceptHooks => {
            let added = plugins::accept_all_hooks(&home, config);
            if added == 0 {
                println!("No pending hooks to accept.");
            } else {
                println!(
                    "✓ Accepted {added} hook command(s) into {}.",
                    home.join("shell-hooks-allowlist.json").display()
                );
            }
        }
        PluginsAction::Install {
            identifier,
            force,
            enable,
            no_enable,
        } => {
            if identifier.starts_with("http://") || identifier.starts_with("file://") {
                println!(
                    "Warning: Using insecure/local URL scheme. Consider using https:// or git@ for production installs."
                );
            }
            println!("Cloning {identifier}...");
            let installed = plugins::install_plugin(&home, &identifier, force)?;
            if !installed.has_manifest {
                println!(
                    "Warning: {} doesn't contain plugin.toml/plugin.yaml. It may not be a valid plugin.",
                    installed.name
                );
            }
            println!(
                "✓ Installed plugin {} → {}",
                installed.name,
                installed.dir.display()
            );
            let should_enable = if enable {
                true
            } else if no_enable {
                false
            } else if std::io::IsTerminal::is_terminal(&std::io::stdin())
                && std::io::IsTerminal::is_terminal(&std::io::stdout())
            {
                print!("  Enable '{}' now? [y/N]: ", installed.name);
                std::io::Write::flush(&mut std::io::stdout()).ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer).ok();
                matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
            } else {
                false
            };
            if should_enable {
                println!("{}", plugins::enable_plugin(&home, &installed.name)?);
            } else {
                println!("{}", plugins::disable_plugin(&home, &installed.name)?);
                println!(
                    "Plugin installed but not enabled. Run `ulnclaw plugins enable {}` to activate.",
                    installed.name
                );
            }
            println!("Restart the gateway for the plugin to take effect.");
        }
        PluginsAction::Update { name } => {
            println!("Updating {name}...");
            let output = plugins::update_plugin(&home, &name)?;
            if output.contains("Already up to date") {
                println!("✓ Plugin {name} is already up to date.");
            } else {
                println!("✓ Plugin {name} updated.");
                let trimmed = output.trim();
                if !trimmed.is_empty() {
                    println!("{trimmed}");
                }
            }
        }
        PluginsAction::Remove { name } => {
            println!("{}", plugins::remove_plugin(&home, &name)?);
        }
    }
    Ok(())
}

async fn pairing_cmd(action: PairingAction) -> Result<(), String> {
    use ulnclaw::pairing::PairingStore;
    let home = ulnclaw::config::ulnclaw_home();
    let store = PairingStore::open(&home);
    match action {
        PairingAction::List => {
            let platforms = store.known_platforms();
            if platforms.is_empty() {
                println!("No pairing activity yet.");
                println!("Unknown senders who DM an enabled bot receive a pairing code;");
                println!("approve it with: ulnclaw pairing approve <platform> <code>");
                return Ok(());
            }
            for platform in &platforms {
                let pending = store.list_pending(platform);
                if !pending.is_empty() {
                    println!("Pending ({platform}):");
                    for request in &pending {
                        let name = if request.user_name.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", request.user_name)
                        };
                        println!(
                            "  {}  user {}{}  ({}m old)  approve: ulnclaw pairing approve {platform} {}",
                            request.request_id, request.user_id, name, request.age_minutes, request.request_id
                        );
                    }
                }
                let approved = store.list_approved(platform);
                if !approved.is_empty() {
                    println!("Approved ({platform}):");
                    for grant in &approved {
                        let name = if grant.user_name.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", grant.user_name)
                        };
                        println!("  {}{}", grant.user_id, name);
                    }
                }
            }
        }
        PairingAction::Approve { platform, code } => match store.approve_code(&platform, &code) {
            Some(grant) => {
                let name = if grant.user_name.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", grant.user_name)
                };
                println!("✓ Approved {}{} on {platform}.", grant.user_id, name);
                println!("  The pairing store joins the allowlist — the gateway authorizes them on the next message.");
            }
            None => {
                if store.is_locked_out(&platform) {
                    return Err(format!(
                        "{platform} is locked out after {} failed approvals — try again in an hour",
                        ulnclaw::pairing::MAX_FAILED_ATTEMPTS
                    ));
                }
                return Err(format!(
                        "no pending pairing request matched {code:?} on {platform} (codes expire after 1 hour)"
                    ));
            }
        },
        PairingAction::Revoke { platform, user_id } => {
            if store.revoke(&platform, &user_id) {
                println!("✓ Revoked {user_id} on {platform}.");
            } else {
                return Err(format!("{user_id} is not paired on {platform}"));
            }
        }
        PairingAction::ClearPending { platform } => {
            let targets: Vec<String> = match platform {
                Some(platform) => vec![platform],
                None => {
                    let mut all = store.known_platforms();
                    if all.is_empty() {
                        all = vec!["telegram".into(), "discord".into(), "slack".into()];
                    }
                    all
                }
            };
            let mut total = 0usize;
            for platform in &targets {
                total += store.clear_pending(platform);
            }
            println!("✓ Cleared {total} pending pairing code(s).");
        }
    }
    Ok(())
}

/// `ulnclaw profiles` — manage `[profiles.*]` config overrides
/// (hermes profile management, lean port; same storage as the gateway
/// `/api/profiles*` endpoints and the desktop Profiles view).
fn profiles_cmd(action: ProfilesAction) -> Result<(), String> {
    use ulnclaw::profiles_cmd as profiles;
    match action {
        ProfilesAction::List => {
            let rows = profiles::list_profiles()?;
            let path = ulnclaw::config_cmd::config_path();
            if rows.is_empty() {
                println!("No profiles configured ({}).", path.display());
                println!(
                    "Create one: ulnclaw profiles set <name> --provider openai --model gpt-4.1"
                );
                return Ok(());
            }
            println!("Profiles in {}:", path.display());
            for (name, profile) in &rows {
                let model = match &profile.model {
                    Some(model) => format!("{}:{}", model.provider, model.model),
                    None => "(default model)".to_string(),
                };
                println!("  {name:<24} {model}");
                if let Some(toolsets) = &profile.enabled_toolsets {
                    println!("    enabled:  {}", toolsets.join(", "));
                }
                if let Some(toolsets) = &profile.disabled_toolsets {
                    println!("    disabled: {}", toolsets.join(", "));
                }
            }
            Ok(())
        }
        ProfilesAction::Show { name } => {
            let rows = profiles::list_profiles()?;
            let Some((_, profile)) = rows.iter().find(|(row_name, _)| row_name == &name) else {
                return Err(format!("no profile named '{name}'"));
            };
            let spec = profiles::ProfileSpec {
                provider: profile.model.as_ref().map(|model| model.provider.clone()),
                model: profile.model.as_ref().map(|model| model.model.clone()),
                base_url: profile
                    .model
                    .as_ref()
                    .and_then(|model| model.base_url.clone()),
                temperature: profile.model.as_ref().and_then(|model| model.temperature),
                enabled_toolsets: profile.enabled_toolsets.clone(),
                disabled_toolsets: profile.disabled_toolsets.clone(),
            };
            let mut wrapper = toml::Table::new();
            wrapper.insert(
                name.clone(),
                toml::Value::Table(profiles::build_profile_table(&spec)),
            );
            let mut root = toml::Table::new();
            root.insert("profiles".to_string(), toml::Value::Table(wrapper));
            let rendered = toml::to_string_pretty(&root).map_err(|e| e.to_string())?;
            println!("{rendered}");
            Ok(())
        }
        ProfilesAction::Set {
            name,
            provider,
            model,
            base_url,
            temperature,
            enable,
            disable,
        } => {
            let parse = |raw: Option<String>| -> Option<Vec<String>> {
                raw.map(|raw| {
                    raw.split(',')
                        .map(|entry| entry.trim().to_string())
                        .filter(|entry| !entry.is_empty())
                        .collect()
                })
            };
            let spec = profiles::ProfileSpec {
                provider,
                model,
                base_url,
                temperature,
                enabled_toolsets: parse(enable),
                disabled_toolsets: parse(disable),
            };
            let created = profiles::save_profile(&name, &spec)?;
            let verb = if created { "Created" } else { "Updated" };
            println!(
                "✓ {verb} profile '{name}' in {}.",
                ulnclaw::config_cmd::config_path().display()
            );
            println!("  Restart the gateway (or start a new CLI process) to apply.");
            Ok(())
        }
        ProfilesAction::Rename { name, new_name } => {
            profiles::rename_profile(&name, &new_name)?;
            println!("✓ Renamed profile '{name}' → '{new_name}'.");
            Ok(())
        }
        ProfilesAction::Delete { name } => {
            profiles::delete_profile(&name)?;
            println!("✓ Deleted profile '{name}'.");
            Ok(())
        }
    }
}

async fn hooks_cmd(config: &UlncLawConfig, action: HooksAction) -> Result<(), String> {
    use ulnclaw::plugins;
    let home = ulnclaw::config::ulnclaw_home();
    // Populate the runtime so test/doctor see the same callbacks chat does.
    let _ = plugins::init(&home, config).await;
    match action {
        HooksAction::List => {
            let allowlist = plugins::allowlist_entries(&home);
            if config.hooks.events.is_empty() {
                println!("No shell hooks configured in [hooks].");
            } else {
                println!("Config shell hooks ([hooks]):");
                let mut events: Vec<(&String, &Vec<String>)> = config.hooks.events.iter().collect();
                events.sort_by(|a, b| a.0.cmp(b.0));
                for (event, commands) in events {
                    let known = ulnclaw::plugins::VALID_HOOKS.contains(&event.as_str());
                    for command in commands {
                        let key = format!("{event}\t{command}");
                        let consented = allowlist.iter().any(|a| a == &key);
                        let state = if !known {
                            "unknown-event"
                        } else if consented {
                            "consented"
                        } else {
                            "pending"
                        };
                        println!("  [{state:>13}] {event}: {command}");
                    }
                }
            }
            let loaded = plugins::loaded_plugins();
            let plugin_hooks: Vec<String> = loaded
                .iter()
                .filter(|p| !p.disabled)
                .flat_map(|p| {
                    p.manifest.hooks.iter().map(move |h| {
                        format!(
                            "  [    consented] {}: {} (plugin {})",
                            h, p.manifest.name, p.manifest.name
                        )
                    })
                })
                .collect();
            if !plugin_hooks.is_empty() {
                println!("Plugin hooks (trusted by installation):");
                for line in plugin_hooks {
                    println!("{line}");
                }
            }
            println!(
                "\nConsent allowlist: {} ({} entries)",
                home.join("shell-hooks-allowlist.json").display(),
                allowlist.len()
            );
        }
        HooksAction::Test {
            event,
            payload_file,
        } => {
            if !plugins::VALID_HOOKS.contains(&event.as_str()) {
                return Err(format!(
                    "unknown hook event {event:?} — expected one of the {} hermes hook names",
                    plugins::VALID_HOOKS.len()
                ));
            }
            let payload = match payload_file {
                Some(path) => {
                    let text = std::fs::read_to_string(&path)
                        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                    serde_json::from_str(&text)
                        .map_err(|e| format!("payload file is not valid JSON: {e}"))?
                }
                None => plugins::default_hook_payload(&event),
            };
            println!("Firing {event:?} for registered callbacks…");
            let responses = plugins::invoke_hook(&event, payload).await;
            if responses.is_empty() {
                println!("No responses (no consented hooks for this event, or none produced valid JSON).");
            } else {
                for (idx, response) in responses.iter().enumerate() {
                    println!(
                        "response {}: {}",
                        idx + 1,
                        serde_json::to_string_pretty(response).unwrap_or_default()
                    );
                }
            }
        }
        HooksAction::Revoke { command } => {
            let removed = plugins::revoke_allowlist(&home, &command);
            if removed == 0 {
                println!("No consent entries matched {command:?}.");
            } else {
                println!(
                    "✓ Revoked {removed} consent entr{} for {command:?}.",
                    if removed == 1 { "y" } else { "ies" }
                );
            }
        }
        HooksAction::Doctor => {
            let probes = plugins::doctor_hooks(&home, config).await;
            if probes.is_empty() {
                println!("No shell hooks configured in [hooks] — nothing to check.");
                return Ok(());
            }
            let mut failed = 0usize;
            for probe in &probes {
                let mark = if probe.ok { "ok " } else { "ERR" };
                println!(
                    "[{mark}] {}: {} — {}",
                    probe.event, probe.command, probe.detail
                );
                if !probe.ok {
                    failed += 1;
                }
            }
            if failed > 0 {
                return Err(format!("{failed} hook(s) failed the doctor run"));
            }
        }
    }
    Ok(())
}

async fn computer_use_cmd(config: &UlncLawConfig, action: ComputerUseAction) -> Result<(), String> {
    use ulnclaw::computer_use as cu;
    let cfg = &config.computer_use;
    match action {
        ComputerUseAction::Status => {
            match cu::resolve_cua_driver_cmd() {
                Some(driver) => {
                    println!("cua-driver: {driver}");
                    if let Some(version) = cu::driver_version(&driver) {
                        println!("version:    {version}");
                    }
                }
                None => {
                    println!("cua-driver: NOT INSTALLED");
                    println!("{}", cu::cua_driver_install_hint());
                }
            }
            println!(
                "config:     telemetry={} max_image_dimension={} capture_after_mode={} no_overlay={}",
                if cfg.cua_telemetry { "on" } else { "off (default)" },
                cfg.max_image_dimension,
                cfg.capture_after_mode,
                match cfg.no_overlay {
                    Some(true) => "force-off",
                    Some(false) => "force-on",
                    None => "auto",
                }
            );
        }
        ComputerUseAction::Doctor { json } => {
            let payload = cu::health_report(cfg).await.map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            let overall = payload
                .get("overall")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            println!("overall: {overall}");
            if let Some(checks) = payload.get("checks").and_then(|v| v.as_array()) {
                for check in checks {
                    let name = check.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let status = check.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                    let detail = check
                        .get("detail")
                        .or_else(|| check.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if detail.is_empty() {
                        println!("  [{status:>7}] {name}");
                    } else {
                        println!("  [{status:>7}] {name}: {detail}");
                    }
                }
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
                );
            }
            if overall != "ok" {
                return Err("computer-use doctor reported a degraded state".to_string());
            }
        }
        ComputerUseAction::Install { upgrade, yes } => {
            if cfg!(windows) {
                return Err("automated install is POSIX-only; run the PowerShell installer: \
                    irm https://raw.githubusercontent.com/trycua/cua/main/libs/cua-driver/scripts/install.ps1 | iex"
                    .to_string());
            }
            if !upgrade && cu::resolve_cua_driver_cmd().is_some() {
                println!("cua-driver already installed — use --upgrade to re-run the installer.");
                return Ok(());
            }
            let script = "/bin/bash -c \"$(curl -fsSL \
                https://raw.githubusercontent.com/trycua/cua/main/libs/cua-driver/scripts/install.sh)\"";
            println!("About to run the upstream cua-driver installer:");
            println!("  {script}");
            if !yes {
                print!("Continue? [y/N] ");
                use std::io::Write;
                std::io::stdout().flush().ok();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| e.to_string())?;
                if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
                    println!("aborted.");
                    return Ok(());
                }
            }
            let status = std::process::Command::new("/bin/bash")
                .arg("-c")
                .arg("curl -fsSL https://raw.githubusercontent.com/trycua/cua/main/libs/cua-driver/scripts/install.sh | /bin/bash")
                .status()
                .map_err(|e| e.to_string())?;
            if !status.success() {
                return Err(format!("installer exited with {status}"));
            }
            println!("install finished.");
        }
    }
    Ok(())
}

fn uninstall_cmd(full: bool, dry_run: bool, yes: bool) -> Result<(), String> {
    let plan = ulnclaw::uninstall::build_plan(full);
    if dry_run {
        ulnclaw::uninstall::print_dry_run(&plan);
        return Ok(());
    }
    // Non-interactive fast path (hermes --yes): no prompts. Named-profile
    // cleanup stays interactive-only in hermes; ulnclaw has no profiles
    // service to worry about.
    if yes {
        ulnclaw::uninstall::perform_uninstall(&plan);
        return Ok(());
    }
    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│              ⚕ ulnclaw Uninstaller                      │");
    println!("└─────────────────────────────────────────────────────────┘");
    println!();
    println!("Current Installation:");
    match &plan.project_root {
        Some(root) => println!("  Code:    {}", root.display()),
        None => println!("  Code:    (no checkout detected next to the binary)"),
    }
    println!("  Config:  {}", plan.home.join("config.toml").display());
    println!("  Data:    {}", plan.home.display());
    println!();
    println!("Uninstall Options:");
    println!();
    println!("  1) Keep data - Remove code only, keep configs/sessions/logs");
    println!("     (Recommended - you can reinstall later with your settings intact)");
    println!();
    println!("  2) Full uninstall - Remove everything including all data");
    println!("     (Warning: This deletes all configs, sessions, and logs permanently)");
    println!();
    println!("  3) Cancel - Don't uninstall");
    println!();
    print!("Select option [1/2/3]: ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut choice_line = String::new();
    std::io::stdin()
        .read_line(&mut choice_line)
        .map_err(|e| e.to_string())?;
    let choice = choice_line.trim().to_string();
    if choice == "3"
        || matches!(
            choice.to_ascii_lowercase().as_str(),
            "" | "c" | "cancel" | "q" | "quit" | "n" | "no"
        )
    {
        println!();
        println!("Uninstall cancelled.");
        return Ok(());
    }
    if choice != "1" && choice != "2" {
        return Err(format!("invalid choice '{choice}' (expected 1, 2 or 3)"));
    }
    let full_uninstall = choice == "2";
    let plan = ulnclaw::uninstall::build_plan(full_uninstall);
    println!();
    if full_uninstall {
        println!("⚠️  WARNING: This will permanently delete ALL ulnclaw data!");
        println!("   Including: configs, API keys, sessions, scheduled jobs, logs");
    } else {
        println!("This will remove the ulnclaw code but keep your configuration and data.");
    }
    println!();
    print!("Type 'yes' to confirm: ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut confirm_line = String::new();
    std::io::stdin()
        .read_line(&mut confirm_line)
        .map_err(|e| e.to_string())?;
    if confirm_line.trim().to_ascii_lowercase() != "yes" {
        println!();
        println!("Uninstall cancelled.");
        return Ok(());
    }
    ulnclaw::uninstall::perform_uninstall(&plan);
    Ok(())
}

fn secrets_cmd(config: &UlncLawConfig, action: SecretsAction) -> Result<(), String> {
    use ulnclaw::secrets as sec;
    let home = ulnclaw::config::ulnclaw_home();
    match action {
        SecretsAction::Status => {
            let cfg = &config.secrets;
            let order = sec::ordered_sources(cfg);
            if order.is_empty() {
                println!("No secret sources enabled (all disabled or unknown).");
            } else {
                println!("Source order (first claim wins): {}", order.join(" -> "));
            }
            if cfg.command.enabled {
                let cmd = if cfg.command.command.is_empty() {
                    "(no command set)"
                } else {
                    &cfg.command.command
                };
                println!(
                    "  command:   enabled  timeout={}s  {}",
                    cfg.command.timeout_seconds, cmd
                );
            } else {
                println!("  command:   disabled");
            }
            if cfg.bitwarden.enabled {
                let bws = sec::find_bws(&home)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "bws NOT FOUND".to_string());
                let token_present = std::env::var(&cfg.bitwarden.access_token_env).is_ok();
                println!(
                    "  bitwarden: enabled  bws={}  token({})={}  project={}  override_existing={}",
                    bws,
                    cfg.bitwarden.access_token_env,
                    if token_present { "present" } else { "MISSING" },
                    if cfg.bitwarden.project_id.is_empty() {
                        "(unset)"
                    } else {
                        &cfg.bitwarden.project_id
                    },
                    cfg.bitwarden.override_existing
                );
            } else {
                println!("  bitwarden: disabled");
            }
            if cfg.onepassword.enabled {
                let op = sec::find_op(&cfg.onepassword.binary_path)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "op NOT FOUND".to_string());
                let token_present =
                    std::env::var(&cfg.onepassword.service_account_token_env).is_ok();
                println!(
                    "  onepassword: enabled  op={}  token({})={}  bindings={}  override_existing={}",
                    op,
                    cfg.onepassword.service_account_token_env,
                    if token_present { "present" } else { "absent (interactive op auth may still work)" },
                    cfg.onepassword.env.len(),
                    cfg.onepassword.override_existing
                );
            } else {
                println!("  onepassword: disabled");
            }
            if !cfg.preserve_existing.is_empty() {
                println!("  preserve_existing: {}", cfg.preserve_existing.join(", "));
            }
        }
        SecretsAction::Sync { apply } => {
            let fetches = sec::fetch_all(&config.secrets, &home);
            if fetches.is_empty() {
                println!("No secret sources enabled — nothing to sync.");
                return Ok(());
            }
            for (name, result) in &fetches {
                if result.ok {
                    println!("fetched {name}: {} secret(s)", result.secrets.len());
                } else if let Some(err) = &result.error {
                    println!("fetched {name}: FAILED — {err}");
                }
                for warn in &result.warnings {
                    println!("  warning: {warn}");
                }
            }
            // Base view: process env plus .env (same merge apply_all uses).
            let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
            for (k, v) in ulnclaw::config::load_env_file(&home.join(".env")) {
                env.entry(k).or_insert(v);
            }
            let report = sec::apply_to_env(&mut env, &config.secrets, &fetches);
            for (var, source) in &report.applied {
                if apply {
                    // SAFETY: single-threaded CLI path; mirrors hermes
                    // exporting winners at startup.
                    unsafe {
                        std::env::set_var(var, env.get(var).map(|s| s.as_str()).unwrap_or(""))
                    };
                    println!("exported {var} (from {source})");
                } else {
                    println!("would export {var} (from {source})");
                }
            }
            for var in &report.skipped_existing {
                println!("kept existing {var}");
            }
            for var in &report.skipped_protected {
                println!("skipped protected {var}");
            }
            for conflict in &report.conflicts {
                println!("conflict: {conflict}");
            }
            for err in &report.errors {
                println!("error: {err}");
            }
            if apply {
                println!(
                    "Applied {} secret(s). Note: exports only affect this                      process; ulnclaw applies secrets automatically at startup.",
                    report.applied.len()
                );
            } else if !report.applied.is_empty() {
                println!("Dry run — re-run with --apply to export into this process.");
            }
        }
        SecretsAction::Bitwarden { action } => {
            let cmd = match action {
                BitwardenSecretsAction::Setup {
                    access_token,
                    server_url,
                    project_id,
                } => ulnclaw::secrets_cmd::BitwardenCmd::Setup {
                    access_token,
                    server_url,
                    project_id,
                },
                BitwardenSecretsAction::Install { force } => {
                    ulnclaw::secrets_cmd::BitwardenCmd::Install { force }
                }
                BitwardenSecretsAction::Status => ulnclaw::secrets_cmd::BitwardenCmd::Status,
                BitwardenSecretsAction::Token {
                    access_token,
                    no_verify,
                } => ulnclaw::secrets_cmd::BitwardenCmd::Token {
                    access_token,
                    no_verify,
                },
                BitwardenSecretsAction::Disable => ulnclaw::secrets_cmd::BitwardenCmd::Disable,
            };
            ulnclaw::secrets_cmd::bitwarden_cmd(cmd)?;
        }
        SecretsAction::OnePassword { action } => {
            let cmd = match action {
                OnePasswordSecretsAction::Setup {
                    binary_path,
                    account,
                    token,
                } => ulnclaw::secrets_cmd::OnePasswordCmd::Setup {
                    binary_path,
                    account,
                    token,
                },
                OnePasswordSecretsAction::Status => ulnclaw::secrets_cmd::OnePasswordCmd::Status,
                OnePasswordSecretsAction::Set { name, reference } => {
                    ulnclaw::secrets_cmd::OnePasswordCmd::Set { name, reference }
                }
                OnePasswordSecretsAction::Remove { name } => {
                    ulnclaw::secrets_cmd::OnePasswordCmd::Remove { name }
                }
                OnePasswordSecretsAction::Disable => ulnclaw::secrets_cmd::OnePasswordCmd::Disable,
            };
            ulnclaw::secrets_cmd::onepassword_cmd(cmd)?;
        }
    }
    Ok(())
}

/// Hermes `_fmt_task_line`.
fn kanban_task_line(task: &ulnclaw::kanban::Task) -> String {
    let assignee = task
        .assignee
        .clone()
        .unwrap_or_else(|| "(unassigned)".into());
    let tenant = task
        .tenant
        .as_ref()
        .map(|t| format!(" [{t}]"))
        .unwrap_or_default();
    format!(
        "{} {}  {:8}  {:20}{}  {}",
        ulnclaw::kanban::status_icon(&task.status),
        task.id,
        task.status,
        assignee,
        tenant,
        task.title
    )
}

fn kanban_epoch_label(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = now - ts;
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[derive(Subcommand)]
enum WeixinAction {
    /// Log into WeChat by scanning an iLink QR code (persists credentials
    /// under <home>/weixin/accounts/ for [messaging.weixin])
    Login {
        /// QR login timeout in seconds
        #[arg(long, default_value = "480")]
        timeout: u64,
        /// iLink bot type
        #[arg(long, default_value = "3")]
        bot_type: String,
    },
}

async fn weixin_cmd(action: WeixinAction) -> Result<(), String> {
    match action {
        WeixinAction::Login { timeout, bot_type } => {
            let home = ulnclaw::config::ulnclaw_home();
            match ulnclaw::weixin::qr_login(&home, &bot_type, timeout).await {
                Ok(Some(creds)) => {
                    println!(
                        "Saved credentials for account {}. Enable the adapter with:",
                        creds.get("account_id").map(|s| s.as_str()).unwrap_or("?")
                    );
                    println!("  [messaging.weixin]");
                    println!("  enabled = true");
                    println!(
                        "  account_id = \"{}\"",
                        creds.get("account_id").map(|s| s.as_str()).unwrap_or("")
                    );
                    Ok(())
                }
                Ok(None) => Err("weixin login did not complete".into()),
                Err(e) => Err(e),
            }
        }
    }
}

#[derive(Subcommand)]
enum QqAction {
    /// Set up QQ Bot by scanning a QR code (persists credentials under
    /// <home>/qq/ for [messaging.qq])
    Login {
        /// QR registration timeout in seconds (hermes default 600)
        #[arg(long, default_value = "600")]
        timeout: u64,
    },
    /// Full interactive QQ Bot setup wizard (hermes `_setup_qqbot`):
    /// scan-to-configure or manual credentials, DM security policy
    /// (pairing / open / allowlist) and home-channel selection,
    /// persisted to [messaging.qq] + <home>/.env
    Setup {
        /// QR registration timeout in seconds (hermes default 600)
        #[arg(long, default_value = "600")]
        timeout: u64,
    },
}

async fn qq_cmd(action: QqAction) -> Result<(), String> {
    match action {
        QqAction::Setup { timeout } => qq_setup_wizard(timeout).await,
        QqAction::Login { timeout } => {
            let home = ulnclaw::config::ulnclaw_home();
            match ulnclaw::qqbot::qr_register(timeout).await? {
                Some(creds) => {
                    ulnclaw::qqbot::save_onboard_credentials(&home, &creds)?;
                    println!();
                    println!(
                        "Saved credentials to {}.",
                        home.join("qq").join("credentials.json").display()
                    );
                    println!("Enable the adapter with:");
                    println!("  [messaging.qq]");
                    println!("  enabled = true");
                    if !creds.user_openid.is_empty() {
                        println!();
                        println!("Optional — allow your own DMs:");
                        println!("  allow_from = [\"{}\"]", creds.user_openid);
                    }
                    Ok(())
                }
                None => Err("qq login did not complete".into()),
            }
        }
    }
}
// ---------------------------------------------------------------------------
// QQ Bot setup wizard (hermes `_setup_qqbot` in hermes_cli/gateway.py)
// ---------------------------------------------------------------------------

/// Read one line from stdin with a prompt and optional default.
fn qq_prompt_line(prompt: &str, default: &str) -> Result<String, String> {
    if default.is_empty() {
        print!("  {}: ", prompt);
    } else {
        print!("  {} [{}]: ", prompt, default);
    }
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?
        == 0
    {
        return Ok(default.to_string());
    }
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}

/// Hidden input for secrets (crossterm raw mode; falls back to plain
/// echo when the terminal cannot go raw, e.g. pipes).
fn qq_prompt_hidden(prompt: &str) -> Result<String, String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    print!("  {}: ", prompt);
    use std::io::Write;
    std::io::stdout().flush().ok();
    if enable_raw_mode().is_err() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        println!();
        return Ok(line.trim().to_string());
    }
    let mut value = String::new();
    loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => break,
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(c) => value.push(c),
                KeyCode::Esc => {
                    disable_raw_mode().ok();
                    println!();
                    return Ok(String::new());
                }
                _ => {}
            },
            Err(e) => {
                disable_raw_mode().ok();
                return Err(e.to_string());
            }
            _ => {}
        }
    }
    disable_raw_mode().ok();
    println!();
    Ok(value)
}

/// y/N prompt returning the choice (default when EOF/blank).
fn qq_prompt_yes_no(prompt: &str, default: bool) -> Result<bool, String> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let answer = qq_prompt_line(&format!("{} {}", prompt, suffix), "")?;
    Ok(match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    })
}

/// Numbered-choice prompt; returns the selected index (default on
/// blank/EOF, re-asks on invalid input).
fn qq_prompt_choice(prompt: &str, choices: &[&str], default_index: usize) -> Result<usize, String> {
    println!();
    println!("  {}", prompt);
    for (idx, choice) in choices.iter().enumerate() {
        println!("    {}. {}", idx + 1, choice);
    }
    loop {
        let answer = qq_prompt_line(&format!("Choice [{}]", default_index + 1), "")?;
        if answer.trim().is_empty() {
            return Ok(default_index);
        }
        if let Ok(n) = answer.trim().parse::<usize>() {
            if n >= 1 && n <= choices.len() {
                return Ok(n - 1);
            }
        }
        println!("  Please enter a number between 1 and {}.", choices.len());
    }
}

async fn qq_setup_wizard(timeout: u64) -> Result<(), String> {
    let home = ulnclaw::config::ulnclaw_home();
    println!();
    println!("  ─── 🐧 QQ Bot Setup ───");

    // Already configured? (env credentials or a saved onboard file)
    let env_app_id = std::env::var("QQ_APP_ID")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let env_secret = std::env::var("QQ_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let has_saved = ulnclaw::qqbot::load_onboard_credentials(&home).is_some();
    if (env_app_id.is_some() && env_secret.is_some()) || has_saved {
        println!();
        println!("  ✓ QQ Bot is already configured.");
        if !qq_prompt_yes_no("  Reconfigure QQ Bot?", false)? {
            return Ok(());
        }
    }

    // ── Choose setup method ──
    let method = qq_prompt_choice(
        "How would you like to set up QQ Bot?",
        &[
            "Scan QR code to add bot automatically (recommended)",
            "Enter existing App ID and App Secret manually",
        ],
        0,
    )?;

    let mut credentials: Option<ulnclaw::qqbot::QQOnboardCredentials> = None;
    if method == 0 {
        match ulnclaw::qqbot::qr_register(timeout).await {
            Ok(creds) => credentials = creds,
            Err(e) => {
                println!("  ⚠ QR setup failed: {e}. Continuing with manual input.");
            }
        }
        if credentials.is_none() {
            println!("  ℹ QR setup did not complete. Continuing with manual input.");
        }
    }

    // ── Manual credential input ──
    if credentials.is_none() {
        println!();
        println!("  ℹ Go to https://q.qq.com to register a QQ Bot application.");
        println!("  ℹ Note your App ID and App Secret from the application page.");
        println!();
        let app_id = qq_prompt_line("App ID", "")?;
        if app_id.is_empty() {
            println!("  ⚠ Skipped — QQ Bot won't work without an App ID.");
            return Ok(());
        }
        let client_secret = qq_prompt_hidden("App Secret")?;
        if client_secret.is_empty() {
            println!("  ⚠ Skipped — QQ Bot won't work without an App Secret.");
            return Ok(());
        }
        credentials = Some(ulnclaw::qqbot::QQOnboardCredentials {
            app_id,
            client_secret,
            user_openid: String::new(),
        });
    }
    let credentials = credentials.expect("credentials resolved above");

    // ── Save core credentials ──
    ulnclaw::qqbot::save_onboard_credentials(&home, &credentials)?;

    // ── DM security policy ──
    let access = qq_prompt_choice(
        "How should direct messages be authorized?",
        &[
            "Use DM pairing approval (recommended)",
            "Allow all direct messages",
            "Only allow listed user OpenIDs",
        ],
        0,
    )?;
    let (dm_policy, allow_from) = match access {
        0 => {
            let mut allow_from: Vec<String> = Vec::new();
            if !credentials.user_openid.is_empty() {
                println!();
                if qq_prompt_yes_no(
                    &format!(
                        "  Add yourself ({}) to the allow list?",
                        credentials.user_openid
                    ),
                    true,
                )? {
                    allow_from.push(credentials.user_openid.clone());
                    println!("  ✓ Allow list set to {}", credentials.user_openid);
                }
            }
            println!("  ✓ DM pairing enabled.");
            println!(
                "  ℹ Unknown users can request access; approve with `ulnclaw pairing approve`."
            );
            ("pairing", allow_from)
        }
        1 => {
            println!("  ⚠ Open DM access enabled for QQ Bot.");
            ("open", Vec::new())
        }
        _ => {
            let default_allow = credentials.user_openid.clone();
            let allowlist =
                qq_prompt_line("Allowed user OpenIDs (comma-separated)", &default_allow)?;
            let allow_from: Vec<String> = allowlist
                .split(',')
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect();
            println!("  ✓ Allowlist saved.");
            ("allowlist", allow_from)
        }
    };

    // ── Persist policy into [messaging.qq] ──
    ulnclaw::qqbot::apply_setup_to_config(&home, dm_policy, &allow_from)?;

    // ── Home channel ──
    if !credentials.user_openid.is_empty() {
        println!();
        if qq_prompt_yes_no(
            &format!(
                "  Use your QQ user ID ({}) as the home channel?",
                credentials.user_openid
            ),
            true,
        )? {
            ulnclaw::qqbot::upsert_env_value(
                &home,
                "QQBOT_HOME_CHANNEL",
                &credentials.user_openid,
            )?;
            println!("  ✓ Home channel set to {}", credentials.user_openid);
        }
    } else {
        println!();
        let home_channel =
            qq_prompt_line("Home channel OpenID (for cron/notifications, or empty)", "")?;
        if !home_channel.is_empty() {
            ulnclaw::qqbot::upsert_env_value(&home, "QQBOT_HOME_CHANNEL", &home_channel)?;
            println!("  ✓ Home channel set to {}", home_channel);
        }
    }

    println!();
    println!("  ✓ 🐧 QQ Bot configured!");
    println!("  ℹ App ID: {}", credentials.app_id);
    println!(
        "  ℹ Policy persisted to {}",
        home.join("config.toml").display()
    );
    Ok(())
}

/// Spotify PKCE OAuth CLI (hermes `hermes auth spotify` —
/// `login_spotify_command`).
#[derive(Subcommand)]
enum SpotifyAuthAction {
    /// Authorize via browser + loopback PKCE callback
    Login {
        /// Spotify app client id (fallback ULNCLAW_SPOTIFY_CLIENT_ID /
        /// SPOTIFY_CLIENT_ID / stored state)
        #[arg(long)]
        client_id: Option<String>,
        /// Loopback redirect URI (default http://127.0.0.1:43827/spotify/callback)
        #[arg(long)]
        redirect_uri: Option<String>,
        /// Override the requested OAuth scope
        #[arg(long)]
        scope: Option<String>,
        /// Only print the authorization URL
        #[arg(long)]
        no_browser: bool,
        /// Seconds to wait for the loopback callback (default 180)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Show stored Spotify auth state
    Status,
    /// Remove stored Spotify credentials
    Logout,
}

async fn spotify_auth_cmd(action: SpotifyAuthAction) -> Result<(), String> {
    match action {
        SpotifyAuthAction::Login {
            client_id,
            redirect_uri,
            scope,
            no_browser,
            timeout,
        } => ulnclaw::spotify_auth::login(
            client_id.as_deref(),
            redirect_uri.as_deref(),
            scope.as_deref(),
            !no_browser,
            timeout,
        )
        .await
        .map(|message| println!("{message}"))
        .map_err(|e| e.to_string()),
        SpotifyAuthAction::Status => {
            let status = ulnclaw::spotify_auth::auth_status();
            if status["logged_in"] == serde_json::json!(true) {
                println!("Spotify: logged in");
                if let Some(scope) = status.get("scope").and_then(serde_json::Value::as_str) {
                    println!("  scope:      {scope}");
                }
                if let Some(expires) = status.get("expires_at").and_then(serde_json::Value::as_str)
                {
                    println!("  expires_at: {expires}");
                }
                println!(
                    "  refresh:    {}",
                    if status["has_refresh_token"] == serde_json::json!(true) {
                        "stored"
                    } else {
                        "missing"
                    }
                );
            } else {
                println!("Spotify: not logged in (run `ulnclaw spotify-auth login`)");
            }
            Ok(())
        }
        SpotifyAuthAction::Logout => {
            ulnclaw::spotify_auth::logout().map_err(|e| e.to_string())?;
            println!("Spotify credentials removed.");
            Ok(())
        }
    }
}

/// Google Chat per-user OAuth CLI (hermes
/// `python -m plugins.platforms.google_chat.oauth`). The Chat API's
/// media.upload endpoint rejects service accounts, so each user grants
/// the `chat.messages.create` scope once; tokens persist under
/// `<home>/google_chat_user_tokens/`.
#[derive(Subcommand)]
enum GoogleChatOauthAction {
    /// Store the OAuth client-secret JSON (one-time host step; from
    /// Google Cloud Console → OAuth client ID → Desktop app)
    ClientSecret {
        /// Path to the downloaded client_secret*.json
        path: PathBuf,
    },
    /// Generate the authorization URL and persist the pending PKCE
    /// session (hermes `get_auth_url`)
    AuthUrl {
        /// User email slot (default: shared legacy slot)
        #[arg(long)]
        email: Option<String>,
    },
    /// Exchange the pasted auth code or failed-redirect URL for tokens
    /// (hermes `exchange_auth_code`)
    AuthCode {
        /// The `code` value or the full http://localhost:1/?... URL
        code_or_url: String,
        /// User email slot (default: shared legacy slot)
        #[arg(long)]
        email: Option<String>,
    },
    /// Revoke and delete a stored user token (hermes `revoke`)
    Revoke {
        /// User email slot (default: shared legacy slot)
        #[arg(long)]
        email: Option<String>,
    },
    /// Show stored client-secret and per-user token status
    Check,
}

async fn google_chat_oauth_cmd(action: GoogleChatOauthAction) -> Result<(), String> {
    let home = ulnclaw::config::ulnclaw_home();
    match action {
        GoogleChatOauthAction::ClientSecret { path } => {
            let dest = ulnclaw::google_chat_oauth::store_client_secret(&home, &path)?;
            println!("Stored client secret at {}", dest.display());
            println!(
                "Next: `ulnclaw google-chat-oauth auth-url` or send `/setup-files start` in chat."
            );
            Ok(())
        }
        GoogleChatOauthAction::AuthUrl { email } => {
            let url = ulnclaw::google_chat_oauth::start_auth(&home, email.as_deref())?;
            println!("{url}");
            println!();
            println!("After authorizing, the browser fails at http://localhost:1 — paste the");
            println!("full failed URL back here:");
            println!("  ulnclaw google-chat-oauth auth-code '<URL>'");
            Ok(())
        }
        GoogleChatOauthAction::AuthCode { code_or_url, email } => {
            let client = reqwest::Client::new();
            let scope = ulnclaw::google_chat_oauth::exchange_code(
                &home,
                &client,
                &code_or_url,
                email.as_deref(),
            )
            .await?;
            println!("✅ Authorized (scope: {scope}).");
            Ok(())
        }
        GoogleChatOauthAction::Revoke { email } => {
            let client = reqwest::Client::new();
            let output = ulnclaw::google_chat_oauth::revoke(&home, &client, email.as_deref()).await;
            println!("{output}");
            Ok(())
        }
        GoogleChatOauthAction::Check => {
            let secret = ulnclaw::google_chat_oauth::client_secret_path(&home);
            if secret.exists() {
                println!("client secret: {}", secret.display());
            } else {
                println!("client secret: not stored (run `ulnclaw google-chat-oauth client-secret <path>`)");
            }
            let legacy = ulnclaw::google_chat_oauth::token_path(&home, None);
            if legacy.exists() {
                println!("legacy token:  {}", legacy.display());
            }
            let emails = ulnclaw::google_chat_oauth::list_authorized_emails(&home);
            if emails.is_empty() {
                println!("user tokens:   none");
            } else {
                for email in emails {
                    println!("user token:    {email}");
                }
            }
            Ok(())
        }
    }
}

async fn pets_cmd(action: PetsAction) -> Result<(), String> {
    // Network downloads (reqwest blocking) and the animation loop must run
    // off the async main context.
    let code = tokio::task::spawn_blocking(move || {
        let home = ulnclaw::config::ulnclaw_home();
        match action {
            PetsAction::List {
                query,
                installed,
                limit,
            } => ulnclaw::pets::cmd_list(&home, &query.join(" "), installed, limit),
            PetsAction::Install {
                slug,
                force,
                select,
            } => ulnclaw::pets::cmd_install(&home, &slug, force, select),
            PetsAction::Select { slug } => {
                ulnclaw::pets::cmd_select(&home, slug.as_deref().unwrap_or(""))
            }
            PetsAction::Show {
                slug,
                state,
                cycle,
                once,
                mode,
                scale,
            } => ulnclaw::pets::cmd_show(
                &home,
                &ulnclaw::pets::ShowOptions {
                    slug: slug.unwrap_or_default(),
                    state: state.unwrap_or_default(),
                    cycle,
                    once,
                    mode,
                    scale,
                },
            ),
            PetsAction::Off => ulnclaw::pets::cmd_off(),
            PetsAction::Scale { factor } => ulnclaw::pets::cmd_scale(&factor),
            PetsAction::Remove { slug } => ulnclaw::pets::cmd_remove(&home, &slug),
            PetsAction::Doctor => ulnclaw::pets::cmd_doctor(&home),
            PetsAction::Hatch {
                description,
                style,
                name,
                base,
                drafts,
            } => ulnclaw::pets_generate::cmd_hatch(
                &home,
                &description.join(" "),
                style.as_deref(),
                name.as_deref(),
                base.as_deref(),
                drafts,
            ),
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code)
    }
}

/// Resolve the task set for `kanban specify`/`decompose` (single id or
/// the whole triage column with --all).
fn triage_targets(
    store: &ulnclaw::kanban::KanbanStore,
    id: Option<&str>,
    all: bool,
    verb: &str,
) -> Result<Vec<String>, String> {
    if all {
        return ulnclaw::kanban_triage::list_triage_ids(store).map_err(|e| e.to_string());
    }
    match id {
        Some(raw) => {
            let resolved = store
                .resolve_task_id(raw)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("task '{raw}' not found"))?;
            Ok(vec![resolved])
        }
        None => Err(format!("usage: ulnclaw kanban {verb} <id> | --all")),
    }
}

/// Print a project the way hermes `project show` does.
fn print_project(proj: &ulnclaw::projects_db::Project) {
    let flags = if proj.archived { " (archived)" } else { "" };
    println!("{}  [{}]{}", proj.slug, proj.id, flags);
    println!("  name:    {}", proj.name);
    if let Some(d) = &proj.description {
        println!("  about:   {d}");
    }
    if let Some(b) = &proj.board_slug {
        println!("  board:   {b}");
    }
    if let Some(p) = &proj.primary_path {
        println!("  primary: {p}");
    }
    if !proj.folders.is_empty() {
        println!("  folders:");
        for f in &proj.folders {
            let mark = if f.is_primary { " *" } else { "  " };
            let label = f
                .label
                .as_deref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default();
            println!("   {mark} {}{label}", f.path);
        }
    }
}

/// Best-effort: point the bound board's default_workdir at the
/// project's primary repo (hermes `_sync_board_default_workdir`).
/// Failures are non-fatal — the binding itself already succeeded.
fn sync_board_default_workdir(proj: &ulnclaw::projects_db::Project, board_slug: &str) {
    let Some(primary) = proj
        .primary_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        return;
    };
    let slug = board_slug.trim().to_lowercase();
    if slug.is_empty() {
        return;
    }
    let Ok(store) = ulnclaw::kanban::KanbanStore::open_default() else {
        return;
    };
    let exists = store
        .list_boards()
        .map(|boards| boards.iter().any(|b| b.slug == slug))
        .unwrap_or(false);
    if !exists {
        return;
    }
    let _ = store.set_board_workdir(&slug, Some(primary));
}

fn projects_cmd(action: ProjectsAction) -> Result<(), String> {
    use ulnclaw::projects_db as pdb;
    let conn = pdb::connect(None).map_err(|e| e.to_string())?;
    let resolve = |id_or_slug: &str| -> Result<pdb::Project, String> {
        pdb::get_project(&conn, id_or_slug)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("project: no such project: {id_or_slug}"))
    };
    match action {
        ProjectsAction::Create {
            name,
            folders,
            slug,
            primary,
            description,
            icon,
            color,
            board,
            r#use,
        } => {
            let folder_refs: Vec<&str> = folders.iter().map(String::as_str).collect();
            let args = pdb::CreateProject {
                name: &name,
                slug: slug.as_deref(),
                folders: &folder_refs,
                primary_path: primary.as_deref(),
                description: description.as_deref(),
                icon: icon.as_deref(),
                color: color.as_deref(),
                board_slug: board.as_deref(),
            };
            let pid = pdb::create_project(&conn, &args).map_err(|e| format!("project: {e}"))?;
            if r#use {
                pdb::set_active(&conn, Some(&pid)).map_err(|e| e.to_string())?;
            }
            let proj = pdb::get_project(&conn, &pid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "project: vanished after create".to_string())?;
            println!("Created project {} ({})", proj.slug, pid);
            print_project(&proj);
        }
        ProjectsAction::List { all } => {
            let active = pdb::get_active_id(&conn).map_err(|e| e.to_string())?;
            let projects = pdb::list_projects(&conn, all).map_err(|e| e.to_string())?;
            if projects.is_empty() {
                println!("No projects yet. Create one with `ulnclaw project create <name>`.");
                return Ok(());
            }
            for p in &projects {
                let marker = if Some(p.id.as_str()) == active.as_deref() {
                    "*"
                } else {
                    " "
                };
                let flags = if p.archived { " (archived)" } else { "" };
                println!(
                    "{marker} {:<24} {}{}  [{} folder(s)]",
                    p.slug,
                    p.name,
                    flags,
                    p.folders.len()
                );
            }
        }
        ProjectsAction::Show { project } => {
            let proj = resolve(&project)?;
            print_project(&proj);
        }
        ProjectsAction::AddFolder {
            project,
            path,
            label,
            primary,
        } => {
            let proj = resolve(&project)?;
            let norm = pdb::add_folder(&conn, &proj.id, &path, label.as_deref(), primary)
                .map_err(|e| format!("project: {e}"))?;
            println!("Added {norm} to {}", proj.slug);
        }
        ProjectsAction::RemoveFolder { project, path } => {
            let proj = resolve(&project)?;
            if !pdb::remove_folder(&conn, &proj.id, &path).map_err(|e| e.to_string())? {
                return Err(format!("project: folder not in project: {path}"));
            }
            println!("Removed {path} from {}", proj.slug);
        }
        ProjectsAction::Rename { project, name } => {
            let proj = resolve(&project)?;
            pdb::update_project(
                &conn,
                &proj.id,
                &pdb::UpdateProject {
                    name: Some(&name),
                    description: None,
                    icon: None,
                    color: None,
                    board_slug: None,
                },
            )
            .map_err(|e| format!("project: {e}"))?;
            println!("Renamed {} -> {name}", proj.slug);
        }
        ProjectsAction::SetPrimary { project, path } => {
            let proj = resolve(&project)?;
            if !pdb::set_primary(&conn, &proj.id, &path).map_err(|e| e.to_string())? {
                return Err(format!(
                    "project: '{path}' is not a folder of {}; add it first with                      `ulnclaw project add-folder`",
                    proj.slug
                ));
            }
            println!("Set primary of {} -> {path}", proj.slug);
        }
        ProjectsAction::Use { project } => match project {
            None => {
                pdb::set_active(&conn, None).map_err(|e| e.to_string())?;
                println!("Cleared active project");
            }
            Some(id_or_slug) => {
                let proj = resolve(&id_or_slug)?;
                pdb::set_active(&conn, Some(&proj.id)).map_err(|e| e.to_string())?;
                println!("Active project: {}", proj.slug);
            }
        },
        ProjectsAction::Archive { project } => {
            let proj = resolve(&project)?;
            pdb::archive_project(&conn, &proj.id).map_err(|e| e.to_string())?;
            println!("Archived {}", proj.slug);
        }
        ProjectsAction::Restore { project } => {
            let proj = resolve(&project)?;
            pdb::restore_project(&conn, &proj.id).map_err(|e| e.to_string())?;
            println!("Restored {}", proj.slug);
        }
        ProjectsAction::BindBoard { project, board } => {
            let proj = resolve(&project)?;
            let board_arg = board.clone().unwrap_or_default();
            pdb::update_project(
                &conn,
                &proj.id,
                &pdb::UpdateProject {
                    name: None,
                    description: None,
                    icon: None,
                    color: None,
                    board_slug: Some(&board_arg),
                },
            )
            .map_err(|e| format!("project: {e}"))?;
            if board_arg.trim().is_empty() {
                println!("Unbound board from {}", proj.slug);
            } else {
                println!("Bound {} -> board {}", proj.slug, board_arg.trim());
                sync_board_default_workdir(&proj, board_arg.trim());
            }
        }
        ProjectsAction::Scan { roots, max_depth } => {
            let roots: Vec<std::path::PathBuf> = if roots.is_empty() {
                let home = dirs::home_dir().ok_or_else(|| {
                    "project scan: cannot determine the home directory; pass --root PATH"
                        .to_string()
                })?;
                vec![home]
            } else {
                roots.iter().map(std::path::PathBuf::from).collect()
            };
            println!(
                "Scanning {} root(s), max depth {} ...",
                roots.len(),
                max_depth
            );
            let repos = ulnclaw::projects_scan::scan_for_repos(&roots, max_depth);
            let pairs: Vec<(String, Option<String>)> = repos
                .iter()
                .map(|r| (r.root.clone(), Some(r.label.clone())))
                .collect();
            let recorded = pdb::record_discovered_repos(
                &conn,
                &pairs,
                true,
                Some(ulnclaw::projects_scan::CLI_SCAN_POLICY_KEY),
            )
            .map_err(|e| e.to_string())?;
            if recorded == 0 {
                println!("No git repos found.");
            } else {
                println!("Recorded {recorded} repo(s):");
                for repo in &repos {
                    println!("  {:<24} {}", repo.label, repo.root);
                }
            }
        }
        ProjectsAction::Repos { clear } => {
            if clear {
                pdb::clear_discovered_repos(&conn, None).map_err(|e| e.to_string())?;
                println!("Cleared the discovered-repos cache.");
                return Ok(());
            }
            let rows = pdb::list_discovered_repos(&conn).map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("No discovered repos cached. Run `ulnclaw project scan`.");
                return Ok(());
            }
            for row in &rows {
                let root = row["root"].as_str().unwrap_or_default();
                let label = row["label"].as_str().unwrap_or_default();
                let last_seen = row["last_seen"].as_i64().unwrap_or(0);
                println!(
                    "{label:<24} {root}  (seen {})",
                    ulnclaw::status::relative_time(last_seen as f64)
                );
            }
        }
    }
    Ok(())
}

async fn kanban_cmd(action: KanbanAction) -> Result<(), String> {
    use ulnclaw::kanban::{KanbanStore, NewTask, DEFAULT_CLAIM_TTL_SECS};
    let store = KanbanStore::open_default().map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let resolve = |id: &str| -> Result<String, String> {
        store
            .resolve_task_id(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("task '{id}' not found"))
    };
    match action {
        KanbanAction::Init => {
            println!("kanban store: {}", store.path().display());
            println!(
                "current board: {}",
                store.current_board().map_err(|e| e.to_string())?
            );
        }
        KanbanAction::Boards { action } => match action {
            KanbanBoardsAction::List => {
                let counts = store.board_task_counts().map_err(|e| e.to_string())?;
                let current = store.current_board().map_err(|e| e.to_string())?;
                for (slug, total, active) in counts {
                    let marker = if slug == current { "*" } else { " " };
                    println!("{marker} {slug:16}  {active} active / {total} tasks");
                }
            }
            KanbanBoardsAction::Create {
                slug,
                name,
                workdir,
            } => {
                store
                    .create_board(&slug, name.as_deref(), workdir.as_deref())
                    .map_err(|e| e.to_string())?;
                println!("created board '{slug}'");
            }
            KanbanBoardsAction::Rm { slug } => {
                store.remove_board(&slug).map_err(|e| e.to_string())?;
                println!("removed board '{slug}'");
            }
            KanbanBoardsAction::Switch { slug } => {
                store.switch_board(&slug).map_err(|e| e.to_string())?;
                println!("switched to board '{slug}'");
            }
            KanbanBoardsAction::Rename { slug, name } => {
                let name = name.join(" ");
                store
                    .rename_board(&slug, &name)
                    .map_err(|e| e.to_string())?;
                println!("board '{slug}' renamed to '{name}'");
            }
            KanbanBoardsAction::SetWorkdir { slug, workdir } => {
                store
                    .set_board_workdir(&slug, workdir.as_deref())
                    .map_err(|e| e.to_string())?;
                match workdir.as_deref().filter(|w| !w.trim().is_empty()) {
                    Some(dir) => println!("board '{slug}' workdir: {dir}"),
                    None => println!("board '{slug}' workdir cleared"),
                }
            }
            KanbanBoardsAction::Show => {
                let current = store.current_board().map_err(|e| e.to_string())?;
                println!("current board: {current}");
                for (slug, total, active) in store.board_task_counts().map_err(|e| e.to_string())? {
                    println!("  {slug:16}  {active} active / {total} tasks");
                }
            }
        },
        KanbanAction::Create {
            title,
            body,
            assignee,
            priority,
            tenant,
            model,
            provider,
            project,
            skills,
            max_runtime,
            idempotency_key,
            reasoning,
            triage,
            max_retries,
            workspace,
            branch,
            initial_status,
            goal,
            goal_max_turns,
            json,
        } => {
            let title = title.join(" ");
            if title.trim().is_empty() {
                return Err("usage: ulnclaw kanban create <title>".into());
            }
            let max_runtime_seconds = match max_runtime.as_deref() {
                Some(raw) => match ulnclaw::kanban::parse_duration(raw) {
                    Ok(secs) => secs,
                    Err(e) => return Err(format!("kanban: --max-runtime: {e}")),
                },
                None => None,
            };
            if let Some(max_retries) = max_retries {
                if max_retries < 1 {
                    return Err(format!(
                        "kanban: --max-retries must be >= 1 (got {max_retries}); use 1 to trip on the first failure"
                    ));
                }
            }
            if provider.is_some() && model.is_none() {
                return Err("kanban: --provider requires --model".into());
            }
            if let Some(turns) = goal_max_turns {
                if turns < 1 {
                    return Err(format!(
                        "kanban: --goal-max-turns must be >= 1 (got {turns})"
                    ));
                }
            }
            let (mut workspace_kind, workspace_path) = match workspace.as_deref() {
                Some(raw) => ulnclaw::kanban::parse_workspace_flag(raw)
                    .map_err(|e| format!("kanban: {e}"))?,
                None => ("scratch".to_string(), None),
            };
            let branch_name = match branch.as_deref() {
                Some(raw) => Some(
                    ulnclaw::kanban::parse_branch_flag(raw).map_err(|e| format!("kanban: {e}"))?,
                ),
                None => None,
            };
            if branch_name.is_some() && workspace_kind != "worktree" {
                return Err("kanban: --branch is only valid with --workspace worktree".into());
            }
            if workspace.is_none()
                && ulnclaw::config::UlncLawConfig::load(None)
                    .map(|c| c.kanban.worktrees)
                    .unwrap_or(false)
            {
                workspace_kind = "worktree".to_string();
            }
            let task = store
                .create_task(&NewTask {
                    title,
                    body: body.unwrap_or_default(),
                    assignee,
                    priority,
                    tenant,
                    model,
                    provider,
                    reasoning_effort: reasoning,
                    project_id: project,
                    created_by: KanbanStore::claimer_id(),
                    skills: if skills.is_empty() {
                        None
                    } else {
                        Some(skills)
                    },
                    max_runtime_seconds,
                    idempotency_key,
                    triage,
                    max_retries,
                    initial_status,
                    goal_mode: goal,
                    goal_max_turns,
                    workflow_template_id: None,
                    current_step_key: None,
                    workspace_kind: Some(workspace_kind),
                    workspace_path,
                    branch_name,
                    session_id: None,
                })
                .map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&task).map_err(|e| e.to_string())?
                );
            } else {
                println!("{} (todo, board '{}')", task.id, task.board);
                println!("  {}", task.title);
            }
        }
        KanbanAction::List {
            status,
            assignee,
            board,
            workflow_template_id,
            limit,
            json,
        } => {
            let tasks = store
                .list_tasks(
                    board.as_deref(),
                    status.as_deref(),
                    assignee.as_deref(),
                    workflow_template_id.as_deref(),
                    limit,
                )
                .map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&tasks).map_err(|e| e.to_string())?
                );
            } else if tasks.is_empty() {
                println!(
                    "no tasks on board '{}'.",
                    store.current_board().map_err(|e| e.to_string())?
                );
            } else {
                for task in &tasks {
                    println!("{}", kanban_task_line(task));
                }
            }
        }
        KanbanAction::Show { id, json } => {
            let resolved = resolve(&id)?;
            let task = store
                .get_task(&resolved)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("task '{id}' not found"))?;
            let comments = store.comments(&resolved).map_err(|e| e.to_string())?;
            let events = store.events(&resolved).map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task": task,
                        "comments": comments,
                        "events": events,
                    }))
                    .map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            println!("{}", kanban_task_line(&task));
            if !task.body.is_empty() {
                println!("\n{}", task.body);
            }
            println!(
                "\ncreated {} by {}",
                kanban_epoch_label(task.created_at),
                task.created_by
            );
            if let Some(skills) = task.skills.as_deref().filter(|s| !s.is_empty()) {
                println!("  skills:    {}", skills.join(", "));
            }
            if let Some(limit) = task.max_runtime_seconds {
                println!("  max runtime: {limit}s per attempt");
            }
            if let Some(result) = &task.result {
                println!("result: {result}");
            }
            if !comments.is_empty() {
                println!("\ncomments:");
                for comment in &comments {
                    println!(
                        "  {} — {}: {}",
                        kanban_epoch_label(comment.created_at),
                        comment.author,
                        comment.body
                    );
                }
            }
            if !events.is_empty() {
                println!("\nevents:");
                for event in &events {
                    let detail = if event
                        .payload
                        .as_object()
                        .map(|o| o.is_empty())
                        .unwrap_or(true)
                    {
                        String::new()
                    } else {
                        format!(" {}", event.payload)
                    };
                    println!(
                        "  {} {}{detail}",
                        kanban_epoch_label(event.created_at),
                        event.kind
                    );
                }
            }
        }
        KanbanAction::Ready { id } => {
            let resolved = resolve(&id)?;
            let task = store.ready_task(&resolved).map_err(|e| e.to_string())?;
            println!("{}", kanban_task_line(&task));
        }
        KanbanAction::Assign { id, assignee } => {
            let resolved = resolve(&id)?;
            let task = store
                .assign_task(&resolved, &assignee)
                .map_err(|e| e.to_string())?;
            println!("{}", kanban_task_line(&task));
        }
        KanbanAction::Claim { id, ttl, claimer } => {
            let resolved = resolve(&id)?;
            let claimer = claimer.unwrap_or_else(KanbanStore::claimer_id);
            let task = store
                .claim_task(&resolved, &claimer, ttl.unwrap_or(DEFAULT_CLAIM_TTL_SECS))
                .map_err(|e| e.to_string())?;
            // Resolve the workspace on claim (hermes _cmd_claim) so the
            // claimer sees where the work should happen.
            {
                let home = ulnclaw::config::ulnclaw_home();
                match store.resolve_workspace(&home, &task) {
                    Ok((workspace, branch)) => {
                        let _ = store.set_workspace_path(&task.id, &workspace);
                        if let Some(branch) = branch {
                            let _ = store.set_branch_name(&task.id, &branch);
                        }
                        println!("Workspace: {}", workspace.display());
                    }
                    Err(e) => eprintln!("kanban: workspace: {e}"),
                }
            }
            ulnclaw::plugins::fire_session_event(
                "kanban_task_claimed",
                &task.id,
                &cwd,
                serde_json::json!({
                    "task_id": task.id,
                    "board": task.board,
                    "assignee": task.assignee,
                }),
            )
            .await;
            println!("{}", kanban_task_line(&task));
        }
        KanbanAction::Heartbeat { id } => {
            let resolved = resolve(&id)?;
            let task = store
                .heartbeat_task(
                    &resolved,
                    &KanbanStore::claimer_id(),
                    DEFAULT_CLAIM_TTL_SECS,
                )
                .map_err(|e| e.to_string())?;
            println!("heartbeat recorded for {}", task.id);
        }
        KanbanAction::Done {
            id,
            result,
            summary,
            metadata,
            artifact,
            created_card,
        } => {
            if id.is_empty() {
                return Err("usage: ulnclaw kanban done <task-id> [more ids]".into());
            }
            let metadata_value = match metadata.as_deref() {
                Some(raw) => {
                    let value: serde_json::Value = serde_json::from_str(raw)
                        .map_err(|e| format!("kanban: --metadata must be a JSON object: {e}"))?;
                    if !value.is_object() {
                        return Err("kanban: --metadata must be a JSON object".into());
                    }
                    Some(value)
                }
                None => None,
            };
            let home = ulnclaw::config::ulnclaw_home();
            let mut failed: Vec<String> = Vec::new();
            for raw in &id {
                let resolved = match resolve(raw) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                        continue;
                    }
                };
                // Goal-mode judge gate (hermes #38367): a goal-mode card
                // completed from the CLI must carry evidence the auxiliary
                // judge accepts; fail-open when no judge can run.
                if let Ok(Some(task)) = store.get_task(&resolved) {
                    if task.goal_mode {
                        let mut goal_parts = vec![task.title.clone()];
                        if !task.body.trim().is_empty() {
                            goal_parts.push(task.body.clone());
                        }
                        let goal_text = goal_parts.join("\n\n");
                        let text = summary
                            .as_deref()
                            .or(result.as_deref())
                            .unwrap_or("")
                            .to_string();
                        let config = ulnclaw::config::UlncLawConfig::load(None).unwrap_or_default();
                        let gate = match build_provider(&config, None) {
                            Ok(provider) => {
                                ulnclaw::goals::goal_completion_gate(
                                    &config,
                                    Some(provider),
                                    &goal_text,
                                    &text,
                                )
                                .await
                            }
                            Err(_) => Ok(()),
                        };
                        if let Err(reason) = gate {
                            eprintln!(
                                "kanban: goal completion of {resolved} rejected by judge: {reason}. \
                                 Provide evidence matching the task's acceptance criteria."
                            );
                            failed.push(raw.clone());
                            continue;
                        }
                    }
                }
                match store.complete_task_with_artifacts(
                    &home,
                    &resolved,
                    result.as_deref(),
                    summary.as_deref(),
                    metadata_value.as_ref(),
                    &artifact,
                    &created_card,
                    ulnclaw::kanban::worker_run_id_for(&resolved),
                ) {
                    Ok(task) => {
                        ulnclaw::plugins::fire_session_event(
                            "kanban_task_completed",
                            &task.id,
                            &cwd,
                            serde_json::json!({
                                "task_id": task.id,
                                "board": task.board,
                                "assignee": task.assignee,
                                "result": task.result,
                            }),
                        )
                        .await;
                        println!("{}", kanban_task_line(&task));
                    }
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                    }
                }
            }
            if !failed.is_empty() {
                return Err(format!("kanban: could not complete: {}", failed.join(", ")));
            }
        }
        KanbanAction::Review { id, reason } => {
            if id.is_empty() {
                return Err(
                    "usage: ulnclaw kanban review <task-id> [more ids] [--reason TEXT]".into(),
                );
            }
            let reason = reason.unwrap_or_else(|| "ready for review".into());
            let mut failed: Vec<String> = Vec::new();
            for raw in &id {
                let resolved = match resolve(raw) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                        continue;
                    }
                };
                match store.request_review(&resolved, &reason) {
                    Ok(task) => println!("{}", kanban_task_line(&task)),
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                    }
                }
            }
            if !failed.is_empty() {
                return Err(format!(
                    "kanban: could not request review: {}",
                    failed.join(", ")
                ));
            }
        }
        KanbanAction::Block {
            id,
            reason,
            kind,
            extra_ids,
        } => {
            let reason = reason.join(" ");
            let mut ids = vec![id];
            ids.extend(extra_ids);
            let mut failed: Vec<String> = Vec::new();
            for raw in &ids {
                let resolved = match resolve(raw) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                        continue;
                    }
                };
                if !reason.trim().is_empty() {
                    store
                        .add_comment(
                            &resolved,
                            &KanbanStore::claimer_id(),
                            &format!("BLOCKED: {reason}"),
                        )
                        .ok();
                }
                match store.block_task_guarded(
                    &resolved,
                    &reason,
                    kind.as_deref(),
                    ulnclaw::kanban::worker_run_id_for(&resolved),
                ) {
                    Ok(task) => {
                        ulnclaw::plugins::fire_session_event(
                            "kanban_task_blocked",
                            &task.id,
                            &cwd,
                            serde_json::json!({
                                "task_id": task.id,
                                "board": task.board,
                                "assignee": task.assignee,
                                "reason": reason,
                            }),
                        )
                        .await;
                        println!("{}", kanban_task_line(&task));
                    }
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                    }
                }
            }
            if !failed.is_empty() {
                return Err(format!("kanban: could not block: {}", failed.join(", ")));
            }
        }
        KanbanAction::Unblock { id, reason } => {
            if id.is_empty() {
                return Err("usage: ulnclaw kanban unblock <task-id> [more ids]".into());
            }
            let mut failed: Vec<String> = Vec::new();
            for raw in &id {
                let resolved = match resolve(raw) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                        continue;
                    }
                };
                if let Some(note) = reason.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                    store.add_comment(&resolved, "unblocker", note).ok();
                }
                match store.unblock_task(&resolved) {
                    Ok(task) => println!("{}", kanban_task_line(&task)),
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                    }
                }
            }
            if !failed.is_empty() {
                return Err(format!("kanban: could not unblock: {}", failed.join(", ")));
            }
        }
        KanbanAction::Archive { id, purge } => {
            if !id.is_empty() && !purge.is_empty() {
                return Err(
                    "kanban: choose either task ids to archive or --rm archived task ids".into(),
                );
            }
            if id.is_empty() && purge.is_empty() {
                return Err("kanban: at least one task id is required".into());
            }
            let mut failed: Vec<String> = Vec::new();
            if !purge.is_empty() {
                for raw in &purge {
                    let resolved = match resolve(raw) {
                        Ok(resolved) => resolved,
                        Err(e) => {
                            eprintln!("kanban: {raw}: {e}");
                            failed.push(raw.clone());
                            continue;
                        }
                    };
                    match store.delete_archived_task(&resolved) {
                        Ok(true) => println!("deleted {resolved}"),
                        Ok(false) => {
                            eprintln!(
                                "kanban: cannot delete {resolved} (must already be archived)"
                            );
                            failed.push(raw.clone());
                        }
                        Err(e) => {
                            eprintln!("kanban: {raw}: {e}");
                            failed.push(raw.clone());
                        }
                    }
                }
            } else {
                for raw in &id {
                    let resolved = match resolve(raw) {
                        Ok(resolved) => resolved,
                        Err(e) => {
                            eprintln!("kanban: {raw}: {e}");
                            failed.push(raw.clone());
                            continue;
                        }
                    };
                    match store.archive_task(&resolved) {
                        Ok(task) => println!("archived {}", task.id),
                        Err(e) => {
                            eprintln!("kanban: {raw}: {e}");
                            failed.push(raw.clone());
                        }
                    }
                }
            }
            if !failed.is_empty() {
                return Err(format!("kanban: could not archive: {}", failed.join(", ")));
            }
        }
        KanbanAction::Comment { id, text } => {
            let resolved = resolve(&id)?;
            let text = text.join(" ");
            if text.trim().is_empty() {
                return Err("usage: ulnclaw kanban comment <id> <text>".into());
            }
            store
                .add_comment(&resolved, &KanbanStore::claimer_id(), &text)
                .map_err(|e| e.to_string())?;
            println!("comment added to {resolved}");
        }
        KanbanAction::Link { parent, child } => {
            let parent = resolve(&parent)?;
            let child = resolve(&child)?;
            store
                .link_tasks(&parent, &child)
                .map_err(|e| e.to_string())?;
            println!("linked {parent} → {child} (child waits for parent)");
        }
        KanbanAction::Unlink { parent, child } => {
            let parent = resolve(&parent)?;
            let child = resolve(&child)?;
            store
                .unlink_tasks(&parent, &child)
                .map_err(|e| e.to_string())?;
            println!("unlinked {parent} → {child}");
        }
        KanbanAction::Swarm {
            goal,
            workers,
            verifier,
            synthesizer,
            idempotency_key,
            json,
        } => {
            let goal = goal.join(" ");
            let mut specs: Vec<ulnclaw::kanban::SwarmWorkerSpec> = Vec::new();
            for raw in &workers {
                let mut parts = raw.splitn(3, ':');
                let assignee = parts.next().unwrap_or("").trim().to_string();
                let title = parts.next().unwrap_or("").trim().to_string();
                let skills: Vec<String> = parts
                    .next()
                    .map(|list| {
                        list.split(',')
                            .map(|skill| skill.trim().to_string())
                            .filter(|skill| !skill.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                if assignee.is_empty() || title.is_empty() {
                    return Err(format!(
                        "kanban swarm: bad --worker '{raw}' (expected ASSIGNEE:TITLE[:skill,skill])"
                    ));
                }
                specs.push(ulnclaw::kanban::SwarmWorkerSpec {
                    assignee,
                    title: title.clone(),
                    body: title.clone(),
                    priority: 0,
                    skills,
                });
            }
            let created = store
                .create_swarm(
                    &goal,
                    &specs,
                    &verifier,
                    &synthesizer,
                    "",
                    idempotency_key.as_deref(),
                )
                .map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&created).unwrap_or_default()
                );
            } else {
                println!("swarm root: {} (blackboard)", created.root_id);
                for id in &created.worker_ids {
                    println!("  ▶ worker {id} (ready)");
                }
                println!("  ◇ verifier {} (waits for workers)", created.verifier_id);
                println!(
                    "  ◆ synthesizer {} (waits for verifier)",
                    created.synthesizer_id
                );
            }
        }
        KanbanAction::Specify { id, all } => {
            let ids = triage_targets(&store, id.as_deref(), all, "specify")?;
            let config = ulnclaw::config::UlncLawConfig::load(None).unwrap_or_default();
            let provider = build_provider(&config, None)?;
            for task_id in ids {
                let outcome = ulnclaw::kanban_triage::specify_task(
                    &store,
                    &config,
                    provider.clone(),
                    &task_id,
                    None,
                )
                .await;
                if outcome.ok {
                    let title_note = outcome
                        .new_title
                        .map(|t| format!(" — {t}"))
                        .unwrap_or_default();
                    println!("✓ {task_id} specified{title_note}");
                } else {
                    println!("✗ {task_id} — {}", outcome.reason);
                }
            }
        }
        KanbanAction::Decompose { id, all } => {
            let ids = triage_targets(&store, id.as_deref(), all, "decompose")?;
            let config = ulnclaw::config::UlncLawConfig::load(None).unwrap_or_default();
            let provider = build_provider(&config, None)?;
            for task_id in ids {
                let outcome = ulnclaw::kanban_triage::decompose_task(
                    &store,
                    &config,
                    provider.clone(),
                    &task_id,
                    None,
                )
                .await;
                if outcome.ok {
                    match &outcome.child_ids {
                        Some(children) => {
                            println!("✓ {task_id} decomposed into {} children:", children.len());
                            for child in children {
                                println!("  ▶ {child}");
                            }
                        }
                        None => {
                            let title_note = outcome
                                .new_title
                                .map(|t| format!(" — {t}"))
                                .unwrap_or_default();
                            println!("✓ {task_id} kept single ({}){title_note}", outcome.reason);
                        }
                    }
                } else {
                    println!("✗ {task_id} — {}", outcome.reason);
                }
            }
        }
        KanbanAction::Diagnostics {
            id,
            min_severity,
            json,
        } => {
            let config = ulnclaw::config::UlncLawConfig::load(None).unwrap_or_default();
            let tasks: Vec<ulnclaw::kanban::Task> = match id.as_deref() {
                Some(raw) => {
                    let resolved = resolve(raw)?;
                    vec![store
                        .get_task(&resolved)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("task '{raw}' not found"))?]
                }
                None => store
                    .list_tasks(None, None, None, None, 500)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|t| t.status != "done" && t.status != "archived")
                    .collect(),
            };
            let mut total = 0usize;
            for task in &tasks {
                let diagnostics =
                    ulnclaw::kanban_diagnostics::compute_task_diagnostics(&store, &config, task);
                let filtered: Vec<&ulnclaw::kanban_diagnostics::Diagnostic> = diagnostics
                    .iter()
                    .filter(|d| {
                        ulnclaw::kanban_diagnostics::severity_at_or_above(
                            &d.severity,
                            min_severity.as_deref(),
                        )
                    })
                    .collect();
                if filtered.is_empty() {
                    continue;
                }
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "task_id": task.id,
                            "diagnostics": filtered,
                        })
                    );
                } else {
                    println!(
                        "{} {} ({})",
                        ulnclaw::kanban::status_icon(&task.status),
                        task.id,
                        task.title
                    );
                    for diagnostic in &filtered {
                        println!(
                            "  [{}] {}: {}",
                            diagnostic.severity, diagnostic.kind, diagnostic.title
                        );
                        for action in &diagnostic.actions {
                            println!("      → {} — {}", action.label, action.hint);
                        }
                    }
                }
                total += filtered.len();
            }
            if total == 0 {
                println!("no diagnostics — the board looks healthy");
            }
        }
        KanbanAction::Schedule {
            id,
            reason,
            extra_ids,
        } => {
            let reason = reason.join(" ");
            let mut ids = vec![id];
            ids.extend(extra_ids);
            let mut failed: Vec<String> = Vec::new();
            for raw in &ids {
                let resolved = match resolve(raw) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                        continue;
                    }
                };
                match store.schedule_task(&resolved, &reason) {
                    Ok(task) => println!("\u{23F1} {} scheduled", task.id),
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                    }
                }
            }
            if !failed.is_empty() {
                return Err(format!("kanban: could not schedule: {}", failed.join(", ")));
            }
        }
        KanbanAction::Promote {
            id,
            reason,
            force,
            extra_ids,
            dry_run,
            json,
        } => {
            let reason = reason.join(" ");
            let mut ids = vec![id];
            ids.extend(extra_ids);
            let mut failed: Vec<String> = Vec::new();
            let mut results: Vec<serde_json::Value> = Vec::new();
            for raw in &ids {
                let resolved = match resolve(raw) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                        results.push(serde_json::json!({ "task_id": raw, "ok": false, "error": e.to_string() }));
                        continue;
                    }
                };
                if dry_run {
                    match store.validate_promote(&resolved, force) {
                        Ok(()) => {
                            results.push(
                                serde_json::json!({ "task_id": resolved, "would_promote": true }),
                            );
                        }
                        Err(e) => {
                            results.push(serde_json::json!({ "task_id": resolved, "would_promote": false, "error": e.to_string() }));
                        }
                    }
                    continue;
                }
                match store.promote_task(&resolved, &reason, force) {
                    Ok(task) => {
                        println!("\u{25B6} {} promoted to ready", task.id);
                        results.push(serde_json::json!({ "task_id": task.id, "ok": true }));
                    }
                    Err(e) => {
                        eprintln!("kanban: {raw}: {e}");
                        failed.push(raw.clone());
                        results.push(serde_json::json!({ "task_id": resolved, "ok": false, "error": e.to_string() }));
                    }
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "dry_run": dry_run,
                        "results": results,
                    }))
                    .map_err(|e| e.to_string())?
                );
            }
            if !failed.is_empty() {
                return Err(format!("kanban: could not promote: {}", failed.join(", ")));
            }
        }
        KanbanAction::Reclaim { id, reason } => {
            let resolved = resolve(&id)?;
            let task = store
                .reclaim_task(&resolved, reason.as_deref().unwrap_or("manual reclaim"))
                .map_err(|e| e.to_string())?;
            println!("\u{25FB} {} reclaimed to ready", task.id);
        }
        KanbanAction::Reassign {
            id,
            profile,
            reclaim,
            reason,
        } => {
            let resolved = resolve(&id)?;
            let task = store
                .reassign_task(
                    &resolved,
                    &profile,
                    reclaim,
                    reason.as_deref().unwrap_or(""),
                )
                .map_err(|e| e.to_string())?;
            match &task.assignee {
                Some(assignee) => println!("{} reassigned to {assignee}", task.id),
                None => println!("{} unassigned", task.id),
            }
        }
        KanbanAction::Edit {
            id,
            title,
            body,
            result,
            summary,
            metadata,
        } => {
            let resolved = resolve(&id)?;
            if let Some(result) = result {
                // Recovery edit on a completed task (hermes kanban edit).
                let metadata_value = match metadata.as_deref() {
                    Some(raw) => {
                        let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
                            format!("kanban: --metadata must be a JSON object: {e}")
                        })?;
                        if !value.is_object() {
                            return Err("kanban: --metadata must be a JSON object".into());
                        }
                        Some(value)
                    }
                    None => None,
                };
                let edited = store
                    .edit_completed_task_result(
                        &resolved,
                        &result,
                        summary.as_deref(),
                        metadata_value.as_ref(),
                    )
                    .map_err(|e| e.to_string())?;
                if !edited {
                    return Err(format!(
                        "kanban: cannot edit {resolved} (unknown id or task is not done)"
                    ));
                }
                println!("edited {resolved}");
            } else {
                let task = store
                    .edit_task(&resolved, title.as_deref(), body.as_deref())
                    .map_err(|e| e.to_string())?;
                println!("{} edited — {}", task.id, task.title);
            }
        }
        KanbanAction::SetModel {
            id,
            model,
            provider,
        } => {
            let resolved = resolve(&id)?;
            let task = store
                .set_model(&resolved, model.as_deref(), provider.as_deref())
                .map_err(|e| e.to_string())?;
            match &task.model {
                Some(model) => println!("{} model pinned to {model}", task.id),
                None => println!("{} model override cleared", task.id),
            }
        }
        KanbanAction::SetReasoning { id, effort } => {
            let resolved = resolve(&id)?;
            let task = store
                .set_reasoning_effort(&resolved, effort.as_deref())
                .map_err(|e| e.to_string())?;
            match &task.reasoning_effort {
                Some(level) => println!("{} reasoning pinned to {level}", task.id),
                None => println!("{} reasoning override cleared", task.id),
            }
        }
        KanbanAction::Attach { id, path } => {
            let resolved = resolve(&id)?;
            if !path.is_file() {
                return Err(format!("{} is not a file", path.display()));
            }
            let absolute = std::fs::canonicalize(&path).unwrap_or(path.clone());
            store
                .attach(&resolved, "file", &absolute.display().to_string())
                .map_err(|e| e.to_string())?;
            println!("attached {} to {}", absolute.display(), resolved);
        }
        KanbanAction::Attachments { id } => {
            let resolved = resolve(&id)?;
            let rows = store
                .attachments_with_ids(&resolved)
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("no attachments on {resolved}");
            } else {
                for (attachment_id, kind, value) in rows {
                    println!("  [{attachment_id}] {kind}: {value}");
                }
            }
        }
        KanbanAction::AttachRm { attachment_id } => {
            if store
                .remove_attachment(attachment_id)
                .map_err(|e| e.to_string())?
            {
                println!("attachment {attachment_id} removed");
            } else {
                return Err(format!("attachment {attachment_id} not found"));
            }
        }
        KanbanAction::Tail { id, follow, limit } => {
            let resolved = resolve(&id)?;
            let print_event = |event: &ulnclaw::kanban::TaskEvent| {
                println!(
                    "{}  {:<14} {}",
                    kanban_epoch_label(event.created_at),
                    event.kind,
                    event.payload
                );
            };
            let events = store.events(&resolved).map_err(|e| e.to_string())?;
            for event in events.iter().rev().take(limit).rev() {
                print_event(event);
            }
            if follow {
                let mut last_id = events.last().map(|e| e.id).unwrap_or(0);
                println!("— following {resolved} (Ctrl+C to stop)");
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let fresh = store.events(&resolved).map_err(|e| e.to_string())?;
                    let new_events: Vec<ulnclaw::kanban::TaskEvent> =
                        fresh.into_iter().filter(|e| e.id > last_id).collect();
                    for event in &new_events {
                        print_event(event);
                        last_id = event.id;
                    }
                }
            }
        }
        KanbanAction::Log { id, tail } => {
            let resolved = resolve(&id)?;
            let home = ulnclaw::config::ulnclaw_home();
            match ulnclaw::kanban::read_worker_log(&home, &resolved, tail) {
                Some(content) => {
                    print!("{content}");
                    if !content.ends_with('\n') {
                        println!();
                    }
                }
                None => {
                    return Err(format!(
                        "(no log for {resolved} — task may not have spawned yet)"
                    ));
                }
            }
        }
        KanbanAction::Runs {
            id,
            json,
            state_type,
            state_name,
        } => {
            let resolved = resolve(&id)?;
            match (&state_type, &state_name) {
                (Some(_), None) | (None, Some(_)) => {
                    return Err(
                        "kanban runs: pass both --state-type and --state-name, or omit both".into(),
                    );
                }
                _ => {}
            }
            let runs = store
                .list_runs(
                    &resolved,
                    true,
                    state_type.as_deref(),
                    state_name.as_deref(),
                )
                .map_err(|e| e.to_string())?;
            if json {
                let rows: Vec<serde_json::Value> = runs
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "profile": r.profile,
                            "status": r.status,
                            "outcome": r.outcome,
                            "started_at": r.started_at,
                            "ended_at": r.ended_at,
                            "summary": r.summary,
                            "error": r.error,
                            "metadata": r.metadata,
                            "worker_pid": r.worker_pid,
                            "step_key": r.step_key,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
                );
            } else if runs.is_empty() {
                println!("(no runs yet for {resolved})");
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                println!(
                    "{:3}  {:12}  {:16}  {:>8}  STARTED",
                    "#", "OUTCOME", "PROFILE", "ELAPSED"
                );
                for (i, r) in runs.iter().enumerate() {
                    let end = r.ended_at.unwrap_or(now);
                    let elapsed = (end - r.started_at).max(0);
                    let el = if elapsed < 60 {
                        format!("{elapsed}s")
                    } else if elapsed < 3600 {
                        format!("{}m", elapsed / 60)
                    } else {
                        format!("{:.1}h", elapsed as f64 / 3600.0)
                    };
                    let outcome = r.outcome.clone().unwrap_or_else(|| {
                        if r.ended_at.is_none() {
                            "(running)".into()
                        } else {
                            r.status.clone()
                        }
                    });
                    println!(
                        "{:3}  {:12}  {:16}  {:>8}  {}",
                        i + 1,
                        outcome,
                        r.profile.as_deref().unwrap_or("-"),
                        el,
                        kanban_epoch_label(r.started_at)
                    );
                    if let Some(summary) = r.summary.as_deref().filter(|s| !s.is_empty()) {
                        let first: String = summary
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(100)
                            .collect();
                        println!("     → {first}");
                    }
                    if let Some(err) = r.error.as_deref().filter(|s| !s.is_empty()) {
                        let first: String = err.chars().take(100).collect();
                        println!("     ✖ {first}");
                    }
                }
            }
        }
        KanbanAction::Context { id } => {
            let resolved = resolve(&id)?;
            let text = store
                .build_worker_context(&resolved)
                .map_err(|e| e.to_string())?;
            print!("{text}");
        }
        KanbanAction::Repair { json } => {
            let db_path = ulnclaw::config::ulnclaw_home().join("kanban.db");
            let report = ulnclaw::kanban::repair_db(&db_path);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
                );
            } else {
                match report.status.as_str() {
                    "missing" => {
                        println!(
                            "No kanban DB at {} — nothing to repair.",
                            report.db_path.display()
                        );
                    }
                    "ok" => {
                        println!(
                            "{}: integrity_check ok — no repair needed.",
                            report.db_path.display()
                        );
                    }
                    "repaired" => {
                        println!("{}: repaired.", report.db_path.display());
                        println!("  reindexed: {}", report.reindexed.join(", "));
                        if let Some(backup) = &report.backup_path {
                            println!("  pre-repair backup: {}", backup.display());
                        }
                        println!("  integrity_check now ok.");
                    }
                    _ => {
                        eprintln!("{}: CORRUPT.", report.db_path.display());
                        for line in report.messages.iter().take(10) {
                            eprintln!("  {line}");
                        }
                        if report.reindexed.is_empty() {
                            eprintln!(
                                "  Not an index-only failure — automatic REINDEX repair does not apply (fail-closed)."
                            );
                        } else {
                            eprintln!(
                                "  REINDEX ({}) attempted but integrity_check is still failing:",
                                report.reindexed.join(", ")
                            );
                            for line in report.post_repair_messages.iter().take(10) {
                                eprintln!("    {line}");
                            }
                        }
                        return Err("kanban repair: database still corrupt".into());
                    }
                }
            }
        }
        KanbanAction::Assignees { json } => {
            let config = ulnclaw::config::UlncLawConfig::load(None).unwrap_or_default();
            let stats = store.board_stats().map_err(|e| e.to_string())?;
            // Roster: configured profiles (+ implicit default) merged with
            // assignees actually seen on the board.
            let mut names: Vec<String> = vec!["default".into()];
            for name in config.profiles.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            for (assignee, _) in &stats.by_assignee {
                if !names.contains(assignee) {
                    names.push(assignee.clone());
                }
            }
            names.sort();
            let on_disk =
                |name: &str| -> bool { name == "default" || config.profiles.contains_key(name) };
            if json {
                let rows: Vec<serde_json::Value> = names
                    .iter()
                    .map(|name| {
                        let counts = stats
                            .by_assignee
                            .iter()
                            .find(|(a, _)| a == name)
                            .map(|(_, counts)| counts.clone())
                            .unwrap_or_default();
                        serde_json::json!({
                            "name": name,
                            "on_disk": on_disk(name),
                            "counts": counts,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?
                );
            } else if names.is_empty() {
                println!("(no assignees)");
            } else {
                println!("{:20}  {:8}  COUNTS", "NAME", "ON DISK");
                for name in &names {
                    let counts = stats
                        .by_assignee
                        .iter()
                        .find(|(a, _)| a == name)
                        .map(|(_, counts)| {
                            let mut parts: Vec<String> = counts
                                .iter()
                                .map(|(status, n)| format!("{status}={n}"))
                                .collect();
                            parts.sort();
                            parts.join(", ")
                        })
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "(idle)".into());
                    println!(
                        "{:20}  {:8}  {counts}",
                        name,
                        if on_disk(name) { "yes" } else { "no" }
                    );
                }
            }
        }
        KanbanAction::Daemon {
            interval,
            pidfile,
            force,
        } => {
            if !force {
                let guidance = [
                    "ulnclaw kanban daemon: DEPRECATED — the dispatcher now runs",
                    "inside the gateway. To use kanban:",
                    "",
                    "    ulnclaw gateway            # starts the gateway + embedded dispatcher",
                    "",
                    "Ready tasks are picked up on the next dispatcher tick",
                    "(default: every 60 seconds). Configure via config.toml:",
                    "",
                    "    [kanban]",
                    "    dispatch_in_gateway = true     # default",
                    "    dispatch_interval_secs = 60",
                    "",
                    "Running both the gateway AND this standalone daemon will",
                    "race for claims. If you truly need the old standalone",
                    "daemon (no gateway available), rerun with --force.",
                ]
                .join("\n");
                return Err(guidance);
            }
            if let Some(pidfile) = pidfile {
                if let Some(parent) = pidfile.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                if std::fs::write(&pidfile, std::process::id().to_string()).is_err() {
                    eprintln!("warning: could not write pidfile {}", pidfile.display());
                }
            }
            eprintln!(
                "Kanban dispatcher running STANDALONE via --force (interval={}s, pid={}).                  Ctrl-C to stop. NOTE: if a gateway is also running with                  dispatch_in_gateway=true (default), you have two dispatchers racing for claims.",
                interval,
                std::process::id()
            );
            let home = ulnclaw::config::ulnclaw_home();
            let boot_config = ulnclaw::config::UlncLawConfig::load(None).unwrap_or_default();
            let stale_timeout = boot_config.kanban.stale_timeout_seconds;
            let known_profiles: std::collections::HashSet<String> =
                boot_config.profiles.keys().cloned().collect();
            loop {
                match store.dispatch_once(
                    &home,
                    true,
                    |task, workspace| ulnclaw::kanban::dispatch_spawn(&home, task, workspace),
                    None,
                    false,
                    2,
                    stale_timeout,
                    Some(&known_profiles),
                    boot_config.kanban.max_in_progress_per_profile,
                    boot_config.kanban.max_in_progress,
                ) {
                    Ok(result) if !result.spawned.is_empty() || !result.reclaimed.is_empty() => {
                        println!(
                            "kanban daemon: {} reclaimed, {} promoted, {} spawned",
                            result.reclaimed.len(),
                            result.promoted.len(),
                            result.spawned.len()
                        );
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("kanban daemon tick failed: {e}"),
                }
                std::thread::sleep(std::time::Duration::from_secs(interval.max(5)));
            }
        }
        KanbanAction::NotifySubscribe {
            id,
            platform,
            chat_id,
            chat_type,
            thread_id,
            user_id,
            notifier_profile,
        } => {
            let resolved = resolve(&id)?;
            store
                .add_notify_sub(
                    &resolved,
                    &ulnclaw::kanban::NewNotifySub {
                        platform: &platform,
                        chat_id: &chat_id,
                        chat_type: chat_type.as_deref(),
                        thread_id: thread_id.as_deref(),
                        user_id: user_id.as_deref(),
                        notifier_profile: notifier_profile.as_deref(),
                        delivery_metadata: None,
                    },
                )
                .map_err(|e| e.to_string())?;
            let thread_suffix = thread_id
                .as_deref()
                .map(|t| format!(":{t}"))
                .unwrap_or_default();
            println!("Subscribed {platform}:{chat_id}{thread_suffix} to {resolved}");
        }
        KanbanAction::NotifyList { id, json } => {
            let resolved = match id {
                Some(id) => Some(resolve(&id)?),
                None => None,
            };
            let subs = store
                .list_notify_subs(resolved.as_deref())
                .map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&subs).map_err(|e| e.to_string())?
                );
            } else if subs.is_empty() {
                println!("(no subscriptions)");
            } else {
                for s in &subs {
                    let thr = if s.thread_id.is_empty() {
                        String::new()
                    } else {
                        format!(":{}", s.thread_id)
                    };
                    let owner = s
                        .notifier_profile
                        .as_deref()
                        .map(|p| format!("  owner={p}"))
                        .unwrap_or_default();
                    println!(
                        "  {:10}  {}:{}{}  (since event {}){owner}",
                        s.task_id, s.platform, s.chat_id, thr, s.last_event_id
                    );
                }
            }
        }
        KanbanAction::NotifyUnsubscribe {
            id,
            platform,
            chat_id,
            thread_id,
        } => {
            let resolved = resolve(&id)?;
            let removed = store
                .remove_notify_sub(&resolved, &platform, &chat_id, thread_id.as_deref())
                .map_err(|e| e.to_string())?;
            if !removed {
                return Err("(no such subscription)".into());
            }
            println!("Unsubscribed from {resolved}");
        }
        KanbanAction::Stats { json } => {
            let stats = store.board_stats().map_err(|e| e.to_string())?;
            let board = store.current_board().map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "board": board,
                        "stats": stats,
                    }))
                    .map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            println!("board '{board}':");
            let order = [
                "triage",
                "todo",
                "ready",
                "running",
                "scheduled",
                "blocked",
                "done",
            ];
            for status in order {
                if let Some((_, count)) = stats.by_status.iter().find(|(s, _)| s == status) {
                    println!(
                        "  {} {:<10} {}",
                        ulnclaw::kanban::status_icon(status),
                        status,
                        count
                    );
                }
            }
            if !stats.by_assignee.is_empty() {
                println!("  by assignee:");
                for (assignee, counts) in &stats.by_assignee {
                    let joined = counts
                        .iter()
                        .map(|(status, count)| format!("{status}:{count}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("    {assignee}: {joined}");
                }
            }
            match stats.oldest_ready_age_seconds {
                Some(age) if age > 0 => {
                    println!("  oldest ready: {}m waiting", age / 60);
                }
                _ => {}
            }
        }
        KanbanAction::Watch {
            assignee,
            tenant,
            kinds,
            interval,
        } => {
            let kinds: Option<Vec<String>> = kinds.map(|raw| {
                raw.split(',')
                    .map(|kind| kind.trim().to_string())
                    .filter(|kind| !kind.is_empty())
                    .collect()
            });
            let mut last_id = store.last_event_id().map_err(|e| e.to_string())?;
            println!("— watching board events (Ctrl+C to stop)");
            let sleep = std::time::Duration::from_secs_f64(interval.max(0.1));
            loop {
                let fresh = store
                    .board_events_since(
                        last_id,
                        assignee.as_deref(),
                        tenant.as_deref(),
                        kinds.as_deref(),
                        200,
                    )
                    .map_err(|e| e.to_string())?;
                for (event, title) in &fresh {
                    println!(
                        "{}  {} {:<14} {} ({})",
                        kanban_epoch_label(event.created_at),
                        event.task_id,
                        event.kind,
                        title,
                        event.payload
                    );
                    last_id = event.id;
                }
                std::thread::sleep(sleep);
            }
        }
        KanbanAction::Gc => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let (removed, skipped) =
                ulnclaw::kanban::gc_worktrees(&cwd, &store).map_err(|e| e.to_string())?;
            println!("worktree gc: {removed} removed, {skipped} kept (active or not a worktree)");
        }
        KanbanAction::Dispatch {
            max_spawn,
            dry_run,
            failure_limit,
            json,
        } => {
            let home = ulnclaw::config::ulnclaw_home();
            let config = ulnclaw::config::UlncLawConfig::load(None).unwrap_or_default();
            let use_worktrees = config.kanban.worktrees;
            let known_profiles: std::collections::HashSet<String> =
                config.profiles.keys().cloned().collect();
            let result = store
                .dispatch_once(
                    &home,
                    use_worktrees,
                    |task, workspace| ulnclaw::kanban::dispatch_spawn(&home, task, workspace),
                    Some(max_spawn.max(1)),
                    dry_run,
                    failure_limit.max(1),
                    config.kanban.stale_timeout_seconds,
                    Some(&known_profiles),
                    config.kanban.max_in_progress_per_profile,
                    config.kanban.max_in_progress,
                )
                .map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            if result.skipped_locked {
                println!("dispatch: skipped — another dispatcher holds the board's tick lock");
                return Ok(());
            }
            if dry_run {
                println!("dry run — would spawn {} task(s)", result.would_spawn.len());
                for id in &result.would_spawn {
                    println!("  ▶ {id}");
                }
            } else {
                println!(
                    "dispatch: {} reclaimed, {} promoted, {} spawned",
                    result.reclaimed.len(),
                    result.promoted.len(),
                    result.spawned.len()
                );
                for (id, pid) in &result.spawned {
                    let pid = pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                    println!("  ● {id} → worker pid {pid}");
                }
            }
            for id in &result.skipped_capped {
                println!("  ⏭ {id} skipped (concurrency cap)");
            }
            for id in &result.skipped_nonspawnable {
                println!(
                    "  ⏭ {id} skipped (assignee is not a configured profile — claim-pulled lane)"
                );
            }
            for id in &result.skipped_unassigned {
                println!("  ⏭ {id} skipped (review task has no assignee)");
            }
            for (id, assignee, current) in &result.skipped_per_profile_capped {
                println!("  ⏭ {id} skipped ({assignee} already running {current} task(s))");
            }
            for id in &result.auto_blocked {
                println!("  ⊘ {id} auto-blocked (spawn failures)");
            }
        }
    }
    Ok(())
}

async fn sessions_cmd(action: SessionAction, config: &UlncLawConfig) -> Result<(), String> {
    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
    if let SessionAction::Repair {
        check_only,
        no_backup,
    } = &action
    {
        // Repair must run BEFORE opening the store: a malformed schema is
        // exactly the case where open fails (hermes sessions repair).
        let db_path = home.join("state.db");
        if !db_path.exists() {
            println!(
                "No session database at {} (nothing to repair).",
                db_path.display()
            );
            return Ok(());
        }
        match ulnclaw::session::repair::db_opens_cleanly(&db_path) {
            None => println!("✓ {} opens cleanly — no repair needed.", db_path.display()),
            Some(reason) => {
                println!("✗ {} does not open cleanly: {}", db_path.display(), reason);
                if *check_only {
                    return Ok(());
                }
                println!("Repairing (a backup copy is made first)…");
                let report =
                    ulnclaw::session::repair::repair_state_db_schema(&db_path, !*no_backup);
                if report.repaired {
                    if let Some(backup) = &report.backup_path {
                        println!("  backup: {}", backup.display());
                    }
                    println!(
                        "  strategy: {}",
                        report.strategy.as_deref().unwrap_or("unknown")
                    );
                    match SqliteSessionStore::open(&db_path) {
                        Ok(store) => {
                            let n = store.count_sessions().map_err(|e| e.to_string())?;
                            println!("✓ Repaired — {} sessions recovered.", n);
                        }
                        Err(_) => println!("✓ Repaired."),
                    }
                } else {
                    println!(
                        "✗ Repair failed: {}",
                        report.error.as_deref().unwrap_or("unknown error")
                    );
                    if let Some(backup) = &report.backup_path {
                        println!("  A backup is preserved at: {}", backup.display());
                    }
                    println!("  Keep state.db and the backup; do not delete them.");
                    println!();
                    println!("  Next step — offline recovery (never modifies the source):");
                    let source_hint = report.backup_path.as_ref().unwrap_or(&db_path);
                    println!("    ulnclaw sessions recover {}", source_hint.display());
                }
            }
        }
        return Ok(());
    }
    let store = SqliteSessionStore::open(home.join("state.db")).map_err(|e| e.to_string())?;
    match action {
        SessionAction::List { limit } => {
            let sessions = store.list_sessions(limit).map_err(|e| e.to_string())?;
            for session in sessions {
                println!(
                    "{}  msgs={:4}  model={:?}",
                    session.id,
                    session.messages.len(),
                    session.metadata.model
                );
            }
        }
        SessionAction::Show {
            id,
            raw,
            json,
            timestamps,
        } => {
            let id = resolve_session_or_err(&store, &id)?;
            if raw || json {
                // P740: raw mode renders from the timestamped load so
                // tool calls, tool ids and times are all visible.
                let messages = store
                    .load_messages_with_timestamps(&id)
                    .map_err(|e| e.to_string())?;
                if messages.is_empty() {
                    return Err(format!("session '{}' not found", id));
                }
                print!(
                    "{}",
                    ulnclaw::session::export::render_session_raw(&messages, json)
                );
                return Ok(());
            }
            let Some(session) = store.load_session(&id).map_err(|e| e.to_string())? else {
                return Err(format!("session '{}' not found", id));
            };
            if timestamps {
                let stamped = store
                    .load_messages_with_timestamps(&id)
                    .map_err(|e| e.to_string())?;
                for (ts, message) in &stamped {
                    println!(
                        "{}",
                        ulnclaw::session::export::render_show_line(*ts, message, true)
                    );
                }
            } else {
                for message in &session.messages {
                    let content = message.content.as_deref().unwrap_or("");
                    println!("── {} ──\n{}", message.role, content);
                }
            }
        }
        SessionAction::Search { query } => {
            let query = query.join(" ");
            let hits = store
                .search_messages(&query, 20)
                .map_err(|e| e.to_string())?;
            for (session_id, snippet) in hits {
                println!("[{}] {}", session_id, snippet);
            }
        }
        SessionAction::Export {
            id,
            out,
            format,
            no_verification,
        } => {
            let id = resolve_session_or_err(&store, &id)?;
            let row = store
                .get_session_row(&id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("session '{}' not found", id))?;
            let messages = store
                .load_messages_with_timestamps(&id)
                .map_err(|e| e.to_string())?;
            let session = ulnclaw::session::export::ExportSession {
                id: row.id.clone(),
                title: row.title.clone(),
                source: row.source.clone(),
                model: row.model.clone(),
                cwd: row.cwd.clone(),
                started_at: row.started_at,
                ended_at: row.ended_at,
                messages,
            };
            if no_verification {
                let body = match format.as_str() {
                    "html" => ulnclaw::session::export::render_session_html(&session, false),
                    _ => ulnclaw::session::export::render_session_markdown(&session, false),
                };
                println!("{}", body);
                return Ok(());
            }
            let dir = out.unwrap_or_else(|| home.join("exports"));
            let path = ulnclaw::session::export::write_session_export(&dir, &session, &format)
                .map_err(|e| e.to_string())?;
            println!(
                "✅ Exported {} messages to {}",
                session.messages.len(),
                path.display()
            );
        }
        SessionAction::Recover { source, out } => {
            let output = out.unwrap_or_else(|| {
                let stem = source
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("state");
                source.with_file_name(format!("{}.recovered.db", stem))
            });
            match ulnclaw::session::recovery::recover_session_database(&source, &output) {
                Ok(report) => {
                    println!("Recovery complete:");
                    println!("  source:      {}", report.source);
                    println!("  output:      {}", report.output);
                    println!("  sessions:    {}", report.sessions);
                    println!("  messages:    {}", report.messages);
                    println!(
                        "  rebuilt session rows for orphaned messages: {}",
                        report.reconstructed_sessions
                    );
                    for (table, stats) in &report.tables {
                        let mode = if stats.salvaged { "salvaged" } else { "copied" };
                        println!(
                            "  table {:<20} {} rows {} ({} skipped)",
                            table, mode, stats.copied, stats.skipped
                        );
                    }
                    println!(
                        "  integrity:   {}",
                        if report.integrity_ok { "ok" } else { "FAILED" }
                    );
                    println!("  fts rebuilt: {}", report.fts_rebuilt);
                    match ulnclaw::session::recovery::write_recovery_report(&report) {
                        Ok(path) => println!("  report:      {}", path.display()),
                        Err(e) => eprintln!("  (report write failed: {})", e),
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        SessionAction::Recap { id } => {
            let id = resolve_session_or_err(&store, &id)?;
            let row = store
                .get_session_row(&id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("session '{}' not found", id))?;
            let messages = store.load_messages(&id).map_err(|e| e.to_string())?;
            println!(
                "{}",
                ulnclaw::session::recap::build_recap(
                    &messages,
                    row.title.as_deref(),
                    Some(&row.id)
                )
            );
        }
        SessionAction::Prune {
            filters,
            include_archived,
            dry_run,
            yes,
        } => {
            let mut filters = filters.build()?;
            // Hermes semantics: a truly bare `sessions prune` (no time
            // window and no filters) means "older than 90 days". ANY
            // filter suppresses the implicit cutoff.
            if filters.is_empty() {
                let cutoff = ulnclaw::session::filters::parse_point_in_time("90", "--older-than")
                    .map_err(|e| e.to_string())?;
                filters.last_active_before = Some(cutoff);
            }
            // Prune skips archived sessions unless --include-archived.
            filters.archived = if include_archived { None } else { Some(false) };
            run_session_prune(&store, filters, true, dry_run, yes)?;
        }
        SessionAction::Archive {
            filters,
            dry_run,
            yes,
        } => {
            let mut filters = filters.build()?;
            if filters.is_empty() {
                return Err(
                    "Refusing to archive every ended session: pass at least one filter                      (e.g. --newer-than 5h, --source cli, --title codex)."
                        .to_string(),
                );
            }
            // Archive only targets not-yet-archived rows (idempotent).
            filters.archived = Some(false);
            run_session_prune(&store, filters, false, dry_run, yes)?;
        }
        SessionAction::Stats => {
            let total = store.count_sessions().map_err(|e| e.to_string())?;
            let messages = store.count_messages().map_err(|e| e.to_string())?;
            println!("Total sessions: {}", total);
            println!("Total messages: {}", messages);
            for (source, count) in store.session_count_by_source().map_err(|e| e.to_string())? {
                println!("  {}: {} sessions", source, count);
            }
            let db_path = home.join("state.db");
            if let Ok(metadata) = std::fs::metadata(&db_path) {
                println!(
                    "Database size: {:.1} MB",
                    metadata.len() as f64 / (1024.0 * 1024.0)
                );
            }
        }
        SessionAction::Browse { source, limit } => {
            // Hermes browse excludes "tool" sessions unless a source filter
            // is given explicitly.
            let excludes: Vec<&str> = if source.is_some() {
                vec![]
            } else {
                vec!["tool"]
            };
            let rows = store
                .list_sessions_for_browse(limit.max(1), source.as_deref(), &excludes, false)
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("No sessions found.");
                return Ok(());
            }
            // Owning project per session (longest-prefix cwd match in
            // projects.db) — rendered as a badge in both pickers (P165).
            let project_by_session = resolve_browse_projects(&rows);
            // Raw-mode TUI on a real terminal (hermes curses picker),
            // plain stdin loop otherwise (pipes, CI).
            let selected = if std::io::IsTerminal::is_terminal(&std::io::stdout())
                && std::io::IsTerminal::is_terminal(&std::io::stdin())
            {
                // P224: F5 reload + F8 archive callbacks backed by fresh
                // store connections (the picker may outlive this scope's
                // borrow of `store`).
                let reload_home = home.clone();
                let reload_source = source.clone();
                let reload_limit = limit.max(1);
                let reload_excludes: Vec<String> = excludes.iter().map(|s| s.to_string()).collect();
                let reload = move |include_archived: bool| {
                    let fresh_store = SqliteSessionStore::open(reload_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    let exclude_refs: Vec<&str> =
                        reload_excludes.iter().map(String::as_str).collect();
                    let fresh = fresh_store
                        .list_sessions_for_browse(
                            reload_limit,
                            reload_source.as_deref(),
                            &exclude_refs,
                            include_archived,
                        )
                        .map_err(|e| e.to_string())?;
                    let fresh_projects = resolve_browse_projects(&fresh);
                    Ok((fresh, fresh_projects))
                };
                let archive_home = home.clone();
                let archive = move |id: &str, archived: bool| {
                    let archive_store = SqliteSessionStore::open(archive_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    archive_store
                        .set_session_archived(id, archived)
                        .map_err(|e| e.to_string())
                };
                // P278: conversation preview callback — first user +
                // assistant messages of the highlighted session, loaded
                // through a fresh store connection (the picker may outlive
                // this scope's borrow of `store`).
                let preview_home = home.clone();
                let preview = move |id: &str| {
                    let preview_store = SqliteSessionStore::open(preview_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    let messages = preview_store.load_messages(id).map_err(|e| e.to_string())?;
                    let mut exchange: Vec<(String, Option<String>)> = Vec::new();
                    for message in &messages {
                        let role = match message.role {
                            ulnclaw::provider::Role::User => "you",
                            ulnclaw::provider::Role::Assistant => "assistant",
                            _ => continue,
                        };
                        exchange.push((role.to_string(), message.content.clone()));
                        // P534: two full rounds (four messages) so the pane
                        // has real transcript depth to scroll through.
                        if exchange.len() >= 4 {
                            break;
                        }
                    }
                    // P515: also surface the newest non-empty message so a
                    // session can be identified by its last activity too.
                    if let Some(last) = messages.iter().rev().find(|message| {
                        message
                            .content
                            .as_deref()
                            .map(|text| !text.trim().is_empty())
                            .unwrap_or(false)
                    }) {
                        let already_shown =
                            exchange.iter().any(|(_, content)| *content == last.content);
                        if !already_shown {
                            let role = match last.role {
                                ulnclaw::provider::Role::User => "you",
                                ulnclaw::provider::Role::Assistant => "assistant",
                                ulnclaw::provider::Role::System => "system",
                                _ => "tool",
                            };
                            exchange.push((format!("last \u{00B7} {role}"), last.content.clone()));
                        }
                    }
                    Ok(exchange)
                };
                // P340: transcript search (FTS over message bodies) and
                // permanent delete — both through fresh store connections
                // (the picker may outlive this scope's borrow of `store`).
                let search_home = home.clone();
                let transcript_search = move |query: &str| {
                    let search_store = SqliteSessionStore::open(search_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    search_store
                        .search_messages(query, 200)
                        .map_err(|e| e.to_string())
                };
                let delete_home = home.clone();
                let delete = move |id: &str| {
                    let delete_store = SqliteSessionStore::open(delete_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    delete_store.delete_session(id).map_err(|e| e.to_string())
                };
                // P512: rename (title write) and fork (mark the source
                // branched, create a child session carrying the
                // transcript forward) — fresh store connections like
                // the other browser callbacks.
                let rename_home = home.clone();
                let rename = move |id: &str, title: &str| {
                    let rename_store = SqliteSessionStore::open(rename_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    rename_store
                        .set_session_title(id, title)
                        .map_err(|e| e.to_string())
                };
                let fork_home = home.clone();
                let fork = move |id: &str| {
                    let fork_store = SqliteSessionStore::open(fork_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    let source = fork_store
                        .get_session_row(id)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("session {id} not found"))?;
                    let fork_id = uuid::Uuid::new_v4().to_string();
                    fork_store
                        .end_session(id, "branched")
                        .map_err(|e| e.to_string())?;
                    fork_store
                        .create_named_session(&fork_id, "cli", source.model.as_deref(), Some(id))
                        .map_err(|e| e.to_string())?;
                    let messages = fork_store.load_messages(id).map_err(|e| e.to_string())?;
                    for message in &messages {
                        fork_store
                            .append_message(&fork_id, message)
                            .map_err(|e| e.to_string())?;
                    }
                    let title = format!("{} fork", source.title.as_deref().unwrap_or("fork"));
                    fork_store
                        .set_session_title(&fork_id, &title)
                        .map_err(|e| e.to_string())?;
                    Ok(fork_id)
                };
                // P585: F11 export — write the highlighted session's
                // transcript as Markdown into <home>/exports.
                let export_home = home.clone();
                let export = move |id: &str| {
                    let export_store = SqliteSessionStore::open(export_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    let row = export_store
                        .get_session_row(id)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("session {id} not found"))?;
                    let messages = export_store
                        .load_messages_with_timestamps(id)
                        .map_err(|e| e.to_string())?;
                    let session = ulnclaw::session::export::ExportSession {
                        id: row.id.clone(),
                        title: row.title.clone(),
                        source: row.source.clone(),
                        model: row.model.clone(),
                        cwd: row.cwd.clone(),
                        started_at: row.started_at,
                        ended_at: row.ended_at,
                        messages,
                    };
                    ulnclaw::session::export::write_session_export(
                        &export_home.join("exports"),
                        &session,
                        "md",
                    )
                    .map_err(|e| e.to_string())
                };
                // P630: conversation-token estimate vs the configured
                // budget for the details pane (same chars/4 heuristic
                // as the gateway breakdown endpoint).
                let context_home = home.clone();
                let context_budget = config.agent.context_budget_tokens;
                let context = move |id: &str| -> Option<(usize, usize)> {
                    let store = SqliteSessionStore::open(context_home.join("state.db")).ok()?;
                    let history: Vec<ulnclaw::provider::Message> = store
                        .load_messages(id)
                        .ok()?
                        .into_iter()
                        .filter(|m| m.role != ulnclaw::provider::Role::System)
                        .collect();
                    Some((
                        ulnclaw::context::ContextCompressor::estimate_tokens(&history),
                        context_budget,
                    ))
                };
                // P651: full transcript for the in-browser viewer —
                // every user/system/assistant message (capped so a
                // giant session cannot blow up the redraw buffer).
                let transcript_home = home.clone();
                let transcript = move |id: &str| {
                    let transcript_store =
                        SqliteSessionStore::open(transcript_home.join("state.db"))
                            .map_err(|e| e.to_string())?;
                    let messages = transcript_store
                        .load_messages(id)
                        .map_err(|e| e.to_string())?;
                    let mut exchange: Vec<(String, Option<String>)> = Vec::new();
                    for message in &messages {
                        let role = match message.role {
                            ulnclaw::provider::Role::User => "you",
                            ulnclaw::provider::Role::Assistant => "assistant",
                            ulnclaw::provider::Role::System => "system",
                            _ => continue,
                        };
                        exchange.push((role.to_string(), message.content.clone()));
                        if exchange.len() >= 500 {
                            break;
                        }
                    }
                    Ok(exchange)
                };
                // P741: raw trace rendering for the viewer's `r`
                // toggle — timestamped messages including tool calls,
                // loaded through a fresh store connection (capped so a
                // giant session cannot blow up the redraw buffer).
                let raw_home = home.clone();
                let raw_transcript = move |id: &str| {
                    let raw_store = SqliteSessionStore::open(raw_home.join("state.db"))
                        .map_err(|e| e.to_string())?;
                    let messages = raw_store
                        .load_messages_with_timestamps(id)
                        .map_err(|e| e.to_string())?;
                    Ok(messages.into_iter().take(2000).collect::<Vec<_>>())
                };
                match run_session_browse_tui(
                    rows.clone(),
                    project_by_session.clone(),
                    Some(&reload),
                    Some(&archive),
                    Some(&preview),
                    Some(&transcript_search),
                    Some(&delete),
                    Some(&rename),
                    Some(&fork),
                    Some(&export),
                    Some(&context),
                    Some(&transcript),
                    Some(&raw_transcript),
                ) {
                    Ok(selected) => selected,
                    Err(_) => run_session_browse_stdin(&rows, &project_by_session)?, // raw mode unavailable
                }
            } else {
                run_session_browse_stdin(&rows, &project_by_session)?
            };
            if let Some(id) = selected {
                println!("Resuming session: {}", id);
                relaunch_resume(&id)?;
            }
            return Ok(());
        }
        SessionAction::RetitleSkills { limit, apply } => {
            let rows = store
                .list_skill_scaffolded_sessions(limit.max(1))
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("No sessions were titled from a /skill invocation.");
                return Ok(());
            }
            println!(
                "{} session(s) opened with a /skill{}:",
                rows.len(),
                if apply {
                    ""
                } else {
                    " (dry run — pass --apply to write)"
                }
            );
            let provider = build_provider(config, None)?;
            let mut changed = 0usize;
            for row in &rows {
                let typed = ulnclaw::session::retitle::describe_skill_invocation(&row.content)
                    .unwrap_or_default();
                let first_reply = store
                    .get_first_assistant_text(&row.id)
                    .map_err(|e| e.to_string())?;
                let Some(new_title) = ulnclaw::title_generator::generate_title_forced(
                    config,
                    provider.clone(),
                    &typed,
                    &first_reply,
                )
                .await
                else {
                    continue;
                };
                let old_title = row.title.as_deref().unwrap_or("");
                if new_title == old_title {
                    continue;
                }
                if !ulnclaw::session::retitle::is_titlelike(&new_title) {
                    println!(
                        "  {}\n    kept {:?} — got {:?}",
                        row.id, old_title, new_title
                    );
                    continue;
                }
                println!("  {}\n    {:?}\n    → {:?}", row.id, old_title, new_title);
                changed += 1;
                if !apply {
                    continue;
                }
                match store.set_session_title(&row.id, &new_title) {
                    Ok(()) => {}
                    Err(_) => {
                        // Unique-title collision: dedupe the same way the
                        // live auto-titler would (base #2, base #3, ...).
                        let deduped = store
                            .get_next_title_in_lineage(&new_title)
                            .map_err(|e| e.to_string())?;
                        match store.set_session_title(&row.id, &deduped) {
                            Ok(()) => println!("    (renamed to {:?} — title was taken)", deduped),
                            Err(e) => {
                                println!("    skipped: {}", e);
                                changed -= 1;
                            }
                        }
                    }
                }
            }
            if changed == 0 {
                println!("  every title already reflects the user's request.");
            } else if apply {
                println!("\u{2713} Re-titled {} session(s).", changed);
            }
        }
        SessionAction::Retitle { id, apply } => {
            // P569: single-session retitler — the CLI twin of
            // POST /api/sessions/:id/retitle (P559).
            let resolved = store
                .resolve_session_id(&id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("Session '{}' not found.", id))?;
            let row = store
                .get_session_row(&resolved)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("Session '{}' not found.", id))?;
            let user_text = store
                .get_first_user_text(&resolved)
                .map_err(|e| e.to_string())?;
            if user_text.trim().is_empty() {
                return Err("Session has no user message to title from.".to_string());
            }
            let first_reply = store
                .get_first_assistant_text(&resolved)
                .map_err(|e| e.to_string())?;
            let provider = build_provider(config, None)?;
            let Some(new_title) = ulnclaw::title_generator::generate_title_forced(
                config,
                provider,
                &user_text,
                &first_reply,
            )
            .await
            else {
                return Err("Title generation failed (provider unreachable?).".to_string());
            };
            let old_title = row.title.as_deref().unwrap_or("");
            if !ulnclaw::session::retitle::is_titlelike(&new_title) {
                println!(
                    "Kept {:?} — got {:?} (rejected as not title-like).",
                    old_title, new_title
                );
                return Ok(());
            }
            if new_title == old_title {
                println!("Title already up to date: {:?}", old_title);
                return Ok(());
            }
            println!(
                "{}\n    {:?}\n    \u{2192} {:?}",
                resolved, old_title, new_title
            );
            if !apply {
                println!("(dry run — pass --apply to write)");
                return Ok(());
            }
            match store.set_session_title(&resolved, &new_title) {
                Ok(()) => println!("\u{2713} Re-titled."),
                Err(_) => {
                    // Unique-title collision: dedupe like the live titler.
                    let deduped = store
                        .get_next_title_in_lineage(&new_title)
                        .map_err(|e| e.to_string())?;
                    store
                        .set_session_title(&resolved, &deduped)
                        .map_err(|e| e.to_string())?;
                    println!("\u{2713} Re-titled to {:?} (title was taken).", deduped);
                }
            }
        }
        SessionAction::Import { file, dry_run } => {
            // P577: the CLI twin of POST /api/sessions/import — works
            // straight against the store, no running gateway needed.
            let text = std::fs::read_to_string(&file)
                .map_err(|e| format!("read {}: {e}", file.display()))?;
            let parsed: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("parse {}: {e}", file.display()))?;
            let sessions = match &parsed {
                serde_json::Value::Array(list) => list.clone(),
                obj => obj
                    .get("sessions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .ok_or_else(|| {
                        "expected {\"sessions\": [...]} or a top-level array".to_string()
                    })?,
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let mut imported = 0usize;
            let mut skipped = 0usize;
            let mut messages = 0usize;
            for raw in &sessions {
                let id = raw
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let msgs = raw
                    .get("messages")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if dry_run {
                    match store.get_session_row(&id) {
                        Ok(Some(_)) => skipped += 1,
                        Ok(None) => {
                            imported += 1;
                            messages += msgs.len();
                        }
                        Err(e) => return Err(e.to_string()),
                    }
                    continue;
                }
                let started_at = raw
                    .get("started_at")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(now);
                match store.insert_imported_session(
                    &id,
                    raw.get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or("import"),
                    raw.get("model").and_then(|v| v.as_str()),
                    raw.get("title").and_then(|v| v.as_str()),
                    raw.get("cwd").and_then(|v| v.as_str()),
                    started_at,
                    raw.get("ended_at").and_then(|v| v.as_f64()),
                    raw.get("end_reason").and_then(|v| v.as_str()),
                    raw.get("archived")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                ) {
                    Ok(true) => {}
                    Ok(false) => {
                        skipped += 1;
                        continue;
                    }
                    Err(e) => return Err(e.to_string()),
                }
                for (offset, message) in msgs.iter().enumerate() {
                    let role = match message.get("role").and_then(|v| v.as_str()).unwrap_or("") {
                        "system" => ulnclaw::provider::Role::System,
                        "user" => ulnclaw::provider::Role::User,
                        "assistant" => ulnclaw::provider::Role::Assistant,
                        "tool" => ulnclaw::provider::Role::Tool,
                        other => {
                            return Err(format!("session {id}: invalid message role: {other}"))
                        }
                    };
                    let msg = ulnclaw::provider::Message {
                        role,
                        content: message
                            .get("content")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        tool_calls: message
                            .get("tool_calls")
                            .cloned()
                            .and_then(|v| serde_json::from_value(v).ok()),
                        tool_call_id: message
                            .get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        name: message
                            .get("tool_name")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    };
                    let at = message
                        .get("timestamp")
                        .and_then(|v| v.as_f64())
                        .filter(|t| *t > 0.0)
                        .unwrap_or(started_at + offset as f64 * 0.001);
                    store
                        .append_message_at(&id, &msg, at)
                        .map_err(|e| format!("session {id} message {offset}: {e}"))?;
                    messages += 1;
                }
                imported += 1;
            }
            if dry_run {
                println!(
                    "Dry run: would import {} session(s), {} message(s); {} already present.",
                    imported, messages, skipped
                );
            } else {
                println!(
                    "\u{2713} Imported {} session(s), {} message(s); skipped {}.",
                    imported, messages, skipped
                );
            }
        }
        SessionAction::Delete { id, yes } => {
            let resolved = store
                .resolve_session_id(&id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("Session '{}' not found.", id))?;
            if !yes {
                print!("Delete session '{}' and all its messages? [y/N] ", resolved);
                std::io::Write::flush(&mut std::io::stdout()).ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer).ok();
                if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            store.delete_session(&resolved).map_err(|e| e.to_string())?;
            println!("Deleted session '{}'.", resolved);
        }
        SessionAction::Rename { id, title } => {
            let resolved = store
                .resolve_session_id(&id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("Session '{}' not found.", id))?;
            let title = title.join(" ");
            store
                .set_session_title(&resolved, &title)
                .map_err(|e| e.to_string())?;
            match store
                .get_session_title(&resolved)
                .map_err(|e| e.to_string())?
            {
                Some(new_title) => println!("Session '{}' renamed to: {}", resolved, new_title),
                None => println!("Session '{}' title cleared.", resolved),
            }
        }
        SessionAction::Optimize => {
            let db_path = home.join("state.db");
            let before_mb = std::fs::metadata(&db_path)
                .map(|m| m.len() as f64 / (1024.0 * 1024.0))
                .unwrap_or(0.0);
            println!("Optimizing session store (FTS merge + VACUUM)…");
            let merged = store
                .optimize_storage()
                .map_err(|e| format!("optimization failed: {}", e))?;
            let mut after_mb = std::fs::metadata(&db_path)
                .map(|m| m.len() as f64 / (1024.0 * 1024.0))
                .unwrap_or(0.0);
            // In WAL mode the main file lags until checkpointed back;
            // SQLite's page accounting is correct immediately.
            if let Some(logical) = store.logical_size_bytes() {
                after_mb = logical as f64 / (1024.0 * 1024.0);
            }
            let saved = before_mb - after_mb;
            let delta = if saved >= 0.0 {
                format!("saved {:.1} MB", saved)
            } else {
                format!("grew {:.1} MB", -saved)
            };
            println!("Optimized {} FTS index(es).", merged);
            println!(
                "Database size: {:.1} MB -> {:.1} MB ({})",
                before_mb, after_mb, delta
            );
        }
        SessionAction::Repair { .. } => {
            // Handled before the store opens (a malformed schema is exactly
            // the case where open fails).
            unreachable!("sessions repair is dispatched before the store opens")
        }
    }
    Ok(())
}

/// Resolve the owning project slug for each browse row's cwd
/// (best-effort: a missing/closed projects store yields an empty map).
fn resolve_browse_projects(
    rows: &[ulnclaw::session::sqlite::BrowseRow],
) -> std::collections::HashMap<String, String> {
    let cwds: Vec<String> = rows
        .iter()
        .filter_map(|row| row.cwd.clone())
        .filter(|cwd| !cwd.trim().is_empty())
        .collect();
    if cwds.is_empty() {
        return std::collections::HashMap::new();
    }
    let Ok(conn) = ulnclaw::projects_db::connect(None) else {
        return std::collections::HashMap::new();
    };
    let Ok(mapping) = ulnclaw::projects_db::projects_for_paths(&conn, &cwds) else {
        return std::collections::HashMap::new();
    };
    rows.iter()
        .filter_map(|row| {
            let cwd = row.cwd.as_ref()?;
            let slug = mapping.get(cwd)?.slug.clone();
            Some((row.id.clone(), slug))
        })
        .collect()
}

/// Format one browse row: title (preview/id fallback) + relative time +
/// source + truncated id, adaptive to the available width (hermes
/// `_format_row`). When the session belongs to a project, a `⌂ slug`
/// badge prefixes the name (P165).
fn format_browse_row(
    row: &ulnclaw::session::sqlite::BrowseRow,
    name_width: usize,
    project: Option<&str>,
) -> String {
    let title = row
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| row.preview.clone())
        .unwrap_or_else(|| row.id.clone());
    let label = match project {
        Some(slug) => format!("\u{2302}{slug} {title}"),
        None => title,
    };
    // P544: ended rows carry their end-reason marker (desktop END_BADGES
    // parity — archived rows already get the ␡ marker below).
    let label = match row.end_reason.as_deref() {
        Some("complete") | Some("completed") => format!("\u{2713} {label}"),
        Some("max_iterations") => format!("\u{221E} {label}"),
        Some("compression") => format!("\u{29C9} {label}"),
        Some("branched") => format!("\u{2442} {label}"),
        Some("ended") => format!("\u{25A0} {label}"),
        _ => label,
    };
    // P513: archived rows carry a ␡ marker when surfaced (F4 toggle).
    let label = if row.archived {
        format!("\u{2421} {label}")
    } else {
        label
    };
    let name: String = label.chars().take(name_width).collect();
    format!(
        "{:<width$}  {:<10}  {:<6}  {}",
        name,
        relative_time(row.last_active),
        row.source,
        &row.id[..row.id.len().min(18)],
        width = name_width
    )
}

/// Case-insensitive browse filter over title/preview/id/source (shared by
/// the TUI and the stdin picker).
fn browse_row_matches(
    row: &ulnclaw::session::sqlite::BrowseRow,
    project: Option<&str>,
    filter: &str,
) -> bool {
    if filter.is_empty() {
        return true;
    }
    let q = filter.to_lowercase();
    project
        .map(|slug| slug.to_lowercase().contains(&q))
        .unwrap_or(false)
        || row
            .title
            .as_deref()
            .map(|t| t.to_lowercase().contains(&q))
            .unwrap_or(false)
        || row
            .preview
            .as_deref()
            .map(|p| p.to_lowercase().contains(&q))
            .unwrap_or(false)
        || row.id.to_lowercase().contains(&q)
        || row.source.to_lowercase().contains(&q)
}

/// Raw-mode terminal session browser (hermes curses
/// `_session_browse_picker` port + ulnclaw interaction upgrades):
/// arrow-key navigation with scrolling, wrapping at the list edges,
/// live type-to-filter, Enter selects, bare `q` quits while no filter
/// is active, and Esc clears the filter first (quitting on the second
/// press). Renders hermes' dim column-header strip and a bottom footer
/// with the cursor position + filtered-from count. Upgrades over the
/// hermes picker: a right-hand details pane (terminal ≥ 90 cols) with
/// the highlighted session's full title, id, source, project, cwd,
/// last-active timestamp, and first-message preview; `Tab` cycles a
/// per-source filter; `F2` toggles recent-first ↔ alphabetical sort.
///
/// P224 interaction upgrades: `F1` opens a dismissible keybinding help
/// overlay, `F5` reloads the session list from disk through `reload`
/// (a live gateway may create sessions mid-browse), `F8` toggles the
/// highlighted session's archived state after an inline `y`
/// confirmation (via `archive(id, archived)`, then auto-reloads; P520),
/// `Shift+Tab` cycles the source filter backwards, and transient footer
/// notices report reload/archive outcomes until the next keypress.
///
/// P512 interaction upgrades: `F6` renames the highlighted session
/// through an inline title prompt (Enter saves via `rename`, Esc
/// cancels), and `F7` forks it after an inline `y` confirmation (via
/// `fork`, which returns the new session id). Returns the selected
/// session id, or `None` when cancelled.
fn run_session_browse_tui(
    mut rows: Vec<ulnclaw::session::sqlite::BrowseRow>,
    mut projects: std::collections::HashMap<String, String>,
    reload: Option<
        &dyn Fn(
            bool,
        ) -> Result<
            (
                Vec<ulnclaw::session::sqlite::BrowseRow>,
                std::collections::HashMap<String, String>,
            ),
            String,
        >,
    >,
    archive: Option<&dyn Fn(&str, bool) -> Result<(), String>>,
    preview: Option<&dyn Fn(&str) -> Result<Vec<(String, Option<String>)>, String>>,
    transcript_search: Option<&dyn Fn(&str) -> Result<Vec<(String, String)>, String>>,
    delete: Option<&dyn Fn(&str) -> Result<(), String>>,
    rename: Option<&dyn Fn(&str, &str) -> Result<(), String>>,
    fork: Option<&dyn Fn(&str) -> Result<String, String>>,
    export: Option<&dyn Fn(&str) -> Result<std::path::PathBuf, String>>,
    context: Option<&dyn Fn(&str) -> Option<(usize, usize)>>,
    transcript: Option<&dyn Fn(&str) -> Result<Vec<(String, Option<String>)>, String>>,
    raw_transcript: Option<&dyn Fn(&str) -> Result<Vec<(f64, ulnclaw::provider::Message)>, String>>,
) -> Result<Option<String>, String> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute, queue,
        style::{Color, Print, ResetColor, SetForegroundColor},
        terminal::{self, Clear, ClearType},
    };
    use std::collections::BTreeSet;

    /// Minimum terminal width before the details pane is shown.
    const PANE_MIN_COLS: usize = 90;
    /// Details pane width (excluding the gutter column).
    const PANE_WIDTH: usize = 40;

    /// Restore the terminal on every exit path.
    struct TuiGuard;
    impl Drop for TuiGuard {
        fn drop(&mut self) {
            terminal::disable_raw_mode().ok();
            let mut out = std::io::stdout();
            execute!(out, cursor::Show, terminal::LeaveAlternateScreen).ok();
        }
    }

    let mut out = std::io::stdout();
    execute!(out, terminal::EnterAlternateScreen, cursor::Hide).map_err(|e| e.to_string())?;
    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let _guard = TuiGuard;

    let mut source_idx: usize = 0; // 0 = all sources
    let mut sort_alpha = false;
    let mut cursor_idx: usize = 0;
    let mut scroll_offset: usize = 0;
    let mut filter = String::new();
    // P224 interaction state: help overlay, archive confirmation, and a
    // transient footer notice (cleared by the next keypress).
    let mut show_help = false;
    let mut confirm_archive = false;
    let mut notice: Option<String> = None;
    // P278: on-demand conversation preview for the details pane — loaded
    // per highlighted session, cached by id, toggled with F3.
    let mut show_preview = preview.is_some();
    let mut preview_cache: std::collections::HashMap<String, Vec<(String, Option<String>)>> =
        std::collections::HashMap::new();
    // P651: full-screen transcript viewer — `v` opens the complete
    // transcript of the highlighted session (scroll keys navigate,
    // Esc/q returns to the list).
    let mut transcript_mode = false;
    let mut transcript_title = String::new();
    let mut transcript_lines: Vec<String> = Vec::new();
    let mut transcript_scroll: usize = 0;
    // P741: raw-mode rendering inside the transcript viewer — `r`
    // swaps between the pretty rendering and the raw trace rendering
    // (timestamps, tool names/ids, tool_calls JSON).
    let mut transcript_raw_mode = false;
    let mut transcript_alt_lines: Vec<String> = Vec::new();
    // P630: live context-in-use per highlighted session (used, budget),
    // cached by id so redraws don't re-read the store.
    let mut context_cache: std::collections::HashMap<String, Option<(usize, usize)>> =
        std::collections::HashMap::new();
    // P340: transcript-search state — `/` opens the query prompt, Enter
    // runs the FTS search and narrows the list to matching sessions,
    // Esc clears the results. F9 deletes the highlighted session after
    // an inline confirmation (mirrors the F8 archive flow).
    let mut search_mode = false;
    let mut search_query = String::new();
    let mut search_hits: Option<Vec<(String, String)>> = None;
    let mut confirm_delete = false;
    // P512: rename prompt state (F6) and fork confirmation (F7).
    let mut rename_mode = false;
    let mut rename_buffer = String::new();
    let mut confirm_fork = false;
    // P513: F4 toggles archived sessions into the list (reloads via the
    // include_archived flag on the reload closure).
    let mut show_archived = false;
    // P528: F10 cycles a model quick-filter — 0 shows every model, then
    // each distinct model present in the loaded rows.
    let mut model_idx: usize = 0;
    // P529: details-pane scroll offset (Ctrl+U / Ctrl+D) for previews
    // longer than the pane; resets when the highlighted session moves.
    let mut pane_scroll: usize = 0;
    let mut last_pane_id: Option<String> = None;

    loop {
        // Tab cycles "all sources" plus each distinct source present in
        // the loaded rows (recomputed after every F5 reload).
        let sources: Vec<String> = rows
            .iter()
            .map(|r| r.source.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if source_idx > sources.len() {
            source_idx = 0;
        }
        let source_filter: Option<&str> = if source_idx == 0 {
            None
        } else {
            sources.get(source_idx - 1).map(String::as_str)
        };
        // P528: distinct non-empty models across the loaded rows, in
        // alphabetical order, cycled with F10 (Shift+F10 backwards).
        let models: Vec<String> = rows
            .iter()
            .filter_map(|r| r.model.clone().filter(|m| !m.is_empty()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if model_idx > models.len() {
            model_idx = 0;
        }
        let model_filter: Option<&str> = if model_idx == 0 {
            None
        } else {
            models.get(model_idx - 1).map(String::as_str)
        };
        let mut filtered: Vec<&ulnclaw::session::sqlite::BrowseRow> = rows
            .iter()
            .filter(|r| source_filter.map(|s| r.source == s).unwrap_or(true))
            .filter(|r| {
                model_filter
                    .map(|m| r.model.as_deref() == Some(m))
                    .unwrap_or(true)
            })
            .filter(|r| browse_row_matches(r, projects.get(&r.id).map(String::as_str), &filter))
            .collect();
        if let Some(hits) = search_hits.as_ref() {
            let hit_ids: std::collections::HashSet<&str> =
                hits.iter().map(|(id, _)| id.as_str()).collect();
            filtered.retain(|r| hit_ids.contains(r.id.as_str()));
        }
        if sort_alpha {
            filtered.sort_by(|a, b| {
                ulnclaw::tui_text::browse_title_sort_key(a.title.as_deref())
                    .cmp(&ulnclaw::tui_text::browse_title_sort_key(
                        b.title.as_deref(),
                    ))
                    .then_with(|| {
                        b.last_active
                            .partial_cmp(&a.last_active)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            });
        }
        if cursor_idx >= filtered.len() {
            cursor_idx = filtered.len().saturating_sub(1);
        }
        // Owned copy for modal actions (archive confirm) so the key
        // handler can mutate `rows` without fighting the borrow of
        // `filtered`.
        let highlighted_id: Option<String> = filtered.get(cursor_idx).map(|row| row.id.clone());
        let highlighted_label: Option<String> = filtered.get(cursor_idx).map(|row| {
            row.title
                .clone()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| row.id.clone())
        });
        // P520: whether the highlighted row is archived (F8 toggles).
        let highlighted_archived: bool = filtered
            .get(cursor_idx)
            .map(|row| row.archived)
            .unwrap_or(false);
        // P529: a moving highlight restarts the pane at the top.
        if highlighted_id != last_pane_id {
            pane_scroll = 0;
            last_pane_id = highlighted_id.clone();
        }

        // P651: while the transcript viewer is open it owns the screen
        // — its own draw + key handling, then straight to the next pass.
        if transcript_mode {
            let (tcols, trows) = terminal::size().map_err(|e| e.to_string())?;
            let (tcols, trows) = (tcols as usize, trows as usize);
            queue!(out, Clear(ClearType::All), cursor::MoveTo(0, 0)).map_err(|e| e.to_string())?;
            let body_h = trows.saturating_sub(2);
            let max_scroll = transcript_lines.len().saturating_sub(body_h);
            if transcript_scroll > max_scroll {
                transcript_scroll = max_scroll;
            }
            if trows >= 3 && tcols >= 20 {
                let header = format!(
                    "  {} \u{2014} transcript{}  (\u{2191}\u{2193}/PgUp/PgDn scroll, Home/End, r {}, Esc back)",
                    transcript_title,
                    if transcript_raw_mode { " [RAW]" } else { "" },
                    if transcript_raw_mode { "pretty" } else { "raw" },
                );
                queue!(
                    out,
                    SetForegroundColor(Color::Green),
                    Print(header.chars().take(tcols).collect::<String>()),
                    ResetColor
                )
                .map_err(|e| e.to_string())?;
                for (i, line) in transcript_lines
                    .iter()
                    .skip(transcript_scroll)
                    .take(body_h)
                    .enumerate()
                {
                    queue!(out, cursor::MoveTo(0, (i + 1) as u16)).map_err(|e| e.to_string())?;
                    let clipped = line.chars().take(tcols).collect::<String>();
                    if line.starts_with("\u{2500}\u{2500} ") {
                        queue!(
                            out,
                            SetForegroundColor(Color::Cyan),
                            Print(clipped),
                            ResetColor
                        )
                        .map_err(|e| e.to_string())?;
                    } else {
                        queue!(out, Print(clipped)).map_err(|e| e.to_string())?;
                    }
                }
                let pct = if max_scroll == 0 {
                    100
                } else {
                    (transcript_scroll * 100) / max_scroll
                };
                let footer = format!(
                    " line {}/{} \u{00B7} {}% ",
                    transcript_scroll + 1,
                    transcript_lines.len().max(1),
                    pct
                );
                queue!(
                    out,
                    cursor::MoveTo(0, trows.saturating_sub(1) as u16),
                    SetForegroundColor(Color::DarkGrey),
                    Print(footer),
                    ResetColor
                )
                .map_err(|e| e.to_string())?;
            }
            out.flush().map_err(|e| e.to_string())?;
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        transcript_mode = false;
                    }
                    KeyCode::Up => {
                        transcript_scroll = transcript_scroll.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        transcript_scroll = (transcript_scroll + 1).min(max_scroll);
                    }
                    KeyCode::PageUp => {
                        transcript_scroll = transcript_scroll.saturating_sub(body_h);
                    }
                    KeyCode::PageDown => {
                        transcript_scroll = (transcript_scroll + body_h).min(max_scroll);
                    }
                    KeyCode::Home => transcript_scroll = 0,
                    KeyCode::End => transcript_scroll = max_scroll,
                    KeyCode::Char('r') => {
                        // P741: swap pretty \u{2194} raw renderings.
                        if !transcript_alt_lines.is_empty() {
                            std::mem::swap(&mut transcript_lines, &mut transcript_alt_lines);
                            transcript_raw_mode = !transcript_raw_mode;
                            transcript_scroll = 0;
                        }
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
            continue;
        }

        let (cols, rows_h) = terminal::size().map_err(|e| e.to_string())?;
        let (cols, rows_h) = (cols as usize, rows_h as usize);
        queue!(out, Clear(ClearType::All), cursor::MoveTo(0, 0)).map_err(|e| e.to_string())?;

        if rows_h < 5 || cols < 40 {
            queue!(out, Print("Terminal too small")).map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
            // Wait for any key before bailing out.
            let _ = event::read();
            return Ok(None);
        }

        let pane_enabled = cols >= PANE_MIN_COLS;
        let list_width = if pane_enabled {
            cols.saturating_sub(PANE_WIDTH + 2)
        } else {
            cols
        };

        // Header (hermes: filter line with block cursor, else key hints),
        // plus green chips for the active source filter / sort mode.
        if filter.is_empty() {
            queue!(
                out,
                SetForegroundColor(Color::Yellow),
                Print("  Browse sessions — \u{2191}\u{2193} navigate  Enter select  v transcript  r raw  Type to filter  Tab source  F10 model  F2 sort  F3 preview  Ctrl+U/D pane  F1 help  F5 reload  F8 archive  Esc quit"),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
        } else {
            queue!(
                out,
                SetForegroundColor(Color::Cyan),
                Print(format!("  Browse sessions — filter: {filter}\u{2588}")),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(name) = source_filter {
            queue!(
                out,
                SetForegroundColor(Color::Green),
                Print(format!("  [source: {name}]")),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
        }
        if let Some(name) = model_filter {
            queue!(
                out,
                SetForegroundColor(Color::Green),
                Print(format!("  [model: {name}]")),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
        }
        if sort_alpha {
            queue!(
                out,
                SetForegroundColor(Color::Green),
                Print("  [sort: A-Z]"),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
        }
        if show_archived {
            queue!(
                out,
                SetForegroundColor(Color::Green),
                Print("  [+archived]"),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
        }
        queue!(out, Print("\r\n")).map_err(|e| e.to_string())?;

        // Column-header strip (hermes dim "Title / Preview  Active  Src  ID").
        let name_width = list_width.saturating_sub(3 + 10 + 6 + 18 + 6).max(20);
        queue!(
            out,
            SetForegroundColor(Color::DarkGrey),
            Print(format!(
                "   {:<nw$}  {:<10}  {:<6}  {}",
                "Title / Preview",
                "Active",
                "Src",
                "ID",
                nw = name_width
            )),
            ResetColor,
            Print("\r\n\r\n")
        )
        .map_err(|e| e.to_string())?;

        // Viewport: header + column header + spacer above, footer below
        // (hermes reserves four chrome rows).
        let page_height = rows_h.saturating_sub(4).max(1);
        if cursor_idx < scroll_offset {
            scroll_offset = cursor_idx;
        }
        if cursor_idx >= scroll_offset + page_height {
            scroll_offset = cursor_idx - page_height + 1;
        }
        if scroll_offset >= filtered.len() {
            scroll_offset = filtered.len().saturating_sub(1);
        }
        let end = (scroll_offset + page_height).min(filtered.len());
        for (i, row) in filtered[scroll_offset..end].iter().enumerate() {
            let selected = scroll_offset + i == cursor_idx;
            if selected {
                queue!(out, SetForegroundColor(Color::Green)).map_err(|e| e.to_string())?;
            }
            let arrow = if selected { " \u{25B6} " } else { "   " };
            queue!(
                out,
                Print(arrow),
                Print(format_browse_row(
                    row,
                    name_width,
                    projects.get(&row.id).map(String::as_str)
                ))
            )
            .map_err(|e| e.to_string())?;
            if selected {
                queue!(out, ResetColor).map_err(|e| e.to_string())?;
            }
            queue!(out, Print("\r\n")).map_err(|e| e.to_string())?;
        }
        if filtered.is_empty() {
            queue!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print("  No sessions match the filter — backspace or Esc clears it."),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
        }

        // Details pane for the highlighted session (upgrade over hermes).
        if pane_enabled {
            let pane_x = (list_width + 1) as u16;
            let content_w = cols.saturating_sub(list_width + 3).max(8);
            queue!(
                out,
                cursor::MoveTo(pane_x, 1),
                SetForegroundColor(Color::DarkGrey),
                Print("Details"),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
            if let Some(selected_row) = filtered.get(cursor_idx) {
                let context_value = context
                    .map(|context_fn| {
                        *context_cache
                            .entry(selected_row.id.clone())
                            .or_insert_with(|| context_fn(&selected_row.id))
                    })
                    .flatten();
                let mut pane_lines = browse_details_pane_lines(
                    selected_row,
                    projects.get(&selected_row.id).map(String::as_str),
                    content_w,
                    context_value,
                );
                // P340: transcript-match snippet above the preview.
                if let Some(hits) = search_hits.as_ref() {
                    if let Some((_, snippet)) = hits.iter().find(|(id, _)| *id == selected_row.id) {
                        pane_lines.push(String::new());
                        pane_lines.push("\u{2500} transcript match \u{2500}".to_string());
                        pane_lines.extend(ulnclaw::tui_text::wrap_display_text(snippet, content_w));
                    }
                }
                // P278: first-exchange preview under the metadata.
                if show_preview {
                    if let Some(preview_fn) = preview {
                        let exchange = match preview_cache.get(&selected_row.id) {
                            Some(cached) => cached.clone(),
                            None => {
                                let loaded = preview_fn(&selected_row.id).unwrap_or_default();
                                preview_cache.insert(selected_row.id.clone(), loaded.clone());
                                if preview_cache.len() > 64 {
                                    preview_cache.clear();
                                }
                                loaded
                            }
                        };
                        let text =
                            // P534: deeper per-message budget — the scrollable
                            // pane (P529) pages through the extra text.
                            ulnclaw::tui_text::browse_conversation_preview(&exchange, 600);
                        if !text.is_empty() {
                            pane_lines.push(String::new());
                            pane_lines.push("\u{2500} conversation \u{2500}".to_string());
                            pane_lines
                                .extend(ulnclaw::tui_text::wrap_display_text(&text, content_w));
                        }
                    }
                }
                let max_lines = rows_h.saturating_sub(3);
                // P529: scrollable viewport over the pane lines.
                pane_scroll = pane_scroll.min(pane_lines.len().saturating_sub(1));
                let visible: Vec<&String> = pane_lines
                    .iter()
                    .skip(pane_scroll)
                    .take(max_lines)
                    .collect();
                for (offset, line) in visible.iter().enumerate() {
                    queue!(
                        out,
                        cursor::MoveTo(pane_x, (2 + offset) as u16),
                        Print(line)
                    )
                    .map_err(|e| e.to_string())?;
                }
                // Dim scroll cue beside the "Details" header.
                let has_more = pane_scroll + max_lines < pane_lines.len();
                if has_more || pane_scroll > 0 {
                    let cue = match (pane_scroll > 0, has_more) {
                        (true, true) => "\u{2195} Ctrl+U/D",
                        (true, false) => "\u{2191} Ctrl+U",
                        (false, true) => "\u{2193} Ctrl+D",
                        (false, false) => "",
                    };
                    queue!(
                        out,
                        cursor::MoveTo(pane_x + 8, 1),
                        SetForegroundColor(Color::DarkGrey),
                        Print(cue),
                        ResetColor
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
        }

        // Help overlay (P224, F1): keybinding table drawn over the list
        // area; any key dismisses it.
        if show_help {
            for (offset, (key, desc)) in ulnclaw::tui_text::browse_help_entries().iter().enumerate()
            {
                let y = 2 + offset;
                if y + 1 >= rows_h {
                    break;
                }
                queue!(
                    out,
                    cursor::MoveTo(2, y as u16),
                    SetForegroundColor(Color::Green),
                    Print(format!("{:<12}", key)),
                    ResetColor,
                    SetForegroundColor(Color::DarkGrey),
                    Print(desc),
                    ResetColor
                )
                .map_err(|e| e.to_string())?;
            }
        }

        // Footer on the bottom row: archive confirmation > transient
        // notice > cursor position + filtered-from count (hermes dim
        // footer).
        queue!(out, cursor::MoveTo(0, rows_h.saturating_sub(1) as u16))
            .map_err(|e| e.to_string())?;
        if search_mode {
            queue!(
                out,
                SetForegroundColor(Color::Yellow),
                Print(ulnclaw::tui_text::browse_transcript_search_prompt(
                    &search_query
                )),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        } else if rename_mode {
            let label = highlighted_label.clone().unwrap_or_default();
            queue!(
                out,
                SetForegroundColor(Color::Cyan),
                Print(ulnclaw::tui_text::browse_rename_prompt_text(
                    &label,
                    &rename_buffer
                )),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        } else if confirm_delete {
            let label = highlighted_label.clone().unwrap_or_default();
            queue!(
                out,
                SetForegroundColor(Color::Red),
                Print(ulnclaw::tui_text::browse_delete_confirm_text(&label)),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        } else if confirm_fork {
            let label = highlighted_label.clone().unwrap_or_default();
            queue!(
                out,
                SetForegroundColor(Color::Cyan),
                Print(ulnclaw::tui_text::browse_fork_confirm_text(&label)),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        } else if confirm_archive {
            let label = highlighted_label.clone().unwrap_or_default();
            let prompt = if highlighted_archived {
                ulnclaw::tui_text::browse_unarchive_confirm_text(&label)
            } else {
                ulnclaw::tui_text::browse_archive_confirm_text(&label)
            };
            queue!(
                out,
                SetForegroundColor(Color::Yellow),
                Print(prompt),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        } else if let Some(text) = notice.clone() {
            queue!(
                out,
                SetForegroundColor(Color::Green),
                Print(format!("  {text}")),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        } else {
            let footer = if filtered.is_empty() {
                format!("  0/{} sessions", rows.len())
            } else {
                let mut text = format!("  {}/{} sessions", cursor_idx + 1, filtered.len());
                if filtered.len() < rows.len() {
                    text.push_str(&format!(" (filtered from {})", rows.len()));
                }
                text
            };
            queue!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print(footer),
                ResetColor
            )
            .map_err(|e| e.to_string())?;
            out.flush().map_err(|e| e.to_string())?;
        }

        match event::read().map_err(|e| e.to_string())? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
                {
                    return Ok(None);
                }
                // Any keypress clears the transient notice (P224).
                notice = None;
                // P340: transcript-search prompt captures keys until
                // Enter (run) or Esc (cancel).
                if search_mode {
                    match key.code {
                        KeyCode::Esc => {
                            search_mode = false;
                            search_query.clear();
                        }
                        KeyCode::Enter => {
                            search_mode = false;
                            let query = search_query.trim().to_string();
                            search_query.clear();
                            if query.is_empty() {
                                continue;
                            }
                            match transcript_search {
                                Some(search_fn) => match search_fn(&query) {
                                    Ok(hits) => {
                                        let total = hits.len();
                                        let hit_ids: std::collections::HashSet<&str> =
                                            hits.iter().map(|(id, _)| id.as_str()).collect();
                                        let in_list = rows
                                            .iter()
                                            .filter(|r| hit_ids.contains(r.id.as_str()))
                                            .count();
                                        search_hits = Some(hits);
                                        cursor_idx = 0;
                                        scroll_offset = 0;
                                        notice = Some(format!(
                                            "Transcript search \u{201C}{query}\u{201D} \u{2014} {total} match(es), {in_list} in list. Esc clears."
                                        ));
                                    }
                                    Err(e) => {
                                        notice = Some(format!("Search failed: {e}"));
                                    }
                                },
                                None => {
                                    notice =
                                        Some("Transcript search is unavailable here.".to_string());
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            search_query.pop();
                        }
                        KeyCode::Char(ch) => {
                            search_query.push(ch);
                        }
                        _ => {}
                    }
                    continue;
                }
                // P512: rename prompt captures keys until Enter (save)
                // or Esc (cancel).
                if rename_mode {
                    match key.code {
                        KeyCode::Esc => {
                            rename_mode = false;
                            rename_buffer.clear();
                        }
                        KeyCode::Enter => {
                            rename_mode = false;
                            let title = rename_buffer.trim().to_string();
                            rename_buffer.clear();
                            if title.is_empty() {
                                continue;
                            }
                            if let Some(id) = highlighted_id.clone() {
                                match rename {
                                    Some(rename_fn) => match rename_fn(&id, &title) {
                                        Ok(()) => {
                                            if let Some(reload_fn) = reload {
                                                if let Ok((fresh_rows, fresh_projects)) =
                                                    reload_fn(show_archived)
                                                {
                                                    rows = fresh_rows;
                                                    projects = fresh_projects;
                                                }
                                            }
                                            cursor_idx = 0;
                                            scroll_offset = 0;
                                            notice = Some(format!(
                                                "Renamed to \u{201C}{title}\u{201D}."
                                            ));
                                        }
                                        Err(e) => {
                                            notice = Some(format!("Rename failed: {e}"));
                                        }
                                    },
                                    None => {
                                        notice = Some("Renaming is unavailable here.".to_string());
                                    }
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            rename_buffer.pop();
                        }
                        KeyCode::Char(ch) => {
                            rename_buffer.push(ch);
                        }
                        _ => {}
                    }
                    continue;
                }
                // Help overlay is modal: any key dismisses it (P224).
                if show_help {
                    show_help = false;
                    continue;
                }
                // P340: delete confirmation is modal — y deletes forever,
                // any other key cancels.
                if confirm_delete {
                    confirm_delete = false;
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        if let Some(id) = highlighted_id.clone() {
                            match delete {
                                Some(delete_fn) => match delete_fn(&id) {
                                    Ok(()) => {
                                        rows.retain(|r| r.id != id);
                                        if let Some(hits) = search_hits.as_mut() {
                                            hits.retain(|(hit_id, _)| *hit_id != id);
                                        }
                                        if let Some(reload_fn) = reload {
                                            if let Ok((fresh_rows, fresh_projects)) =
                                                reload_fn(show_archived)
                                            {
                                                rows = fresh_rows;
                                                projects = fresh_projects;
                                            }
                                        }
                                        cursor_idx = 0;
                                        scroll_offset = 0;
                                        notice = Some(format!("Deleted {id}."));
                                    }
                                    Err(e) => {
                                        notice = Some(format!("Delete failed: {e}"));
                                    }
                                },
                                None => {
                                    notice = Some("Deleting is unavailable here.".to_string());
                                }
                            }
                        }
                    }
                    continue;
                }
                // P512: fork confirmation is modal — y forks, any other
                // key cancels.
                if confirm_fork {
                    confirm_fork = false;
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        if let Some(id) = highlighted_id.clone() {
                            match fork {
                                Some(fork_fn) => match fork_fn(&id) {
                                    Ok(new_id) => {
                                        if let Some(reload_fn) = reload {
                                            if let Ok((fresh_rows, fresh_projects)) =
                                                reload_fn(show_archived)
                                            {
                                                rows = fresh_rows;
                                                projects = fresh_projects;
                                            }
                                        }
                                        cursor_idx = 0;
                                        scroll_offset = 0;
                                        notice = Some(format!("Forked {id} as {new_id}."));
                                    }
                                    Err(e) => {
                                        notice = Some(format!("Fork failed: {e}"));
                                    }
                                },
                                None => {
                                    notice = Some("Forking is unavailable here.".to_string());
                                }
                            }
                        }
                    }
                    continue;
                }
                // Archive confirmation is modal (P224): y archives, any
                // other key cancels.
                if confirm_archive {
                    confirm_archive = false;
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        if let Some(id) = highlighted_id.clone() {
                            // P520: F8 toggles — archived rows unarchive.
                            let archiving = !highlighted_archived;
                            match archive {
                                Some(archive_fn) => match archive_fn(&id, archiving) {
                                    Ok(()) => {
                                        if archiving && !show_archived {
                                            rows.retain(|r| r.id != id);
                                        }
                                        if let Some(reload_fn) = reload {
                                            if let Ok((fresh_rows, fresh_projects)) =
                                                reload_fn(show_archived)
                                            {
                                                rows = fresh_rows;
                                                projects = fresh_projects;
                                            }
                                        }
                                        cursor_idx = 0;
                                        scroll_offset = 0;
                                        notice = Some(if archiving {
                                            format!("Archived {id}.")
                                        } else {
                                            format!("Unarchived {id}.")
                                        });
                                    }
                                    Err(e) => {
                                        notice = Some(format!("Archive failed: {e}"));
                                    }
                                },
                                None => {
                                    notice = Some("Archiving is unavailable here.".to_string());
                                }
                            }
                        }
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Esc => {
                        // P340: an active transcript-search result clears
                        // first, then the filter, then quit (hermes ladder).
                        if search_hits.is_some() {
                            search_hits = None;
                            cursor_idx = 0;
                            scroll_offset = 0;
                            continue;
                        }
                        if filter.is_empty() {
                            return Ok(None);
                        }
                        filter.clear();
                        cursor_idx = 0;
                        scroll_offset = 0;
                    }
                    // Ctrl+J (LF) / Ctrl+M (CR) are the classic Enter
                    // equivalents — some terminal paths deliver LF.
                    KeyCode::Enter | KeyCode::Char('j') | KeyCode::Char('m')
                        if matches!(key.code, KeyCode::Enter)
                            || key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        if let Some(row) = filtered.get(cursor_idx) {
                            return Ok(Some(row.id.clone()));
                        }
                    }
                    KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // P551: scroll the details pane up one line
                        // (Ctrl+U/D move four at a time).
                        pane_scroll = pane_scroll.saturating_sub(1);
                    }
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // P551: scroll the details pane down one line.
                        pane_scroll = pane_scroll.saturating_add(1);
                    }
                    KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // P551: jump the details pane to the top.
                        pane_scroll = 0;
                    }
                    KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // P551: jump the details pane to the bottom —
                        // clamped to the last line on the next render.
                        pane_scroll = usize::MAX;
                    }
                    KeyCode::Up => {
                        // hermes: navigation wraps around the list.
                        if !filtered.is_empty() {
                            cursor_idx = if cursor_idx == 0 {
                                filtered.len() - 1
                            } else {
                                cursor_idx - 1
                            };
                        }
                    }
                    KeyCode::Down => {
                        if !filtered.is_empty() {
                            cursor_idx = (cursor_idx + 1) % filtered.len();
                        }
                    }
                    KeyCode::PageUp => {
                        cursor_idx = cursor_idx.saturating_sub(page_height);
                    }
                    KeyCode::PageDown => {
                        if !filtered.is_empty() {
                            cursor_idx = (cursor_idx + page_height).min(filtered.len() - 1);
                        }
                    }
                    KeyCode::Home => cursor_idx = 0,
                    KeyCode::End => {
                        cursor_idx = filtered.len().saturating_sub(1);
                    }
                    KeyCode::Tab => {
                        // Upgrade: cycle all → each source → all.
                        if !sources.is_empty() {
                            source_idx = (source_idx + 1) % (sources.len() + 1);
                            cursor_idx = 0;
                            scroll_offset = 0;
                        }
                    }
                    KeyCode::BackTab => {
                        // P224: Shift+Tab cycles the source filter backwards.
                        if !sources.is_empty() {
                            source_idx = if source_idx == 0 {
                                sources.len()
                            } else {
                                source_idx - 1
                            };
                            cursor_idx = 0;
                            scroll_offset = 0;
                        }
                    }
                    KeyCode::F(2) => {
                        // Upgrade: toggle recent-first ↔ alphabetical.
                        sort_alpha = !sort_alpha;
                        cursor_idx = 0;
                        scroll_offset = 0;
                    }
                    KeyCode::F(3) => {
                        // P278: toggle the conversation preview pane.
                        if preview.is_some() {
                            show_preview = !show_preview;
                        }
                    }
                    KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // P278: Ctrl+L forces a redraw (stray output, resize
                        // artifacts) — the loop repaints unconditionally.
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // P529: scroll the details pane down four lines.
                        pane_scroll = pane_scroll.saturating_add(4);
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // P529: scroll the details pane back up four lines.
                        pane_scroll = pane_scroll.saturating_sub(4);
                    }
                    KeyCode::F(1) => {
                        // P224: keybinding help overlay.
                        show_help = true;
                    }
                    KeyCode::F(4) => {
                        // P513: toggle archived sessions in the list —
                        // re-queries through the reload closure with the
                        // include_archived flag.
                        show_archived = !show_archived;
                        cursor_idx = 0;
                        scroll_offset = 0;
                        match reload {
                            Some(reload_fn) => match reload_fn(show_archived) {
                                Ok((fresh_rows, fresh_projects)) => {
                                    rows = fresh_rows;
                                    projects = fresh_projects;
                                    notice = Some(if show_archived {
                                        "Showing archived sessions (F4 hides).".to_string()
                                    } else {
                                        "Hiding archived sessions.".to_string()
                                    });
                                }
                                Err(e) => {
                                    notice = Some(format!("Reload failed: {e}"));
                                }
                            },
                            None => {
                                notice = Some("Reload is unavailable here.".to_string());
                            }
                        }
                    }
                    KeyCode::F(5) => {
                        // P224: reload the session list from disk — a live
                        // gateway may create sessions mid-browse.
                        match reload {
                            Some(reload_fn) => match reload_fn(show_archived) {
                                Ok((fresh_rows, fresh_projects)) => {
                                    let count = fresh_rows.len();
                                    rows = fresh_rows;
                                    projects = fresh_projects;
                                    cursor_idx = 0;
                                    scroll_offset = 0;
                                    notice = Some(format!("Reloaded — {count} session(s)."));
                                }
                                Err(e) => {
                                    notice = Some(format!("Reload failed: {e}"));
                                }
                            },
                            None => {
                                notice = Some("Reload is unavailable here.".to_string());
                            }
                        }
                    }
                    KeyCode::F(6) => {
                        // P512: rename the highlighted session through an
                        // inline title prompt.
                        if highlighted_id.is_some() && rename.is_some() {
                            rename_buffer.clear();
                            rename_mode = true;
                        }
                    }
                    KeyCode::F(7) => {
                        // P512: fork the highlighted session after an
                        // inline confirmation.
                        if highlighted_id.is_some() && fork.is_some() {
                            confirm_fork = true;
                        }
                    }
                    KeyCode::F(8) => {
                        // P224/P520: toggle archive on the highlighted
                        // session after an inline confirmation.
                        if highlighted_id.is_some() && archive.is_some() {
                            confirm_archive = true;
                        }
                    }
                    KeyCode::F(9) => {
                        // P340: delete the highlighted session after an
                        // inline confirmation (permanent).
                        if highlighted_id.is_some() && delete.is_some() {
                            confirm_delete = true;
                        }
                    }
                    KeyCode::F(10) => {
                        // P528: cycle the model quick-filter; Shift+F10
                        // cycles backwards (all → model → … → all).
                        if !models.is_empty() {
                            model_idx = if key.modifiers.contains(KeyModifiers::SHIFT) {
                                if model_idx == 0 {
                                    models.len()
                                } else {
                                    model_idx - 1
                                }
                            } else {
                                (model_idx + 1) % (models.len() + 1)
                            };
                            cursor_idx = 0;
                            scroll_offset = 0;
                        }
                    }
                    KeyCode::F(11) => {
                        // P585: export the highlighted session's transcript
                        // as Markdown; the footer reports the written path.
                        if let (Some(id), Some(export_fn)) = (highlighted_id.clone(), export) {
                            match export_fn(&id) {
                                Ok(path) => {
                                    notice = Some(format!("Exported to {}", path.display()));
                                }
                                Err(e) => {
                                    notice = Some(format!("Export failed: {e}"));
                                }
                            }
                        }
                    }
                    KeyCode::Char('v') if filter.is_empty() => {
                        // P651: open the full transcript of the
                        // highlighted session in a scrollable viewer.
                        if let (Some(transcript_fn), Some(row)) =
                            (transcript, filtered.get(cursor_idx))
                        {
                            match transcript_fn(&row.id) {
                                Ok(exchange) => {
                                    let width = 100usize;
                                    let mut lines: Vec<String> = Vec::new();
                                    for (role, content) in exchange {
                                        lines.push(format!("\u{2500}\u{2500} {role}"));
                                        let text = content.unwrap_or_default();
                                        if text.trim().is_empty() {
                                            lines.push("(empty)".to_string());
                                        } else {
                                            lines.extend(ulnclaw::tui_text::wrap_display_text(
                                                &text, width,
                                            ));
                                        }
                                        lines.push(String::new());
                                    }
                                    // P741: pre-render the raw trace view too
                                    // so `r` inside the viewer can swap
                                    // pretty \u{2194} raw without a reload.
                                    let mut alt: Vec<String> = Vec::new();
                                    if let Some(raw_fn) = raw_transcript {
                                        match raw_fn(&row.id) {
                                            Ok(messages) => {
                                                let raw =
                                                    ulnclaw::session::export::render_session_raw(
                                                        &messages, false,
                                                    );
                                                alt.extend(raw.lines().map(str::to_string));
                                            }
                                            Err(_) => {}
                                        }
                                    }
                                    transcript_lines = lines;
                                    transcript_alt_lines = alt;
                                    transcript_raw_mode = false;
                                    transcript_scroll = 0;
                                    transcript_title = row
                                        .title
                                        .clone()
                                        .filter(|title| !title.trim().is_empty())
                                        .unwrap_or_else(|| row.id.clone());
                                    transcript_mode = true;
                                }
                                Err(e) => {
                                    notice = Some(format!("Transcript failed: {e}"));
                                }
                            }
                        }
                    }
                    KeyCode::Char('/') if filter.is_empty() && search_hits.is_none() => {
                        // P340: open the transcript-search prompt. A `/`
                        // typed into an active filter still filters.
                        search_mode = true;
                        search_query.clear();
                    }
                    KeyCode::Backspace => {
                        if filter.pop().is_some() {
                            cursor_idx = 0;
                            scroll_offset = 0;
                        }
                    }
                    KeyCode::Char(ch) => {
                        // hermes: bare `q` with no active filter quits.
                        if ch == 'q' && filter.is_empty() {
                            return Ok(None);
                        }
                        filter.push(ch);
                        cursor_idx = 0;
                        scroll_offset = 0;
                    }
                    _ => {}
                }
            }
            // P278: terminal resize falls through to the next loop pass,
            // which recomputes the viewport from the fresh terminal size.
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

/// P562: compact duration (45s / 12m / 3h05m / 2d4h) for the browse
/// details pane.
fn browse_format_duration(total_secs: u64) -> String {
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let minutes = total_secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{}h{:02}m", hours, minutes % 60);
    }
    format!("{}d{}h", hours / 24, hours % 24)
}

/// Content lines for the browse details pane: full title, id, source,
/// project, cwd, last-active (relative + absolute local time), and the
/// wrapped first-user-message preview. All lines are pre-truncated to
/// `width` display chars.
fn browse_details_pane_lines(
    row: &ulnclaw::session::sqlite::BrowseRow,
    project: Option<&str>,
    width: usize,
    context: Option<(usize, usize)>,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let title = row
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());
    lines.extend(ulnclaw::tui_text::wrap_display_text(&title, width));
    lines.push(String::new());
    lines.extend(ulnclaw::tui_text::wrap_display_text(
        &format!("id: {}", row.id),
        width,
    ));
    lines.extend(ulnclaw::tui_text::wrap_display_text(
        &format!("source: {}", row.source),
        width,
    ));
    // P528: the session's model, when recorded.
    if let Some(model) = row.model.as_deref().filter(|m| !m.is_empty()) {
        lines.extend(ulnclaw::tui_text::wrap_display_text(
            &format!("model: {model}"),
            width,
        ));
    }
    // P542: how the session ended (when it has).
    if let Some(reason) = row.end_reason.as_deref().filter(|r| !r.is_empty()) {
        lines.extend(ulnclaw::tui_text::wrap_display_text(
            &format!("ended: {reason}"),
            width,
        ));
    }
    if row.archived {
        lines.extend(ulnclaw::tui_text::wrap_display_text(
            "status: archived",
            width,
        ));
    }
    // P524: stored message count for a size cue without opening.
    lines.extend(ulnclaw::tui_text::wrap_display_text(
        &format!("messages: {}", row.message_count),
        width,
    ));
    // P546: stored token totals, when the session has usage recorded.
    if row.total_tokens > 0 {
        lines.extend(ulnclaw::tui_text::wrap_display_text(
            &format!("tokens: {}", row.total_tokens),
            width,
        ));
    }
    // P630: live context-in-use (conversation estimate vs budget),
    // mirroring the desktop session-info row (P627).
    if let Some((used, budget)) = context {
        let line = if budget > 0 {
            let percent = ulnclaw::context::breakdown::percent_of(used, budget);
            format!("context: {used} / {budget} tokens ({percent}%)")
        } else {
            format!("context: {used} tokens")
        };
        lines.extend(ulnclaw::tui_text::wrap_display_text(&line, width));
    }
    // P562: session duration (started → last activity).
    let duration_secs = (row.last_active - row.started_at).max(0.0) as u64;
    if duration_secs > 0 {
        lines.extend(ulnclaw::tui_text::wrap_display_text(
            &format!("duration: {}", browse_format_duration(duration_secs)),
            width,
        ));
    }
    if let Some(slug) = project {
        lines.extend(ulnclaw::tui_text::wrap_display_text(
            &format!("project: {slug}"),
            width,
        ));
    }
    if let Some(cwd) = row.cwd.as_deref().filter(|c| !c.is_empty()) {
        lines.extend(ulnclaw::tui_text::wrap_display_text(
            &format!("cwd: {cwd}"),
            width,
        ));
    }
    let absolute = chrono::DateTime::from_timestamp(row.last_active as i64, 0)
        .map(|dt| {
            chrono::DateTime::<chrono::Local>::from(dt)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    let active = if absolute.is_empty() {
        format!("active: {}", relative_time(row.last_active))
    } else {
        format!(
            "active: {} \u{00B7} {}",
            relative_time(row.last_active),
            absolute
        )
    };
    lines.extend(ulnclaw::tui_text::wrap_display_text(&active, width));
    if let Some(preview) = row.preview.as_deref().filter(|p| !p.trim().is_empty()) {
        lines.push(String::new());
        lines.extend(ulnclaw::tui_text::wrap_display_text(
            &format!("first message: {preview}"),
            width,
        ));
    }
    lines
        .into_iter()
        .map(|line| line.chars().take(width).collect::<String>())
        .collect()
}

/// Plain-stdin session picker fallback for non-TTY contexts (pipes, CI):
/// numbered list, live substring filter, resume by number. Returns the
/// selected session id, or `None` when cancelled.
fn run_session_browse_stdin(
    rows: &[ulnclaw::session::sqlite::BrowseRow],
    projects: &std::collections::HashMap<String, String>,
) -> Result<Option<String>, String> {
    let mut filter = String::new();
    loop {
        let filtered: Vec<&ulnclaw::session::sqlite::BrowseRow> = rows
            .iter()
            .filter(|r| browse_row_matches(r, projects.get(&r.id).map(String::as_str), &filter))
            .collect();
        println!();
        if filter.is_empty() {
            println!(
                "Browse sessions ({} total) — type a number to resume, text to filter, q to quit",
                rows.len()
            );
        } else {
            println!(
                "Browse sessions — filter: {filter} ({}) — number to resume, Enter clears filter, q to quit",
                filtered.len()
            );
        }
        let page = &filtered[..filtered.len().min(20)];
        for (idx, row) in page.iter().enumerate() {
            println!(
                "  {:>2}. {}",
                idx + 1,
                format_browse_row(row, 50, projects.get(&row.id).map(String::as_str))
            );
        }
        if filtered.len() > page.len() {
            println!(
                "      \u{2026} {} more — type text to narrow",
                filtered.len() - page.len()
            );
        }
        print!("\n> ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let mut line = String::new();
        if std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            return Ok(None); // EOF
        }
        let input = line.trim().to_string();
        if input == "q" || input == "quit" {
            println!("Cancelled.");
            return Ok(None);
        }
        if input.is_empty() {
            filter.clear();
            continue;
        }
        if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= page.len() {
                return Ok(Some(page[n - 1].id.clone()));
            }
            println!(
                "No such entry — pick 1-{} or type text to filter.",
                page.len()
            );
            continue;
        }
        filter = input;
    }
}

/// Hermes `_relative_time` rendering for the browse picker.
fn relative_time(ts: f64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let secs = (now - ts) as i64;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Relaunch the current binary with `--resume <id>` (hermes `relaunch`);
/// the child inherits the environment (ULNCLAW_HOME, config overrides).
fn relaunch_resume(session_id: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = std::process::Command::new(&exe)
        .arg("--resume")
        .arg(session_id)
        .status()
        .map_err(|e| format!("failed to relaunch {}: {}", exe.display(), e))?;
    std::process::exit(status.code().unwrap_or(0));
}

/// Resolve a user-supplied session id or unique prefix for the id-taking
/// `sessions` actions (hermes resolve_session_id everywhere an id is
/// accepted).
fn resolve_session_or_err(store: &SqliteSessionStore, id: &str) -> Result<String, String> {
    store
        .resolve_session_id(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("session '{}' not found", id))
}

/// Shared preview/confirm/execute flow for `sessions prune` and
/// `sessions archive` (hermes sessions_cmd prune/archive branch).
fn run_session_prune(
    store: &SqliteSessionStore,
    filters: ulnclaw::session::filters::PruneFilters,
    delete: bool,
    dry_run: bool,
    yes: bool,
) -> Result<(), String> {
    use ulnclaw::session::filters::format_epoch;
    let candidates = store
        .list_prune_candidates(&filters)
        .map_err(|e| e.to_string())?;
    let verb = if delete { "Delete" } else { "Archive" };
    if candidates.is_empty() {
        println!("No sessions match ({}).", filters.describe());
        return Ok(());
    }
    // Candidates are ordered oldest-activity-first; surface the span so a
    // long-lived but recently used conversation cannot look old merely
    // because of its creation date.
    let oldest = candidates.first().map(|c| c.last_active);
    let newest = candidates.last().map(|c| c.last_active);
    let span = format!(
        "oldest activity {}, newest activity {}",
        format_epoch(oldest),
        format_epoch(newest)
    );
    if dry_run || !yes {
        let shown: Vec<_> = if dry_run {
            candidates.iter().collect()
        } else {
            candidates.iter().take(15).collect()
        };
        let shown_count = shown.len();
        println!(
            "{} session(s) match ({}; {}):",
            candidates.len(),
            filters.describe(),
            span
        );
        for candidate in shown {
            let title = candidate.title.as_deref().unwrap_or("");
            let title: String = title.chars().take(36).collect();
            let model = candidate
                .model
                .as_deref()
                .unwrap_or("-")
                .rsplit('/')
                .next()
                .unwrap_or("-");
            let model: String = model.chars().take(24).collect();
            println!(
                "  {}  {:<17} {:<10} {:<24} {:>4} msgs  {}",
                candidate.id,
                format_epoch(Some(candidate.last_active)),
                candidate.source,
                model,
                candidate.message_count,
                title
            );
        }
        if candidates.len() > shown_count {
            println!("  … and {} more", candidates.len() - shown_count);
        }
        if dry_run {
            println!(
                "Dry run — nothing {}.",
                if delete { "deleted" } else { "archived" }
            );
            return Ok(());
        }
    }
    if !yes {
        print!(
            "{} these {} session(s) ({})? [y/N] ",
            verb,
            candidates.len(),
            span
        );
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            println!("Cancelled.");
            return Ok(());
        }
    }
    let count = if delete {
        store.prune_sessions(&filters).map_err(|e| e.to_string())?
    } else {
        store
            .archive_sessions(&filters)
            .map_err(|e| e.to_string())?
    };
    if delete {
        println!("Pruned {} session(s).", count);
    } else {
        println!(
            "Archived {} session(s). They're hidden from listings but fully recoverable (nothing was deleted).",
            count
        );
    }
    Ok(())
}

fn print_moa_presets(config: &UlncLawConfig) {
    let moa = &config.moa;
    println!("Mixture of Agents presets");
    let default_name = moa
        .default_preset
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());
    println!("Default: {}", default_name);
    let mut names: Vec<&String> = moa.presets.keys().collect();
    names.sort();
    if names.is_empty() {
        println!("(none configured — add [moa.presets.<name>] to config.toml)");
    }
    for name in names {
        let preset = &moa.presets[name];
        let marker = if *name == default_name { "*" } else { " " };
        println!("\n{} {}", marker, name);
        println!("  Reference models:");
        for (idx, slot) in preset.reference_models.iter().enumerate() {
            let state = if slot.enabled { "" } else { " [disabled]" };
            println!("    {}. {}{}", idx + 1, slot.label(), state);
        }
        println!("  Aggregator: {}", preset.aggregator.label());
    }
}

async fn moa_cmd(
    config: &UlncLawConfig,
    action: MoaAction,
    config_path: Option<&str>,
) -> Result<(), String> {
    match action {
        MoaAction::List => {
            print_moa_presets(config);
        }
        MoaAction::Run { prompt, preset } => {
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                return Err("usage: ulnclaw moa run <prompt> [--preset <name>]".into());
            }
            let outcome = ulnclaw::moa::run_moa(config, &prompt, preset.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            for reference in &outcome.references {
                if reference.failed() {
                    eprintln!("  ✗ {} failed", reference.label);
                } else {
                    eprintln!("  ✓ {}", reference.label);
                }
            }
            eprintln!("  ⇢ aggregator: {}", outcome.aggregator_label);
            println!("{}", outcome.synthesis);
        }
        MoaAction::Delete { name } => {
            let mut updated = config.clone();
            if updated.moa.presets.remove(&name).is_none() {
                return Err(format!("Unknown MoA preset: {}", name));
            }
            if updated.moa.presets.is_empty() {
                return Err("Cannot delete the only MoA preset".into());
            }
            let is_default = updated
                .moa
                .default_preset
                .as_deref()
                .map(|d| d == name)
                .unwrap_or(name == "default");
            if is_default {
                let next = updated.moa.presets.keys().next().cloned();
                updated.moa.default_preset = next;
            }
            let path = config_path
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| ulnclaw::config::ulnclaw_home().join("config.toml"));
            let content =
                toml::to_string_pretty(&updated).map_err(|e| format!("serialize config: {}", e))?;
            std::fs::write(&path, content)
                .map_err(|e| format!("write {}: {}", path.display(), e))?;
            println!("Deleted MoA preset: {}", name);
            print_moa_presets(&updated);
        }
    }
    Ok(())
}

/// `ulnclaw migrate [xai] [--apply] [--no-backup]` — hermes migrate
/// parity: diagnose (and optionally rewrite) config references to
/// retired models.
fn migrate_cmd(
    migration: Option<&str>,
    apply: bool,
    no_backup: bool,
    config_override: Option<String>,
) -> Result<(), String> {
    match migration {
        None => {
            println!("usage: ulnclaw migrate <migration> [--apply] [--no-backup]");
            println!();
            println!("Available migrations:");
            println!(
                "  xai    Replace xAI models retired {} (dry-run by default)",
                ulnclaw::migrate::XAI_RETIREMENT_DATE
            );
            Ok(())
        }
        Some("xai") => {
            let path = config_override
                .map(std::path::PathBuf::from)
                .unwrap_or_else(ulnclaw::config_cmd::config_path);
            let (issues, applied) = ulnclaw::migrate::run_xai_migration(&path, apply, !no_backup)?;
            println!(
                "\u{25C6} xAI Model Retirement Migration ({})",
                ulnclaw::migrate::XAI_RETIREMENT_DATE
            );
            println!();
            if issues.is_empty() {
                println!("  \u{2713} No retired xAI models in config \u{2014} nothing to migrate.");
                return Ok(());
            }
            println!("  Found {} retired xAI model reference(s):", issues.len());
            println!();
            for issue in &issues {
                print!(
                    "    \u{26A0} {} = \"{}\" \u{2192} \"{}\"",
                    issue.path, issue.current, issue.replacement
                );
                if let Some(effort) = issue.reasoning_effort {
                    print!(
                        "  (set reasoning_effort = \"{effort}\" to keep non-reasoning behavior)"
                    );
                }
                println!();
            }
            println!();
            println!(
                "    \u{2192} Migration guide: {}",
                ulnclaw::migrate::XAI_MIGRATION_GUIDE_URL
            );
            println!();
            if apply {
                println!(
                    "  \u{2713} Rewrote {applied} reference(s) in {}{}",
                    path.display(),
                    if no_backup {
                        ""
                    } else {
                        " (backup: .toml.bak)"
                    }
                );
            } else {
                println!("  Dry-run mode \u{2014} no changes written.");
                println!(
                    "  Re-run with `ulnclaw migrate xai --apply` to rewrite {} in-place.",
                    path.display()
                );
            }
            Ok(())
        }
        Some(other) => Err(format!("Unknown migration: {other} (available: xai)")),
    }
}

fn tools_cmd(
    config: &UlncLawConfig,
    action: &str,
    name: Option<&str>,
    json: bool,
) -> Result<(), String> {
    match action {
        "list" => {
            let mut registry = ToolRegistry::new();
            register_builtin_tools(&mut registry);
            ulnclaw::plugins::register_plugin_tools(&mut registry);
            toolsets::apply_toolset_policy(
                &mut registry,
                &config.enabled_toolsets,
                &config.disabled_toolsets,
            );
            let enabled = registry.enabled_toolset_names();
            if json {
                let toolsets: Vec<serde_json::Value> = registry
                    .toolset_names()
                    .iter()
                    .map(|ts| {
                        serde_json::json!({
                            "name": ts,
                            "enabled": enabled.contains(ts),
                            "tools": registry
                                .toolset_tools(ts)
                                .iter()
                                .map(|tool| {
                                    serde_json::json!({
                                        "name": tool.definition.name,
                                        "description": tool
                                            .definition
                                            .description
                                            .lines()
                                            .next()
                                            .unwrap_or("")
                                            .trim(),
                                        "dangerous": tool.dangerous,
                                    })
                                })
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "toolsets": toolsets }))
                        .map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            println!("Toolsets:");
            for ts in registry.toolset_names() {
                let count = registry.toolset_tools(&ts).len();
                let state = if enabled.contains(&ts) {
                    "enabled"
                } else {
                    "disabled"
                };
                println!("  {:<16} {:<9} {} tools", ts, state, count);
            }
            println!("\nEnabled tools ({}):", registry.len());
            for def in registry.definitions() {
                println!("  {}", def.name);
            }
            Ok(())
        }
        "enable" | "disable" => {
            let Some(toolset) = name else {
                return Err(format!("Usage: ulnclaw tools {action} <toolset>"));
            };
            let mut probe = ToolRegistry::new();
            register_builtin_tools(&mut probe);
            let known = probe.toolset_names();
            if !known.iter().any(|n| n == toolset) && toolsets::resolve_toolset(toolset).is_empty()
            {
                return Err(format!(
                    "Unknown toolset: {toolset}\nKnown toolsets: {}",
                    known.join(", ")
                ));
            }
            let mut disabled = config.disabled_toolsets.clone();
            let mut enabled_list = config.enabled_toolsets.clone();
            if action == "disable" {
                if !disabled.iter().any(|n| n == toolset) {
                    disabled.push(toolset.to_string());
                }
            } else {
                disabled.retain(|n| n != toolset);
                if !enabled_list.is_empty() && !enabled_list.iter().any(|n| n == toolset) {
                    enabled_list.push(toolset.to_string());
                }
            }
            let render = |list: &[String]| {
                format!(
                    "[{}]",
                    list.iter()
                        .map(|n| format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            print!(
                "{}",
                ulnclaw::config_cmd::set_config_value(
                    "disabled_toolsets",
                    &render(&disabled),
                    true
                )?
            );
            if !enabled_list.is_empty() || !config.enabled_toolsets.is_empty() {
                print!(
                    "{}",
                    ulnclaw::config_cmd::set_config_value(
                        "enabled_toolsets",
                        &render(&enabled_list),
                        true
                    )?
                );
            }
            println!(
                "{} toolset '{toolset}' — restart running gateways to apply.",
                if action == "disable" {
                    "Disabled"
                } else {
                    "Enabled"
                }
            );
            Ok(())
        }
        other => Err(format!(
            "Unknown tools action: {other} (expected list, enable, or disable)"
        )),
    }
}

// ── curator: skill library curation (hermes hermes_cli/curator.py) ────────

fn curator_cmd(action: CuratorAction) -> Result<(), String> {
    use ulnclaw::{curator, skill_usage};

    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
    match action {
        CuratorAction::Status => {
            for (label, count) in curator::status_summary(&home) {
                println!("  {:<28} {}", label, count);
            }
            println!(
                "  {:<28} {}",
                "paused",
                if curator::is_paused(&home) {
                    "yes"
                } else {
                    "no"
                }
            );
            Ok(())
        }
        CuratorAction::Run { dry_run } => {
            if curator::is_paused(&home) && !dry_run {
                return Err(
                    "curator is paused — resume it first (`ulnclaw curator resume`)".to_string(),
                );
            }
            let counts = curator::apply_automatic_transitions(&home, dry_run);
            println!(
                "curator: checked={} stale={} archived={} reactivated={}{}",
                counts.checked,
                counts.marked_stale,
                counts.archived,
                counts.reactivated,
                if dry_run { " (dry-run)" } else { "" }
            );
            Ok(())
        }
        CuratorAction::Pause => {
            curator::set_paused(&home, true);
            println!("curator: paused");
            Ok(())
        }
        CuratorAction::Resume => {
            curator::set_paused(&home, false);
            println!("curator: resumed");
            Ok(())
        }
        CuratorAction::Pin { skill } => {
            skill_usage::set_pinned(&home, &skill, true);
            println!("curator: pinned '{}' (will bypass auto-transitions)", skill);
            Ok(())
        }
        CuratorAction::Unpin { skill } => {
            skill_usage::set_pinned(&home, &skill, false);
            println!("curator: unpinned '{}'", skill);
            Ok(())
        }
        CuratorAction::Archive { skill } => {
            if skill_usage::get_record(&home, &skill)
                .get("pinned")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Err(format!(
                    "'{}' is pinned — unpin first with `ulnclaw curator unpin {}`",
                    skill, skill
                ));
            }
            let (ok, message) = skill_usage::archive_skill(&home, &skill);
            println!("curator: {}", message);
            if ok {
                Ok(())
            } else {
                Err(message)
            }
        }
        CuratorAction::Restore { skill } => {
            let (ok, message) = skill_usage::restore_skill(&home, &skill);
            println!("curator: {}", message);
            if ok {
                Ok(())
            } else {
                Err(message)
            }
        }
        CuratorAction::ListArchived => {
            let names = skill_usage::list_archived_skill_names(&home);
            if names.is_empty() {
                println!("curator: no archived skills");
                return Ok(());
            }
            for name in names {
                println!("{}", name);
            }
            Ok(())
        }
        CuratorAction::Usage { sort, json } => {
            let mut rows = skill_usage::usage_report(&home);
            match sort.as_str() {
                "name" => rows.sort_by(|a, b| a.name.cmp(&b.name)),
                "recent" => rows.sort_by(|a, b| {
                    b.last_activity_at
                        .clone()
                        .unwrap_or_default()
                        .cmp(&a.last_activity_at.clone().unwrap_or_default())
                }),
                _ => rows.sort_by(|a, b| b.activity_count.cmp(&a.activity_count)),
            }
            if json {
                let payload: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "name": r.name,
                            "provenance": r.provenance,
                            "use_count": r.use_count,
                            "view_count": r.view_count,
                            "patch_count": r.patch_count,
                            "activity_count": r.activity_count,
                            "last_activity_at": r.last_activity_at,
                            "state": r.state,
                            "pinned": r.pinned,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            if rows.is_empty() {
                println!("curator: no skills found");
                return Ok(());
            }
            let agent = rows.iter().filter(|r| r.provenance == "agent").count();
            println!(
                "skills: {} total  (agent={}  user={})",
                rows.len(),
                agent,
                rows.len() - agent
            );
            println!();
            println!(
                "  {:<40}  {:<6}  {:>4}  {:>4}  {:>5}  {:>4}  last_activity",
                "skill", "origin", "use", "view", "patch", "act"
            );
            for row in &rows {
                let name: String = row.name.chars().take(40).collect();
                println!(
                    "  {:<40}  {:<6}  {:>4}  {:>4}  {:>5}  {:>4}  {}",
                    name,
                    row.provenance,
                    row.use_count,
                    row.view_count,
                    row.patch_count,
                    row.activity_count,
                    curator::fmt_ts(row.last_activity_at.as_deref())
                );
            }
            Ok(())
        }
        CuratorAction::Prune { days, dry_run, yes } => {
            if days < 1 {
                return Err(format!("--days must be >= 1 (got {})", days));
            }
            let candidates = curator::prune_candidates(&home, days);
            if candidates.is_empty() {
                println!(
                    "curator: nothing to prune (no unpinned agent-created skills idle >= {}d)",
                    days
                );
                return Ok(());
            }
            println!("curator: {} skill(s) idle >= {}d:", candidates.len(), days);
            for (name, idle) in &candidates {
                println!("  {:<40} idle {}d", name, idle);
            }
            if dry_run {
                println!("\n(dry run — no changes made)");
                return Ok(());
            }
            if !yes {
                print!("\nArchive {} skill(s)? [y/N] ", candidates.len());
                std::io::Write::flush(&mut std::io::stdout()).ok();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| e.to_string())?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("curator: aborted");
                    return Err("aborted".into());
                }
            }
            let mut archived = 0usize;
            let mut failures: Vec<(String, String)> = Vec::new();
            for (name, _) in &candidates {
                let (ok, message) = skill_usage::archive_skill(&home, name);
                if ok {
                    archived += 1;
                } else {
                    failures.push((name.clone(), message));
                }
            }
            println!("\ncurator: archived {}/{}", archived, candidates.len());
            if !failures.is_empty() {
                println!("failures:");
                for (name, message) in &failures {
                    println!("  {}: {}", name, message);
                }
                return Err("some archives failed".into());
            }
            Ok(())
        }
        CuratorAction::Adopt {
            skill,
            all_unmanaged,
            dry_run,
            yes,
        } => {
            let mut names = skill;
            if all_unmanaged {
                if !names.is_empty() {
                    return Err("pass either skill names or --all-unmanaged, not both".into());
                }
                names = skill_usage::list_unmanaged_skill_names(&home);
                if names.is_empty() {
                    println!("curator: no unmanaged skills to adopt");
                    return Ok(());
                }
            }
            if names.is_empty() {
                return Err("name a skill to adopt, or pass --all-unmanaged".into());
            }
            if dry_run {
                println!("curator: would adopt {} skill(s) (dry run):", names.len());
                for name in &names {
                    println!("  + {}", name);
                }
                return Ok(());
            }
            if all_unmanaged && !yes {
                println!(
                    "curator: adopt {} unmanaged skill(s) into curator management?",
                    names.len()
                );
                println!("  they become eligible for pruning");
                print!("  proceed? [y/N] ");
                std::io::Write::flush(&mut std::io::stdout()).ok();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| e.to_string())?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("curator: aborted");
                    return Err("aborted".into());
                }
            }
            let mut failed = 0usize;
            for name in &names {
                let (ok, message) = skill_usage::adopt_skill(&home, name);
                println!("curator: {}", message);
                if !ok {
                    failed += 1;
                }
            }
            if names.len() > 1 {
                println!("curator: adopted {}/{}", names.len() - failed, names.len());
            }
            if failed > 0 {
                Err("some adoptions failed".into())
            } else {
                Ok(())
            }
        }
        CuratorAction::ListUnmanaged => {
            let rows = skill_usage::unmanaged_report(&home);
            if rows.is_empty() {
                println!("curator: no unmanaged skills — every skill has provenance");
                return Ok(());
            }
            println!("unmanaged skills ({}):", rows.len());
            let mut sorted = rows.clone();
            sorted.sort_by(|a, b| {
                a.get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
            });
            for row in &sorted {
                let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let why = if row
                    .get("has_provenance_key")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    "created_by:null"
                } else {
                    "no marker"
                };
                let activity = row
                    .get("activity_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let last = row
                    .get("last_activity_at")
                    .and_then(|v| v.as_str())
                    .map(|ts| curator::fmt_ts(Some(ts)))
                    .unwrap_or_else(|| "never".to_string());
                println!(
                    "  {:<44} activity={:>4}  last_activity={:<14}  ({})",
                    name, activity, last, why
                );
            }
            println!(
                "\nadopt one with `ulnclaw curator adopt <name>`, or all with `ulnclaw curator adopt --all-unmanaged`"
            );
            Ok(())
        }
    }
}

// ── journey: the learning timeline (hermes hermes_cli/journey.py) ─────────

fn journey_cmd(
    action: Option<JourneyAction>,
    reveal: f64,
    play: bool,
    fps: u32,
    width: Option<usize>,
    height: Option<usize>,
    no_color: bool,
    json_flag: bool,
) -> Result<(), String> {
    use ulnclaw::learning_graph_render as render;

    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;

    match action {
        Some(JourneyAction::List { no_color }) => {
            let payload = ulnclaw::learning_graph::build_learning_graph(&home);
            let mut nodes: Vec<serde_json::Value> = payload
                .get("nodes")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if nodes.is_empty() {
                println!("No learning yet.");
                return Ok(());
            }
            nodes.sort_by_key(|n| n.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0));
            let color = !no_color && std::io::stdout().is_terminal();
            for node in &nodes {
                let glyph = if node.get("kind").and_then(|v| v.as_str()) == Some("memory") {
                    render::MEMORY_GLYPH
                } else {
                    render::SKILL_GLYPH
                };
                let id = node.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let label = node.get("label").and_then(|v| v.as_str()).unwrap_or("");
                let date = render::format_date(node.get("timestamp").and_then(|v| v.as_f64()));
                if color {
                    println!(
                        "\u{1b}[38;2;138;138;138m{}\u{1b}[0m  {} {}  \u{1b}[38;2;138;138;138m{}\u{1b}[0m",
                        id, glyph, label, date
                    );
                } else {
                    println!("{}  {} {}  {}", id, glyph, label, date);
                }
            }
            Ok(())
        }
        Some(JourneyAction::Delete { node, yes }) => {
            let detail = ulnclaw::learning_mutations::node_detail(&home, &node);
            if detail.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                println!(
                    "  {}",
                    detail
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("not found")
                );
                return Err("node not found".into());
            }
            if !yes {
                let label = detail
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&node);
                print!("  Delete '{}'? [y/N] ", label);
                std::io::Write::flush(&mut std::io::stdout()).ok();
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(|e| e.to_string())?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("  aborted");
                    return Err("aborted".into());
                }
            }
            let result = ulnclaw::learning_mutations::delete_node(&home, &node);
            println!(
                "  {}",
                result.get("message").and_then(|v| v.as_str()).unwrap_or("")
            );
            if result.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                Ok(())
            } else {
                Err("delete failed".into())
            }
        }
        Some(JourneyAction::Edit { node }) => {
            let detail = ulnclaw::learning_mutations::node_detail(&home, &node);
            if detail.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                println!(
                    "  {}",
                    detail
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("not found")
                );
                return Err("node not found".into());
            }
            let kind = detail
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("skill");
            let content = detail
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let suffix = if kind == "skill" { ".md" } else { ".txt" };
            let Some(edited) = open_in_editor(&content, suffix)? else {
                println!("  no changes");
                return Ok(());
            };
            if edited.trim() == content.trim() {
                println!("  no changes");
                return Ok(());
            }
            let result = ulnclaw::learning_mutations::edit_node(&home, &node, &edited);
            println!(
                "  {}",
                result.get("message").and_then(|v| v.as_str()).unwrap_or("")
            );
            if result.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                Ok(())
            } else {
                Err("edit failed".into())
            }
        }
        None => {
            let payload = ulnclaw::learning_graph::build_learning_graph(&home);
            if json_flag {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            let color = !no_color && std::io::stdout().is_terminal();
            let (cols, rows) = term_size(width, height);
            let nodes = payload
                .get("nodes")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if nodes == 0 {
                println!(
                    "No learning yet — use ulnclaw a while and your learned skills and memories will start mapping out here."
                );
                return Ok(());
            }
            if play {
                journey_play(&payload, cols, rows, color, fps)
            } else {
                let reveal = reveal.clamp(0.0, 1.0);
                print!(
                    "{}",
                    journey_frame_text(&payload, cols, rows, reveal, color)
                );
                Ok(())
            }
        }
    }
}

fn term_size(width: Option<usize>, height: Option<usize>) -> (usize, usize) {
    let env_cols = std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok());
    let env_lines = std::env::var("LINES").ok().and_then(|v| v.parse().ok());
    let cols = width.or(env_cols).unwrap_or(90).max(40);
    let rows = height.or(env_lines).unwrap_or(30).max(10);
    (cols, rows)
}

fn open_in_editor(initial: &str, suffix: &str) -> Result<Option<String>, String> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let Some(bin) = parts.next() else {
        return Err("no editor configured".into());
    };
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "ulnclaw-journey-{}-{}{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        suffix
    ));
    std::fs::write(&path, initial).map_err(|e| e.to_string())?;
    let status = std::process::Command::new(bin)
        .args(parts)
        .arg(&path)
        .status();
    let result = match status {
        Ok(status) if status.success() => std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|e| e.to_string()),
        Ok(_) => Err("editor exited non-zero".to_string()),
        Err(e) => Err(format!("editor failed: {}", e)),
    };
    std::fs::remove_file(&path).ok();
    result
}

/// Resolve a style run to a concrete 24-bit foreground color.
fn run_color(
    run: &ulnclaw::learning_graph_render::Run,
    palette: &std::collections::HashMap<String, String>,
) -> Option<(u8, u8, u8)> {
    use ulnclaw::learning_graph_render as render;
    let base = run
        .hex
        .clone()
        .or_else(|| palette.get(&run.style).cloned())?;
    let faded = render::fade(palette, Some(&base), run.alpha)?;
    Some(render::hex_to_rgb(&faded))
}

fn row_to_text(
    row: &[ulnclaw::learning_graph_render::Run],
    palette: &std::collections::HashMap<String, String>,
    color: bool,
) -> String {
    let mut out = String::new();
    for run in row {
        if !color {
            out.push_str(&run.text);
            continue;
        }
        match run_color(run, palette) {
            Some((r, g, b)) => {
                out.push_str(&format!(
                    "\u{1b}[38;2;{};{};{}m{}\u{1b}[0m",
                    r, g, b, run.text
                ));
            }
            None => out.push_str(&run.text),
        }
    }
    out
}

fn journey_frame_text(
    payload: &serde_json::Value,
    cols: usize,
    rows: usize,
    reveal: f64,
    color: bool,
) -> String {
    use ulnclaw::learning_graph_render as render;

    let palette = render::derive_palette("#FFD700", true);
    let legend = render::build_legend(payload);
    let categories = render::category_legend(payload, 4);
    let summary = render::build_summary(payload);
    let axis = render::axis_labels(payload);
    // Lines are pad-left(2), so content must fit in cols-2.
    let inner = cols.saturating_sub(2).max(24);
    // Reserve rows for title/legend/blank/axis/footer/labels + summary.
    let field_rows = rows.saturating_sub(10 + summary.len()).max(6);
    let frame = render::render_graph(payload, inner, field_rows, reveal);
    let count = payload
        .get("nodes")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    let mut parts: Vec<String> = Vec::new();

    if color {
        parts.push(format!(
            "\u{1b}[1;38;2;232;196;99m✦ Journey \u{1b}[0m\u{1b}[38;2;158;158;158m· learned skills & memories over time\u{1b}[0m"
        ));
    } else {
        parts.push("✦ Journey · learned skills & memories over time".to_string());
    }

    let mut legend_line = String::from("  ");
    for (i, item) in legend.iter().enumerate() {
        if i > 0 {
            legend_line.push_str("   ");
        }
        let glyph = item.get("glyph").and_then(|v| v.as_str()).unwrap_or("");
        let label = item.get("label").and_then(|v| v.as_str()).unwrap_or("");
        let style = item
            .get("style")
            .and_then(|v| v.as_str())
            .unwrap_or(render::STYLE_DIM);
        if color {
            let fake = render::Run {
                text: format!("{} ", glyph),
                style: style.to_string(),
                alpha: 1.0,
                hex: None,
            };
            legend_line.push_str(&row_to_text(&[fake], &palette, true));
            legend_line.push_str(&format!("\u{1b}[38;2;158;158;158m{}\u{1b}[0m", label));
        } else {
            legend_line.push_str(&format!("{} {}", glyph, label));
        }
    }
    parts.push(legend_line);

    if !categories.is_empty() {
        let mut cat_line = String::from("  ");
        for (i, item) in categories.iter().enumerate() {
            if i > 0 {
                cat_line.push_str("  ");
            }
            let glyph = item.get("glyph").and_then(|v| v.as_str()).unwrap_or("");
            let label = item.get("label").and_then(|v| v.as_str()).unwrap_or("");
            let hex = item.get("color").and_then(|v| v.as_str()).unwrap_or("");
            if color && !hex.is_empty() {
                let (r, g, b) = render::hex_to_rgb(hex);
                cat_line.push_str(&format!(
                    "\u{1b}[38;2;{};{};{}m{} \u{1b}[0m",
                    r, g, b, glyph
                ));
                cat_line.push_str(&format!("\u{1b}[38;2;138;138;138m{}\u{1b}[0m", label));
            } else {
                cat_line.push_str(&format!("{} {}", glyph, label));
            }
        }
        parts.push(cat_line);
    }

    parts.push(String::new());

    for row in &frame.grid {
        if row.is_empty() {
            parts.push(String::new());
        } else {
            parts.push(format!("  {}", row_to_text(row, &palette, color)));
        }
    }

    // Date axis under the field (oldest → now).
    let (start, end) = axis;
    let gap = inner
        .saturating_sub(start.chars().count())
        .saturating_sub(end.chars().count())
        .max(1);
    if color {
        parts.push(format!(
            "  \u{1b}[38;2;138;138;138m{}\u{1b}[0m{}\u{1b}[38;2;138;138;138m{}\u{1b}[0m",
            start,
            " ".repeat(gap),
            end
        ));
    } else {
        parts.push(format!("  {}{}{}", start, " ".repeat(gap), end));
    }

    let pct = (reveal * 100.0).round() as i64;
    if color {
        parts.push(format!(
            "  \u{1b}[38;2;138;138;138m◷ \u{1b}[0m\u{1b}[38;2;232;196;99m{}\u{1b}[0m   \u{1b}[38;2;138;138;138m{}/{} revealed · {}%\u{1b}[0m",
            if frame.date.is_empty() { "—" } else { &frame.date },
            frame.visible,
            count,
            pct
        ));
    } else {
        parts.push(format!(
            "  ◷ {}   {}/{} revealed · {}%",
            if frame.date.is_empty() {
                "—"
            } else {
                &frame.date
            },
            frame.visible,
            count,
            pct
        ));
    }

    if !frame.labels.is_empty() {
        parts.push(String::new());
        if color {
            parts.push("  \u{1b}[38;2;158;158;158mcharted signals\u{1b}[0m".to_string());
        } else {
            parts.push("  charted signals".to_string());
        }
        for item in frame.labels.iter().take(6) {
            let key = item.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let glyph = item.get("glyph").and_then(|v| v.as_str()).unwrap_or("");
            let label = item.get("label").and_then(|v| v.as_str()).unwrap_or("");
            let meta = item.get("meta").and_then(|v| v.as_str()).unwrap_or("");
            let style = item
                .get("style")
                .and_then(|v| v.as_str())
                .unwrap_or(render::STYLE_DIM);
            let alpha = item.get("alpha").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let meta: String = if meta.chars().count() <= 32 {
                meta.to_string()
            } else {
                meta.chars().take(29).collect::<String>() + "…"
            };
            if color {
                let fake = render::Run {
                    text: format!("{} {}", glyph, label),
                    style: style.to_string(),
                    alpha,
                    hex: None,
                };
                parts.push(format!(
                    "  \u{1b}[38;2;178;178;178m{} \u{1b}[0m{}  \u{1b}[38;2;138;138;138m{}\u{1b}[0m",
                    key,
                    row_to_text(&[fake], &palette, true),
                    meta
                ));
            } else {
                parts.push(format!("  {} {} {}  {}", key, glyph, label, meta));
            }
        }
    }

    for line in &summary {
        if color {
            parts.push(format!("  \u{1b}[38;2;158;158;158m{}\u{1b}[0m", line));
        } else {
            parts.push(format!("  {}", line));
        }
    }

    parts.join("\n") + "\n"
}

fn journey_play(
    payload: &serde_json::Value,
    cols: usize,
    rows: usize,
    color: bool,
    fps: u32,
) -> Result<(), String> {
    let frames = 42usize;
    let delay = std::time::Duration::from_secs_f64(1.0 / fps.clamp(1, 60) as f64);
    let mut out = std::io::stdout();
    if color {
        // Clear the screen once, then home the cursor per frame.
        write!(out, "\u{1b}[2J").ok();
    }
    for i in 0..frames {
        let reveal = i as f64 / (frames - 1) as f64;
        let text = journey_frame_text(payload, cols, rows, reveal, color);
        if color {
            write!(out, "\u{1b}[H{}", text).ok();
        } else {
            write!(out, "{}", text).ok();
        }
        out.flush().ok();
        std::thread::sleep(delay);
    }
    let text = journey_frame_text(payload, cols, rows, 1.0, color);
    if color {
        write!(out, "\u{1b}[H{}", text).ok();
    } else {
        write!(out, "{}", text).ok();
    }
    out.flush().ok();
    Ok(())
}

fn models_cmd(action: ModelsAction) -> Result<(), String> {
    use ulnclaw::models_dev as md;
    match action {
        ModelsAction::Providers => {
            let providers = md::list_providers(true);
            if providers.is_empty() {
                return Err(
                    "models.dev catalog unavailable (network offline and no local cache)"
                        .to_string(),
                );
            }
            println!("{:<24} {:<36} {:>7}  env", "id", "name", "models");
            for provider in providers {
                println!(
                    "{:<24} {:<36} {:>7}  {}",
                    provider.id,
                    provider.name,
                    provider.model_count,
                    provider.env.join(",")
                );
            }
            let cache = md::cache_info();
            println!(
                "\ncatalog: {} providers (age {}s, fresh={})",
                cache.providers,
                cache.age_secs.round() as u64,
                cache.fresh
            );
        }
        ModelsAction::List {
            provider,
            all,
            refresh,
        } => {
            if refresh {
                md::fetch_models_dev_opts(true, true);
            }
            let models = if all {
                md::list_provider_models(&provider)
            } else {
                md::list_agentic_models(&provider)
            };
            if models.is_empty() {
                return Err(format!(
                    "no models found for provider '{provider}' (not in the models.dev catalog?)"
                ));
            }
            for model in models {
                println!("{model}");
            }
        }
        ModelsAction::Info { provider, model } => {
            let Some(info) = md::get_model_info(&provider, &model) else {
                return Err(format!(
                    "model '{model}' not found for provider '{provider}'"
                ));
            };
            println!("{} ({})", info.name, info.id);
            println!("  provider:    {}", info.provider_id);
            if !info.family.is_empty() {
                println!("  family:      {}", info.family);
            }
            println!(
                "  limits:      context={} output={}{}",
                info.context_window,
                info.max_output,
                info.max_input
                    .map(|v| format!(" input={v}"))
                    .unwrap_or_default()
            );
            println!("  cost:        {}", info.format_cost());
            println!("  capabilities: {}", info.format_capabilities());
            if !info.input_modalities.is_empty() {
                println!(
                    "  modalities:  in={} out={}",
                    info.input_modalities.join("+"),
                    info.output_modalities.join("+")
                );
            }
            if !info.knowledge_cutoff.is_empty() {
                println!("  knowledge:   {}", info.knowledge_cutoff);
            }
            if !info.status.is_empty() {
                println!("  status:      {}", info.status);
            }
        }
        ModelsAction::Refresh => {
            md::fetch_models_dev_opts(true, true);
            let cache = md::cache_info();
            if cache.providers == 0 {
                return Err(
                    "models.dev refresh failed (see debug log); no cache available".to_string(),
                );
            }
            println!(
                "models.dev cache refreshed: {} providers (fresh={})",
                cache.providers, cache.fresh
            );
        }
    }
    Ok(())
}

async fn skills_cmd(action: SkillAction) -> Result<(), String> {
    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
    let dir = home.join("skills");
    match action {
        SkillAction::List => {
            let skills = ulnclaw::skills::list_skills(&dir);
            if skills.is_empty() {
                println!("No skills installed ({}).", dir.display());
            }
            for skill in skills {
                let skill_md =
                    std::fs::read_to_string(skill.path.join("SKILL.md")).unwrap_or_default();
                let blueprint_note = match ulnclaw::skills::blueprint::parse_blueprint(&skill_md) {
                    Ok(Some(spec)) => format!("  ⏰ {}", spec.schedule),
                    _ => String::new(),
                };
                println!("  {} — {}{}", skill.name, skill.description, blueprint_note);
            }
        }
        SkillAction::Blueprints => {
            let mut found = false;
            // hermes registers a suggestion when a blueprint skill is
            // installed; ulnclaw has no install surface, so the listing
            // registers lazily for every unscheduled blueprint (dedup
            // latching keeps it idempotent).
            let active_job_names: std::collections::HashSet<String> =
                ulnclaw::cron::CronStore::open(&home.join("state.db"))
                    .ok()
                    .and_then(|store| store.list().ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|job| job.name)
                    .collect();
            let suggestion_store = ulnclaw::cron::suggestions::SuggestionStore::open_default();
            for skill in ulnclaw::skills::list_skills(&dir) {
                let content =
                    std::fs::read_to_string(skill.path.join("SKILL.md")).unwrap_or_default();
                let Ok(Some(mut spec)) = ulnclaw::skills::blueprint::parse_blueprint(&content)
                else {
                    continue;
                };
                if spec.skill_name.is_empty() {
                    spec.skill_name = skill.name.clone();
                }
                found = true;
                println!("  {} — schedule: {}", skill.name, spec.schedule);
                if spec.deliver != "origin" {
                    println!("    deliver: {}", spec.deliver);
                }
                if let Some(ref prompt) = spec.prompt {
                    println!("    prompt: {}", prompt);
                }
                if !active_job_names.contains(&format!("blueprint:{}", spec.skill_name)) {
                    if let Some(record) = ulnclaw::skills::blueprint::register_blueprint_suggestion(
                        &suggestion_store,
                        &spec,
                    ) {
                        println!(
                            "    blueprint: '{}' is an automation — added to suggestions; run /suggestions to schedule or dismiss it ({})",
                            spec.skill_name, record.id
                        );
                    }
                }
            }
            if !found {
                println!("No blueprint skills installed (add `metadata.hermes.blueprint.schedule` to a SKILL.md frontmatter).");
            }
        }
        SkillAction::Schedule { name, job_name } => {
            let Some(spec) = ulnclaw::skills::blueprint::blueprint_spec_for_installed(&dir, &name)
            else {
                return Err(format!(
                    "skill '{}' is not an installed blueprint (no metadata.hermes.blueprint block)",
                    name
                ));
            };
            let job = ulnclaw::skills::blueprint::blueprint_to_job(&spec, job_name.as_deref())
                .map_err(|e| e.to_string())?;
            let store = ulnclaw::cron::CronStore::open(&home.join("state.db"))
                .map_err(|e| e.to_string())?;
            store.add(&job).map_err(|e| e.to_string())?;
            println!(
                "scheduled blueprint '{}' as job '{}' ({})",
                name, job.name, job.schedule
            );
        }
        SkillAction::Unschedule { name } => {
            let store = ulnclaw::cron::CronStore::open(&home.join("state.db"))
                .map_err(|e| e.to_string())?;
            let wanted = format!("blueprint:{}", name);
            let jobs = store.list().map_err(|e| e.to_string())?;
            let matches: Vec<_> = jobs.into_iter().filter(|job| job.name == wanted).collect();
            if matches.is_empty() {
                return Err(format!("no cron job named '{}' found", wanted));
            }
            for job in matches {
                store.remove(&job.id).map_err(|e| e.to_string())?;
                println!("removed job {} ({})", job.id, job.name);
            }
        }
        SkillAction::Scan {
            name,
            source,
            json,
            force,
        } => {
            let Some(skill_dir) = ulnclaw::skills::guard::find_skill_dir(&dir, &name) else {
                return Err(format!("skill '{}' not found in {}", name, dir.display()));
            };
            let result = ulnclaw::skills::guard::scan_skill(&skill_dir, &source);
            if json {
                let mut value = serde_json::to_value(&result).map_err(|e| e.to_string())?;
                let (allowed, reason) =
                    ulnclaw::skills::guard::should_allow_install(&result, force);
                value["decision"] = serde_json::json!({
                    "allowed": allowed,
                    "reason": reason,
                    "scanner": ulnclaw::skills::guard::SCANNER_VERSION,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
                );
            } else {
                println!("{}", ulnclaw::skills::guard::format_scan_report(&result));
            }
            let (allowed, _) = ulnclaw::skills::guard::should_allow_install(&result, force);
            if allowed == Some(false) {
                return Err("scan blocked the skill".to_string());
            }
        }
        SkillAction::View { name } => {
            let Some(skill) = ulnclaw::skills::find_skill(&dir, &name) else {
                return Err(format!("skill '{}' not found", name));
            };
            let content =
                std::fs::read_to_string(skill.path.join("SKILL.md")).map_err(|e| e.to_string())?;
            println!("{}", content);
            for file in ulnclaw::skills::linked_files(&skill.path) {
                println!("  + {}", file);
            }
        }
    }
    Ok(())
}

fn bundles_cmd(action: BundlesAction) -> Result<(), String> {
    use ulnclaw::bundles;
    match action {
        BundlesAction::List => {
            let list = bundles::list_bundles();
            if list.is_empty() {
                println!(
                    "No bundles installed yet. Create one with:\n  \
                     ulnclaw bundles create <name> --skill skill1 --skill skill2"
                );
                println!("Bundles directory: {}", bundles::bundles_dir().display());
                return Ok(());
            }
            println!("Skill Bundles ({})", list.len());
            for info in &list {
                println!(
                    "  /{:<20} {:<24} {:>2} skills  {}",
                    info.slug,
                    info.name,
                    info.skills.len(),
                    info.description
                );
            }
            println!("Bundles directory: {}", bundles::bundles_dir().display());
        }
        BundlesAction::Show { name } => {
            let Some(info) = bundles::get_bundle(&name) else {
                return Err(format!("Bundle {name:?} not found."));
            };
            println!("/{}  {}", info.slug, info.name);
            if !info.description.is_empty() {
                println!("  {}", info.description);
            }
            println!("  File: {}", info.path.display());
            println!("  Skills ({}):", info.skills.len());
            for skill in &info.skills {
                println!("    - {}", skill);
            }
            if !info.instruction.is_empty() {
                println!("  Instruction: {}", info.instruction);
            }
        }
        BundlesAction::Create {
            name,
            skills,
            description,
            instruction,
            overwrite,
        } => {
            let path = bundles::save_bundle(
                &name,
                &skills,
                description.as_deref().unwrap_or(""),
                instruction.as_deref().unwrap_or(""),
                overwrite,
            )?;
            println!("✓ Saved bundle '{}' -> {}", name, path.display());
        }
        BundlesAction::Delete { name } => {
            let path = bundles::delete_bundle(&name)?;
            println!("✓ Deleted bundle '{}' ({})", name, path.display());
        }
        BundlesAction::Reload => {
            let before = bundles::scan_bundles();
            let diff = bundles::reload_diff(&before);
            // A one-shot CLI rescans from disk, so the diff is against the
            // snapshot taken moments earlier; report the live total.
            println!(
                "Reloaded bundles directory: {}",
                bundles::bundles_dir().display()
            );
            println!("Total bundles: {}", diff.total);
        }
    }
    Ok(())
}

async fn cron_cmd(config: &UlncLawConfig, action: CronAction) -> Result<(), String> {
    let home = ulnclaw::config::ensure_home().map_err(|e| e.to_string())?;
    let store =
        ulnclaw::cron::CronStore::open(&home.join("state.db")).map_err(|e| e.to_string())?;
    match action {
        CronAction::List => {
            let jobs = store.list().map_err(|e| e.to_string())?;
            if jobs.is_empty() {
                println!("No cron jobs.");
            }
            for job in jobs {
                println!(
                    "{}  [{}]  {}  next={:?}  enabled={}",
                    job.id, job.schedule, job.name, job.next_run, job.enabled
                );
            }
        }
        CronAction::Remove { id } => {
            store.remove(&id).map_err(|e| e.to_string())?;
            println!("removed {}", id);
        }
        CronAction::Pause { id } => {
            let Some(mut job) = store.get(&id).map_err(|e| e.to_string())? else {
                return Err(format!("job '{}' not found", id));
            };
            job.enabled = false;
            store.update(&job).map_err(|e| e.to_string())?;
            println!("paused {}", id);
        }
        CronAction::Resume { id } => {
            let Some(mut job) = store.get(&id).map_err(|e| e.to_string())? else {
                return Err(format!("job '{}' not found", id));
            };
            job.enabled = true;
            if let Ok(schedule) = ulnclaw::cron::parse_schedule(&job.schedule) {
                job.next_run = ulnclaw::cron::next_run(&schedule);
            }
            store.update(&job).map_err(|e| e.to_string())?;
            println!("resumed {}", id);
        }
        CronAction::Run { id } => {
            let Some(mut job) = store.get(&id).map_err(|e| e.to_string())? else {
                return Err(format!("no cron job named '{}' found", id));
            };
            if job.prompt.trim().is_empty() {
                return Err("job has no prompt to run".into());
            }
            // Unattended execution: the agent runs inside the cron
            // approval scope (`approvals.cron_mode` applies).
            use ulnclaw::tools::context::CronRunner;
            let agent = make_agent(config, false, None, None).await?;
            let started = std::time::Instant::now();
            match agent.run_prompt(&job.prompt, &job.skills).await {
                Ok(answer) => {
                    job.last_run = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs_f64())
                            .unwrap_or(0.0),
                    );
                    job.last_status =
                        Some(format!("ok (manual run, {}s)", started.elapsed().as_secs()));
                    store.update(&job).map_err(|e| e.to_string())?;
                    println!("{}", answer);
                }
                Err(e) => {
                    job.last_status = Some(format!("error: {}", e));
                    store.update(&job).ok();
                    return Err(e.to_string());
                }
            }
        }
        CronAction::Create {
            name,
            schedule,
            prompt,
            deliver,
            skills,
            repeat,
        } => {
            // P661: hermes `cron create` parity.
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err("name must not be empty".into());
            }
            let schedule = schedule.trim().to_string();
            let parsed = ulnclaw::cron::parse_schedule(&schedule)
                .map_err(|e| format!("invalid schedule '{}': {}", schedule, e))?;
            if let Some(repeat) = repeat {
                if repeat < 1 {
                    return Err("repeat must be a positive integer".into());
                }
            }
            let deliver = deliver.as_deref().map(|raw| {
                ulnclaw::cron::delivery::normalize_deliver_value(Some(&serde_json::Value::String(
                    raw.to_string(),
                )))
            });
            let job = ulnclaw::cron::CronJob {
                id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
                name,
                schedule,
                prompt,
                skills: skills
                    .as_deref()
                    .map(|raw| {
                        raw.split(',')
                            .map(str::trim)
                            .filter(|skill| !skill.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                enabled: true,
                repeat,
                next_run: ulnclaw::cron::next_run(&parsed),
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
                last_run: None,
                last_status: None,
                deliver,
                origin: None,
                last_delivery_error: None,
                attach_to_session: None,
            };
            let id = job.id.clone();
            store.add(&job).map_err(|e| e.to_string())?;
            println!("created cron job {id}");
        }
        CronAction::Show { id } => {
            // P661: hermes `cron show` parity.
            let Some(job) = store.get(&id).map_err(|e| e.to_string())? else {
                return Err(format!("job '{}' not found", id));
            };
            println!("id:       {}", job.id);
            println!("name:     {}", job.name);
            println!("schedule: {}", job.schedule);
            println!("enabled:  {}", job.enabled);
            if let Some(repeat) = job.repeat {
                println!("repeat:   {repeat} run(s) remaining");
            }
            if let Some(deliver) = &job.deliver {
                println!("deliver:  {deliver}");
            }
            if !job.skills.is_empty() {
                println!("skills:   {}", job.skills.join(", "));
            }
            if let Some(next) = job.next_run {
                println!("next:     {next}");
            }
            if let Some(last) = job.last_run {
                println!("last:     {last}");
            }
            if let Some(status) = &job.last_status {
                println!("status:   {status}");
            }
            if let Some(err) = &job.last_delivery_error {
                println!("deliver-error: {err}");
            }
            println!("prompt:");
            println!("{}", job.prompt);
        }
        CronAction::Blueprints { name, values } => {
            // P661: hermes `cron blueprints` parity — the catalog or a
            // direct fill+create; guided setup lives in the REPL
            // /blueprint command (it needs an agent turn).
            let mut args = String::new();
            if let Some(name) = name {
                args.push_str(&name);
                for value in values {
                    args.push(' ');
                    args.push_str(&value);
                }
            }
            let result = ulnclaw::cron::blueprints::handle_blueprint_command(&args);
            println!("{}", result.text);
            if result.agent_seed.is_some() {
                println!("(guided setup needs a chat — run /blueprint {args} in the REPL)");
            }
        }
    }
    Ok(())
}

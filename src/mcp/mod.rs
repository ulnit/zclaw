//! MCP client — port of the core of hermes' tools/mcp_tool.py
//!
//! Model Context Protocol (stdio transport) client: spawns the server
//! process, performs `initialize`, discovers tools via `tools/list`, and
//! proxies `tools/call`. Discovered tools are registered into the tool
//! registry as `mcp__<server>__<tool>`.

use crate::error::{AgentError, Result};
use crate::tools::{tool, ToolContext, ToolRegistry};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
pub mod osv;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

pub mod dashboard_oauth;
pub mod oauth;
pub mod oauth_manager;
pub mod remote;
pub mod schema_cache;

/// Configuration for one MCP server (from config.toml [[mcp.servers]]).
///
/// Stdio servers set `command` (+ args/env); remote servers set `url`
/// (and optionally `transport = "sse"` for the pre-2025-03-26 protocol,
/// plus static `headers` such as `Authorization`). Hermes parity —
/// `mcp_tool.py` accepts both shapes.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Remote server URL — when set, the server speaks Streamable HTTP
    /// (or SSE with `transport = "sse"`) instead of stdio.
    #[serde(default)]
    pub url: Option<String>,
    /// `"sse"` selects the SSE transport; anything else (absent) uses
    /// Streamable HTTP for `url` servers.
    #[serde(default)]
    pub transport: Option<String>,
    /// Static headers sent on every remote request (hermes
    /// `mcp_servers.<name>.headers`, e.g. bearer tokens).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// `"oauth"` = browser-based OAuth 2.1 + PKCE instead of a static
    /// bearer header (hermes `mcp_servers.<name>.auth`).
    #[serde(default)]
    pub auth: Option<String>,
    /// OAuth settings when `auth = "oauth"` (all optional; hermes
    /// `mcp_servers.<name>.oauth`).
    #[serde(default)]
    pub oauth: oauth::McpOAuthConfig,
    /// Lazy startup (hermes `mcp_servers.<name>.lazy`): when a matching
    /// schema-cache entry exists, register the tools WITHOUT spawning the
    /// server; the child is started on first tool call.
    #[serde(default)]
    pub lazy: bool,
    /// Enabled flag (hermes `PUT /api/mcp/servers/{name}/enabled`);
    /// disabled servers are skipped at registration. Defaults true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Stdio subprocess environment filtering (hermes mcp_tool.py
// `_SAFE_ENV_KEYS` / `_build_safe_env` / `_interpolate_env_vars`)
// ---------------------------------------------------------------------------

/// Baseline variables that never carry secrets (hermes `_SAFE_ENV_KEYS`).
const SAFE_ENV_KEYS: &[&str] = &[
    "PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "SHELL", "TMPDIR",
];

/// Windows process/location vars needed by launcher-style tools; no secrets
/// (hermes `_SAFE_ENV_KEYS_CASE_INSENSITIVE`).
const SAFE_ENV_KEYS_CASE_INSENSITIVE: &[&str] = &[
    "ALLUSERSPROFILE",
    "APPDATA",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "COMPUTERNAME",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PUBLIC",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERDOMAIN",
    "USERNAME",
    "USERPROFILE",
    "WINDIR",
];

/// A variable is "secret-source tagged" when ulnclaw explicitly injected it
/// via the dotenv layer (`~/.ulnclaw/.env`) — the hermes exception for
/// credentials the user configured a backend to provide (hermes
/// `get_secret_source` tagging during dotenv loading).
fn is_secret_source_var(key: &str) -> bool {
    let env_file = crate::config::ulnclaw_home().join(".env");
    crate::config::load_env_file(&env_file).contains_key(key)
}

/// Build the filtered environment for a stdio MCP subprocess (hermes
/// `_build_safe_env`): safe baseline + XDG_* + dotenv-tagged secrets from
/// the process environment, then the server's declared `env` on top.
/// Prevents leaking unrelated API keys/tokens to MCP servers.
pub fn build_safe_env(user_env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for (key, value) in std::env::vars() {
        let pass = SAFE_ENV_KEYS.contains(&key.as_str())
            || SAFE_ENV_KEYS_CASE_INSENSITIVE.contains(&key.to_uppercase().as_str())
            || key.starts_with("XDG_")
            || is_secret_source_var(&key);
        if pass {
            env.insert(key, value);
        }
    }
    for (key, value) in user_env {
        env.insert(key.clone(), interpolate_env_value(value));
    }
    env
}

/// Resolve `${VAR}` / Cursor-style `${env:VAR}` placeholders in one config
/// value (hermes `_interpolate_env_vars` for strings): secret scope first,
/// then the process environment; unset vars keep the literal placeholder.
pub fn interpolate_env_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let mut name = after[..end].trim();
                if let Some(stripped) = name.strip_prefix("env:") {
                    name = stripped.trim();
                }
                let resolved = crate::secret_scope::get_secret_lenient(name, None)
                    .filter(|v| !v.is_empty())
                    .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()));
                match resolved {
                    Some(v) => out.push_str(&v),
                    None => {
                        // Unset vars keep the literal placeholder (hermes).
                        out.push_str(&rest[start..start + 2 + end + 1]);
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// A running MCP server connection (stdio JSON-RPC).
pub struct McpClient {
    child: Child,
    stdin: tokio::process::ChildStdin,
    pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Result<Value>>>>>,
    next_id: Arc<Mutex<u64>>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
}

impl McpClient {
    /// Spawn the server and run the initialize handshake.
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        let command = interpolate_env_value(&config.command);
        let mut cmd = Command::new(&command);
        cmd.args(config.args.iter().map(|a| interpolate_env_value(a)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Filtered environment (hermes `_build_safe_env`): safe baseline +
        // dotenv-tagged secrets + the server's declared env only — never
        // the full parent environment.
        cmd.env_clear();
        for (key, value) in build_safe_env(&config.env) {
            cmd.env(key, value);
        }
        // Parent-death watchdog (hermes `mcp_stdio_watchdog.py`, P235):
        // if ulnclaw dies hard (kill -9 / crash), the kernel kills the
        // stdio MCP child instead of leaving it orphaned and racing the
        // next startup for the same upstream session. Linux-only —
        // macOS/Windows keep hermes' graceful-exit reaping.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| AgentError::Tool(format!("spawn MCP server '{}': {}", config.name, e)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Tool("MCP stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Tool("MCP stdout missing".into()))?;

        let pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();

        let reader_handle = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let (Some(id), Some(result)) = (
                    message.get("id").and_then(|v| v.as_u64()),
                    message.get("result").cloned(),
                ) {
                    let mut pending = pending_clone.lock().await;
                    if let Some(sender) = pending.remove(&id) {
                        sender.send(Ok(result)).ok();
                    }
                } else if let (Some(id), Some(error)) = (
                    message.get("id").and_then(|v| v.as_u64()),
                    message.get("error").cloned(),
                ) {
                    let mut pending = pending_clone.lock().await;
                    if let Some(sender) = pending.remove(&id) {
                        sender
                            .send(Err(AgentError::Tool(format!("MCP error: {}", error))))
                            .ok();
                    }
                }
            }
        });

        let mut client = Self {
            child,
            stdin,
            pending,
            next_id: Arc::new(Mutex::new(1)),
            reader_handle: Some(reader_handle),
        };

        // Initialize handshake.
        let init = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "ulnclaw", "version": crate::VERSION}
        });
        let result = client.request("initialize", init).await?;
        let _server_info = result.get("serverInfo").cloned();
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&message)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| AgentError::Tool(format!("MCP write: {}", e)))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| AgentError::Tool(format!("MCP flush: {}", e)))?;
        match tokio::time::timeout(std::time::Duration::from_secs(30), receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AgentError::Tool("MCP channel closed".into())),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(AgentError::Tool(format!("MCP '{}' timed out", method)))
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let mut line = serde_json::to_string(&message)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| AgentError::Tool(format!("MCP write: {}", e)))?;
        Ok(())
    }

    /// List the server's tools.
    pub async fn list_tools(&mut self) -> Result<Vec<Value>> {
        let result = self.request("tools/list", json!({})).await?;
        Ok(result
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Call a tool on the server.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    /// Shut down the server process.
    pub async fn close(&mut self) {
        self.child.kill().await.ok();
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.child.start_kill().ok();
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}

/// Either MCP transport behind one interface.
pub enum AnyMcpClient {
    Stdio(McpClient),
    Remote(remote::RemoteMcpClient),
}

impl AnyMcpClient {
    pub async fn list_tools(&mut self) -> Result<Vec<Value>> {
        match self {
            AnyMcpClient::Stdio(c) => c.list_tools().await,
            AnyMcpClient::Remote(c) => c.list_tools().await,
        }
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        match self {
            AnyMcpClient::Stdio(c) => c.call_tool(name, arguments).await,
            AnyMcpClient::Remote(c) => c.call_tool(name, arguments).await,
        }
    }

    pub async fn close(&mut self) {
        match self {
            AnyMcpClient::Stdio(c) => c.close().await,
            AnyMcpClient::Remote(c) => c.close().await,
        }
    }
}

/// Connection test for one server (hermes `POST
/// /api/mcp/servers/{name}/test`): connect, list tools, close, and
/// report the discovered tool names.
pub async fn test_server(config: &McpServerConfig) -> Result<Vec<String>> {
    let mut client = connect_any(config).await?;
    let tools = client.list_tools().await?;
    let names = tools
        .iter()
        .filter_map(|tool| {
            tool.get("name")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .collect();
    client.close().await;
    Ok(names)
}

/// Connect one server using the transport its config selects.
async fn connect_any(config: &McpServerConfig) -> Result<AnyMcpClient> {
    if let Some(url) = &config.url {
        let mut bearer: Option<String> = None;
        let mut reauth: Option<remote::ReauthFn> = None;
        let mut token_watch: Option<remote::TokenWatchFn> = None;
        if config.auth.as_deref() == Some("oauth") {
            // OAuth 2.1 + PKCE (hermes mcp_oauth.py): initial token now,
            // recovery hook on 401. Full flows need a human; unattended
            // contexts (cron) fail fast unless cached/refreshable tokens
            // suffice.
            let home = crate::config::ulnclaw_home();
            let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin())
                && !crate::agent::is_cron_context();
            bearer = Some(
                oauth::get_access_token(&home, &config.name, url, &config.oauth, interactive)
                    .await?,
            );
            // Baseline the disk-watch watermark at the token we just
            // obtained (hermes MCPOAuthManager last_mtime_ns seeding).
            oauth_manager::manager().note_mtime(&home, &config.name);

            let hook_home = home.clone();
            let hook_name = config.name.clone();
            let hook_url = url.clone();
            let hook_cfg = config.oauth.clone();
            // 401 recovery through the process-wide OAuth manager: dedupe
            // by failed token + external-refresh short-circuit (hermes
            // MCPOAuthManager.handle_401).
            reauth = Some(Arc::new(move |failed_token: Option<String>| {
                let home = hook_home.clone();
                let name = hook_name.clone();
                let url = hook_url.clone();
                let cfg = hook_cfg.clone();
                Box::pin(async move {
                    let rec_home = home.clone();
                    let rec_name = name.clone();
                    let rec_url = url.clone();
                    let rec_cfg = cfg.clone();
                    oauth_manager::manager()
                        .handle_401(&home, &name, failed_token.as_deref(), move || async move {
                            oauth::recover_token(
                                &rec_home,
                                &rec_name,
                                &rec_url,
                                &rec_cfg,
                                interactive,
                            )
                            .await
                        })
                        .await
                })
            }));

            // Pre-request disk watch: one stat() per request picks up
            // tokens refreshed by another process (hermes
            // HermesMCPOAuthProvider pre-flow reload).
            let watch_home = home.clone();
            let watch_name = config.name.clone();
            token_watch = Some(Arc::new(move || {
                let home = watch_home.clone();
                let name = watch_name.clone();
                Box::pin(async move {
                    if oauth_manager::manager().invalidate_if_disk_changed(&home, &name) {
                        if let Some(tokens) = oauth::load_tokens(&home, &name) {
                            if tokens.is_valid() {
                                return Some(tokens.access_token);
                            }
                        }
                    }
                    None
                })
            }));
        }
        let client = remote::RemoteMcpClient::connect(
            url,
            config.transport.as_deref(),
            &config.headers,
            bearer,
            reauth,
            token_watch,
        )
        .await?;
        return Ok(AnyMcpClient::Remote(client));
    }
    // OSV malware preflight for npx/uvx-launched servers (hermes
    // `tools/osv_check.py`): only confirmed MAL-* advisories block.
    if let Some(reason) = osv::check_package_for_malware(&config.command, &config.args).await {
        return Err(AgentError::config(format!(
            "MCP server '{}' refused: {}",
            config.name, reason
        )));
    }
    let client = McpClient::connect(config).await?;
    Ok(AnyMcpClient::Stdio(client))
}

/// Register all tools of one MCP server into the registry.
/// Tool names are prefixed `mcp__<server>__`.
pub async fn register_mcp_server(
    registry: &mut ToolRegistry,
    config: &McpServerConfig,
) -> Result<usize> {
    let mut client = connect_any(config).await?;
    let tools = client.list_tools().await?;
    let client = Arc::new(Mutex::new(client));
    let mut count = 0usize;
    let mut cache_payload: Vec<Value> = Vec::new();
    for tool_def in &tools {
        let Some(remote_name) = tool_def.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        cache_payload.push(json!({
            "name": remote_name,
            "description": tool_def.get("description").and_then(|v| v.as_str()).unwrap_or(""),
            "inputSchema": tool_def.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
        }));
        let description = tool_def
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("(no description)")
            .to_string();
        let parameters = tool_def
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        let qualified = format!("mcp__{}__{}", config.name, remote_name);
        let remote_name = remote_name.to_string();
        let client = client.clone();
        registry.register(
            tool(qualified)
                .description(format!("[MCP {}] {}", config.name, description))
                .parameters(parameters)
                .handler(move |args, _ctx: Arc<ToolContext>| {
                    let client = client.clone();
                    let remote_name = remote_name.clone();
                    async move {
                        let mut client = client.lock().await;
                        let result = client.call_tool(&remote_name, args).await?;
                        // MCP results: {content: [{type: text, text}], isError}
                        if result
                            .get("isError")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            let text = result
                                .pointer("/content/0/text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown MCP error");
                            return Ok(json!({"success": false, "error": text}));
                        }
                        Ok(result)
                    }
                })
                .toolset(format!("mcp:{}", config.name))
                .emoji("🔗")
                .build()?,
        );
        count += 1;
    }
    // Write-through schema cache (hermes mcp_tool.py #56832 +
    // mcp_schema_cache.py): refresh the on-disk manifest after a live
    // connect so the next startup can lazily register this server without
    // spawning it. Cache failures never break registration.
    if count > 0 {
        if let Err(e) = schema_cache::write_cache_entry(
            &config.name,
            &schema_cache::config_fingerprint(config),
            &cache_payload,
            &[],
        ) {
            eprintln!("[mcp] {}: schema cache write failed: {}", config.name, e);
        }
    }
    Ok(count)
}

/// Shared state of one lazily-registered MCP server (hermes
/// `_lazy_server_configs` + the lazy branch of
/// `_get_connected_server_for_call`).
struct LazyServer {
    config: McpServerConfig,
    /// Connected client, spawned on first use.
    client: Mutex<Option<AnyMcpClient>>,
    /// Tool names registered from the cache manifest (for reconciliation).
    cached_names: Mutex<Vec<String>>,
    /// Live tool names discovered on first connect (None until connected).
    live_names: Mutex<Option<std::collections::BTreeSet<String>>>,
}

static LAZY_SERVERS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Arc<LazyServer>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Register one MCP server, lazily when possible (hermes
/// `_register_from_cache_sync` + `_resolve_server_lazy`).
///
/// With `lazy = true` and a schema-cache entry whose fingerprint matches
/// the current config, the tools are registered from the cached manifest
/// and the server process is only spawned on the first tool call.
/// Otherwise this falls back to the eager `register_mcp_server` (which
/// write-through-fills the cache for the next startup).
pub async fn register_mcp_server_lazy(
    registry: &mut ToolRegistry,
    config: &McpServerConfig,
) -> Result<usize> {
    if !config.lazy {
        return register_mcp_server(registry, config).await;
    }
    let fingerprint = schema_cache::config_fingerprint(config);
    let Some(entry) = schema_cache::get_cached_entry(&config.name, &fingerprint) else {
        return register_mcp_server(registry, config).await;
    };
    let state = Arc::new(LazyServer {
        config: config.clone(),
        client: Mutex::new(None),
        cached_names: Mutex::new(Vec::new()),
        live_names: Mutex::new(None),
    });
    let mut count = 0usize;
    for raw in schema_cache::tools_from_cache_entry(&entry) {
        let Some(remote_name) = raw.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let description = raw
            .get("description")
            .and_then(|v| v.as_str())
            .filter(|d| !d.is_empty())
            .unwrap_or("(no description)")
            .to_string();
        let parameters = raw
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        let qualified = format!("mcp__{}__{}", config.name, remote_name);
        let remote_name = remote_name.to_string();
        state.cached_names.lock().await.push(remote_name.clone());
        let state = state.clone();
        registry.register(
            tool(qualified)
                .description(format!("[MCP {}] {}", config.name, description))
                .parameters(parameters)
                .handler(move |args, _ctx: Arc<ToolContext>| {
                    let state = state.clone();
                    let remote_name = remote_name.clone();
                    async move { lazy_call(state, &remote_name, args).await }
                })
                .toolset(format!("mcp:{}", config.name))
                .emoji("🔗")
                .build()?,
        );
        count += 1;
    }
    if count > 0 {
        LAZY_SERVERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(config.name.clone(), state);
        eprintln!(
            "[mcp] {} (lazy): registered {} tool(s) from schema cache",
            config.name, count
        );
    }
    Ok(count)
}

/// First-use connect + call for a lazily-registered MCP tool (hermes
/// `_ensure_lazy_server_connected`).
async fn lazy_call(state: Arc<LazyServer>, remote_name: &str, args: Value) -> Result<Value> {
    let mut guard = state.client.lock().await;
    if guard.is_none() {
        // connect_any runs the OSV malware preflight for stdio servers —
        // it guards the spawn, so it happens here where the spawn
        // actually happens (hermes lazy-connect ordering).
        eprintln!("[mcp] {}: lazy start on first use", state.config.name);
        let mut client = connect_any(&state.config).await?;
        let live_tools = client.list_tools().await?;
        let live_names: std::collections::BTreeSet<String> = live_tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let cached_names: std::collections::BTreeSet<String> =
            state.cached_names.lock().await.iter().cloned().collect();
        if live_names != cached_names {
            // Stale manifest: refresh the on-disk entry from the live list
            // so the next startup registers the right set. Phantom tools
            // stay registered for this process (ulnclaw handlers hold no
            // registry handle to deregister mid-run; hermes deregisters
            // them) but fail fast below.
            let payload: Vec<Value> = live_tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.get("name").cloned().unwrap_or(Value::Null),
                        "description": t.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                        "inputSchema": t.get("inputSchema").cloned().unwrap_or_else(|| json!({})),
                    })
                })
                .collect();
            if let Err(e) = schema_cache::write_cache_entry(
                &state.config.name,
                &schema_cache::config_fingerprint(&state.config),
                &payload,
                &[],
            ) {
                eprintln!(
                    "[mcp] {}: schema cache refresh failed: {}",
                    state.config.name, e
                );
            } else {
                eprintln!(
                    "[mcp] {}: live tool list differs from schema cache; cache refreshed",
                    state.config.name
                );
            }
        }
        *state.live_names.lock().await = Some(live_names);
        *guard = Some(client);
    }
    if let Some(live) = state.live_names.lock().await.as_ref() {
        if !live.contains(remote_name) {
            return Err(AgentError::Tool(format!(
                "MCP server '{}' no longer provides tool '{}' (stale cached schema; cache refreshed on connect)",
                state.config.name, remote_name
            )));
        }
    }
    let client = guard.as_mut().expect("connected above");
    let result = client.call_tool(remote_name, args).await?;
    // MCP results: {content: [{type: text, text}], isError}
    if result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let text = result
            .pointer("/content/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown MCP error");
        return Ok(json!({"success": false, "error": text}));
    }
    Ok(result)
}

/// Outcome of an MCP reload (hermes `_reload_mcp` change report).
#[derive(Debug, Default)]
pub struct ReloadReport {
    pub reconnected: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub tool_count: usize,
}

/// Reload MCP servers: drop every registered `mcp:*` tool, then reconnect
/// from the (freshly loaded) config (hermes `_reload_mcp`). The registry
/// is read live on every LLM call, so the rebuilt surface takes effect on
/// the next turn without a snapshot refresh.
pub async fn reload_mcp_servers(
    registry: &mut ToolRegistry,
    config: &crate::config::UlncLawConfig,
) -> ReloadReport {
    let mut report = ReloadReport::default();

    // Capture old server names, then unregister all mcp tools.
    let old_servers: Vec<String> = registry
        .toolset_names()
        .into_iter()
        .filter(|t| t.starts_with("mcp:"))
        .map(|t| t.trim_start_matches("mcp:").to_string())
        .collect();
    for toolset in registry
        .toolset_names()
        .into_iter()
        .filter(|t| t.starts_with("mcp:"))
    {
        let names: Vec<String> = registry
            .toolset_tools(&toolset)
            .into_iter()
            .map(|t| t.definition.name.clone())
            .collect();
        for name in names {
            registry.unregister(&name);
        }
    }

    // Reconnect from the fresh config (disabled servers stay down).
    let mut connected: Vec<String> = Vec::new();
    for server in config.mcp.servers.iter().filter(|server| server.enabled) {
        match register_mcp_server_lazy(registry, server).await {
            Ok(count) => {
                report.tool_count += count;
                connected.push(server.name.clone());
            }
            Err(e) => eprintln!("[mcp] {}: unavailable ({})", server.name, e),
        }
    }

    for name in &connected {
        if old_servers.contains(name) {
            report.reconnected.push(name.clone());
        } else {
            report.added.push(name.clone());
        }
    }
    for name in &old_servers {
        if !connected.contains(name) {
            report.removed.push(name.clone());
        }
    }
    report.reconnected.sort();
    report.added.sort();
    report.removed.sort();
    report
}

/// Render the reload change report (hermes `_reload_mcp` output lines).
pub fn format_reload_report(report: &ReloadReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    if !report.reconnected.is_empty() {
        lines.push(format!(
            "  ♻️  Reconnected: {}",
            report.reconnected.join(", ")
        ));
    }
    if !report.added.is_empty() {
        lines.push(format!("  ➕ Added: {}", report.added.join(", ")));
    }
    if !report.removed.is_empty() {
        lines.push(format!("  ➖ Removed: {}", report.removed.join(", ")));
    }
    if report.reconnected.is_empty() && report.added.is_empty() && report.removed.is_empty() {
        lines.push("  No MCP servers connected.".to_string());
    } else {
        let servers = report.reconnected.len() + report.added.len();
        lines.push(format!(
            "  🔧 {} tool(s) available from {} server(s)",
            report.tool_count, servers
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;
    use std::io::Write;

    /// Minimal stdio MCP server (initialize / tools/list / tools/call)
    /// used to exercise the live + lazy paths without external deps.
    const FAKE_SERVER_PY: &str = r#"#!/usr/bin/env python3
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

while True:
    line = sys.stdin.readline()
    if not line:
        break
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"],
              "result": {"serverInfo": {"name": "fake"},
                         "protocolVersion": "2024-11-05"}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [
            {"name": "echo", "description": "echo tool",
             "inputSchema": {"type": "object",
                             "properties": {"text": {"type": "string"}}}},
            {"name": "envcheck", "description": "report one env var",
             "inputSchema": {"type": "object",
                             "properties": {"name": {"type": "string"}}}}
        ]}})
    elif method == "tools/call":
        params = msg.get("params") or {}
        args = params.get("arguments") or {}
        if params.get("name") == "envcheck":
            import os as _os
            text = _os.environ.get(str(args.get("name")), "<unset>")
        else:
            text = "echo:" + str(args.get("text"))
        send({"jsonrpc": "2.0", "id": msg["id"],
              "result": {"content": [{"type": "text", "text": text}],
                         "isError": False}})
"#;

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn write_fake_server(dir: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("fake_mcp_server.py");
        let mut f = std::fs::File::create(&script).unwrap();
        f.write_all(FAKE_SERVER_PY.as_bytes()).unwrap();
        script
    }

    fn fake_config(script: &std::path::Path, name: &str, lazy: bool) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: "python3".into(),
            args: vec![script.display().to_string()],
            env: HashMap::new(),
            url: None,
            transport: None,
            headers: HashMap::new(),
            auth: None,
            oauth: oauth::McpOAuthConfig::default(),
            lazy,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn live_registration_writes_through_cache() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_server(tmp.path());
        let config = fake_config(&script, "live-srv", false);
        // Point the cache at this test's home.
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());

        let mut registry = ToolRegistry::new();
        let count = register_mcp_server(&mut registry, &config).await.unwrap();
        assert_eq!(count, 2);
        assert!(registry.has("mcp__live-srv__echo"));
        assert!(registry.has("mcp__live-srv__envcheck"));

        // Write-through: a matching cache entry now exists.
        let fp = schema_cache::config_fingerprint(&config);
        let entry = schema_cache::get_cached_entry("live-srv", &fp).expect("cache written");
        let tools = schema_cache::tools_from_cache_entry(&entry);
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"envcheck"));

        // The registered tool actually talks to the server.
        let ctx = Arc::new(ToolContext::default());
        let result = registry
            .dispatch("mcp__live-srv__echo", json!({"text": "hi"}), ctx)
            .await
            .unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(|v| v.as_str()),
            Some("echo:hi")
        );

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[tokio::test]
    async fn lazy_registration_serves_from_cache_without_spawning() {
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        // Command does not exist — registration can only succeed from cache.
        let config = McpServerConfig {
            name: "lazy-srv".into(),
            command: "/nonexistent/ulnclaw-fake-mcp".into(),
            args: vec![],
            env: HashMap::new(),
            url: None,
            transport: None,
            headers: HashMap::new(),
            auth: None,
            oauth: oauth::McpOAuthConfig::default(),
            lazy: true,
            enabled: true,
        };
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());

        // No cache yet: falls back to the live path, which fails.
        let mut registry = ToolRegistry::new();
        assert!(register_mcp_server_lazy(&mut registry, &config)
            .await
            .is_err());

        // Seed a matching cache entry: registration succeeds without spawn.
        let fp = schema_cache::config_fingerprint(&config);
        schema_cache::write_cache_entry(
            "lazy-srv",
            &fp,
            &[json!({
                "name": "cached_tool",
                "description": "from cache",
                "inputSchema": {"type": "object", "properties": {}}
            })],
            &[],
        )
        .unwrap();
        let count = register_mcp_server_lazy(&mut registry, &config)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(registry.has("mcp__lazy-srv__cached_tool"));

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn safe_env_filters_secrets_but_keeps_baseline_and_declared() {
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        // Dotenv-tagged secret: present in ~/.ulnclaw/.env -> passes.
        std::fs::write(tmp.path().join(".env"), "DOTENV_SECRET=from-dotenv\n").unwrap();
        std::env::set_var("DOTENV_SECRET", "from-dotenv");
        std::env::set_var("ULNCLAW_MCP_LEAK_TEST", "should-not-pass");
        std::env::set_var("XDG_TEST_VAR", "xdg-value");

        let mut declared = HashMap::new();
        declared.insert("DECLARED_KEY".to_string(), "declared-value".to_string());
        let env = build_safe_env(&declared);

        assert!(env.contains_key("PATH"), "PATH must pass");
        assert_eq!(
            env.get("XDG_TEST_VAR").map(String::as_str),
            Some("xdg-value")
        );
        assert_eq!(
            env.get("DOTENV_SECRET").map(String::as_str),
            Some("from-dotenv")
        );
        assert_eq!(
            env.get("DECLARED_KEY").map(String::as_str),
            Some("declared-value")
        );
        assert!(
            !env.contains_key("ULNCLAW_MCP_LEAK_TEST"),
            "undeclared secret leaked"
        );

        std::env::remove_var("DOTENV_SECRET");
        std::env::remove_var("ULNCLAW_MCP_LEAK_TEST");
        std::env::remove_var("XDG_TEST_VAR");
        match prev_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn env_interpolation_resolves_and_keeps_unknown() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("ULNCLAW_INTERP_TEST", "resolved");
        assert_eq!(
            interpolate_env_value("prefix-${ULNCLAW_INTERP_TEST}-suffix"),
            "prefix-resolved-suffix"
        );
        assert_eq!(
            interpolate_env_value("${env:ULNCLAW_INTERP_TEST}"),
            "resolved",
            "Cursor-style env: prefix"
        );
        assert_eq!(
            interpolate_env_value("${ULNCLAW_INTERP_MISSING}"),
            "${ULNCLAW_INTERP_MISSING}",
            "unset vars keep the literal placeholder"
        );
        assert_eq!(interpolate_env_value("no placeholders"), "no placeholders");
        assert_eq!(interpolate_env_value("dangling ${oops"), "dangling ${oops");
        std::env::remove_var("ULNCLAW_INTERP_TEST");
    }

    #[tokio::test]
    async fn stdio_child_sees_filtered_env_only() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_server(tmp.path());
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        std::env::set_var("ULNCLAW_MCP_LEAK_E2E", "leaky");

        let mut config = fake_config(&script, "env-srv", false);
        config.env.insert("DECLARED_E2E".into(), "visible".into());

        let mut registry = ToolRegistry::new();
        register_mcp_server(&mut registry, &config).await.unwrap();
        let ctx = Arc::new(ToolContext::default());

        // Undeclared parent env var is NOT inherited.
        let result = registry
            .dispatch(
                "mcp__env-srv__envcheck",
                json!({"name": "ULNCLAW_MCP_LEAK_E2E"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(|v| v.as_str()),
            Some("<unset>"),
            "undeclared parent env must not reach the MCP child"
        );
        // Declared env var arrives.
        let result = registry
            .dispatch(
                "mcp__env-srv__envcheck",
                json!({"name": "DECLARED_E2E"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(|v| v.as_str()),
            Some("visible")
        );
        // Safe baseline (PATH) still present.
        let result = registry
            .dispatch("mcp__env-srv__envcheck", json!({"name": "PATH"}), ctx)
            .await
            .unwrap();
        assert_ne!(
            result.pointer("/content/0/text").and_then(|v| v.as_str()),
            Some("<unset>")
        );

        std::env::remove_var("ULNCLAW_MCP_LEAK_E2E");
        match prev_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[tokio::test]
    async fn reload_reconnects_and_reports_changes() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_server(tmp.path());
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());

        let server_a = fake_config(&script, "srv-a", false);
        let mut config = crate::config::UlncLawConfig::default();
        config.mcp.servers.push(server_a.clone());

        let mut registry = ToolRegistry::new();
        assert_eq!(
            register_mcp_server(&mut registry, &server_a).await.unwrap(),
            2
        );

        // Reload with the same server list: reconnect, no adds/removes.
        let report = reload_mcp_servers(&mut registry, &config).await;
        assert_eq!(report.reconnected, vec!["srv-a"]);
        assert!(report.added.is_empty());
        assert!(report.removed.is_empty());
        assert_eq!(report.tool_count, 2);
        assert!(registry.has("mcp__srv-a__echo"));
        assert!(registry.has("mcp__srv-a__envcheck"));

        // Reload with the server removed: reports removal, tool is gone.
        config.mcp.servers.clear();
        let report = reload_mcp_servers(&mut registry, &config).await;
        assert_eq!(report.removed, vec!["srv-a"]);
        assert!(report.reconnected.is_empty());
        assert_eq!(report.tool_count, 0);
        assert!(!registry.has("mcp__srv-a__echo"));
        let formatted = format_reload_report(&report);
        assert!(formatted.contains("Removed: srv-a"), "{}", formatted);

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[tokio::test]
    async fn lazy_first_use_connects_reconciles_and_rejects_phantoms() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let script = write_fake_server(tmp.path());
        let config = fake_config(&script, "recon-srv", true);
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());

        // Seed a stale manifest: real tool "echo" + phantom tool "ghost".
        let fp = schema_cache::config_fingerprint(&config);
        schema_cache::write_cache_entry(
            "recon-srv",
            &fp,
            &[
                json!({"name": "echo", "description": "echo tool",
                       "inputSchema": {"type": "object", "properties": {}}}),
                json!({"name": "ghost", "description": "gone server-side",
                       "inputSchema": {"type": "object", "properties": {}}}),
            ],
            &[],
        )
        .unwrap();

        let mut registry = ToolRegistry::new();
        let count = register_mcp_server_lazy(&mut registry, &config)
            .await
            .unwrap();
        assert_eq!(count, 2, "both cached tools register without spawning");

        let ctx = Arc::new(ToolContext::default());
        // First real call spawns the server and works.
        let result = registry
            .dispatch("mcp__recon-srv__echo", json!({"text": "lazy"}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(|v| v.as_str()),
            Some("echo:lazy")
        );
        // Phantom tool fails fast with a stale-schema error.
        let err = registry
            .dispatch("mcp__recon-srv__ghost", json!({}), ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no longer provides"), "{}", err);

        // The cache entry was reconciled to the live (single) tool.
        let entry = schema_cache::get_cached_entry("recon-srv", &fp).unwrap();
        let cached_tools = schema_cache::tools_from_cache_entry(&entry);
        let names: Vec<&str> = cached_tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"envcheck"));
        assert!(!names.contains(&"ghost"), "phantom tool must be dropped");

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }
}

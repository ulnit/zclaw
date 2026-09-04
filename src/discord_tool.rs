//! Discord server introspection and management tools (hermes
//! `tools/discord_tool.py`).
//!
//! Two tools share one action dispatch: `discord` (core: fetch_messages,
//! search_members, create_thread) and `discord_admin` (server management).
//! Uses the Discord REST API directly with the bot token — no dependency
//! on a gateway adapter.
//!
//! The schema exposed to the model is filtered by two gates:
//! 1. Privileged intents detected from `GET /applications/@me` at schema
//!    build time (non-blocking: memory cache → disk cache → permissive
//!    default + background detection). Actions requiring an intent the
//!    bot lacks (search_members / member_info → GUILD_MEMBERS) are hidden.
//! 2. User config allowlist at `[discord] server_actions` — only listed
//!    actions appear; empty/unset exposes everything intent-available.
//!
//! Per-guild permissions are NOT pre-checked — Discord returns 403 at
//! call time and [`enrich_403`] maps it to actionable guidance.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::AgentError;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const RESPONSE_BODY_MAX_BYTES: usize = 4 * 1024 * 1024;
const ERROR_BODY_MAX_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT_SECS: u64 = 15;
const CAPABILITY_TIMEOUT_SECS: u64 = 5;

/// Capability disk-cache TTL — privileged intents change only via the
/// Developer Portal, so 24 h staleness is harmless (hermes
/// `_CAPABILITY_DISK_TTL_SECONDS`).
const CAPABILITY_DISK_TTL_SECS: u64 = 24 * 3600;

// Application flag bits (GET /applications/@me → "flags").
const FLAG_GATEWAY_GUILD_MEMBERS: u64 = 1 << 14;
const FLAG_GATEWAY_GUILD_MEMBERS_LIMITED: u64 = 1 << 15;
const FLAG_GATEWAY_MESSAGE_CONTENT: u64 = 1 << 18;
const FLAG_GATEWAY_MESSAGE_CONTENT_LIMITED: u64 = 1 << 19;

#[derive(Debug)]
pub struct DiscordApiError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for DiscordApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Discord API error {}: {}", self.status, self.body)
    }
}

/// Resolve the bot token under the active profile secret scope (hermes
/// `_get_bot_token`).
pub fn bot_token() -> Option<String> {
    let token = crate::secret_scope::get_secret_lenient("DISCORD_BOT_TOKEN", None)
        .or_else(|| crate::config::get_env_value("DISCORD_BOT_TOKEN"))
        .unwrap_or_default();
    let token = token.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// One Discord REST request (hermes `_discord_request`).
async fn discord_request(
    method: &str,
    path: &str,
    token: &str,
    params: Option<&[(&str, String)]>,
    body: Option<Value>,
    timeout_secs: u64,
) -> Result<Value, DiscordApiError> {
    let mut url = format!("{}{}", DISCORD_API_BASE, path);
    if let Some(params) = params {
        let query: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, percent_encode(v)))
            .collect();
        url = format!("{}?{}", url, query.join("&"));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| DiscordApiError {
            status: 0,
            body: format!("http client: {e}"),
        })?;
    let mut request = client
        .request(
            method.parse().map_err(|_| DiscordApiError {
                status: 0,
                body: format!("bad method {method}"),
            })?,
            &url,
        )
        .header("Authorization", format!("Bot {}", token))
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            "ulnclaw-agent (https://gitee.com/ushaw/ulnclaw)",
        );
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|e| DiscordApiError {
        status: 0,
        body: format!("request failed: {e}"),
    })?;
    let status = response.status().as_u16();
    if status == 204 {
        return Ok(Value::Null);
    }
    let bytes = response.bytes().await.map_err(|e| DiscordApiError {
        status,
        body: format!("read body: {e}"),
    })?;
    if status >= 400 {
        let truncated = if bytes.len() > ERROR_BODY_MAX_BYTES {
            String::from_utf8_lossy(&bytes[..ERROR_BODY_MAX_BYTES]).into_owned()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        return Err(DiscordApiError {
            status,
            body: truncated,
        });
    }
    if bytes.len() > RESPONSE_BODY_MAX_BYTES {
        return Err(DiscordApiError {
            status: 502,
            body: "Discord API response body exceeded 4 MiB.".into(),
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| DiscordApiError {
        status,
        body: format!("parse body: {e}"),
    })
}

/// Minimal query-value percent-encoding (unreserved set per RFC 3986).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn channel_type_name(type_id: i64) -> String {
    match type_id {
        0 => "text",
        2 => "voice",
        4 => "category",
        5 => "announcement",
        10 => "announcement_thread",
        11 => "public_thread",
        12 => "private_thread",
        13 => "stage",
        15 => "forum",
        16 => "media",
        other => return format!("unknown({other})"),
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Capability detection (application intents)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    pub has_members_intent: bool,
    pub has_message_content: bool,
    pub detected: bool,
}

impl Caps {
    fn permissive() -> Self {
        Self {
            has_members_intent: true,
            has_message_content: true,
            detected: false,
        }
    }
}

fn caps_memory_cache() -> &'static Mutex<HashMap<String, Caps>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Caps>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn caps_bg_started() -> &'static Mutex<HashSet<String>> {
    static STARTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    STARTED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Stable non-reversible cache key for a bot token (hermes
/// `_token_cache_key`: sha256 hex[:16]).
fn token_cache_key(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    hex[..16].to_string()
}

fn capability_disk_cache_path() -> std::path::PathBuf {
    crate::config::ulnclaw_home()
        .join("cache")
        .join("discord_capabilities.json")
}

fn load_caps_from_disk(token: &str) -> Option<Caps> {
    let path = capability_disk_cache_path();
    let data: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let entry = data.get(&token_cache_key(token))?;
    let ts = entry.get("ts")?.as_f64()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if now - ts > CAPABILITY_DISK_TTL_SECS as f64 {
        return None;
    }
    let caps = entry.get("caps")?;
    if caps.get("has_members_intent")?.is_boolean() {
        Some(Caps {
            has_members_intent: caps["has_members_intent"].as_bool().unwrap_or(true),
            has_message_content: caps["has_message_content"].as_bool().unwrap_or(true),
            detected: caps["detected"].as_bool().unwrap_or(false),
        })
    } else {
        None
    }
}

fn save_caps_to_disk(token: &str, caps: &Caps) {
    let path = capability_disk_cache_path();
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).ok();
    }
    let mut data: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    data[token_cache_key(token)] = json!({
        "caps": {
            "has_members_intent": caps.has_members_intent,
            "has_message_content": caps.has_message_content,
            "detected": caps.detected,
        },
        "ts": now,
    });
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, data.to_string()).is_ok() {
        std::fs::rename(&tmp, &path).ok();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
        }
    }
}

/// Pure network fetch — does NOT touch the in-process cache (background
/// detection must not mutate schemas mid-process; hermes
/// `_fetch_capabilities`).
async fn fetch_capabilities(token: &str) -> Caps {
    let mut caps = Caps::permissive();
    match discord_request(
        "GET",
        "/applications/@me",
        token,
        None,
        None,
        CAPABILITY_TIMEOUT_SECS,
    )
    .await
    {
        Ok(app) => {
            let flags = app.get("flags").and_then(|v| v.as_u64()).unwrap_or(0);
            caps.has_members_intent =
                flags & (FLAG_GATEWAY_GUILD_MEMBERS | FLAG_GATEWAY_GUILD_MEMBERS_LIMITED) != 0;
            caps.has_message_content =
                flags & (FLAG_GATEWAY_MESSAGE_CONTENT | FLAG_GATEWAY_MESSAGE_CONTENT_LIMITED) != 0;
            caps.detected = true;
        }
        Err(e) => {
            eprintln!(
                "[discord] capability detection failed ({}); exposing all actions.",
                e
            );
        }
    }
    caps
}

/// Non-blocking capability lookup for schema builds (hermes
/// `_detect_capabilities_nonblocking`): memory → disk → permissive default
/// + fire-and-forget background detection for the NEXT process.
pub fn detect_capabilities_nonblocking(token: &str) -> Caps {
    if let Some(cached) = caps_memory_cache().lock().unwrap().get(token).cloned() {
        return cached;
    }
    if let Some(disk) = load_caps_from_disk(token) {
        caps_memory_cache()
            .lock()
            .unwrap()
            .insert(token.to_string(), disk.clone());
        return disk;
    }
    // Cold start — pin the permissive default for THIS process (schema
    // stability across agent inits), detect in the background.
    let caps = Caps::permissive();
    caps_memory_cache()
        .lock()
        .unwrap()
        .insert(token.to_string(), caps.clone());
    let already_started = {
        let mut started = caps_bg_started().lock().unwrap();
        !started.insert(token.to_string())
    };
    if !already_started {
        let token = token.to_string();
        tokio::spawn(async move {
            let detected = fetch_capabilities(&token).await;
            if detected.detected {
                save_caps_to_disk(&token, &detected);
            }
        });
    }
    caps
}

/// Test hook (hermes `_reset_capability_cache`).
#[cfg(test)]
fn reset_capability_cache() {
    caps_memory_cache().lock().unwrap().clear();
    caps_bg_started().lock().unwrap().clear();
}

// ---------------------------------------------------------------------------
// Action implementations (hermes `_list_guilds` … `_remove_role`)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct ActionArgs {
    guild_id: String,
    channel_id: String,
    user_id: String,
    role_id: String,
    message_id: String,
    query: String,
    name: String,
    limit: i64,
    before: String,
    after: String,
    auto_archive_duration: i64,
}

impl ActionArgs {
    fn from_value(args: &Value) -> Self {
        let s = |key: &str| {
            args.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Self {
            guild_id: s("guild_id"),
            channel_id: s("channel_id"),
            user_id: s("user_id"),
            role_id: s("role_id"),
            message_id: s("message_id"),
            query: s("query"),
            name: s("name"),
            limit: args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50),
            before: s("before"),
            after: s("after"),
            auto_archive_duration: args
                .get("auto_archive_duration")
                .and_then(|v| v.as_i64())
                .unwrap_or(1440),
        }
    }
}

async fn list_guilds(token: &str, _args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let guilds = discord_request(
        "GET",
        "/users/@me/guilds",
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    let result: Vec<Value> = guilds
        .as_array()
        .map(|list| {
            list.iter()
                .map(|g| {
                    json!({
                        "id": g.get("id"),
                        "name": g.get("name"),
                        "icon": g.get("icon"),
                        "owner": g.get("owner").and_then(|v| v.as_bool()).unwrap_or(false),
                        "permissions": g.get("permissions"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({"guilds": result, "count": result.len()}))
}

async fn server_info(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let g = discord_request(
        "GET",
        &format!("/guilds/{}", args.guild_id),
        token,
        Some(&[("with_counts", "true".to_string())]),
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    Ok(json!({
        "id": g.get("id"),
        "name": g.get("name"),
        "description": g.get("description"),
        "icon": g.get("icon"),
        "owner_id": g.get("owner_id"),
        "member_count": g.get("approximate_member_count"),
        "online_count": g.get("approximate_presence_count"),
        "features": g.get("features").cloned().unwrap_or_else(|| json!([])),
        "premium_tier": g.get("premium_tier"),
        "premium_subscription_count": g.get("premium_subscription_count"),
        "verification_level": g.get("verification_level"),
    }))
}

async fn list_channels(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let channels = discord_request(
        "GET",
        &format!("/guilds/{}/channels", args.guild_id),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    let list = channels.as_array().cloned().unwrap_or_default();

    // Categories first, then channels grouped under each (hermes layout).
    let mut categories: HashMap<String, Value> = HashMap::new();
    let mut uncategorized: Vec<Value> = Vec::new();
    for ch in &list {
        if ch.get("type").and_then(|v| v.as_i64()) == Some(4) {
            let id = ch
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            categories.insert(
                id,
                json!({
                    "id": ch.get("id"),
                    "name": ch.get("name"),
                    "position": ch.get("position").and_then(|v| v.as_i64()).unwrap_or(0),
                    "channels": [],
                }),
            );
        }
    }
    for ch in &list {
        if ch.get("type").and_then(|v| v.as_i64()) == Some(4) {
            continue;
        }
        let entry = json!({
            "id": ch.get("id"),
            "name": ch.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            "type": channel_type_name(ch.get("type").and_then(|v| v.as_i64()).unwrap_or(-1)),
            "position": ch.get("position").and_then(|v| v.as_i64()).unwrap_or(0),
            "topic": ch.get("topic"),
            "nsfw": ch.get("nsfw").and_then(|v| v.as_bool()).unwrap_or(false),
        });
        let parent = ch.get("parent_id").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(category) = categories.get_mut(parent) {
            category["channels"].as_array_mut().map(|c| c.push(entry));
        } else {
            uncategorized.push(entry);
        }
    }
    let position_of = |v: &Value| v.get("position").and_then(|p| p.as_i64()).unwrap_or(0);
    uncategorized.sort_by_key(position_of);
    let mut sorted_cats: Vec<Value> = categories.into_values().collect();
    sorted_cats.sort_by_key(position_of);
    for cat in &mut sorted_cats {
        if let Some(channels) = cat["channels"].as_array_mut() {
            channels.sort_by_key(position_of);
        }
    }
    let mut result: Vec<Value> = Vec::new();
    if !uncategorized.is_empty() {
        result.push(json!({"category": null, "channels": uncategorized}));
    }
    for cat in sorted_cats {
        result.push(json!({
            "category": {"id": cat["id"].clone(), "name": cat["name"].clone()},
            "channels": cat["channels"],
        }));
    }
    let total: usize = result
        .iter()
        .map(|group| group["channels"].as_array().map(|c| c.len()).unwrap_or(0))
        .sum();
    Ok(json!({"channel_groups": result, "total_channels": total}))
}

async fn channel_info(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let ch = discord_request(
        "GET",
        &format!("/channels/{}", args.channel_id),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    Ok(json!({
        "id": ch.get("id"),
        "name": ch.get("name"),
        "type": channel_type_name(ch.get("type").and_then(|v| v.as_i64()).unwrap_or(-1)),
        "guild_id": ch.get("guild_id"),
        "topic": ch.get("topic"),
        "nsfw": ch.get("nsfw").and_then(|v| v.as_bool()).unwrap_or(false),
        "position": ch.get("position"),
        "parent_id": ch.get("parent_id"),
        "rate_limit_per_user": ch.get("rate_limit_per_user").cloned().unwrap_or_else(|| json!(0)),
        "last_message_id": ch.get("last_message_id"),
    }))
}

async fn list_roles(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let roles = discord_request(
        "GET",
        &format!("/guilds/{}/roles", args.guild_id),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    let mut list = roles.as_array().cloned().unwrap_or_default();
    list.sort_by(|a, b| {
        b.get("position")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .cmp(&a.get("position").and_then(|v| v.as_i64()).unwrap_or(0))
    });
    let result: Vec<Value> = list
        .iter()
        .map(|r| {
            let color = r.get("color").and_then(|v| v.as_u64()).unwrap_or(0);
            json!({
                "id": r.get("id"),
                "name": r.get("name"),
                "color": if color != 0 { Some(format!("#{color:06x}")) } else { None },
                "position": r.get("position").and_then(|v| v.as_i64()).unwrap_or(0),
                "mentionable": r.get("mentionable").and_then(|v| v.as_bool()).unwrap_or(false),
                "managed": r.get("managed").and_then(|v| v.as_bool()).unwrap_or(false),
                "member_count": r.get("member_count"),
                "hoist": r.get("hoist").and_then(|v| v.as_bool()).unwrap_or(false),
            })
        })
        .collect();
    Ok(json!({"roles": result, "count": result.len()}))
}

fn member_entry(member: &Value) -> Value {
    let user = member.get("user").cloned().unwrap_or_else(|| json!({}));
    json!({
        "user_id": user.get("id"),
        "username": user.get("username"),
        "display_name": user.get("global_name"),
        "nickname": member.get("nick"),
        "bot": user.get("bot").and_then(|v| v.as_bool()).unwrap_or(false),
        "roles": member.get("roles").cloned().unwrap_or_else(|| json!([])),
    })
}

async fn member_info(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let member = discord_request(
        "GET",
        &format!("/guilds/{}/members/{}", args.guild_id, args.user_id),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    let mut entry = member_entry(&member);
    entry["joined_at"] = member.get("joined_at").cloned().unwrap_or(Value::Null);
    Ok(entry)
}

async fn search_members(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let limit = if args.limit <= 0 {
        20
    } else {
        args.limit.min(100)
    };
    let members = discord_request(
        "GET",
        &format!("/guilds/{}/members/search", args.guild_id),
        token,
        Some(&[("query", args.query.clone()), ("limit", limit.to_string())]),
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    let result: Vec<Value> = members
        .as_array()
        .map(|list| list.iter().map(member_entry).collect())
        .unwrap_or_default();
    Ok(json!({"members": result, "count": result.len()}))
}

async fn fetch_messages(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let limit = if args.limit <= 0 {
        50
    } else {
        args.limit.min(100)
    };
    let mut params: Vec<(&str, String)> = vec![("limit", limit.to_string())];
    if !args.before.is_empty() {
        params.push(("before", args.before.clone()));
    }
    if !args.after.is_empty() {
        params.push(("after", args.after.clone()));
    }
    let messages = discord_request(
        "GET",
        &format!("/channels/{}/messages", args.channel_id),
        token,
        Some(&params),
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    let result: Vec<Value> = messages
        .as_array()
        .map(|list| {
            list.iter()
                .map(|msg| {
                    let author = msg.get("author").cloned().unwrap_or_else(|| json!({}));
                    let attachments: Vec<Value> = msg
                        .get("attachments")
                        .and_then(|v| v.as_array())
                        .map(|list| {
                            list.iter()
                                .map(|a| {
                                    json!({
                                        "filename": a.get("filename"),
                                        "url": a.get("url"),
                                        "size": a.get("size"),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let reactions: Vec<Value> = msg
                        .get("reactions")
                        .and_then(|v| v.as_array())
                        .map(|list| {
                            list.iter()
                                .map(|r| {
                                    json!({
                                        "emoji": r.pointer("/emoji/name"),
                                        "count": r.get("count").and_then(|v| v.as_u64()).unwrap_or(0),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "id": msg.get("id"),
                        "content": msg.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                        "author": {
                            "id": author.get("id"),
                            "username": author.get("username"),
                            "display_name": author.get("global_name"),
                            "bot": author.get("bot").and_then(|v| v.as_bool()).unwrap_or(false),
                        },
                        "timestamp": msg.get("timestamp"),
                        "edited_timestamp": msg.get("edited_timestamp"),
                        "attachments": attachments,
                        "reactions": reactions,
                        "pinned": msg.get("pinned").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({"messages": result, "count": result.len()}))
}

async fn list_pins(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let messages = discord_request(
        "GET",
        &format!("/channels/{}/pins", args.channel_id),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    let result: Vec<Value> = messages
        .as_array()
        .map(|list| {
            list.iter()
                .map(|msg| {
                    let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let truncated: String = content.chars().take(200).collect();
                    json!({
                        "id": msg.get("id"),
                        "content": truncated,
                        "author": msg.pointer("/author/username"),
                        "timestamp": msg.get("timestamp"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({"pinned_messages": result, "count": result.len()}))
}

async fn pin_message(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    discord_request(
        "PUT",
        &format!("/channels/{}/pins/{}", args.channel_id, args.message_id),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    Ok(json!({"success": true, "message": format!("Message {} pinned.", args.message_id)}))
}

async fn unpin_message(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    discord_request(
        "DELETE",
        &format!("/channels/{}/pins/{}", args.channel_id, args.message_id),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    Ok(json!({"success": true, "message": format!("Message {} unpinned.", args.message_id)}))
}

async fn delete_message(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    discord_request(
        "DELETE",
        &format!("/channels/{}/messages/{}", args.channel_id, args.message_id),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    Ok(json!({"success": true, "message": format!("Message {} deleted.", args.message_id)}))
}

async fn create_thread(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    let (path, body) = if !args.message_id.is_empty() {
        (
            format!(
                "/channels/{}/messages/{}/threads",
                args.channel_id, args.message_id
            ),
            json!({
                "name": args.name,
                "auto_archive_duration": args.auto_archive_duration,
            }),
        )
    } else {
        (
            format!("/channels/{}/threads", args.channel_id),
            json!({
                "name": args.name,
                "auto_archive_duration": args.auto_archive_duration,
                "type": 11,
            }),
        )
    };
    let thread =
        discord_request("POST", &path, token, None, Some(body), REQUEST_TIMEOUT_SECS).await?;
    Ok(json!({
        "success": true,
        "thread_id": thread.get("id"),
        "name": thread.get("name"),
    }))
}

async fn add_role(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    discord_request(
        "PUT",
        &format!(
            "/guilds/{}/members/{}/roles/{}",
            args.guild_id, args.user_id, args.role_id
        ),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    Ok(
        json!({"success": true, "message": format!("Role {} added to user {}.", args.role_id, args.user_id)}),
    )
}

async fn remove_role(token: &str, args: &ActionArgs) -> Result<Value, DiscordApiError> {
    discord_request(
        "DELETE",
        &format!(
            "/guilds/{}/members/{}/roles/{}",
            args.guild_id, args.user_id, args.role_id
        ),
        token,
        None,
        None,
        REQUEST_TIMEOUT_SECS,
    )
    .await?;
    Ok(
        json!({"success": true, "message": format!("Role {} removed from user {}.", args.role_id, args.user_id)}),
    )
}

// ---------------------------------------------------------------------------
// Action table, manifest, gates (hermes `_ACTIONS` / `_ACTION_MANIFEST` /
// `_REQUIRED_PARAMS` / `_INTENT_GATED_MEMBERS`)
// ---------------------------------------------------------------------------

pub const CORE_ACTIONS: &[&str] = &["fetch_messages", "search_members", "create_thread"];

/// Server-management actions (hermes `_ADMIN_ACTION_NAMES` = all − core).
pub const ADMIN_ACTIONS: &[&str] = &[
    "list_guilds",
    "server_info",
    "list_channels",
    "channel_info",
    "list_roles",
    "member_info",
    "list_pins",
    "pin_message",
    "unpin_message",
    "delete_message",
    "add_role",
    "remove_role",
];

pub const ALL_ACTIONS: &[&str] = &[
    "list_guilds",
    "server_info",
    "list_channels",
    "channel_info",
    "list_roles",
    "member_info",
    "search_members",
    "fetch_messages",
    "list_pins",
    "pin_message",
    "unpin_message",
    "delete_message",
    "create_thread",
    "add_role",
    "remove_role",
];

/// (action, signature, one-line description) — single source of truth for
/// the schema description (hermes `_ACTION_MANIFEST`).
const ACTION_MANIFEST: [(&str, &str, &str); 15] = [
    ("list_guilds", "()", "list servers the bot is in"),
    (
        "server_info",
        "(guild_id)",
        "server details + member counts",
    ),
    (
        "list_channels",
        "(guild_id)",
        "all channels grouped by category",
    ),
    ("channel_info", "(channel_id)", "single channel details"),
    ("list_roles", "(guild_id)", "roles sorted by position"),
    (
        "member_info",
        "(guild_id, user_id)",
        "lookup a specific member",
    ),
    (
        "search_members",
        "(guild_id, query)",
        "find members by name prefix",
    ),
    (
        "fetch_messages",
        "(channel_id)",
        "recent messages; optional before/after snowflakes",
    ),
    ("list_pins", "(channel_id)", "pinned messages in a channel"),
    ("pin_message", "(channel_id, message_id)", "pin a message"),
    (
        "unpin_message",
        "(channel_id, message_id)",
        "unpin a message",
    ),
    (
        "delete_message",
        "(channel_id, message_id)",
        "delete a message",
    ),
    (
        "create_thread",
        "(channel_id, name)",
        "create a public thread; optional message_id anchor",
    ),
    ("add_role", "(guild_id, user_id, role_id)", "assign a role"),
    (
        "remove_role",
        "(guild_id, user_id, role_id)",
        "remove a role",
    ),
];

/// Actions that require the GUILD_MEMBERS privileged intent.
const INTENT_GATED_MEMBERS: &[&str] = &["member_info", "search_members"];

fn required_params(action: &str) -> &'static [&'static str] {
    match action {
        "server_info" | "list_channels" | "list_roles" => &["guild_id"],
        "member_info" => &["guild_id", "user_id"],
        "search_members" => &["guild_id", "query"],
        "channel_info" | "fetch_messages" | "list_pins" => &["channel_id"],
        "pin_message" | "unpin_message" | "delete_message" => &["channel_id", "message_id"],
        "create_thread" => &["channel_id", "name"],
        "add_role" | "remove_role" => &["guild_id", "user_id", "role_id"],
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// Config-based action allowlist (hermes `_load_allowed_actions_config` —
// `[discord] server_actions`, comma-separated string or list)
// ---------------------------------------------------------------------------

pub fn load_allowed_actions_config() -> Option<Vec<String>> {
    let config = crate::config::UlncLawConfig::load(None).ok()?;
    let raw = config.discord.server_actions?;
    let names: Vec<String> = match raw {
        crate::config::StringOrList::Single(string) => string
            .split(',')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect(),
        crate::config::StringOrList::List(list) => list
            .into_iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect(),
    };
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for name in names {
        if ALL_ACTIONS.contains(&name.as_str()) {
            valid.push(name);
        } else {
            invalid.push(name);
        }
    }
    if !invalid.is_empty() {
        eprintln!(
            "[discord] server_actions: unknown action(s) ignored: {}. Known: {}",
            invalid.join(", "),
            ALL_ACTIONS.join(", ")
        );
    }
    Some(valid)
}

/// Visible action list from intents + config allowlist, canonical order
/// (hermes `_available_actions`).
pub fn available_actions(
    caps: &Caps,
    allowlist: Option<&[String]>,
    subset: &[&str],
) -> Vec<String> {
    ALL_ACTIONS
        .iter()
        .filter(|name| subset.contains(name))
        .filter(|name| caps.has_members_intent || !INTENT_GATED_MEMBERS.contains(name))
        .filter(|name| {
            allowlist
                .map(|allowed| allowed.iter().any(|a| a == *name))
                .unwrap_or(true)
        })
        .map(|name| name.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Schema construction (hermes `_build_schema`)
// ---------------------------------------------------------------------------

pub fn build_schema(actions: &[String], caps: &Caps, tool_name: &str) -> Option<Value> {
    if actions.is_empty() {
        return None;
    }
    let manifest_lines: Vec<String> = ACTION_MANIFEST
        .iter()
        .filter(|(name, _, _)| actions.iter().any(|a| a == name))
        .map(|(name, signature, description)| format!("  {name}{signature}  — {description}"))
        .collect();
    let manifest_block = manifest_lines.join("\n");

    let mut content_note = String::new();
    let affected: Vec<&str> = ["fetch_messages", "list_pins"]
        .iter()
        .filter(|name| actions.iter().any(|a| a == *name))
        .copied()
        .collect();
    if !affected.is_empty() && caps.detected && !caps.has_message_content {
        let mut names = affected.clone();
        names.sort_unstable();
        content_note = format!(
            "\n\nNOTE: Bot does NOT have the MESSAGE_CONTENT privileged intent. {} will \
             return message metadata (author, timestamps, attachments, reactions, pin \
             state) but `content` will be empty for messages not sent as a direct mention \
             to the bot or in DMs. Enable the intent in the Discord Developer Portal to \
             see all content.",
            names.join(" and ")
        );
    }

    let description = if tool_name == "discord_admin" {
        format!(
            "Manage a Discord server via the REST API.\n\nAvailable actions:\n{}\n\n\
             Call list_guilds first to discover guild_ids, then list_channels for \
             channel_ids. Runtime errors will tell you if the bot lacks a specific \
             per-guild permission (e.g. MANAGE_ROLES for add_role).{}",
            manifest_block, content_note
        )
    } else {
        format!(
            "Read and participate in a Discord server.\n\nAvailable actions:\n{}\n\n\
             Use the channel_id from the current conversation context. Use search_members \
             to look up user IDs by name prefix.{}",
            manifest_block, content_note
        )
    };

    Some(json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": actions},
            "guild_id": {"type": "string", "description": "Discord server (guild) ID."},
            "channel_id": {"type": "string", "description": "Discord channel ID."},
            "user_id": {"type": "string", "description": "Discord user ID."},
            "role_id": {"type": "string", "description": "Discord role ID."},
            "message_id": {"type": "string", "description": "Discord message ID."},
            "query": {"type": "string", "description": "Member name prefix to search for (search_members)."},
            "name": {"type": "string", "description": "New thread name (create_thread)."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100,
                      "description": "Max results (default 50). Applies to fetch_messages, search_members."},
            "before": {"type": "string", "description": "Snowflake ID for reverse pagination (fetch_messages)."},
            "after": {"type": "string", "description": "Snowflake ID for forward pagination (fetch_messages)."},
            "auto_archive_duration": {"type": "integer", "enum": [60, 1440, 4320, 10080],
                      "description": "Thread archive duration in minutes (create_thread, default 1440)."}
        },
        "required": ["action"],
        "__description": description,
    }))
}

// ---------------------------------------------------------------------------
// 403 enrichment (hermes `_ACTION_403_HINT` / `_enrich_403`)
// ---------------------------------------------------------------------------

fn action_403_hint(action: &str) -> Option<&'static str> {
    Some(match action {
        "pin_message" => "Bot lacks MANAGE_MESSAGES permission in this channel. Ask the server admin to grant the bot a role that has MANAGE_MESSAGES, or a per-channel overwrite.",
        "unpin_message" => "Bot lacks MANAGE_MESSAGES permission in this channel.",
        "delete_message" => "Bot lacks MANAGE_MESSAGES permission in this channel, or cannot view the channel/message.",
        "create_thread" => "Bot lacks CREATE_PUBLIC_THREADS in this channel, or cannot view it.",
        "add_role" => "Either the bot lacks MANAGE_ROLES, or the target role sits higher than the bot's highest role. Roles can only be assigned below the bot's own position in the role hierarchy.",
        "remove_role" => "Either the bot lacks MANAGE_ROLES, or the target role sits higher than the bot's highest role.",
        "fetch_messages" => "Bot cannot view this channel (missing VIEW_CHANNEL or READ_MESSAGE_HISTORY).",
        "list_pins" => "Bot cannot view this channel (missing VIEW_CHANNEL or READ_MESSAGE_HISTORY).",
        "channel_info" => "Bot cannot view this channel (missing VIEW_CHANNEL).",
        "search_members" => "Likely missing the Server Members privileged intent — enable it in the Discord Developer Portal under your bot's settings.",
        "member_info" => "Bot cannot see this guild member (missing Server Members intent or insufficient permissions).",
        _ => return None,
    })
}

pub fn enrich_403(action: &str, body: &str) -> String {
    let base = format!("Discord API 403 (forbidden) on '{action}'.");
    match action_403_hint(action) {
        Some(hint) => format!("{base} {hint} (Raw: {body})"),
        None => format!("{base} (Raw: {body})"),
    }
}

// ---------------------------------------------------------------------------
// Dispatch (hermes `_run_discord_action`)
// ---------------------------------------------------------------------------

pub async fn run_discord_action(
    action: &str,
    args: &Value,
    subset: &[&str],
    tool_label: &str,
) -> Result<Value, AgentError> {
    let token = bot_token()
        .ok_or_else(|| AgentError::Tool("DISCORD_BOT_TOKEN not configured.".to_string()))?;
    if !subset.contains(&action) {
        return Err(AgentError::Tool(format!(
            "Unknown action: {action}. Available actions: {}",
            subset.join(", ")
        )));
    }
    // Config-level allowlist gate (defense in depth — the schema already
    // filtered, but a stale cached schema must not smuggle denied actions).
    if let Some(allowlist) = load_allowed_actions_config() {
        if !allowlist.iter().any(|name| name == action) {
            return Err(AgentError::Tool(format!(
                "Action '{action}' is disabled by config (discord.server_actions). Allowed: {}",
                if allowlist.is_empty() {
                    "<none>".to_string()
                } else {
                    allowlist.join(", ")
                }
            )));
        }
    }
    let parsed = ActionArgs::from_value(args);
    let missing: Vec<&str> = required_params(action)
        .iter()
        .filter(|param| {
            let value = match **param {
                "guild_id" => &parsed.guild_id,
                "channel_id" => &parsed.channel_id,
                "user_id" => &parsed.user_id,
                "role_id" => &parsed.role_id,
                "message_id" => &parsed.message_id,
                "query" => &parsed.query,
                "name" => &parsed.name,
                _ => "",
            };
            value.is_empty()
        })
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(AgentError::Tool(format!(
            "Missing required parameters for '{action}': {}",
            missing.join(", ")
        )));
    }

    let result = match action {
        "list_guilds" => list_guilds(&token, &parsed).await,
        "server_info" => server_info(&token, &parsed).await,
        "list_channels" => list_channels(&token, &parsed).await,
        "channel_info" => channel_info(&token, &parsed).await,
        "list_roles" => list_roles(&token, &parsed).await,
        "member_info" => member_info(&token, &parsed).await,
        "search_members" => search_members(&token, &parsed).await,
        "fetch_messages" => fetch_messages(&token, &parsed).await,
        "list_pins" => list_pins(&token, &parsed).await,
        "pin_message" => pin_message(&token, &parsed).await,
        "unpin_message" => unpin_message(&token, &parsed).await,
        "delete_message" => delete_message(&token, &parsed).await,
        "create_thread" => create_thread(&token, &parsed).await,
        "add_role" => add_role(&token, &parsed).await,
        "remove_role" => remove_role(&token, &parsed).await,
        other => {
            return Err(AgentError::Tool(format!("Unknown action: {other}")));
        }
    };
    result.map_err(|e| {
        eprintln!("[discord] API error in {tool_label} action '{action}': {e}");
        if e.status == 403 {
            AgentError::Tool(enrich_403(action, &e.body))
        } else {
            AgentError::Tool(e.to_string())
        }
    })
}

// ---------------------------------------------------------------------------
// Registration (hermes module-level `registry.register` for discord +
// discord_admin; dynamic schema via non-blocking capability detection)
// ---------------------------------------------------------------------------

fn dynamic_schema(subset: &[&str], tool_name: &str) -> Option<Value> {
    let token = bot_token()?;
    let caps = detect_capabilities_nonblocking(&token);
    let allowlist = load_allowed_actions_config();
    let actions = available_actions(&caps, allowlist.as_deref(), subset);
    if actions.is_empty() {
        return None;
    }
    build_schema(&actions, &caps, tool_name)
}

fn static_schema(subset: &[&str], tool_name: &str) -> Value {
    let actions: Vec<String> = subset.iter().map(|name| name.to_string()).collect();
    build_schema(&actions, &Caps::permissive(), tool_name)
        .expect("non-empty action subset always builds a schema")
}

fn schema_with_description(schema: Value) -> (Value, String) {
    let mut schema = schema;
    let description = schema
        .get("__description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(object) = schema.as_object_mut() {
        object.remove("__description");
    }
    (schema, description)
}

pub fn register(registry: &mut crate::tools::ToolRegistry) {
    use crate::tools::tool;
    use crate::tools::ToolAvailability;

    let core_schema = dynamic_schema(CORE_ACTIONS, "discord")
        .unwrap_or_else(|| static_schema(CORE_ACTIONS, "discord"));
    let admin_schema = dynamic_schema(ADMIN_ACTIONS, "discord_admin")
        .unwrap_or_else(|| static_schema(ADMIN_ACTIONS, "discord_admin"));
    let (core_params, core_description) = schema_with_description(core_schema);
    let (admin_params, admin_description) = schema_with_description(admin_schema);

    registry.register(
        tool("discord")
            .description(core_description)
            .parameters(core_params)
            .handler(|args, _ctx| async move {
                let action = args
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match run_discord_action(&action, &args, CORE_ACTIONS, "discord").await {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(json!({"success": false, "error": e.to_string()})),
                }
            })
            .toolset("discord")
            .emoji("\u{1f4ac}")
            .check_fn(|| {
                if bot_token().is_some() {
                    ToolAvailability::available()
                } else {
                    ToolAvailability::unavailable("DISCORD_BOT_TOKEN not set")
                }
            })
            .build()
            .expect("discord builds"),
    );

    registry.register(
        tool("discord_admin")
            .description(admin_description)
            .parameters(admin_params)
            .handler(|args, _ctx| async move {
                let action = args
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match run_discord_action(&action, &args, ADMIN_ACTIONS, "discord_admin").await {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(json!({"success": false, "error": e.to_string()})),
                }
            })
            .toolset("discord_admin")
            .emoji("\u{1f6e1}\u{fe0f}")
            .check_fn(|| {
                if bot_token().is_some() {
                    ToolAvailability::available()
                } else {
                    ToolAvailability::unavailable("DISCORD_BOT_TOKEN not set")
                }
            })
            .build()
            .expect("discord_admin builds"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_type_names() {
        assert_eq!(channel_type_name(0), "text");
        assert_eq!(channel_type_name(4), "category");
        assert_eq!(channel_type_name(11), "public_thread");
        assert_eq!(channel_type_name(15), "forum");
        assert_eq!(channel_type_name(99), "unknown(99)");
    }

    #[test]
    fn percent_encoding_covers_query_values() {
        assert_eq!(percent_encode("simple"), "simple");
        assert_eq!(percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_encode("naïve"), "na%C3%AFve");
    }

    #[test]
    fn token_cache_key_is_stable_and_short() {
        let key = token_cache_key("bot-token-123");
        assert_eq!(key.len(), 16);
        assert_eq!(key, token_cache_key("bot-token-123"));
        assert_ne!(key, token_cache_key("bot-token-124"));
    }

    #[test]
    fn caps_disk_cache_roundtrip() {
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        reset_capability_cache();

        assert!(load_caps_from_disk("tok").is_none());
        let caps = Caps {
            has_members_intent: false,
            has_message_content: true,
            detected: true,
        };
        save_caps_to_disk("tok", &caps);
        let loaded = load_caps_from_disk("tok").expect("caps roundtrip");
        assert_eq!(loaded, caps);
        // Other tokens see nothing.
        assert!(load_caps_from_disk("other").is_none());

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn available_actions_applies_intent_and_allowlist_gates() {
        let full = Caps {
            has_members_intent: true,
            has_message_content: true,
            detected: true,
        };
        // No gates → full admin set, canonical order.
        let actions = available_actions(&full, None, ADMIN_ACTIONS);
        assert_eq!(actions.len(), ADMIN_ACTIONS.len());
        assert!(actions.contains(&"member_info".to_string()));

        // Missing GUILD_MEMBERS intent hides member_info/search_members.
        let no_members = Caps {
            has_members_intent: false,
            has_message_content: true,
            detected: true,
        };
        let actions = available_actions(&no_members, None, ALL_ACTIONS);
        assert!(!actions.contains(&"member_info".to_string()));
        assert!(!actions.contains(&"search_members".to_string()));
        assert!(actions.contains(&"fetch_messages".to_string()));

        // Allowlist keeps only listed actions.
        let allowlist = vec!["list_guilds".to_string(), "fetch_messages".to_string()];
        let actions = available_actions(&full, Some(&allowlist), ALL_ACTIONS);
        assert_eq!(
            actions,
            vec!["list_guilds".to_string(), "fetch_messages".to_string()]
        );
    }

    #[test]
    fn build_schema_shape_and_content_note() {
        let caps = Caps::permissive();
        assert!(build_schema(&[], &caps, "discord").is_none());

        let actions = vec!["fetch_messages".to_string(), "create_thread".to_string()];
        let schema = build_schema(&actions, &caps, "discord").unwrap();
        assert_eq!(schema["properties"]["action"]["enum"], json!(actions));
        assert_eq!(schema["required"], json!(["action"]));
        let description = schema["__description"].as_str().unwrap();
        assert!(description.contains("fetch_messages(channel_id)"));
        // Undetected caps → no MESSAGE_CONTENT note.
        assert!(!description.contains("MESSAGE_CONTENT"));

        // Detected + missing MESSAGE_CONTENT annotates affected actions.
        let caps = Caps {
            has_members_intent: true,
            has_message_content: false,
            detected: true,
        };
        let schema = build_schema(&actions, &caps, "discord").unwrap();
        let description = schema["__description"].as_str().unwrap();
        assert!(description.contains("NOTE: Bot does NOT have the MESSAGE_CONTENT"));
    }

    #[test]
    fn enrich_403_maps_known_and_unknown_actions() {
        let hint = enrich_403("add_role", "{\"code\":50013}");
        assert!(hint.contains("MANAGE_ROLES"), "{hint}");
        assert!(hint.contains("50013"));
        let plain = enrich_403("list_guilds", "denied");
        assert!(plain.starts_with("Discord API 403 (forbidden) on 'list_guilds'."));
        assert!(!plain.contains("MANAGE_ROLES"));
    }

    #[tokio::test]
    async fn run_action_without_token_fails_fast() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = std::env::var("DISCORD_BOT_TOKEN").ok();
        std::env::remove_var("DISCORD_BOT_TOKEN");
        let err = run_discord_action("list_guilds", &json!({}), CORE_ACTIONS, "discord")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("DISCORD_BOT_TOKEN"), "{err}");
        match prev {
            Some(v) => std::env::set_var("DISCORD_BOT_TOKEN", v),
            None => std::env::remove_var("DISCORD_BOT_TOKEN"),
        }
    }

    #[tokio::test]
    async fn run_action_validates_action_and_params_before_network() {
        let _guard = crate::models_dev::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        let prev_token = std::env::var("DISCORD_BOT_TOKEN").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        std::env::set_var("DISCORD_BOT_TOKEN", "test-token");

        // Unknown action lists the available set.
        let err = run_discord_action("explode", &json!({}), CORE_ACTIONS, "discord")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Unknown action: explode"), "{err}");
        assert!(err.to_string().contains("fetch_messages"), "{err}");

        // Missing required parameters are reported without any request.
        let err = run_discord_action("fetch_messages", &json!({}), CORE_ACTIONS, "discord")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Missing required parameters"),
            "{err}"
        );
        assert!(err.to_string().contains("channel_id"), "{err}");

        // Config allowlist gates at call time too (defense in depth).
        std::fs::write(
            tmp.path().join("config.toml"),
            "[discord]\nserver_actions = \"fetch_messages\"\n",
        )
        .unwrap();
        let err = run_discord_action(
            "create_thread",
            &json!({"channel_id": "1", "name": "t"}),
            CORE_ACTIONS,
            "discord",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("disabled by config"), "{err}");

        match prev_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
        match prev_token {
            Some(v) => std::env::set_var("DISCORD_BOT_TOKEN", v),
            None => std::env::remove_var("DISCORD_BOT_TOKEN"),
        }
    }

    #[test]
    fn action_args_defaults_match_hermes() {
        let args = ActionArgs::from_value(&json!({}));
        assert_eq!(args.limit, 50);
        assert_eq!(args.auto_archive_duration, 1440);
        assert!(args.guild_id.is_empty());
        let args = ActionArgs::from_value(&json!({"limit": 200, "auto_archive_duration": 60}));
        assert_eq!(args.limit, 200);
        assert_eq!(args.auto_archive_duration, 60);
    }
}

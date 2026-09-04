//! MCP channel bridge (P260) — port of hermes `mcp_serve.py`: a stdio
//! JSON-RPC 2.0 MCP server that exposes messaging conversations to any
//! MCP client (Claude Code, Cursor, Codex, …). Matches OpenClaw's
//! 9-tool channel-bridge surface plus the hermes-specific
//! `channels_list`:
//!
//! `conversations_list`, `conversation_get`, `messages_read`,
//! `attachments_fetch`, `events_poll`, `events_wait`, `messages_send`,
//! `permissions_list_open`, `permissions_respond`, `channels_list`.
//!
//! Usage: `ulnclaw mcp serve [--verbose]`.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

const QUEUE_LIMIT: usize = 1000;
const POLL_INTERVAL_MS: u64 = 200;
const PROTOCOL_VERSION: &str = "2025-06-18";

// ---------------------------------------------------------------------------
// Helpers (hermes module-level utilities)
// ---------------------------------------------------------------------------

/// hermes `_coerce_int` — clamp tool-boundary integers from arbitrary
/// MCP clients.
pub fn coerce_int(value: &Value, default: i64, minimum: i64, maximum: i64) -> i64 {
    let coerced = match value {
        Value::Number(n) => n.as_i64().unwrap_or(default),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(default),
        _ => default,
    };
    coerced.clamp(minimum, maximum)
}

/// hermes `_extract_attachments` (text-content half): `MEDIA:<path>`
/// tags recorded on stored messages.
pub fn extract_attachments(content: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let re = regex::Regex::new(r"MEDIA:\s*(\S+)").expect("static media regex");
    for caps in re.captures_iter(content) {
        out.push(json!({"type": "media", "path": caps.get(1).map(|m| m.as_str()).unwrap_or("")}));
    }
    out
}

fn iso_timestamp(epoch_secs: f64) -> String {
    if epoch_secs <= 0.0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(epoch_secs as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn opt_arg_str(args: &Value, key: &str) -> Option<String> {
    let value = arg_str(args, key);
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Split `platform-<name>-<chat>` session keys (hermes gateway routing
/// key format).
fn split_session_key(session_key: &str) -> (String, String) {
    let rest = session_key.strip_prefix("platform-").unwrap_or(session_key);
    match rest.split_once('-') {
        Some((platform, chat_id)) => (platform.to_string(), chat_id.to_string()),
        None => (rest.to_string(), String::new()),
    }
}

/// One conversation view shared by conversations_list / conversation_get
/// (hermes `_row_to_index_entry` output shape).
fn conversation_entry(row: &crate::session::sqlite::PlatformSessionRow) -> Value {
    let (key_platform, chat_id) = split_session_key(&row.session_key);
    let platform = if !key_platform.is_empty() {
        key_platform.clone()
    } else {
        row.source
            .strip_prefix("platform:")
            .unwrap_or(&row.source)
            .to_string()
    };
    let directory_entry = crate::channel_directory::channel_info(&platform, &chat_id);
    let display_name = row
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| directory_entry.as_ref().map(|e| e.name.clone()))
        .unwrap_or_default();
    let chat_type = directory_entry
        .as_ref()
        .map(|e| e.chat_type.clone())
        .unwrap_or_default();
    let updated = row.last_activity_at.unwrap_or(row.started_at);
    json!({
        "session_key": row.session_key,
        "session_id": row.id,
        "platform": platform,
        "chat_type": chat_type,
        "display_name": display_name,
        "chat_name": display_name,
        "chat_id": chat_id,
        "user_name": row.user_id.clone().unwrap_or_default(),
        "created_at": iso_timestamp(row.started_at),
        "updated_at": iso_timestamp(updated),
        "input_tokens": row.input_tokens,
        "output_tokens": row.output_tokens,
        "total_tokens": row.input_tokens + row.output_tokens,
    })
}

fn load_platform_rows(limit: usize) -> Vec<crate::session::sqlite::PlatformSessionRow> {
    crate::session::sqlite::SqliteSessionStore::open_default()
        .ok()
        .and_then(|store| store.list_platform_sessions(limit).ok())
        .unwrap_or_default()
}

fn find_row(session_key: &str) -> Option<crate::session::sqlite::PlatformSessionRow> {
    load_platform_rows(1000)
        .into_iter()
        .find(|row| row.session_key == session_key)
}

// ---------------------------------------------------------------------------
// EventBridge — polls state.db for new messages, keeps an event queue
// (hermes `EventBridge`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct QueueEvent {
    cursor: u64,
    event_type: String,
    session_key: String,
    data: Value,
}

/// Background poller that watches the session database for new platform
/// messages and maintains an in-memory event queue with waiter support —
/// the ulnclaw equivalent of hermes' SQLite-polling EventBridge.
pub struct EventBridge {
    queue: Mutex<VecDeque<QueueEvent>>,
    cursor: AtomicU64,
    notify: tokio::sync::Notify,
    last_seen: Mutex<HashMap<String, f64>>,
    db_mtime: Mutex<f64>,
    pending_approvals: Mutex<HashMap<String, Value>>,
    running: AtomicBool,
}

impl EventBridge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            cursor: AtomicU64::new(0),
            notify: tokio::sync::Notify::new(),
            last_seen: Mutex::new(HashMap::new()),
            db_mtime: Mutex::new(0.0),
            pending_approvals: Mutex::new(HashMap::new()),
            running: AtomicBool::new(false),
        })
    }

    /// Start the background polling task. Snapshot existing history
    /// FIRST so pre-existing messages are not replayed as events
    /// (hermes #13414 baseline semantics).
    pub fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        self.poll_once(true);
        let this = self.clone();
        tokio::spawn(async move {
            while this.running.load(Ordering::SeqCst) {
                this.poll_once(false);
                tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
            }
        });
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn db_mtime() -> f64 {
        let path = crate::config::ulnclaw_home().join("state.db");
        std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    fn poll_once(&self, baseline: bool) {
        let mtime = Self::db_mtime();
        {
            let mut cached = self.db_mtime.lock().unwrap();
            if !baseline && mtime == *cached {
                return; // Nothing changed since last poll — skip entirely.
            }
            *cached = mtime;
        }
        let Ok(store) = crate::session::sqlite::SqliteSessionStore::open_default() else {
            return;
        };
        let Ok(rows) = store.list_platform_sessions(500) else {
            return;
        };
        for row in rows {
            let Ok(messages) = store.load_message_rows(&row.id) else {
                continue;
            };
            let seen = self
                .last_seen
                .lock()
                .unwrap()
                .get(&row.session_key)
                .copied()
                .unwrap_or(0.0);
            let mut latest = seen;
            let mut fresh: Vec<&crate::session::sqlite::MessageRow> = Vec::new();
            for message in &messages {
                if message.timestamp > latest {
                    latest = message.timestamp;
                }
                if baseline {
                    continue;
                }
                if message.role != "user" && message.role != "assistant" {
                    continue;
                }
                if message.timestamp > seen && !message.content.trim().is_empty() {
                    fresh.push(message);
                }
            }
            for message in fresh {
                let content: String = message.content.chars().take(500).collect();
                self.enqueue(
                    "message",
                    &row.session_key,
                    json!({
                        "role": message.role,
                        "content": content,
                        "timestamp": message.timestamp.to_string(),
                        "message_id": message.id.to_string(),
                    }),
                );
            }
            if latest > seen {
                self.last_seen
                    .lock()
                    .unwrap()
                    .insert(row.session_key.clone(), latest);
            }
        }
    }

    /// Add an event to the queue and wake waiters (hermes `_enqueue`).
    pub fn enqueue(&self, event_type: &str, session_key: &str, data: Value) {
        let cursor = self.cursor.fetch_add(1, Ordering::SeqCst) + 1;
        let mut queue = self.queue.lock().unwrap();
        queue.push_back(QueueEvent {
            cursor,
            event_type: event_type.to_string(),
            session_key: session_key.to_string(),
            data,
        });
        while queue.len() > QUEUE_LIMIT {
            queue.pop_front();
        }
        drop(queue);
        self.notify.notify_waiters();
    }

    fn event_json(event: &QueueEvent) -> Value {
        let mut value = event.data.clone();
        if let Some(object) = value.as_object_mut() {
            object.insert("cursor".to_string(), json!(event.cursor));
            object.insert("type".to_string(), json!(event.event_type));
            object.insert("session_key".to_string(), json!(event.session_key));
        }
        value
    }

    fn find_event(&self, after_cursor: u64, session_key: Option<&str>) -> Option<Value> {
        let queue = self.queue.lock().unwrap();
        queue
            .iter()
            .find(|event| {
                event.cursor > after_cursor
                    && session_key.map_or(true, |key| event.session_key == key)
            })
            .map(Self::event_json)
    }

    /// hermes `poll_events`.
    pub fn poll_events(&self, after_cursor: u64, session_key: Option<&str>, limit: usize) -> Value {
        let queue = self.queue.lock().unwrap();
        let events: Vec<Value> = queue
            .iter()
            .filter(|event| {
                event.cursor > after_cursor
                    && session_key.map_or(true, |key| event.session_key == key)
            })
            .take(limit)
            .map(Self::event_json)
            .collect();
        let next_cursor = events
            .last()
            .and_then(|event| event.get("cursor").and_then(Value::as_u64))
            .unwrap_or(after_cursor);
        json!({"events": events, "next_cursor": next_cursor})
    }

    /// hermes `wait_for_event` — long-poll with waiter wakeup.
    pub async fn wait_for_event(
        &self,
        after_cursor: u64,
        session_key: Option<&str>,
        timeout_ms: u64,
    ) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some(event) = self.find_event(after_cursor, session_key) {
                return Some(event);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let wait = (deadline - now).min(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// hermes `list_pending_approvals` — approvals observed during this
    /// bridge session (live-session only).
    pub fn list_pending_approvals(&self) -> Vec<Value> {
        let approvals = self.pending_approvals.lock().unwrap();
        let mut list: Vec<Value> = approvals.values().cloned().collect();
        list.sort_by(|a, b| {
            a.get("created_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(b.get("created_at").and_then(Value::as_str).unwrap_or(""))
        });
        list
    }

    /// hermes `respond_to_approval` — best-effort resolution without
    /// gateway IPC.
    pub fn respond_to_approval(&self, approval_id: &str, decision: &str) -> Value {
        let approval = self.pending_approvals.lock().unwrap().remove(approval_id);
        let Some(approval) = approval else {
            return json!({"error": format!("Approval not found: {approval_id}")});
        };
        let session_key = approval
            .get("session_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.enqueue(
            "approval_resolved",
            &session_key,
            json!({"approval_id": approval_id, "decision": decision}),
        );
        json!({"resolved": true, "approval_id": approval_id, "decision": decision})
    }
}

// ---------------------------------------------------------------------------
// Tool implementations (hermes `create_mcp_server` bodies)
// ---------------------------------------------------------------------------

fn tool_conversations_list(args: &Value) -> String {
    let platform = opt_arg_str(args, "platform");
    let search = opt_arg_str(args, "search");
    let limit = coerce_int(args.get("limit").unwrap_or(&json!(50)), 50, 1, 200) as usize;

    let mut conversations: Vec<Value> = Vec::new();
    for row in load_platform_rows(1000) {
        let entry = conversation_entry(&row);
        if let Some(filter) = &platform {
            if entry["platform"].as_str().unwrap_or("").to_lowercase() != filter.to_lowercase() {
                continue;
            }
        }
        if let Some(needle) = &search {
            let needle = needle.to_lowercase();
            let haystack = format!(
                "{} {} {}",
                entry["display_name"].as_str().unwrap_or(""),
                entry["chat_name"].as_str().unwrap_or(""),
                entry["session_key"].as_str().unwrap_or(""),
            )
            .to_lowercase();
            if !haystack.contains(&needle) {
                continue;
            }
        }
        conversations.push(entry);
    }
    conversations.sort_by(|a, b| {
        b["updated_at"]
            .as_str()
            .unwrap_or("")
            .cmp(a["updated_at"].as_str().unwrap_or(""))
    });
    conversations.truncate(limit);
    json!({"count": conversations.len(), "conversations": conversations}).to_string()
}

fn tool_conversation_get(args: &Value) -> String {
    let session_key = arg_str(args, "session_key");
    match find_row(&session_key) {
        Some(row) => conversation_entry(&row).to_string(),
        None => json!({"error": format!("Conversation not found: {session_key}")}).to_string(),
    }
}

fn tool_messages_read(args: &Value) -> String {
    let session_key = arg_str(args, "session_key");
    let limit = coerce_int(args.get("limit").unwrap_or(&json!(50)), 50, 1, 200) as usize;
    let Some(row) = find_row(&session_key) else {
        return json!({"error": format!("Conversation not found: {session_key}")}).to_string();
    };
    let Ok(store) = crate::session::sqlite::SqliteSessionStore::open_default() else {
        return json!({"error": "Session database unavailable"}).to_string();
    };
    let Ok(all) = store.load_message_rows(&row.id) else {
        return json!({"error": "Failed to read messages"}).to_string();
    };
    let filtered: Vec<Value> = all
        .iter()
        .filter(|message| {
            (message.role == "user" || message.role == "assistant")
                && !message.content.trim().is_empty()
        })
        .map(|message| {
            let content: String = message.content.chars().take(2000).collect();
            json!({
                "id": message.id.to_string(),
                "role": message.role,
                "content": content,
                "timestamp": message.timestamp,
            })
        })
        .collect();
    let total = filtered.len();
    let messages: Vec<Value> = filtered
        .into_iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    json!({
        "session_key": session_key,
        "count": messages.len(),
        "total_in_session": total,
        "messages": messages,
    })
    .to_string()
}

fn tool_attachments_fetch(args: &Value) -> String {
    let session_key = arg_str(args, "session_key");
    let message_id = arg_str(args, "message_id");
    let Some(row) = find_row(&session_key) else {
        return json!({"error": format!("Conversation not found: {session_key}")}).to_string();
    };
    let Ok(store) = crate::session::sqlite::SqliteSessionStore::open_default() else {
        return json!({"error": "Session database unavailable"}).to_string();
    };
    let Ok(all) = store.load_message_rows(&row.id) else {
        return json!({"error": "Failed to read messages"}).to_string();
    };
    let Some(message) = all
        .iter()
        .find(|message| message.id.to_string() == message_id)
    else {
        return json!({"error": format!("Message not found: {message_id}")}).to_string();
    };
    let attachments = extract_attachments(&message.content);
    json!({
        "message_id": message_id,
        "count": attachments.len(),
        "attachments": attachments,
    })
    .to_string()
}

fn tool_events_poll(bridge: &EventBridge, args: &Value) -> String {
    let after = coerce_int(
        args.get("after_cursor").unwrap_or(&json!(0)),
        0,
        0,
        1_000_000_000_000_000_000,
    ) as u64;
    let limit = coerce_int(args.get("limit").unwrap_or(&json!(20)), 20, 1, 200) as usize;
    bridge
        .poll_events(after, opt_arg_str(args, "session_key").as_deref(), limit)
        .to_string()
}

async fn tool_events_wait(bridge: &EventBridge, args: &Value) -> String {
    let after = coerce_int(
        args.get("after_cursor").unwrap_or(&json!(0)),
        0,
        0,
        1_000_000_000_000_000_000,
    ) as u64;
    let timeout_ms = coerce_int(
        args.get("timeout_ms").unwrap_or(&json!(30000)),
        30000,
        0,
        300000,
    ) as u64;
    match bridge
        .wait_for_event(
            after,
            opt_arg_str(args, "session_key").as_deref(),
            timeout_ms,
        )
        .await
    {
        Some(event) => json!({"event": event}).to_string(),
        None => json!({"event": null, "reason": "timeout"}).to_string(),
    }
}

async fn tool_messages_send(args: &Value) -> String {
    let target = arg_str(args, "target");
    let message = arg_str(args, "message");
    if target.is_empty() || message.is_empty() {
        return json!({"error": "Both target and message are required"}).to_string();
    }
    crate::send_message_tool::run_send_message(json!({
        "action": "send",
        "target": target,
        "message": message,
    }))
    .await
    .to_string()
}

fn tool_channels_list(args: &Value) -> String {
    let platform = opt_arg_str(args, "platform");
    let entries = crate::channel_directory::list_channels(platform.as_deref());
    if entries.is_empty() {
        // No discovered directory yet — derive targets from the sessions
        // index (hermes fallback path).
        let mut targets: Vec<Value> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for row in load_platform_rows(1000) {
            let entry = conversation_entry(&row);
            let chat_platform = entry["platform"].as_str().unwrap_or("").to_string();
            let chat_id = entry["chat_id"].as_str().unwrap_or("").to_string();
            if chat_platform.is_empty() || chat_id.is_empty() {
                continue;
            }
            if let Some(filter) = &platform {
                if chat_platform.to_lowercase() != filter.to_lowercase() {
                    continue;
                }
            }
            let target = format!("{chat_platform}:{chat_id}");
            if !seen.insert(target.clone()) {
                continue;
            }
            targets.push(json!({
                "target": target,
                "platform": chat_platform,
                "name": entry["display_name"].as_str().unwrap_or(""),
                "chat_type": entry["chat_type"].as_str().unwrap_or(""),
            }));
        }
        return json!({"count": targets.len(), "channels": targets}).to_string();
    }
    let channels: Vec<Value> = entries
        .iter()
        .map(|(plat, entry)| {
            json!({
                "target": if entry.id.is_empty() { plat.clone() } else { format!("{plat}:{}", entry.id) },
                "platform": plat,
                "name": entry.name,
                "chat_type": entry.chat_type,
            })
        })
        .collect();
    json!({"count": channels.len(), "channels": channels}).to_string()
}

fn tool_permissions_list_open(bridge: &EventBridge) -> String {
    let approvals = bridge.list_pending_approvals();
    json!({"count": approvals.len(), "approvals": approvals}).to_string()
}

fn tool_permissions_respond(bridge: &EventBridge, args: &Value) -> String {
    let id = arg_str(args, "id");
    let decision = arg_str(args, "decision");
    if !matches!(decision.as_str(), "allow-once" | "allow-always" | "deny") {
        return json!({
            "error": format!(
                "Invalid decision: {decision}. Must be allow-once, allow-always, or deny"
            )
        })
        .to_string();
    }
    bridge.respond_to_approval(&id, &decision).to_string()
}

async fn dispatch_tool(bridge: &Arc<EventBridge>, name: &str, args: &Value) -> String {
    match name {
        "conversations_list" => tool_conversations_list(args),
        "conversation_get" => tool_conversation_get(args),
        "messages_read" => tool_messages_read(args),
        "attachments_fetch" => tool_attachments_fetch(args),
        "events_poll" => tool_events_poll(bridge, args),
        "events_wait" => tool_events_wait(bridge, args).await,
        "messages_send" => tool_messages_send(args).await,
        "channels_list" => tool_channels_list(args),
        "permissions_list_open" => tool_permissions_list_open(bridge),
        "permissions_respond" => tool_permissions_respond(bridge, args),
        other => json!({"error": format!("Unknown tool: {other}")}).to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tool schemas (tools/list)
// ---------------------------------------------------------------------------

fn tool_definitions() -> Value {
    json!([
        {
            "name": "conversations_list",
            "description": "List active messaging conversations across connected platforms. Returns conversations with their session keys (needed for messages_read), platform, chat type, display name, and last activity time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "platform": {"type": "string", "description": "Filter by platform name (telegram, discord, slack, etc.)"},
                    "limit": {"type": "integer", "description": "Maximum number of conversations to return (default 50)", "default": 50},
                    "search": {"type": "string", "description": "Optional text to filter conversations by name"}
                }
            }
        },
        {
            "name": "conversation_get",
            "description": "Get detailed info about one conversation by its session key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_key": {"type": "string", "description": "The session key from conversations_list"}
                },
                "required": ["session_key"]
            }
        },
        {
            "name": "messages_read",
            "description": "Read recent messages from a conversation. Returns the message history in chronological order with role, content, and timestamp for each message.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_key": {"type": "string", "description": "The session key from conversations_list"},
                    "limit": {"type": "integer", "description": "Maximum number of messages to return (default 50, most recent)", "default": 50}
                },
                "required": ["session_key"]
            }
        },
        {
            "name": "attachments_fetch",
            "description": "List non-text attachments for a message in a conversation. Extracts media files referenced by the specified message.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_key": {"type": "string", "description": "The session key from conversations_list"},
                    "message_id": {"type": "string", "description": "The message ID from messages_read"}
                },
                "required": ["session_key", "message_id"]
            }
        },
        {
            "name": "events_poll",
            "description": "Poll for new conversation events since a cursor position. Returns events that have occurred since the given cursor. Use the returned next_cursor value for subsequent polls. Event types: message, approval_requested, approval_resolved.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "after_cursor": {"type": "integer", "description": "Return events after this cursor (0 for all)", "default": 0},
                    "session_key": {"type": "string", "description": "Optional filter to one conversation"},
                    "limit": {"type": "integer", "description": "Maximum events to return (default 20)", "default": 20}
                }
            }
        },
        {
            "name": "events_wait",
            "description": "Wait for the next conversation event (long-poll). Blocks until a matching event arrives or the timeout expires. Use this for near-real-time event delivery without polling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "after_cursor": {"type": "integer", "description": "Wait for events after this cursor", "default": 0},
                    "session_key": {"type": "string", "description": "Optional filter to one conversation"},
                    "timeout_ms": {"type": "integer", "description": "Maximum wait time in milliseconds (default 30000)", "default": 30000}
                }
            }
        },
        {
            "name": "messages_send",
            "description": "Send a message to a platform conversation. The target format is \"platform:chat_id\" — same format used by the channels_list tool. You can also use human-friendly channel names that will be resolved automatically. Examples: target=\"telegram:6308981865\", target=\"discord:#general\", target=\"slack:#engineering\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Platform target in \"platform:identifier\" format"},
                    "message": {"type": "string", "description": "The message text to send"}
                },
                "required": ["target", "message"]
            }
        },
        {
            "name": "channels_list",
            "description": "List available messaging channels and targets across platforms. Returns channels that you can send messages to. The target strings returned here can be used directly with the messages_send tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "platform": {"type": "string", "description": "Filter by platform name (telegram, discord, slack, etc.)"}
                }
            }
        },
        {
            "name": "permissions_list_open",
            "description": "List pending approval requests observed during this bridge session. Approvals are live-session only — older approvals from before the bridge connected are not included.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "permissions_respond",
            "description": "Respond to a pending approval request.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "The approval ID from permissions_list_open"},
                    "decision": {"type": "string", "enum": ["allow-once", "allow-always", "deny"], "description": "The approval decision"}
                },
                "required": ["id", "decision"]
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 stdio transport (minimal MCP server)
// ---------------------------------------------------------------------------

fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handle one inbound JSON-RPC line; `None` for notifications.
pub async fn handle_line(line: &str, bridge: &Arc<EventBridge>) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            return Some(error_response(Value::Null, -32700, "Parse error"));
        }
    };
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = request.get("params").cloned().unwrap_or(json!({}));
    let is_notification = id.is_none();

    match method.as_str() {
        "initialize" => {
            let client_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            Some(success_response(
                id.unwrap_or(Value::Null),
                json!({
                    "protocolVersion": client_version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "ulnclaw",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": "ulnclaw messaging bridge. Use these tools to interact with conversations across Telegram, Discord, Slack, WhatsApp, Signal, Matrix, and other connected platforms.",
                }),
            ))
        }
        "ping" => Some(success_response(id.unwrap_or(Value::Null), json!({}))),
        "tools/list" => Some(success_response(
            id.unwrap_or(Value::Null),
            json!({"tools": tool_definitions()}),
        )),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let known = tool_definitions()
                .as_array()
                .map(|tools| tools.iter().any(|tool| tool["name"].as_str() == Some(name)))
                .unwrap_or(false);
            if !known {
                return Some(success_response(
                    id.unwrap_or(Value::Null),
                    json!({
                        "content": [{"type": "text", "text": json!({"error": format!("Unknown tool: {name}")}).to_string()}],
                        "isError": true,
                    }),
                ));
            }
            let text = dispatch_tool(bridge, name, &arguments).await;
            Some(success_response(
                id.unwrap_or(Value::Null),
                json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": false,
                }),
            ))
        }
        _ => {
            if is_notification || method.starts_with("notifications/") {
                None
            } else {
                Some(error_response(
                    id.unwrap_or(Value::Null),
                    -32601,
                    &format!("Method not found: {method}"),
                ))
            }
        }
    }
}

/// Run the MCP server on stdio until EOF (hermes `run_mcp_server`).
pub async fn run_stdio(verbose: bool) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let bridge = EventBridge::new();
    bridge.start();

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());

    let result: std::io::Result<()> = async {
        while let Some(line) = lines.next_line().await? {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            if verbose {
                eprintln!("[mcp-serve] <- {line}");
            }
            let Some(response) = handle_line(&line, &bridge).await else {
                continue; // notification — no reply
            };
            let mut out = serde_json::to_string(&response).unwrap_or_default();
            out.push('\n');
            if verbose {
                eprintln!("[mcp-serve] -> {}", out.trim_end_matches('\n'));
            }
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }
        Ok(())
    }
    .await;

    bridge.stop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_int_clamps_and_defaults() {
        assert_eq!(coerce_int(&json!(5), 50, 1, 200), 5);
        assert_eq!(coerce_int(&json!(9999), 50, 1, 200), 200);
        assert_eq!(coerce_int(&json!(0), 50, 1, 200), 1);
        assert_eq!(coerce_int(&json!("oops"), 50, 1, 200), 50);
        assert_eq!(coerce_int(&json!("17"), 50, 1, 200), 17);
        assert_eq!(coerce_int(&Value::Null, 50, 1, 200), 50);
    }

    #[test]
    fn attachments_extract_media_tags() {
        let attachments = extract_attachments("Here you go\nMEDIA: /tmp/report.pdf\nMEDIA: /x.png");
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0]["path"], "/tmp/report.pdf");
        assert!(extract_attachments("plain text").is_empty());
    }

    #[test]
    fn session_key_split() {
        assert_eq!(
            split_session_key("platform-telegram--100123"),
            ("telegram".into(), "-100123".into())
        );
        assert_eq!(
            split_session_key("platform-discord-998877"),
            ("discord".into(), "998877".into())
        );
    }

    #[tokio::test]
    async fn initialize_and_tools_list_shapes() {
        let bridge = EventBridge::new();
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            &bridge,
        )
        .await
        .unwrap();
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(response["result"]["serverInfo"]["name"], "ulnclaw");

        let response = handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &bridge)
            .await
            .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 10);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"messages_send"));
        assert!(names.contains(&"channels_list"));
    }

    #[tokio::test]
    async fn notifications_get_no_reply_and_unknown_method_errors() {
        let bridge = EventBridge::new();
        assert!(handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &bridge
        )
        .await
        .is_none());
        let response = handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"bogus/one"}"#, &bridge)
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn event_bridge_enqueue_poll_wait() {
        let bridge = EventBridge::new();
        bridge.enqueue("message", "platform-telegram-1", json!({"content": "hi"}));
        let polled = bridge.poll_events(0, None, 10);
        assert_eq!(polled["events"].as_array().unwrap().len(), 1);
        assert_eq!(polled["next_cursor"], 1);

        // Wait resolves immediately when an event already matches.
        let event = bridge.wait_for_event(0, None, 50).await.unwrap();
        assert_eq!(event["type"], "message");

        // Wait times out when nothing new arrives.
        let none = bridge.wait_for_event(1, None, 60).await;
        assert!(none.is_none());

        // Session filter respected.
        let filtered = bridge.poll_events(0, Some("platform-discord-9"), 10);
        assert!(filtered["events"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn permissions_surface_validates_decisions() {
        let bridge = EventBridge::new();
        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"permissions_respond","arguments":{"id":"a1","decision":"maybe"}}}"#,
            &bridge,
        )
        .await
        .unwrap();
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Invalid decision"));

        let response = handle_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"permissions_list_open","arguments":{}}}"#,
            &bridge,
        )
        .await
        .unwrap();
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"count\":0"));
    }

    struct HomeGuard {
        prev: Option<String>,
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(value) => std::env::set_var("ULNCLAW_HOME", value),
                None => std::env::remove_var("ULNCLAW_HOME"),
            }
        }
    }

    #[tokio::test]
    async fn conversations_and_messages_roundtrip() {
        let _env = crate::models_dev::test_env_lock();
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-mcp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let guard = HomeGuard {
            prev: std::env::var("ULNCLAW_HOME").ok(),
        };
        std::env::set_var("ULNCLAW_HOME", &dir);

        let store = crate::session::sqlite::SqliteSessionStore::open_default().unwrap();
        store
            .create_named_session("platform-telegram-42", "platform:telegram", None, None)
            .unwrap();
        store
            .append_message(
                "platform-telegram-42",
                &crate::provider::Message {
                    role: crate::provider::Role::User,
                    content: Some("hello bot MEDIA: /tmp/a.png".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();
        store
            .append_message(
                "platform-telegram-42",
                &crate::provider::Message {
                    role: crate::provider::Role::Assistant,
                    content: Some("hi there".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();

        let listed = tool_conversations_list(&json!({}));
        assert!(listed.contains("platform-telegram-42"));
        assert!(listed.contains("\"platform\":\"telegram\""));

        let detail = tool_conversation_get(&json!({"session_key": "platform-telegram-42"}));
        assert!(detail.contains("telegram"));

        let read = tool_messages_read(&json!({"session_key": "platform-telegram-42"}));
        let parsed: Value = serde_json::from_str(&read).unwrap();
        assert_eq!(parsed["count"], 2);
        let first_id = parsed["messages"][0]["id"].as_str().unwrap().to_string();

        let attachments = tool_attachments_fetch(
            &json!({"session_key": "platform-telegram-42", "message_id": first_id}),
        );
        assert!(attachments.contains("/tmp/a.png"));

        let missing = tool_conversation_get(&json!({"session_key": "platform-telegram-999"}));
        assert!(missing.contains("not found"));

        drop(store);
        drop(guard);
    }
}

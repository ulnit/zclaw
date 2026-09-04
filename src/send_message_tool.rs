//! send_message tool (P259) — port of hermes `tools/send_message_tool.py`:
//! cross-channel messaging through the live platform adapters. Actions:
//! `send` (default), `list`, `react`, `unreact`. Targets accept
//! `platform` (home channel), `platform:chat_id`, `platform:#channel-name`,
//! `platform:chat_id:thread_id` (Telegram topics / Discord threads); media
//! rides inside the message as `MEDIA:<path>` tags (hermes extract_media).

use regex::Regex;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::LazyLock;

// -- Target shape guards (hermes module-level regexes) ----------------------

static TELEGRAM_TOPIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(-?\d+)(?::(\d+))?\s*$").unwrap());
static TELEGRAM_USERNAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@[A-Za-z0-9_]{4,32}$").unwrap());
static FEISHU_TARGET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*((?:oc|ou|on|chat|open)_[-A-Za-z0-9]+)(?::([-A-Za-z0-9_]+))?\s*$").unwrap()
});
static SLACK_THREAD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([CGD][A-Z0-9]{8,}):([^\s:]+)\s*$").unwrap());
static SLACK_TARGET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([CGD][A-Z0-9]{8,})\s*$").unwrap());
static SLACK_USER_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(U[A-Z0-9]{8,})\s*$").unwrap());
static SLACK_USER_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*@([A-Za-z0-9._-]{1,80})\s*$").unwrap());
static SLACK_MENTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*<@(U[A-Z0-9]{8,})(?:\|[^>]+)?>\s*$").unwrap());
static WEIXIN_TARGET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*((?:wxid|gh|v\d+|wm|wb)_[A-Za-z0-9_-]+|[A-Za-z0-9._-]+@chatroom|filehelper)\s*$",
    )
    .unwrap()
});
static YUANBAO_TARGET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*((?:group|direct):[^:]+)\s*$").unwrap());
static E164_TARGET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\+(\d{7,15})\s*$").unwrap());
static PHOTON_DM_GUID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^any;-;\+\d{6,}$").unwrap());
static WHATSAPP_JID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*[\w-]+@(?:g\.us|s\.whatsapp\.net|lid|broadcast|newsletter)\s*$").unwrap()
});
static EMAIL_TARGET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\s*$").unwrap()
});

/// Platforms addressed by phone number in E.164 form (hermes
/// `_PHONE_PLATFORMS`).
const PHONE_PLATFORMS: &[&str] = &["photon", "signal", "sms", "whatsapp"];

fn is_digit_string(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit())
}

/// hermes `_parse_target_ref`: split a tool target reference into
/// `(chat_id, thread_id, is_explicit)`.
pub fn parse_target_ref(
    platform: &str,
    target_ref: &str,
) -> (Option<String>, Option<String>, bool) {
    match platform {
        "telegram" => {
            if let Some(caps) = TELEGRAM_TOPIC_RE.captures(target_ref) {
                return (
                    caps.get(1).map(|m| m.as_str().to_string()),
                    caps.get(2).map(|m| m.as_str().to_string()),
                    true,
                );
            }
            let trimmed = target_ref.trim();
            if TELEGRAM_USERNAME_RE.is_match(trimmed) {
                return (Some(trimmed.to_string()), None, true);
            }
        }
        "feishu" => {
            if let Some(caps) = FEISHU_TARGET_RE.captures(target_ref) {
                return (
                    caps.get(1).map(|m| m.as_str().to_string()),
                    caps.get(2).map(|m| m.as_str().to_string()),
                    true,
                );
            }
        }
        "discord" => {
            if let Some(caps) = TELEGRAM_TOPIC_RE.captures(target_ref) {
                return (
                    caps.get(1).map(|m| m.as_str().to_string()),
                    caps.get(2).map(|m| m.as_str().to_string()),
                    true,
                );
            }
        }
        "slack" => {
            if let Some(caps) = SLACK_THREAD_RE.captures(target_ref) {
                return (
                    caps.get(1).map(|m| m.as_str().to_string()),
                    caps.get(2).map(|m| m.as_str().to_string()),
                    true,
                );
            }
            if let Some(caps) = SLACK_TARGET_RE.captures(target_ref) {
                return (caps.get(1).map(|m| m.as_str().to_string()), None, true);
            }
            if let Some(caps) = SLACK_USER_ID_RE
                .captures(target_ref)
                .or_else(|| SLACK_MENTION_RE.captures(target_ref))
            {
                return (
                    Some(format!(
                        "user:{}",
                        caps.get(1).map(|m| m.as_str()).unwrap_or("")
                    )),
                    None,
                    true,
                );
            }
            if let Some(caps) = SLACK_USER_NAME_RE.captures(target_ref) {
                return (
                    Some(format!(
                        "user_name:{}",
                        caps.get(1).map(|m| m.as_str()).unwrap_or("")
                    )),
                    None,
                    true,
                );
            }
        }
        "matrix" => {
            let trimmed = target_ref.trim();
            if let Some(idx) = trimmed.rfind(":$") {
                if idx > 0 {
                    return (
                        Some(trimmed[..idx].to_string()),
                        Some(trimmed[idx + 1..].to_string()),
                        true,
                    );
                }
            }
        }
        "weixin" => {
            if let Some(caps) = WEIXIN_TARGET_RE.captures(target_ref) {
                return (caps.get(1).map(|m| m.as_str().to_string()), None, true);
            }
        }
        "yuanbao" => {
            if let Some(caps) = YUANBAO_TARGET_RE.captures(target_ref) {
                return (caps.get(1).map(|m| m.as_str().to_string()), None, true);
            }
            let trimmed = target_ref.trim();
            if is_digit_string(trimmed) {
                return (Some(format!("group:{trimmed}")), None, true);
            }
            return (None, None, false);
        }
        "ntfy" => {
            let topic = target_ref.trim();
            if !topic.is_empty() {
                return (Some(topic.to_string()), None, true);
            }
        }
        "email" => {
            if EMAIL_TARGET_RE.is_match(target_ref) {
                return (Some(target_ref.trim().to_string()), None, true);
            }
        }
        "whatsapp" => {
            if WHATSAPP_JID_RE.is_match(target_ref) {
                return (Some(target_ref.trim().to_string()), None, true);
            }
        }
        _ => {}
    }
    let stripped = target_ref.trim();
    if platform == "signal" {
        if let Some(group_id) = stripped.strip_prefix("group:") {
            let group_id = group_id.trim();
            if !group_id.is_empty() {
                return (Some(format!("group:{group_id}")), None, true);
            }
            return (None, None, false);
        }
    }
    if PHONE_PLATFORMS.contains(&platform) && E164_TARGET_RE.is_match(target_ref) {
        return (Some(stripped.to_string()), None, true);
    }
    if platform == "photon" && PHOTON_DM_GUID_RE.is_match(stripped) {
        return (Some(stripped.to_string()), None, true);
    }
    if stripped
        .strip_prefix('-')
        .map(is_digit_string)
        .unwrap_or(false)
        || is_digit_string(stripped)
    {
        return (Some(target_ref.to_string()), None, true);
    }
    if platform == "matrix" && (target_ref.starts_with('!') || target_ref.starts_with('@')) {
        return (Some(target_ref.to_string()), None, true);
    }
    if platform == "xmpp" && target_ref.contains('@') {
        return (Some(target_ref.to_string()), None, true);
    }
    (None, None, false)
}

/// hermes `_HOME_CHANNEL_ENV_OVERRIDES` — most platforms read
/// `<PLATFORM>_HOME_CHANNEL`; email reads `EMAIL_HOME_ADDRESS`.
pub fn home_channel_env(platform: &str) -> String {
    if platform == "email" {
        "EMAIL_HOME_ADDRESS".to_string()
    } else {
        format!("{}_HOME_CHANNEL", platform.to_uppercase())
    }
}

fn home_channel(platform: &str) -> Option<String> {
    crate::config::get_env_value(&home_channel_env(platform))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// hermes `_describe_media_for_mirror` — human-readable fallback text when
/// a message is media-only and the platform has no native media path.
pub fn describe_media_for_mirror(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    if paths.len() == 1 {
        let ext = paths[0]
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        let kind = match ext.as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "gif" => "image",
            "mp4" | "mov" | "avi" | "mkv" | "webm" | "3gp" => "video",
            "ogg" | "opus" => return "[Sent voice message]".to_string(),
            "mp3" | "m2a" | "wav" | "m4a" | "flac" => "audio",
            _ => "document",
        };
        return format!("[Sent {kind} attachment]");
    }
    format!("[Sent {} media attachments]", paths.len())
}

fn error_payload(message: &str) -> Value {
    json!({"error": message})
}

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Split `target` into `(platform, reference)` at the first colon.
pub fn split_target(target: &str) -> (String, Option<String>) {
    let mut parts = target.splitn(2, ':');
    let platform = parts.next().unwrap_or("").trim().to_lowercase();
    let reference = parts
        .next()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    (platform, reference)
}

/// Entry point shared by the registered tool and tests (hermes
/// `send_message_tool`).
pub async fn run_send_message(args: Value) -> Value {
    match arg_str(&args, "action").as_str() {
        "list" => handle_list(),
        "react" => handle_react(&args, false).await,
        "unreact" => handle_react(&args, true).await,
        "" | "send" => handle_send(&args).await,
        other => error_payload(&format!("Unknown action: {other}")),
    }
}

/// hermes `_handle_list`.
fn handle_list() -> Value {
    json!({"targets": crate::channel_directory::format_directory_for_display()})
}

/// hermes `_handle_react` (remove=true → `_handle_react(args, remove=True)`).
async fn handle_react(args: &Value, remove: bool) -> Value {
    let target = arg_str(args, "target");
    let emoji = arg_str(args, "emoji");
    let mut message_id = arg_str(args, "message_id");
    if target.is_empty() || (!remove && emoji.is_empty()) {
        return error_payload(if remove {
            "'target' is required when action='unreact'"
        } else {
            "Both 'target' and 'emoji' are required when action='react'"
        });
    }
    let (platform, reference) = split_target(&target);
    let mut chat_id: Option<String> = None;
    if let Some(reference) = &reference {
        let (parsed, _thread, _explicit) = parse_target_ref(&platform, reference);
        chat_id =
            parsed.or_else(|| crate::channel_directory::resolve_by_name(&platform, reference));
        // Opaque platform-native ids match no parser pattern and no
        // directory entry — pass them through verbatim (hermes react
        // handler semantics); the adapter validates.
        if chat_id.is_none() {
            chat_id = Some(reference.clone());
        }
    }
    if chat_id.is_none() {
        match home_channel(&platform) {
            Some(home) => chat_id = Some(home),
            None => {
                return error_payload(&format!(
                    "No chat specified and no home channel set for {platform}. \
                     Use '{platform}:chat_id'."
                ))
            }
        }
    }
    let chat_id = chat_id.unwrap_or_default();
    let Some(sender) = crate::messaging::platform_sender(&platform) else {
        return error_payload(&format!(
            "Reactions require a live {platform} adapter in the running gateway \
             (not available from cron/standalone contexts)."
        ));
    };
    if message_id.is_empty() {
        message_id =
            crate::channel_directory::last_message_id_for(&platform, &chat_id).unwrap_or_default();
    }
    if message_id.is_empty() {
        return error_payload(&format!(
            "No message_id provided and no recent inbound message recorded for this \
             {platform} chat — pass message_id explicitly."
        ));
    }
    match sender
        .send_reaction(&chat_id, &emoji, &message_id, remove)
        .await
    {
        None => error_payload(&format!(
            "Platform '{platform}' does not support message reactions."
        )),
        Some(ok) => json!({"success": ok}),
    }
}

/// hermes `_handle_send`.
async fn handle_send(args: &Value) -> Value {
    let target = arg_str(args, "target");
    let message = arg_str(args, "message");
    if target.is_empty() || message.is_empty() {
        return error_payload("Both 'target' and 'message' are required when action='send'");
    }
    let (platform, reference) = split_target(&target);
    let mut chat_id: Option<String> = None;
    let mut is_explicit = false;
    if let Some(reference) = &reference {
        let (parsed, _thread, explicit) = parse_target_ref(&platform, reference);
        chat_id = parsed;
        is_explicit = explicit;
    }
    // Human-friendly channel names resolve through the directory.
    if let Some(reference) = &reference {
        if !is_explicit && chat_id.is_none() {
            match crate::channel_directory::resolve_by_name(&platform, reference) {
                Some(resolved) => chat_id = Some(resolved),
                None => {
                    return error_payload(&format!(
                        "Could not resolve '{reference}' on {platform}. \
                         Try using a numeric channel ID instead."
                    ))
                }
            }
        }
    }
    let sender = crate::messaging::platform_sender(&platform);
    let mut used_home_channel = false;
    if chat_id.is_none() {
        match home_channel(&platform) {
            Some(home) => {
                chat_id = Some(home);
                used_home_channel = true;
            }
            None => {
                let home_env = home_channel_env(&platform);
                return error_payload(&format!(
                    "No home channel set for {platform} to determine where to send the \
                     message. Either specify a channel directly with \
                     '{platform}:CHANNEL_NAME', or set a home channel via: \
                     ulnclaw config set {home_env} <channel_id>"
                ));
            }
        }
    }
    let chat_id = chat_id.unwrap_or_default();

    let (cleaned, media_paths) = crate::messaging::extract_media_tags(&message);
    let mirror_text = if cleaned.trim().is_empty() {
        describe_media_for_mirror(&media_paths)
    } else {
        cleaned.trim().to_string()
    };
    if mirror_text.is_empty() {
        return error_payload("No deliverable text or media remained after processing MEDIA tags");
    }

    if let Some(sender) = sender {
        if !media_paths.is_empty() {
            if sender.send_media(&chat_id, &cleaned, &media_paths).await {
                // Native delivery handled text + media.
            } else {
                // No native media path: deliver the prose plus an honest
                // text description of the attachments.
                if !cleaned.trim().is_empty() {
                    sender.send_text(&chat_id, cleaned.trim()).await;
                }
                let description = describe_media_for_mirror(&media_paths);
                if !description.is_empty() {
                    sender.send_text(&chat_id, &description).await;
                }
            }
        } else {
            sender.send_text(&chat_id, &mirror_text).await;
        }
    } else {
        // Standalone path (hermes `_send_to_platform` direct-API sends for
        // cron / `hermes send` / MCP bridge contexts): REST delivery for
        // Telegram/Discord/Slack without a live adapter loop.
        if let Err(error) = standalone_send(&platform, &chat_id, &cleaned, &media_paths).await {
            return error_payload(&error);
        }
    }

    let mut result = json!({
        "success": true,
        "platform": platform,
        "chat_id": chat_id,
    });
    if used_home_channel {
        result["note"] = json!(format!(
            "Sent to {platform} home channel (chat_id: {chat_id})"
        ));
    }
    result
}

/// Standalone direct-API delivery for platforms whose adapter loop is not
/// running in this process (hermes `_send_to_platform` standalone path).
/// Telegram/Discord/Slack ride plain REST; other platforms need a live
/// gateway adapter.
async fn standalone_send(
    platform: &str,
    chat_id: &str,
    text: &str,
    media: &[PathBuf],
) -> Result<(), String> {
    let config = crate::config::UlncLawConfig::load(None)
        .map_err(|e| format!("Failed to load gateway config: {e}"))?;
    match platform {
        "telegram" => {
            let Some(token) =
                crate::messaging::resolve_telegram_token_public(&config.messaging.telegram)
            else {
                return Err("Platform 'telegram' is not configured. Set up credentials in                             ~/.ulnclaw/config.toml (messaging.telegram.bot_token) or                             TELEGRAM_BOT_TOKEN."
                    .to_string());
            };
            let client = reqwest::Client::new();
            if media.is_empty() {
                crate::messaging::telegram_send_public(&client, &token, chat_id, text).await;
            } else {
                crate::messaging::telegram_send_media_public(&client, &token, chat_id, text, media)
                    .await;
            }
            Ok(())
        }
        "discord" => {
            let Some(token) =
                crate::messaging::resolve_discord_token_public(&config.messaging.discord)
            else {
                return Err("Platform 'discord' is not configured. Set up credentials in                             ~/.ulnclaw/config.toml (messaging.discord.bot_token) or                             DISCORD_BOT_TOKEN."
                    .to_string());
            };
            if media.is_empty() {
                crate::messaging::discord_send_public(&token, chat_id, text).await;
            } else {
                crate::messaging::discord_send_media_public(&token, chat_id, text, media).await;
            }
            Ok(())
        }
        "slack" => {
            let Some(token) =
                crate::messaging::resolve_slack_bot_token_public(&config.messaging.slack)
            else {
                return Err("Platform 'slack' is not configured. Set up credentials in                             ~/.ulnclaw/config.toml (messaging.slack.bot_token) or                             SLACK_BOT_TOKEN."
                    .to_string());
            };
            if media.is_empty() {
                crate::messaging::slack_send_public(&token, chat_id, text).await;
            } else {
                crate::messaging::slack_send_media_public(&token, chat_id, text, media).await;
            }
            Ok(())
        }
        _ => Err(format!(
            "Platform '{platform}' is not configured. Set up credentials in              ~/.ulnclaw/config.toml or environment variables."
        )),
    }
}

/// Tool availability gate (hermes `_check_send_message`): available when a
/// messaging turn is running or any platform adapter registered a live
/// sender (gateway up).
fn send_message_availability() -> crate::tools::ToolAvailability {
    if crate::messaging::current_messaging_ctx().is_some()
        || crate::messaging::has_platform_senders()
    {
        crate::tools::ToolAvailability::available()
    } else {
        crate::tools::ToolAvailability::unavailable(
            "no messaging platform adapters running (configure messaging platforms first)",
        )
    }
}

pub fn register(registry: &mut crate::tools::ToolRegistry) {
    use crate::tools::tool;

    registry.register(
        tool("send_message")
            .description(
                "Send a message to a connected messaging platform, or list available targets.\n\n\
                 IMPORTANT: When the user asks to send to a specific channel or person \
                 (not just a bare platform name), call send_message(action='list') FIRST to see \
                 available targets, then send to the correct one.\n\
                 If the user just says a platform name like 'send to telegram', send directly \
                 to the home channel without listing first.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["send", "list", "react", "unreact"],
                        "description": "Action to perform. 'send' (default) sends a message. 'list' returns all available channels/contacts across connected platforms. 'react' attaches an emoji reaction to a message (platforms that support it). 'unreact' retracts a previously-added reaction."
                    },
                    "target": {
                        "type": "string",
                        "description": "Delivery target. Format: 'platform' (uses home channel), 'platform:#channel-name', 'platform:chat_id', or 'platform:chat_id:thread_id' for Telegram topics and Discord threads. Examples: 'telegram', 'telegram:-1001234567890:17585', 'discord:999888777:555444333', 'discord:#bot-home', 'slack:#engineering', 'signal:+15551234567', 'matrix:!roomid:server.org', 'matrix:@user:server.org', 'ntfy:alerts-channel' (explicit ntfy topic), 'yuanbao:direct:<account_id>' (DM), 'yuanbao:group:<group_code>' (group chat)"
                    },
                    "message": {
                        "type": "string",
                        "description": "The message text to send. To send an image or file, include MEDIA:<local_path> (e.g. 'MEDIA:/tmp/report.pdf') in the message — the platform will deliver it as a native media attachment."
                    },
                    "emoji": {
                        "type": "string",
                        "description": "For action='react': the emoji to react with (e.g. '❤️')."
                    },
                    "message_id": {
                        "type": "string",
                        "description": "For action='react'/'unreact': id of the message to react to. Omit to target the most recent message received in that chat (usually the one being replied to)."
                    }
                },
                "required": []
            }))
            .handler(|args, _ctx| async move { Ok(run_send_message(args).await) })
            .toolset("messaging")
            .emoji("\u{1f4e8}")
            .check_fn(send_message_availability)
            .build()
            .expect("send_message builds"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_telegram_targets() {
        let (chat, thread, explicit) = parse_target_ref("telegram", "-1001234567890:17585");
        assert_eq!(chat.as_deref(), Some("-1001234567890"));
        assert_eq!(thread.as_deref(), Some("17585"));
        assert!(explicit);

        let (chat, thread, explicit) = parse_target_ref("telegram", "12345");
        assert_eq!(chat.as_deref(), Some("12345"));
        assert!(thread.is_none());
        assert!(explicit);

        let (chat, _thread, explicit) = parse_target_ref("telegram", "@somechannel");
        assert_eq!(chat.as_deref(), Some("@somechannel"));
        assert!(explicit);

        let (_chat, _thread, explicit) = parse_target_ref("telegram", "General Chat");
        assert!(!explicit);
    }

    #[test]
    fn parse_slack_targets() {
        let (chat, thread, explicit) = parse_target_ref("slack", "C0B0QV5434G:1712345678.9");
        assert_eq!(chat.as_deref(), Some("C0B0QV5434G"));
        assert_eq!(thread.as_deref(), Some("1712345678.9"));
        assert!(explicit);

        let (chat, _, _) = parse_target_ref("slack", "U12345678");
        assert_eq!(chat.as_deref(), Some("user:U12345678"));

        let (chat, _, _) = parse_target_ref("slack", "@alice");
        assert_eq!(chat.as_deref(), Some("user_name:alice"));

        let (chat, _, explicit) = parse_target_ref("slack", "#engineering");
        assert!(!explicit);
        assert!(chat.is_none());
    }

    #[test]
    fn parse_misc_platform_targets() {
        assert_eq!(
            parse_target_ref("matrix", "!room:server.org").0.as_deref(),
            Some("!room:server.org")
        );
        assert_eq!(
            parse_target_ref("email", "user@example.com").0.as_deref(),
            Some("user@example.com")
        );
        assert_eq!(
            parse_target_ref("whatsapp", "12345@g.us").0.as_deref(),
            Some("12345@g.us")
        );
        assert_eq!(
            parse_target_ref("signal", "+15551234567").0.as_deref(),
            Some("+15551234567")
        );
        assert_eq!(
            parse_target_ref("signal", "group:abc123").0.as_deref(),
            Some("group:abc123")
        );
        assert_eq!(
            parse_target_ref("yuanbao", "123456").0.as_deref(),
            Some("group:123456")
        );
        assert_eq!(
            parse_target_ref("yuanbao", "direct:acct").0.as_deref(),
            Some("direct:acct")
        );
        assert_eq!(
            parse_target_ref("ntfy", "alerts").0.as_deref(),
            Some("alerts")
        );
        assert_eq!(
            parse_target_ref("xmpp", "user@server.org").0.as_deref(),
            Some("user@server.org")
        );
    }

    #[test]
    fn split_target_platform_and_ref() {
        assert_eq!(
            split_target("telegram:-100123:45"),
            ("telegram".into(), Some("-100123:45".into()))
        );
        assert_eq!(split_target("TELEGRAM"), ("telegram".into(), None));
        assert_eq!(split_target("discord:"), ("discord".into(), None));
    }

    #[test]
    fn home_channel_env_override_for_email() {
        assert_eq!(home_channel_env("email"), "EMAIL_HOME_ADDRESS");
        assert_eq!(home_channel_env("telegram"), "TELEGRAM_HOME_CHANNEL");
        assert_eq!(home_channel_env("slack"), "SLACK_HOME_CHANNEL");
    }

    #[test]
    fn describe_media_fallback_text() {
        assert_eq!(describe_media_for_mirror(&[]), "");
        assert_eq!(
            describe_media_for_mirror(&[PathBuf::from("/tmp/x.png")]),
            "[Sent image attachment]"
        );
        assert_eq!(
            describe_media_for_mirror(&[PathBuf::from("/tmp/x.opus")]),
            "[Sent voice message]"
        );
        assert_eq!(
            describe_media_for_mirror(&[PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.pdf")]),
            "[Sent 2 media attachments]"
        );
    }

    #[tokio::test]
    async fn send_requires_target_and_message() {
        let value = run_send_message(json!({"action": "send"})).await;
        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("Both 'target' and 'message'"));
    }

    #[tokio::test]
    async fn react_validation_errors() {
        let value = run_send_message(json!({"action": "react", "target": "telegram:1"})).await;
        assert!(value["error"].as_str().unwrap().contains("'emoji'"));
        let value = run_send_message(json!({"action": "unreact"})).await;
        assert!(value["error"].as_str().unwrap().contains("'target'"));
    }

    #[tokio::test]
    async fn send_unconfigured_platform_errors() {
        // No adapter registered for this platform in the test process.
        let value = run_send_message(json!({
            "action": "send",
            "target": "telegram:12345",
            "message": "hello",
        }))
        .await;
        let error = value["error"].as_str().unwrap_or("");
        assert!(
            error.contains("not configured") || error.contains("Could not resolve"),
            "unexpected: {error}"
        );
    }

    #[tokio::test]
    async fn standalone_unknown_platform_errors() {
        let error = standalone_send("carrier-pigeon", "123", "hi", &[])
            .await
            .unwrap_err();
        assert!(error.contains("not configured"));
    }

    #[tokio::test]
    async fn list_returns_targets_string() {
        let value = run_send_message(json!({"action": "list"})).await;
        assert!(value["targets"].is_string());
    }

    #[tokio::test]
    async fn bare_platform_without_home_channel_errors() {
        let _env = crate::models_dev::test_env_lock();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-smt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ULNCLAW_HOME", &dir);
        std::env::remove_var("TELEGRAM_HOME_CHANNEL");

        let value = run_send_message(json!({
            "action": "send",
            "target": "telegram",
            "message": "hello",
        }))
        .await;
        let error = value["error"].as_str().unwrap_or("");
        // Either the platform is unconfigured (no live adapter in tests)
        // or the home-channel guidance fires — both are valid rejections.
        assert!(
            error.contains("home channel") || error.contains("not configured"),
            "unexpected: {error}"
        );

        match prev {
            Some(value) => std::env::set_var("ULNCLAW_HOME", value),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }
}

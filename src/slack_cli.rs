//! Slack app manifest generator — port of hermes `hermes_cli/slack_cli.py`.
//!
//! `ulnclaw slack manifest` emits the Slack app manifest JSON registering
//! every platform command as a native Slack slash (`/help`, `/skills`, …)
//! plus the `/ulnclaw` catch-all, so Slack users get the same first-class
//! slash UX as Discord/Telegram. Paste the JSON into Slack app config
//! (Features → App Manifest → Edit); Slack diffs it and prompts for
//! reinstall when scopes/commands change.
//!
//! The slash list is generated from the platform direct-command surface
//! (`crate::platform_slash`) so it stays in sync with what the Slack
//! adapter actually answers.

use serde_json::{json, Value};

pub const SLACK_LONG_DESCRIPTION_MIN_CHARACTERS: usize = 175;
pub const SLACK_LONG_DESCRIPTION_MAX_CHARACTERS: usize = 4000;

const SLACK_MAX_SLASH_COMMANDS: usize = 50;

/// Built-in Slack slash commands that cannot be registered by apps
/// (hermes `_SLACK_RESERVED_COMMANDS`).
const SLACK_RESERVED_COMMANDS: &[&str] = &[
    "me",
    "status",
    "away",
    "dnd",
    "shrug",
    "remind",
    "msg",
    "feed",
    "who",
    "collapse",
    "expand",
    "leave",
    "join",
    "open",
    "search",
    "topic",
    "mute",
    "pro",
    "shortcuts",
];

/// (slash_name, description, usage_hint) triples for the Slack manifest.
/// `/ulnclaw` is always the reserved first entry — the catch-all for
/// anything without a native slot (hermes `/hermes` reservation).
pub fn slack_native_slashes() -> Vec<(String, String, String)> {
    let mut entries: Vec<(String, String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    entries.push((
        "ulnclaw".to_string(),
        "Talk to Ulnclaw or run a command".to_string(),
        "[command] [args]".to_string(),
    ));
    seen.insert("ulnclaw".to_string());

    // The platform direct-command surface (hermes gateway direct set).
    let registry: &[(&str, &str, &str)] = &[
        ("help", "List available commands", ""),
        ("skills", "List installed skills", ""),
        ("tools", "List enabled tools", ""),
        ("recap", "Recap this chat's session", ""),
        ("title", "Show or set the session title", "[new title]"),
        ("usage", "This session's token usage", ""),
        (
            "insights",
            "Usage analytics across sessions",
            "[days] [--days N] [--source S]",
        ),
        ("reload-mcp", "Reload MCP servers (asks confirmation)", ""),
        ("approve", "Approve a pending confirmation", ""),
        ("deny", "Deny a pending confirmation", ""),
    ];

    for (name, desc, hint) in registry {
        let name = sanitize_slack_name(name);
        if name.is_empty() || seen.contains(&name) {
            continue;
        }
        if SLACK_RESERVED_COMMANDS.contains(&name.as_str()) {
            continue;
        }
        if entries.len() >= SLACK_MAX_SLASH_COMMANDS {
            break;
        }
        // Slack caps: description 2000 (keep 140 like hermes), hint 100.
        entries.push((
            name.clone(),
            desc.chars().take(140).collect(),
            hint.chars().take(100).collect(),
        ));
        seen.insert(name);
    }

    entries
}

/// Convert a command name to a valid Slack slash name (hermes
/// `_sanitize_slack_name`): lowercase a-z, digits, hyphens, underscores;
/// max 32 chars.
pub fn sanitize_slack_name(raw: &str) -> String {
    let name: String = raw
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    name.trim_matches(|c| c == '-' || c == '_').to_string()
}

/// The `features.slash_commands` portion of the manifest (hermes
/// `slack_app_manifest`). `request_url` is required by Slack's schema but
/// ignored in Socket Mode — a placeholder is fine.
pub fn slash_commands_manifest(request_url: &str) -> Value {
    let slashes: Vec<Value> = slack_native_slashes()
        .into_iter()
        .map(|(name, desc, hint)| {
            let mut entry = json!({
                "command": format!("/{name}"),
                "description": if desc.is_empty() { format!("Run /{name}") } else { desc },
                "should_escape": false,
                "url": request_url,
            });
            if !hint.is_empty() {
                entry["usage_hint"] = json!(hint);
            }
            entry
        })
        .collect();
    json!({"features": {"slash_commands": slashes}})
}

/// Messaging experience variants (hermes `messaging_experience`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagingExperience {
    /// Legacy Slack AI Assistant (assistant_view) — the default.
    Assistant,
    /// Slack Agent experience (agent_view + app_home_opened).
    Agent,
    /// No assistant surfaces — flat DM chat.
    None,
}

/// Build the full Slack manifest merging display info + the generated
/// slash list (hermes `_build_full_manifest`).
pub fn build_full_manifest(
    bot_name: &str,
    bot_description: &str,
    experience: MessagingExperience,
    long_description: Option<&str>,
) -> Value {
    let slashes = slash_commands_manifest("https://ulnclaw.local/slack/commands")["features"]
        ["slash_commands"]
        .clone();

    let mut features = json!({
        "app_home": {
            "home_tab_enabled": false,
            "messages_tab_enabled": true,
            "messages_tab_read_only_enabled": false,
        },
        "bot_user": {
            "display_name": bot_name.chars().take(80).collect::<String>(),
            "always_online": true,
        },
        "slash_commands": slashes,
    });

    let mut bot_scopes: Vec<String> = vec![
        "app_mentions:read".into(),
        "channels:history".into(),
        "channels:read".into(),
        "chat:write".into(),
        "commands".into(),
        "files:read".into(),
        "files:write".into(),
        "groups:history".into(),
        "groups:read".into(),
        "im:history".into(),
        "im:read".into(),
        "im:write".into(),
        "mpim:history".into(),
        "mpim:read".into(),
        "reactions:read".into(),
        "users:read".into(),
    ];
    let mut bot_events: Vec<String> = vec![
        "app_mention".into(),
        "message.channels".into(),
        "message.groups".into(),
        "message.im".into(),
        "message.mpim".into(),
        "reaction_added".into(),
        "reaction_removed".into(),
    ];

    match experience {
        MessagingExperience::Assistant => {
            features["assistant_view"] = json!({
                "assistant_description": "Chat with Ulnclaw in threads and DMs.",
            });
            bot_scopes.push("assistant:write".into());
            bot_events.push("assistant_thread_context_changed".into());
            bot_events.push("assistant_thread_started".into());
        }
        MessagingExperience::Agent => {
            features["agent_view"] = json!({
                "agent_description": "Chat with Ulnclaw in Slack Messages.",
            });
            bot_scopes.push("assistant:write".into());
            // Slack includes current viewing context in Agent DM events only
            // after this subscription is enabled.
            bot_events.push("app_context_changed".into());
            bot_events.push("app_home_opened".into());
        }
        MessagingExperience::None => {}
    }

    bot_scopes.sort();
    bot_events.sort();

    let mut display_information = json!({
        "name": bot_name.chars().take(35).collect::<String>(),
        "description": (if bot_description.is_empty() {
            "Your Ulnclaw agent on Slack"
        } else {
            bot_description
        })
        .chars()
        .take(140)
        .collect::<String>(),
        "background_color": "#1a1a2e",
    });
    if let Some(long) = long_description {
        display_information["long_description"] = json!(long);
    }

    json!({
        "_metadata": {"major_version": 1, "minor_version": 1},
        "display_information": display_information,
        "features": features,
        "oauth_config": {"scopes": {"bot": bot_scopes}},
        "settings": {
            "event_subscriptions": {"bot_events": bot_events},
            "interactivity": {"is_enabled": true},
            "org_deploy_enabled": false,
            "socket_mode_enabled": true,
            "token_rotation_enabled": false,
        },
    })
}

/// Manifest CLI options (parsed by the binary).
#[derive(Debug, Clone, Default)]
pub struct ManifestOptions {
    pub name: Option<String>,
    pub description: Option<String>,
    pub long_description: Option<String>,
    pub long_description_file: Option<String>,
    pub slashes_only: bool,
    pub no_assistant: bool,
    pub agent_view: bool,
    /// `Some(None)` = --write with no path (default home location).
    pub write: Option<Option<String>>,
}

/// Run the manifest command; returns the process exit code (hermes
/// `slack_manifest_command`). `payload_writer` receives the JSON text:
/// stdout when not --write, file write otherwise (kept injectable for
/// tests).
pub fn run_manifest_command(opts: &ManifestOptions, home: &std::path::Path) -> i32 {
    let name = opts.name.clone().unwrap_or_else(|| "Ulnclaw".to_string());
    let description = opts
        .description
        .clone()
        .unwrap_or_else(|| "Your Ulnclaw agent on Slack".to_string());

    let mut long_description = opts.long_description.clone();
    if opts.slashes_only && (long_description.is_some() || opts.long_description_file.is_some()) {
        eprintln!(
            "ulnclaw slack manifest: long description options cannot be used with --slashes-only"
        );
        return 2;
    }
    if let Some(path) = opts.long_description_file.as_deref() {
        match std::fs::read_to_string(shellexpand_path(path)) {
            Ok(content) => long_description = Some(content),
            Err(e) => {
                eprintln!("ulnclaw slack manifest: cannot read long description from {path}: {e}");
                return 2;
            }
        }
    }
    if let Some(long) = long_description.as_deref() {
        if long.chars().count() < SLACK_LONG_DESCRIPTION_MIN_CHARACTERS {
            eprintln!(
                "ulnclaw slack manifest: long description must be at least \
                 {SLACK_LONG_DESCRIPTION_MIN_CHARACTERS} characters (got {})",
                long.chars().count()
            );
            return 2;
        }
        if long.chars().count() > SLACK_LONG_DESCRIPTION_MAX_CHARACTERS {
            eprintln!(
                "ulnclaw slack manifest: long description must be at most \
                 {SLACK_LONG_DESCRIPTION_MAX_CHARACTERS} characters (got {})",
                long.chars().count()
            );
            return 2;
        }
    }

    let experience = if opts.agent_view {
        MessagingExperience::Agent
    } else if opts.no_assistant {
        MessagingExperience::None
    } else {
        MessagingExperience::Assistant
    };

    let manifest = if opts.slashes_only {
        slash_commands_manifest("https://ulnclaw.local/slack/commands")["features"]
            ["slash_commands"]
            .clone()
    } else {
        build_full_manifest(&name, &description, experience, long_description.as_deref())
    };

    let payload = serde_json::to_string_pretty(&manifest).expect("manifest serializes") + "\n";

    match opts.write.as_ref() {
        None => {
            print!("{payload}");
            0
        }
        Some(target) => {
            let path = match target {
                Some(p) => shellexpand_path(p),
                None => home.join("slack-manifest.json"),
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if let Err(e) = std::fs::write(&path, &payload) {
                eprintln!(
                    "ulnclaw slack manifest: cannot write {}: {e}",
                    path.display()
                );
                return 1;
            }
            eprintln!("Slack manifest written to: {}", path.display());
            eprintln!(
                "\nNext steps:\n  1. Open https://api.slack.com/apps and pick your Ulnclaw app\n     (or create a new one: Create New App → From an app manifest).\n  2. Features → App Manifest → paste the contents of\n     {}\n  3. Save; Slack will prompt to reinstall the app if scopes or\n     slash commands changed.\n  4. Make sure Socket Mode is enabled and you have a bot token\n     (xoxb-...) and app token (xapp-...) configured in\n     [messaging.slack].",
                path.display()
            );
            0
        }
    }
}

fn shellexpand_path(raw: &str) -> std::path::PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home_dir) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        {
            return std::path::PathBuf::from(home_dir).join(rest);
        }
    }
    std::path::PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_slack_name_strips_invalid_chars() {
        assert_eq!(sanitize_slack_name("Reload-MCP"), "reload-mcp");
        assert_eq!(sanitize_slack_name("weird name!"), "weirdname");
        assert_eq!(sanitize_slack_name("__x__"), "x");
        let long = "a".repeat(40);
        assert_eq!(sanitize_slack_name(&long).len(), 32);
    }

    #[test]
    fn native_slashes_reserve_catchall_first_and_skip_reserved() {
        let entries = slack_native_slashes();
        assert_eq!(entries[0].0, "ulnclaw");
        // No reserved Slack built-in names leak through.
        for (name, _, _) in &entries {
            assert!(!SLACK_RESERVED_COMMANDS.contains(&name.as_str()), "{name}");
        }
        // Direct-command surface is present.
        let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
        for expected in [
            "help", "skills", "tools", "recap", "title", "usage", "insights",
        ] {
            assert!(names.contains(&expected), "missing /{expected}");
        }
        assert!(entries.len() <= SLACK_MAX_SLASH_COMMANDS);
    }

    #[test]
    fn slash_commands_manifest_shape() {
        let manifest = slash_commands_manifest("https://example.test/hook");
        let slashes = manifest["features"]["slash_commands"]
            .as_array()
            .expect("array");
        assert_eq!(slashes[0]["command"], "/ulnclaw");
        for entry in slashes {
            assert_eq!(entry["url"], "https://example.test/hook");
            assert_eq!(entry["should_escape"], false);
        }
    }

    #[test]
    fn full_manifest_assistant_default() {
        let manifest = build_full_manifest("My Bot", "desc", MessagingExperience::Assistant, None);
        assert_eq!(manifest["_metadata"]["major_version"], 1);
        assert_eq!(manifest["display_information"]["name"], "My Bot");
        assert!(manifest["features"]["assistant_view"].is_object());
        assert!(manifest["features"].get("agent_view").is_none());
        let scopes = manifest["oauth_config"]["scopes"]["bot"]
            .as_array()
            .unwrap();
        let scope_names: Vec<&str> = scopes.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(scope_names.contains(&"assistant:write"));
        assert!(scope_names.contains(&"commands"));
        // Sorted.
        let mut sorted = scope_names.clone();
        sorted.sort_unstable();
        assert_eq!(scope_names, sorted);
        let events = manifest["settings"]["event_subscriptions"]["bot_events"]
            .as_array()
            .unwrap();
        let event_names: Vec<&str> = events.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(event_names.contains(&"assistant_thread_started"));
        assert_eq!(manifest["settings"]["socket_mode_enabled"], true);
    }

    #[test]
    fn full_manifest_agent_and_none_variants() {
        let agent = build_full_manifest("B", "d", MessagingExperience::Agent, None);
        assert!(agent["features"]["agent_view"].is_object());
        let events: Vec<&str> = agent["settings"]["event_subscriptions"]["bot_events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(events.contains(&"app_home_opened"));
        assert!(events.contains(&"app_context_changed"));

        let none = build_full_manifest("B", "d", MessagingExperience::None, None);
        assert!(none["features"].get("assistant_view").is_none());
        assert!(none["features"].get("agent_view").is_none());
        let scopes: Vec<&str> = none["oauth_config"]["scopes"]["bot"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(!scopes.contains(&"assistant:write"));
    }

    #[test]
    fn long_description_validation_bounds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().to_path_buf();

        let too_short = ManifestOptions {
            long_description: Some("short".into()),
            ..Default::default()
        };
        assert_eq!(run_manifest_command(&too_short, &home), 2);

        let ok = ManifestOptions {
            long_description: Some("x".repeat(200)),
            write: Some(Some(home.join("m.json").display().to_string())),
            ..Default::default()
        };
        assert_eq!(run_manifest_command(&ok, &home), 0);
        let written = std::fs::read_to_string(home.join("m.json")).expect("written");
        assert!(written.contains("long_description"));

        let too_long = ManifestOptions {
            long_description: Some("x".repeat(4001)),
            ..Default::default()
        };
        assert_eq!(run_manifest_command(&too_long, &home), 2);
    }

    #[test]
    fn slashes_only_conflicts_with_long_description() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = ManifestOptions {
            slashes_only: true,
            long_description: Some("anything".into()),
            ..Default::default()
        };
        assert_eq!(run_manifest_command(&opts, dir.path()), 2);
    }

    #[test]
    fn long_description_file_reads_utf8() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("long.txt");
        std::fs::write(&file, "y".repeat(200)).expect("write");
        let opts = ManifestOptions {
            long_description_file: Some(file.display().to_string()),
            write: Some(None),
            ..Default::default()
        };
        assert_eq!(run_manifest_command(&opts, dir.path()), 0);
        let written = std::fs::read_to_string(dir.path().join("slack-manifest.json"))
            .expect("default write path");
        assert!(written.contains("long_description"));

        let missing = ManifestOptions {
            long_description_file: Some(dir.path().join("nope.txt").display().to_string()),
            ..Default::default()
        };
        assert_eq!(run_manifest_command(&missing, dir.path()), 2);
    }
}

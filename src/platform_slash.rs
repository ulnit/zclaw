//! Direct slash commands for messaging platforms — port of the hermes
//! `gateway/slash_commands.py` direct-command subset.
//!
//! Messaging platforms (Telegram, Discord, Slack, …) dispatch chat messages
//! into the agent loop; a small command set answers directly WITHOUT an LLM
//! turn (hermes direct-command semantics), and `/skill-name` / `/<bundle>`
//! invocations expand into scaffolded agent turns exactly like the gateway
//! session-chat endpoints do.

use std::path::Path;

use crate::agent::Agent;
use crate::session::sqlite::SqliteSessionStore;

/// Outcome of resolving a platform slash message.
#[derive(Debug, Clone, PartialEq)]
pub enum PlatformSlashOutcome {
    /// Reply directly — no LLM turn.
    Direct(String),
    /// Replace the inbound text with an expanded agent-turn message.
    AgentTurn(String),
}

const PLATFORM_SLASH_HELP: &str = "Commands you can send as chat messages:
  /help            this list
  /whoami          your identity as the gateway sees it
  /skills          list skills (invoke one: /<skill-name> [instruction])
  /tools           list enabled tools
  /recap           recap this chat's session
  /title [text]    show or set the session title
  /resume [name]   list or switch to a previous session
  /new             start a fresh session (old one stays saved)
  /learn <what>    learn a reusable skill from anything you describe
  /sethome         set this chat as the platform home channel
  /footer [on|off|status]  toggle the runtime-metadata footer
  /usage           this session's token usage
  /context         context-window usage of this chat's session
  /insights [N] [--days N] [--source S]   usage analytics across sessions
  /reload-mcp      reload MCP servers (may ask confirmation)
  /approve, /deny  resolve a pending approval
  /<bundle>        invoke a skill bundle";

/// Resolve a platform chat message as a slash command, if it is one.
/// Returns `None` when the text is not a recognized command — unknown
/// `/…` text stays an ordinary agent message (users legitimately send
/// paths and other slash-prefixed text in chat).
pub async fn resolve(
    agent: &Agent,
    store: &SqliteSessionStore,
    home: &Path,
    session_id: &str,
    text: &str,
) -> Option<PlatformSlashOutcome> {
    let trimmed = text.trim();
    let stripped = trimmed.strip_prefix('/')?;
    if stripped.is_empty() {
        return None;
    }
    let mut parts = trimmed.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    let skills_dir = home.join("skills");
    match cmd {
        "/help" => Some(PlatformSlashOutcome::Direct(
            PLATFORM_SLASH_HELP.to_string(),
        )),
        "/skills" => {
            let skills = crate::skills::list_skills(&skills_dir);
            if skills.is_empty() {
                Some(PlatformSlashOutcome::Direct(
                    "no skills installed (<home>/skills).".to_string(),
                ))
            } else {
                let mut out = String::new();
                for skill in &skills {
                    out.push_str(&format!("  {} — {}\n", skill.name, skill.description));
                }
                Some(PlatformSlashOutcome::Direct(out.trim_end().to_string()))
            }
        }
        "/tools" => Some(PlatformSlashOutcome::Direct(agent.tool_names().join(", "))),
        "/recap" => {
            let row = store.get_session_row(session_id).ok().flatten();
            let messages = store.load_messages(session_id).unwrap_or_default();
            let recap = crate::session::recap::build_recap(
                &messages,
                row.as_ref().and_then(|r| r.title.as_deref()),
                Some(session_id),
            );
            Some(PlatformSlashOutcome::Direct(recap))
        }
        "/title" => {
            if rest.is_empty() {
                let title = store
                    .get_session_row(session_id)
                    .ok()
                    .flatten()
                    .and_then(|row| row.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "(untitled)".to_string());
                Some(PlatformSlashOutcome::Direct(format!("title: {title}")))
            } else {
                match store.set_session_title(session_id, rest) {
                    Ok(()) => Some(PlatformSlashOutcome::Direct(format!("title set: {rest}"))),
                    Err(e) => Some(PlatformSlashOutcome::Direct(format!(
                        "title set failed: {e}"
                    ))),
                }
            }
        }
        "/footer" => {
            // P715: hermes /footer — toggle the runtime-metadata
            // footer; persists to config.toml and latches this
            // process. Platform key best-effort from the session id
            // (platform-<name>-<chat>).
            let platform_key = session_id
                .strip_prefix("platform-")
                .map(|rest| rest.split('-').next().unwrap_or("").to_string())
                .filter(|s| !s.is_empty());
            let display = &agent.context().config.display;
            let model = agent.context().config.model.model.clone();
            Some(PlatformSlashOutcome::Direct(
                crate::runtime_footer::handle_footer_command(
                    rest,
                    display,
                    platform_key.as_deref(),
                    Some(&model),
                ),
            ))
        }
        "/usage" => {
            let Some(row) = store.get_session_row(session_id).ok().flatten() else {
                return Some(PlatformSlashOutcome::Direct(
                    "session not found.".to_string(),
                ));
            };
            Some(PlatformSlashOutcome::Direct(format!(
                "messages: {}  tokens: {} in / {} out",
                row.message_count, row.input_tokens, row.output_tokens
            )))
        }
        "/insights" => {
            let mut days: u32 = 30;
            let mut source: Option<String> = None;
            let mut tokens = rest.split_whitespace();
            while let Some(token) = tokens.next() {
                if token == "--days" {
                    if let Some(value) = tokens.next() {
                        if let Ok(parsed) = value.parse::<u32>() {
                            days = parsed;
                        }
                    }
                } else if token == "--source" {
                    source = tokens.next().map(String::from);
                } else if let Ok(parsed) = token.parse::<u32>() {
                    days = parsed;
                }
            }
            let provider_hint = agent.context().config.model.provider.clone();
            let result = match crate::insights::InsightsEngine::open_default() {
                Ok(engine) => {
                    match engine.generate(days, source.as_deref(), Some(&provider_hint)) {
                        Ok(report) => crate::insights::format_gateway(&report),
                        Err(e) => format!("insights failed: {e}"),
                    }
                }
                Err(e) => format!("insights failed: {e}"),
            };
            Some(PlatformSlashOutcome::Direct(result))
        }
        "/context" => {
            // Lean hermes /context parity for platform chats: session
            // transcript tokens vs the agent's context budget.
            let messages: Vec<crate::provider::Message> = store
                .load_messages(session_id)
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.role != crate::provider::Role::System)
                .collect();
            let tokens = crate::context::ContextCompressor::estimate_tokens(&messages);
            let budget = agent.context_budget_tokens();
            let pct = if budget > 0 { tokens * 100 / budget } else { 0 };
            Some(PlatformSlashOutcome::Direct(format!(
                "context: ~{tokens} tokens of {budget} budget ({pct}% used)"
            )))
        }
        "/new" | "/reset" => {
            // hermes /new + /reset parity: rotate this chat to a fresh
            // session; the old transcript stays saved (/resume returns).
            let Some((platform, chat_id)) = parse_platform_session_key(session_id) else {
                return Some(PlatformSlashOutcome::Direct(
                    "not a platform chat session.".to_string(),
                ));
            };
            let new_id = format!(
                "platform-{platform}-{chat_id}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            if store
                .create_named_session(&new_id, &format!("platform:{platform}"), None, None)
                .is_err()
            {
                return Some(PlatformSlashOutcome::Direct(
                    "reset failed: could not create the new session.".to_string(),
                ));
            }
            // P737: session hooks — the old session ended and a fresh
            // session entry was created (hermes session:end +
            // session:reset).
            crate::event_hooks::emit(
                "session:end",
                serde_json::json!({
                    "platform": platform,
                    "chat_id": chat_id,
                    "session_id": session_id,
                }),
            );
            crate::event_hooks::emit(
                "session:reset",
                serde_json::json!({
                    "platform": platform,
                    "chat_id": chat_id,
                    "session_id": new_id,
                    "previous_session_id": session_id,
                }),
            );
            crate::messaging::set_session_remap(session_id, &new_id);
            crate::messaging::drop_history_cache(session_id).await;
            Some(PlatformSlashOutcome::Direct(
                "\u{2713} Started a fresh session. The previous conversation is saved —                  return with /resume."
                    .to_string(),
            ))
        }
        "/learn" => Some(PlatformSlashOutcome::AgentTurn(
            crate::learn_prompt::build_learn_prompt(rest),
        )),
        "/sethome" | "/set-home" => {
            // hermes /sethome parity: make the current chat the home
            // channel for its platform — cron jobs and cross-platform
            // send_message deliveries land here. Persisted as the
            // legacy home env var (send_message_tool home_channel).
            let Some((platform, chat_id)) = parse_platform_session_key(session_id) else {
                return Some(PlatformSlashOutcome::Direct(
                    "Failed to save home channel: not a platform chat session".to_string(),
                ));
            };
            let chat_name = crate::channel_directory::list_channels(Some(platform))
                .into_iter()
                .find(|(_, entry)| entry.id == chat_id)
                .map(|(_, entry)| entry.name)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| chat_id.to_string());
            let env_name = crate::send_message_tool::home_channel_env(platform);
            match crate::config_cmd::set_env_value(&env_name, chat_id) {
                Ok(()) => Some(PlatformSlashOutcome::Direct(format!(
                    "✅ Home channel set to **{chat_name}** (ID: {chat_id}).
                     Cron jobs and cross-platform messages will be delivered here."
                ))),
                Err(error) => Some(PlatformSlashOutcome::Direct(format!(
                    "Failed to save home channel: {error}"
                ))),
            }
        }
        _ => {
            // Bundles win over single skills (hermes bundle-over-skill slash
            // precedence); unknown names fall through to the agent.
            let cmd_name = cmd.trim_start_matches('/');
            if let Some(key) = crate::bundles::resolve_bundle_command_key(cmd_name) {
                if let Some((message, _loaded, _missing)) =
                    crate::bundles::build_bundle_invocation_message(&key, rest, &skills_dir)
                {
                    return Some(PlatformSlashOutcome::AgentTurn(message));
                }
            }
            if let Some(message) =
                crate::skills::build_skill_invocation_message(&skills_dir, cmd_name, rest)
            {
                return Some(PlatformSlashOutcome::AgentTurn(message));
            }
            None
        }
    }
}

/// Split a platform session key (`platform-{platform}-{chat_id}`,
/// messaging.rs `session_key`) back into (platform, chat_id). Platform
/// ids never contain `-`; chat ids may.
fn parse_platform_session_key(session_id: &str) -> Option<(&str, &str)> {
    let rest = session_id.strip_prefix("platform-")?;
    let (platform, chat_id) = rest.split_once('-')?;
    if platform.is_empty() || chat_id.is_empty() {
        return None;
    }
    Some((platform, chat_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn setup() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Arc<Agent>,
        Arc<SqliteSessionStore>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().to_path_buf();
        std::fs::create_dir_all(home.join("skills")).ok();
        let store =
            Arc::new(SqliteSessionStore::open(dir.path().join("state.db")).expect("store opens"));
        let provider = Arc::new(
            crate::provider::openai::OpenAiProvider::builder()
                .endpoint("http://127.0.0.1:9/v1")
                .model("test-model")
                .name("test")
                .build()
                .expect("provider builds"),
        );
        let agent = Arc::new(
            Agent::new(provider, crate::tools::ToolRegistry::new()).with_store(store.clone()),
        );
        (dir, home, agent, store)
    }

    fn named_session(store: &SqliteSessionStore, id: &str) {
        store
            .create_named_session(id, "slack", None, None)
            .expect("named session");
    }

    #[tokio::test]
    async fn help_lists_commands_without_agent_state() {
        let (_dir, home, agent, store) = setup();
        let outcome = resolve(&agent, &store, &home, "s1", "/help").await;
        let Some(PlatformSlashOutcome::Direct(reply)) = outcome else {
            panic!("expected direct reply");
        };
        assert!(reply.contains("/skills"));
        assert!(reply.contains("/insights"));
        assert!(reply.contains("/approve"));
    }

    #[tokio::test]
    async fn tools_lists_registered_tool_names() {
        let (_dir, home, agent, store) = setup();
        let outcome = resolve(&agent, &store, &home, "s1", "/tools").await;
        let Some(PlatformSlashOutcome::Direct(reply)) = outcome else {
            panic!("expected direct reply");
        };
        // An empty registry still answers directly (empty list).
        assert!(!reply.contains("unknown command"));
    }

    #[tokio::test]
    async fn title_show_and_set_roundtrip() {
        let (_dir, home, agent, store) = setup();
        named_session(&store, "s1");

        let outcome = resolve(&agent, &store, &home, "s1", "/title").await;
        assert!(
            matches!(outcome, Some(PlatformSlashOutcome::Direct(ref t)) if t.contains("(untitled)"))
        );

        let outcome = resolve(&agent, &store, &home, "s1", "/title My chat").await;
        assert!(
            matches!(outcome, Some(PlatformSlashOutcome::Direct(ref t)) if t.contains("title set: My chat"))
        );

        let outcome = resolve(&agent, &store, &home, "s1", "/title").await;
        assert!(
            matches!(outcome, Some(PlatformSlashOutcome::Direct(ref t)) if t.contains("title: My chat"))
        );
    }

    #[tokio::test]
    async fn usage_reports_session_counters() {
        let (_dir, home, agent, store) = setup();
        named_session(&store, "s1");
        let outcome = resolve(&agent, &store, &home, "s1", "/usage").await;
        let Some(PlatformSlashOutcome::Direct(reply)) = outcome else {
            panic!("expected direct reply");
        };
        assert!(reply.starts_with("messages: "), "got: {reply}");
        assert!(reply.contains("tokens:"));
    }

    #[tokio::test]
    async fn usage_missing_session_reports_not_found() {
        let (_dir, home, agent, store) = setup();
        let outcome = resolve(&agent, &store, &home, "ghost", "/usage").await;
        assert!(
            matches!(outcome, Some(PlatformSlashOutcome::Direct(ref t)) if t == "session not found.")
        );
    }

    #[tokio::test]
    async fn unknown_slash_falls_through() {
        let (_dir, home, agent, store) = setup();
        assert!(resolve(&agent, &store, &home, "s1", "/usr/bin/foo")
            .await
            .is_none());
        assert!(resolve(&agent, &store, &home, "s1", "plain text")
            .await
            .is_none());
        assert!(resolve(&agent, &store, &home, "s1", "/").await.is_none());
    }

    #[tokio::test]
    async fn skills_listing_empty_home() {
        let (_dir, home, agent, store) = setup();
        let outcome = resolve(&agent, &store, &home, "s1", "/skills").await;
        assert!(
            matches!(outcome, Some(PlatformSlashOutcome::Direct(ref t)) if t.contains("no skills installed"))
        );
    }

    #[tokio::test]
    async fn skill_invocation_expands_to_agent_turn() {
        let (_dir, home, agent, store) = setup();
        let skill_dir = home.join("skills").join("greet");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greet\ndescription: Greet people\n---\n\nSay hello kindly.\n",
        )
        .expect("skill file");
        let outcome = resolve(&agent, &store, &home, "s1", "/greet the team").await;
        let Some(PlatformSlashOutcome::AgentTurn(message)) = outcome else {
            panic!("expected agent turn expansion");
        };
        assert!(
            message.contains("greet"),
            "expansion should reference the skill: {message}"
        );
    }

    #[tokio::test]
    async fn context_reports_session_usage_vs_budget() {
        // P695: lean /context parity for platform chats.
        let (_dir, home, agent, store) = setup();
        named_session(&store, "platform-telegram-chat-ctx");
        store
            .append_message(
                "platform-telegram-chat-ctx",
                &crate::provider::Message {
                    role: crate::provider::Role::User,
                    content: Some("hello world, this is a context probe".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();
        let outcome = resolve(
            &agent,
            &store,
            &home,
            "platform-telegram-chat-ctx",
            "/context",
        )
        .await;
        let Some(PlatformSlashOutcome::Direct(reply)) = outcome else {
            panic!("expected direct reply");
        };
        assert!(reply.starts_with("context: ~"), "{reply}");
        assert!(reply.contains("budget"), "{reply}");
        assert!(reply.contains("% used"), "{reply}");
    }

    #[tokio::test]
    async fn new_rotates_platform_chat_to_fresh_session() {
        // P692: hermes /new parity — rotate the chat to a fresh
        // session id; the old transcript stays saved.
        let _guard = crate::messaging::remap_test_lock();
        crate::messaging::clear_session_remappings_for_tests();
        let (_dir, home, agent, store) = setup();
        let outcome = resolve(&agent, &store, &home, "platform-telegram-chat-9", "/new").await;
        let Some(PlatformSlashOutcome::Direct(reply)) = outcome else {
            panic!("expected direct reply");
        };
        assert!(reply.contains("fresh session"), "{reply}");
        let remapped = crate::messaging::effective_session_id_for("platform-telegram-chat-9");
        assert!(
            remapped.starts_with("platform-telegram-chat-9-"),
            "{remapped}"
        );
        assert!(store.get_session_row(&remapped).unwrap().is_some());
        // Alias behaves identically.
        let outcome = resolve(&agent, &store, &home, "platform-telegram-chat-9", "/reset").await;
        assert!(
            matches!(outcome, Some(PlatformSlashOutcome::Direct(ref t)) if t.contains("fresh session"))
        );
        // Non-platform session keys are rejected.
        let outcome = resolve(&agent, &store, &home, "s1", "/new").await;
        assert!(
            matches!(outcome, Some(PlatformSlashOutcome::Direct(ref t)) if t.contains("not a platform chat"))
        );
        crate::messaging::clear_session_remappings_for_tests();
    }

    #[tokio::test]
    async fn learn_expands_to_agent_turn() {
        // P687: /learn rewrites the turn into the skill-authoring prompt.
        let (_dir, home, agent, store) = setup();
        let outcome = resolve(
            &agent,
            &store,
            &home,
            "s1",
            "/learn docs/api.md focus on auth",
        )
        .await;
        let Some(PlatformSlashOutcome::AgentTurn(message)) = outcome else {
            panic!("expected agent turn expansion");
        };
        assert!(message.contains("docs/api.md focus on auth"), "{message}");
        assert!(message.contains("skill_manage"), "{message}");

        // Bare /learn falls back to the conversation workflow.
        let outcome = resolve(&agent, &store, &home, "s1", "/learn").await;
        let Some(PlatformSlashOutcome::AgentTurn(message)) = outcome else {
            panic!("expected agent turn expansion");
        };
        assert!(
            message.contains("the workflow we just went through"),
            "{message}"
        );
    }

    #[test]
    fn platform_session_key_parsing() {
        assert_eq!(
            parse_platform_session_key("platform-telegram-chat-123"),
            Some(("telegram", "chat-123"))
        );
        assert_eq!(
            parse_platform_session_key("platform-whatsapp_cloud-1@s.whatsapp.net"),
            Some(("whatsapp_cloud", "1@s.whatsapp.net"))
        );
        assert_eq!(parse_platform_session_key("s1"), None);
        assert_eq!(parse_platform_session_key("platform-"), None);
        assert_eq!(parse_platform_session_key("platform-telegram-"), None);
    }

    #[tokio::test]
    async fn sethome_persists_platform_home_channel() {
        // P685: hermes /sethome parity — persist the calling chat as
        // the platform home channel env var.
        let _guard = crate::models_dev::test_env_lock();
        let (dir, home, agent, store) = setup();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        crate::channel_directory::reset_for_tests();
        crate::channel_directory::record_channel(
            "telegram",
            "chat-123",
            "Team Chat",
            "group",
            "m1",
        );

        let outcome = resolve(
            &agent,
            &store,
            &home,
            "platform-telegram-chat-123",
            "/sethome",
        )
        .await;
        let Some(PlatformSlashOutcome::Direct(reply)) = outcome else {
            panic!("expected direct reply");
        };
        assert!(
            reply.contains("Home channel set to **Team Chat** (ID: chat-123)"),
            "{reply}"
        );
        assert!(reply.contains("Cron jobs and cross-platform"), "{reply}");
        assert_eq!(
            crate::config::get_env_value("TELEGRAM_HOME_CHANNEL").as_deref(),
            Some("chat-123")
        );

        // Alias + chat-id name fallback when the directory has no entry.
        let outcome = resolve(&agent, &store, &home, "platform-discord-999", "/set-home").await;
        let Some(PlatformSlashOutcome::Direct(reply)) = outcome else {
            panic!("expected direct reply");
        };
        assert!(reply.contains("(ID: 999)"), "{reply}");
        assert_eq!(
            crate::config::get_env_value("DISCORD_HOME_CHANNEL").as_deref(),
            Some("999")
        );

        // Non-platform session keys cannot set a home channel.
        let outcome = resolve(&agent, &store, &home, "s1", "/sethome").await;
        assert!(matches!(
            outcome,
            Some(PlatformSlashOutcome::Direct(ref t)) if t.contains("Failed to save home channel")
        ));

        crate::channel_directory::reset_for_tests();
        if let Some(prev) = prev_home {
            std::env::set_var("ULNCLAW_HOME", prev);
        }
    }

    #[tokio::test]
    async fn footer_command_toggles_and_reports() {
        // P715: /footer — status reports the effective state, on
        // persists to config.toml and latches the process, unknown
        // args answer usage. Env-global home + latch: serialize.
        let _env_guard = crate::models_dev::test_env_lock();
        crate::runtime_footer::clear_enabled_latch_for_tests();
        let (dir, home, agent, store) = setup();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        let outcome = resolve(
            &agent,
            &store,
            &home,
            "platform-telegram-chat-1",
            "/footer status",
        )
        .await
        .expect("footer status resolves");
        match outcome {
            PlatformSlashOutcome::Direct(reply) => {
                assert!(reply.contains("runtime footer: off"), "{reply}");
                assert!(reply.contains("model, context_pct, cwd"), "{reply}");
                assert!(reply.contains("telegram"), "{reply}");
            }
            other => panic!("expected Direct, got {other:?}"),
        }

        let outcome = resolve(
            &agent,
            &store,
            &home,
            "platform-telegram-chat-1",
            "/footer on",
        )
        .await
        .expect("footer on resolves");
        match outcome {
            PlatformSlashOutcome::Direct(reply) => {
                assert!(reply.starts_with("runtime footer enabled"), "{reply}");
            }
            other => panic!("expected Direct, got {other:?}"),
        }
        assert_eq!(crate::runtime_footer::enabled_latch(), Some(true));

        let outcome = resolve(
            &agent,
            &store,
            &home,
            "platform-telegram-chat-1",
            "/footer sideways",
        )
        .await
        .expect("footer bad arg resolves");
        match outcome {
            PlatformSlashOutcome::Direct(reply) => {
                assert_eq!(reply, "usage: /footer [on|off|status]")
            }
            other => panic!("expected Direct, got {other:?}"),
        }

        std::env::remove_var("ULNCLAW_HOME");
        crate::runtime_footer::clear_enabled_latch_for_tests();
    }
}

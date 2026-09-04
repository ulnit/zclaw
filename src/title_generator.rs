//! Auto-generate short session titles from the first user/assistant
//! exchange — port of hermes `agent/title_generator.py` (v2026.8.3).
//!
//! Runs asynchronously after the first response is delivered so it never
//! adds latency to the user-facing reply. The LLM call routes through the
//! `title_generation` auxiliary task (`[auxiliary.title_generation]`
//! provider/model/base_url overrides; the main runtime is inherited when
//! unset, hermes `call_llm(task="title_generation")`).
//!
//! Config knobs (hermes `auxiliary.title_generation`):
//!   - `enabled`  — kill switch, default true (`is_truthy_value` semantics)
//!   - `language` — pin the title language; blank matches the user's

use std::sync::Arc;

use crate::config::UlncLawConfig;
use crate::provider::auxiliary::TASK_TITLE_GENERATION;
use crate::provider::{Message, Provider, ProviderRequest, Role};
use crate::session::SqliteSessionStore;
use crate::think_scrubber::strip_think_blocks;

const TITLE_PROMPT: &str =
    "Generate a short, descriptive title (3-7 words) for a conversation that starts with the \
     following exchange. The title should capture the main topic or intent. \
     Write the title in the same language the user is writing in. \
     Return ONLY the title text, nothing else. No quotes, no punctuation at the end, no prefixes.";

const TITLE_PROMPT_PINNED_LANGUAGE: &str =
    "Generate a short, descriptive title (3-7 words) for a conversation that starts with the \
     following exchange. The title should capture the main topic or intent. \
     Write the title in {language}. \
     Return ONLY the title text, nothing else. No quotes, no punctuation at the end, no prefixes.";

/// Exchange snippets are truncated to this many characters before they are
/// sent to the titler (hermes `[:500]`).
const SNIPPET_CHARS: usize = 500;
/// Titles longer than this are cut to 77 chars + "..." (hermes `[:77]`).
const TITLE_MAX_CHARS: usize = 80;

fn task_config(config: &UlncLawConfig) -> crate::config::AuxiliaryTaskConfig {
    config
        .auxiliary
        .get(TASK_TITLE_GENERATION)
        .cloned()
        .unwrap_or_default()
}

/// Whether automatic session title generation is enabled (hermes
/// `_auto_title_enabled`; defaults to true, fail-open).
pub fn auto_title_enabled(config: &UlncLawConfig) -> bool {
    task_config(config).enabled()
}

/// Configured title language, or `None` to match the user's language
/// (hermes `_title_language`).
pub fn title_language(config: &UlncLawConfig) -> Option<String> {
    task_config(config).language()
}

/// Char-safe prefix truncation (Python `s[:n]` on code points).
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Clean a raw model answer into a storable title (hermes post-processing
/// in `generate_title`): strip reasoning blocks, surrounding quotes, a
/// "Title:" prefix, keep the first non-empty line, cap the length.
pub fn clean_title(raw: &str) -> Option<String> {
    // Strip thinking/reasoning blocks that think-enabled models emit even
    // for simple prompts like title generation — reuses the canonical
    // scrubber so all tag variants (unterminated blocks, orphan closes,
    // mixed case) are handled.
    let mut title = strip_think_blocks(raw).trim().to_string();

    // Clean up: remove quotes, trailing punctuation stays the model's
    // problem; strip a leading "Title: " prefix.
    title = title
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .to_string();
    if title.to_ascii_lowercase().starts_with("title:") {
        title = title["title:".len()..].trim().to_string();
    }

    // A title is one line. A model that ignores "return ONLY the title"
    // and answers the prompt instead (a shell transcript, a bulleted plan)
    // would otherwise be stored verbatim and truncated mid-command. Keep
    // the first non-empty line — the closest thing to a title.
    title = title
        .lines()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string();

    // Enforce reasonable length.
    if title.chars().count() > TITLE_MAX_CHARS {
        title = format!("{}...", truncate_chars(&title, TITLE_MAX_CHARS - 3));
    }

    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Generate a session title from the first exchange.
///
/// Uses the `title_generation` auxiliary task routing (main runtime when
/// unset). Returns the cleaned title or `None` on any failure — title
/// generation must never break a turn.
pub async fn generate_title(
    config: &UlncLawConfig,
    main_provider: Arc<dyn Provider>,
    user_message: &str,
    assistant_response: &str,
) -> Option<String> {
    if !auto_title_enabled(config) {
        return None;
    }
    generate_title_forced(config, main_provider, user_message, assistant_response).await
}

/// Title generation ignoring the `auxiliary.title_generation.enabled` gate
/// — used by `sessions retitle-skills`, an explicit repair command that
/// must work even when live auto-titling is switched off.
pub async fn generate_title_forced(
    config: &UlncLawConfig,
    main_provider: Arc<dyn Provider>,
    user_message: &str,
    assistant_response: &str,
) -> Option<String> {
    // Truncate long messages to keep the request small.
    let user_snippet = truncate_chars(user_message, SNIPPET_CHARS);
    let assistant_snippet = truncate_chars(assistant_response, SNIPPET_CHARS);

    let prompt = match title_language(config) {
        Some(language) => TITLE_PROMPT_PINNED_LANGUAGE.replace("{language}", &language),
        None => TITLE_PROMPT.to_string(),
    };

    let messages = vec![
        Message {
            role: Role::System,
            content: Some(prompt),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
        Message {
            role: Role::User,
            content: Some(format!(
                "User: {}\n\nAssistant: {}",
                user_snippet, assistant_snippet
            )),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        },
    ];

    let resolution = match crate::provider::auxiliary::resolve_aux_task(
        config,
        TASK_TITLE_GENERATION,
        main_provider,
    ) {
        Ok(resolution) => resolution,
        Err(e) => {
            // WARNING (not debug) so operators see it without debug mode.
            tracing::warn!("Title generation failed: auxiliary routing: {}", e);
            return None;
        }
    };

    let request = ProviderRequest {
        messages,
        tools: Vec::new(),
        model: resolution.model,
        max_tokens: Some(500),
        temperature: Some(0.3),
        stream: false,
        stop: None,

        images: None,
    };

    let content = match resolution.provider.chat_completion(request).await {
        Ok(response) => response.content.unwrap_or_default(),
        Err(e) => {
            tracing::warn!("Title generation failed: {}", e);
            return None;
        }
    };

    clean_title(&content)
}

/// Fire-and-forget title generation after the first exchange (hermes
/// `maybe_auto_title`). Only generates a title when:
///   - this appears to be one of the first two user turns (the history
///     already includes the exchange that just happened — be generous),
///   - no title is already set (a manual title wins),
///   - `auxiliary.title_generation.enabled` is not false.
///
/// The LLM call runs on a background task, so it never adds latency to the
/// user-facing reply.
pub fn maybe_auto_title(
    config: UlncLawConfig,
    store: Arc<SqliteSessionStore>,
    session_id: String,
    user_message: String,
    assistant_response: String,
    user_turns: usize,
    main_provider: Arc<dyn Provider>,
) {
    if session_id.is_empty() || user_message.is_empty() || assistant_response.is_empty() {
        return;
    }
    if user_turns > 2 {
        return;
    }
    if !auto_title_enabled(&config) {
        tracing::debug!("Auto-title skipped: auxiliary.title_generation.enabled=false");
        return;
    }

    tokio::spawn(async move {
        // Check if a title already exists (the user may have set one
        // before the first response).
        match store.get_session_title(&session_id) {
            Ok(Some(existing)) if !existing.trim().is_empty() => return,
            Err(_) => return,
            _ => {}
        }

        let Some(title) =
            generate_title(&config, main_provider, &user_message, &assistant_response).await
        else {
            return;
        };

        // Atomic predicate write: a manual title set while generation was
        // in flight is never overwritten.
        match store.set_auto_title_if_empty(&session_id, &title) {
            Ok(true) => tracing::debug!("Auto-generated session title: {}", title),
            Ok(false) => tracing::debug!(
                "Skipping auto-generated session title because a title was set while generation was in flight"
            ),
            Err(e) => tracing::debug!("Failed to set auto-generated title: {}", e),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from_toml(toml_text: &str) -> UlncLawConfig {
        toml::from_str(toml_text).expect("test TOML parses")
    }

    #[test]
    fn clean_title_strips_quotes_and_prefix() {
        assert_eq!(
            clean_title("\"Fix the build\"").as_deref(),
            Some("Fix the build")
        );
        assert_eq!(
            clean_title("'Fix the build'").as_deref(),
            Some("Fix the build")
        );
        assert_eq!(
            clean_title("Title: Fix the build").as_deref(),
            Some("Fix the build")
        );
        assert_eq!(
            clean_title("title:   Fix the build").as_deref(),
            Some("Fix the build")
        );
    }

    #[test]
    fn clean_title_strips_reasoning_blocks() {
        let raw = format!("<think>reasoning about the request</think>Fix the build");
        assert_eq!(clean_title(&raw).as_deref(), Some("Fix the build"));
    }

    #[test]
    fn clean_title_keeps_first_nonempty_line() {
        let raw = "Fix the build\n\n- step one\n- step two";
        assert_eq!(clean_title(raw).as_deref(), Some("Fix the build"));
        let raw_with_blank = "\n\nActual title\nrest";
        assert_eq!(clean_title(raw_with_blank).as_deref(), Some("Actual title"));
    }

    #[test]
    fn clean_title_caps_length() {
        let long = "a".repeat(120);
        let cleaned = clean_title(&long).expect("non-empty");
        assert_eq!(cleaned.chars().count(), 80);
        assert!(cleaned.ends_with("..."));
    }

    #[test]
    fn clean_title_empty_input_is_none() {
        assert_eq!(clean_title(""), None);
        assert_eq!(clean_title("   "), None);
        assert_eq!(clean_title("\"\""), None);
    }

    #[test]
    fn enabled_defaults_to_true() {
        let config = UlncLawConfig::default();
        assert!(auto_title_enabled(&config));
    }

    #[test]
    fn enabled_honors_bool_and_string_toggles() {
        let config = config_from_toml("[auxiliary.title_generation]\nenabled = false\n");
        assert!(!auto_title_enabled(&config));

        let config = config_from_toml("[auxiliary.title_generation]\nenabled = \"off\"\n");
        assert!(!auto_title_enabled(&config));

        let config = config_from_toml("[auxiliary.title_generation]\nenabled = \"yes\"\n");
        assert!(auto_title_enabled(&config));

        // Unrecognized text falls back to the default (true).
        let config = config_from_toml("[auxiliary.title_generation]\nenabled = \"maybe\"\n");
        assert!(auto_title_enabled(&config));
    }

    #[test]
    fn language_pin_roundtrip() {
        let config = config_from_toml("[auxiliary.title_generation]\nlanguage = \"German\"\n");
        assert_eq!(title_language(&config).as_deref(), Some("German"));

        let config = config_from_toml("[auxiliary.title_generation]\nlanguage = \"  \"\n");
        assert_eq!(title_language(&config), None);
    }

    #[test]
    fn snippet_truncation_is_char_safe() {
        let text = "汉".repeat(600);
        assert_eq!(truncate_chars(&text, SNIPPET_CHARS).chars().count(), 500);
    }

    #[test]
    fn pinned_language_prompt_substitution() {
        let prompt = TITLE_PROMPT_PINNED_LANGUAGE.replace("{language}", "German");
        assert!(prompt.contains("Write the title in German."));
        assert!(!prompt.contains("{language}"));
    }
}

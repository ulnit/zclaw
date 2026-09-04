//! Shared text-layout helpers for raw-mode terminal surfaces (session
//! browse detail pane and friends).

/// Greedy word-wrap over Unicode text (char-width based, no ANSI
/// awareness — callers strip styling before wrapping). Paragraphs split
/// on `\n`; whitespace runs collapse to single spaces; words longer than
/// `width` hard-break.
pub fn wrap_display_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        let mut pending: std::collections::VecDeque<String> = paragraph
            .split_whitespace()
            .map(|w| w.to_string())
            .collect();
        if pending.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut len = 0usize;
        while let Some(word) = pending.pop_front() {
            let word_len = word.chars().count();
            if word_len > width {
                if len > 0 {
                    out.push(std::mem::take(&mut line));
                    len = 0;
                }
                let split_at = word
                    .char_indices()
                    .nth(width)
                    .map(|(idx, _)| idx)
                    .unwrap_or(word.len());
                let (head, tail) = word.split_at(split_at);
                out.push(head.to_string());
                if !tail.is_empty() {
                    pending.push_front(tail.to_string());
                }
                continue;
            }
            if len == 0 {
                line.push_str(&word);
                len = word_len;
            } else if len + 1 + word_len <= width {
                line.push(' ');
                line.push_str(&word);
                len += 1 + word_len;
            } else {
                out.push(std::mem::take(&mut line));
                len = 0;
                pending.push_front(word);
            }
        }
        if len > 0 {
            out.push(line);
        }
    }
    out
}

/// Alphabetical browse-sort key: case-insensitive title, untitled
/// sessions last, recency as the tie-breaker (matches the browse TUI
/// `F2` sort toggle).
pub fn browse_title_sort_key(title: Option<&str>) -> (u8, String) {
    match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => (0, t.to_lowercase()),
        None => (1, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_basic_words() {
        let lines = wrap_display_text("hello world foo", 11);
        assert_eq!(lines, vec!["hello world", "foo"]);
    }

    #[test]
    fn wrap_preserves_paragraph_breaks() {
        let lines = wrap_display_text("a b\n\nc", 10);
        assert_eq!(lines, vec!["a b", "", "c"]);
    }

    #[test]
    fn wrap_hard_breaks_long_words() {
        let lines = wrap_display_text("abcdefghij", 4);
        assert_eq!(lines, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_long_word_after_short_line() {
        let lines = wrap_display_text("x abcdefgh", 4);
        assert_eq!(lines, vec!["x", "abcd", "efgh"]);
    }

    #[test]
    fn wrap_collapse_whitespace_runs() {
        let lines = wrap_display_text("a   b\t\tc", 40);
        assert_eq!(lines, vec!["a b c"]);
    }

    #[test]
    fn wrap_counts_unicode_chars_not_bytes() {
        let lines = wrap_display_text("会话浏览 原始模式 升级", 9);
        assert_eq!(lines, vec!["会话浏览 原始模式", "升级"]);
    }

    #[test]
    fn wrap_empty_input() {
        let lines = wrap_display_text("", 10);
        assert_eq!(lines, vec![""]);
    }

    #[test]
    fn wrap_zero_width_degrades_to_one() {
        let lines = wrap_display_text("ab cd", 0);
        assert_eq!(lines, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn sort_key_orders_untitled_last() {
        let a = browse_title_sort_key(Some("Zebra"));
        let b = browse_title_sort_key(Some("apple"));
        let c = browse_title_sort_key(None);
        let d = browse_title_sort_key(Some("   "));
        assert!(b < a); // case-insensitive
        assert!(a < c);
        assert_eq!(c, d); // blank titles count as untitled
    }
}

/// Render the first user/assistant exchange of a session for the browse
/// details pane (P278): role-labelled snippets, each truncated to
/// `per_message_chars` display chars with an ellipsis. Empty or
/// content-less exchanges render as an empty string.
pub fn browse_conversation_preview(
    exchange: &[(String, Option<String>)],
    per_message_chars: usize,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (role, content) in exchange {
        let Some(text) = content.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let truncated: String = if flat.chars().count() > per_message_chars {
            flat.chars().take(per_message_chars).collect::<String>() + "\u{2026}"
        } else {
            flat
        };
        lines.push(format!("{role}: {truncated}"));
    }
    lines.join("\n")
}

/// Keybinding table for the raw-mode session browser help overlay
/// (P224 interaction upgrade). `(key, description)` pairs in display
/// order; the TUI renders them as a dismissible overlay.
pub fn browse_help_entries() -> &'static [(&'static str, &'static str)] {
    &[
        ("\u{2191}/\u{2193}", "navigate (wraps at the list edges)"),
        ("PgUp/PgDn", "scroll a page at a time"),
        ("Home/End", "jump to the first / last session"),
        ("Enter", "select and resume the highlighted session"),
        (
            "v",
            "view the highlighted session's full transcript (Esc returns)",
        ),
        (
            "r",
            "in the transcript viewer: toggle raw \u{2194} pretty (timestamps + tool calls)",
        ),
        (
            "Type",
            "live filter over title, preview, id, source, project",
        ),
        ("Backspace", "delete one filter character"),
        ("Esc", "clear the filter; a second press quits"),
        ("q", "quit while no filter is active"),
        (
            "Tab",
            "cycle the source filter (all \u{2192} cli \u{2192} cron \u{2192} \u{2026})",
        ),
        ("Shift+Tab", "cycle the source filter backwards"),
        ("F2", "toggle recent-first \u{2194} alphabetical sort"),
        ("F3", "toggle the conversation preview in the details pane"),
        (
            "Ctrl+U/D",
            "scroll the details pane when the preview overflows",
        ),
        (
            "Ctrl+\u{2191}/\u{2193}",
            "scroll the details pane one line at a time (P551)",
        ),
        (
            "Ctrl+Home/End",
            "jump the details pane to the top / bottom (P551)",
        ),
        ("F4", "toggle archived sessions into the list"),
        ("F5", "reload the session list from disk"),
        (
            "F6",
            "rename the highlighted session (Enter saves, Esc cancels)",
        ),
        ("F7", "fork the highlighted session (y confirms)"),
        (
            "F8",
            "archive / unarchive the highlighted session (y confirms)",
        ),
        ("F9", "delete the highlighted session forever (y confirms)"),
        (
            "F10",
            "cycle the model filter (all \u{2192} model \u{2192} \u{2026}; Shift cycles backwards)",
        ),
        ("F11", "export the highlighted session to Markdown (P585)"),
        (
            "/",
            "search message bodies (FTS) while no filter is typed \u{2014} Enter runs, Esc cancels",
        ),
        ("F1", "toggle this help overlay"),
        ("Ctrl+L", "redraw the screen"),
        ("Ctrl+C", "quit"),
    ]
}

/// Footer confirmation prompt for archiving the highlighted session from
/// the browser (P224). Keeps the label short so it fits narrow terminals.
pub fn browse_archive_confirm_text(label: &str) -> String {
    let mut label: String = label.chars().take(40).collect();
    if label.chars().count() == 40 {
        label.push('\u{2026}');
    }
    format!("Archive \u{201C}{label}\u{201D}?  y = archive \u{00B7} any other key = cancel")
}

/// Footer confirmation prompt for restoring an archived session from
/// the browser (P520). F8 toggles archive ↔ unarchive.
pub fn browse_unarchive_confirm_text(label: &str) -> String {
    let mut label: String = label.chars().take(40).collect();
    if label.chars().count() == 40 {
        label.push('\u{2026}');
    }
    format!("Unarchive \u{201C}{label}\u{201D}?  y = unarchive \u{00B7} any other key = cancel")
}

/// Footer confirmation prompt for deleting the highlighted session from
/// the browser (P340). Deletion is permanent — the wording says so.
pub fn browse_delete_confirm_text(label: &str) -> String {
    let mut label: String = label.chars().take(40).collect();
    if label.chars().count() == 40 {
        label.push('\u{2026}');
    }
    format!("Delete \u{201C}{label}\u{201D} forever?  y = delete \u{00B7} any other key = cancel")
}

/// Footer prompt while typing a transcript-search query (P340).
pub fn browse_transcript_search_prompt(query: &str) -> String {
    format!("transcript search: {query}\u{258F}  Enter = search \u{00B7} Esc = cancel")
}

/// Footer confirmation prompt for forking the highlighted session from
/// the browser (P512). Forking marks the source branched and opens a
/// child session carrying the transcript forward.
pub fn browse_fork_confirm_text(label: &str) -> String {
    let mut label: String = label.chars().take(40).collect();
    if label.chars().count() == 40 {
        label.push('\u{2026}');
    }
    format!("Fork \u{201C}{label}\u{201D}?  y = fork \u{00B7} any other key = cancel")
}

/// Footer prompt while typing a new title for the highlighted session
/// (P512). Empty input on Enter keeps the current title.
pub fn browse_rename_prompt_text(label: &str, buffer: &str) -> String {
    let mut label: String = label.chars().take(30).collect();
    if label.chars().count() == 30 {
        label.push('\u{2026}');
    }
    format!("Rename \u{201C}{label}\u{201D}: {buffer}\u{258F}  Enter = save \u{00B7} Esc = cancel")
}

#[cfg(test)]
mod browse_tui_upgrade_tests {
    #[test]
    fn browse_help_entries_cover_core_keys() {
        let entries = super::browse_help_entries();
        let keys: Vec<&str> = entries.iter().map(|(k, _)| *k).collect();
        for expected in [
            "Enter",
            "Esc",
            "Tab",
            "F1",
            "F2",
            "F4",
            "F5",
            "F6",
            "F7",
            "F8",
            "F9",
            "F10",
            "Ctrl+U/D",
            "Ctrl+\u{2191}/\u{2193}",
            "Ctrl+Home/End",
            "/",
            "Shift+Tab",
        ] {
            assert!(keys.contains(&expected), "missing help row for {expected}");
        }
        // Every entry has a non-empty description.
        assert!(entries.iter().all(|(_, d)| !d.is_empty()));
    }

    #[test]
    fn browse_unarchive_confirm_text_matches_archive_style() {
        let text = super::browse_unarchive_confirm_text("Fix the build");
        assert!(text.contains("Fix the build"));
        assert!(text.contains("y = unarchive"));
        let long = "x".repeat(80);
        assert!(super::browse_unarchive_confirm_text(&long).contains('\u{2026}'));
    }

    #[test]
    fn browse_fork_and_rename_prompts() {
        let fork = super::browse_fork_confirm_text("Fix the build");
        assert!(fork.contains("Fix the build"));
        assert!(fork.contains("y = fork"));
        let long = "x".repeat(80);
        let fork_long = super::browse_fork_confirm_text(&long);
        assert!(fork_long.contains('\u{2026}'));

        let rename = super::browse_rename_prompt_text("Fix the build", "New title");
        assert!(rename.contains("Fix the build"));
        assert!(rename.contains("New title"));
        assert!(rename.contains("Enter = save"));
        // The buffer renders live as the user types.
        let empty = super::browse_rename_prompt_text("Fix the build", "");
        assert!(empty.contains("Enter = save"));
    }

    #[test]
    fn browse_conversation_preview_renders_and_truncates() {
        let exchange = vec![
            (
                "you".to_string(),
                Some("Fix the   build\nplease".to_string()),
            ),
            (
                "assistant".to_string(),
                Some("On it — running cargo build.".to_string()),
            ),
            ("tool".to_string(), None),
        ];
        let preview = super::browse_conversation_preview(&exchange, 12);
        let lines: Vec<&str> = preview.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("you: Fix the"));
        assert!(lines[0].ends_with("\u{2026}"));
        assert!(lines[1].starts_with("assistant: On it \u{2014} run"));
        // Short content stays whole.
        let short = super::browse_conversation_preview(&exchange, 500);
        assert!(short.contains("you: Fix the build please"));
        // No usable content → empty string.
        assert_eq!(
            super::browse_conversation_preview(&[("tool".to_string(), None)], 100),
            ""
        );
    }

    #[test]
    fn browse_delete_confirm_text_truncates_long_labels() {
        let text = super::browse_delete_confirm_text("Fix the build");
        assert!(text.contains("Fix the build"));
        assert!(text.contains("y = delete"));
        assert!(text.contains("forever"));

        let long = "x".repeat(80);
        let text = super::browse_delete_confirm_text(&long);
        assert!(text.contains(&"x".repeat(40)));
        assert!(!text.contains(&"x".repeat(41)));
        assert!(text.ends_with("cancel"));
    }

    #[test]
    fn browse_transcript_search_prompt_shows_query_and_keys() {
        let text = super::browse_transcript_search_prompt("panic");
        assert!(text.contains("panic"));
        assert!(text.contains("Enter = search"));
        assert!(text.contains("Esc = cancel"));
    }

    #[test]
    fn browse_archive_confirm_text_truncates_long_labels() {
        let text = super::browse_archive_confirm_text("Fix the build");
        assert!(text.contains("Fix the build"));
        assert!(text.contains("y = archive"));

        let long = "x".repeat(80);
        let text = super::browse_archive_confirm_text(&long);
        // 40 kept chars + ellipsis, never the full 80.
        assert!(text.contains(&"x".repeat(40)));
        assert!(!text.contains(&"x".repeat(41)));
        assert!(text.ends_with("cancel"));
    }
}

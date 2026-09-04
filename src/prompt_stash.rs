//! Prompt stash — port of hermes `hermes_cli/prompt_stash.py`.
//!
//! Park a half-written prompt, send something else, then bring the draft
//! back. Mirrors Claude Code's "ctrl + s to stash prompt" affordance; in
//! the line-based ulnclaw REPL the gesture maps onto `/stash [text]`.
//!
//! Gesture (hermes):
//! - Buffer has content  → push it onto the stash, clear the composer.
//! - Buffer empty, 1 item → pop it straight back into the composer.
//! - Buffer empty, 2+ items → open the browse panel (↑↓ / Enter / D / Esc).
//!
//! Newest-first ordering: index 0 is always the most recently stashed
//! draft, so the common "undo my last stash" case is a single keystroke.
//!
//! Nothing is written to disk. Drafts frequently contain credentials,
//! prompts under NDA, or pasted secrets, and a session-scoped stash keeps
//! that material in memory only.

/// Single-line preview length for the browse panel.
pub const PREVIEW_WIDTH: usize = 60;

/// Cap the stack so a user leaning on stash can't grow it without bound.
pub const MAX_STASH_ITEMS: usize = 20;

/// Collapse a possibly multi-line draft into one preview line.
/// Newlines and tabs become `⏎`/space so a 40-line draft still renders as
/// a single panel row, ellipsized to `width` display chars.
pub fn build_preview(text: &str, width: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let flat = text.replace("\r\n", "\n").replace('\r', "\n");
    let flat = flat.replace('\n', " ⏎ ").replace('\t', " ");
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if width > 1 && chars.len() > width {
        let truncated: String = chars[..width - 1].iter().collect();
        format!("{truncated}…")
    } else {
        flat
    }
}

/// One parked draft: exact text plus any image paths that were attached.
#[derive(Debug, Clone)]
pub struct StashEntry {
    pub text: String,
    pub images: Vec<String>,
    pub stashed_at: f64,
    pub preview: String,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Session-scoped stack of parked composer drafts. Pure state: no I/O
/// beyond the wall clock (hermes uses time.monotonic; epoch seconds are
/// close enough for the "stashed N ago" display).
#[derive(Debug, Default)]
pub struct PromptStash {
    items: Vec<StashEntry>,
    max_items: usize,
    pub panel_open: bool,
    pub panel_cursor: usize,
}

impl PromptStash {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            max_items: MAX_STASH_ITEMS,
            panel_open: false,
            panel_cursor: 0,
        }
    }

    pub fn with_max_items(max_items: usize) -> Self {
        Self {
            max_items: max_items.max(1),
            ..Self::new()
        }
    }

    // ------------------------------------------------------------- queries

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Newest-first list of entries (a copy — mutate via the API).
    pub fn items(&self) -> Vec<StashEntry> {
        self.items.clone()
    }

    /// Status-bar indicator, or `""` when the stash is empty.
    /// `📌 2` when idle, `📌 2 ▲` while the browse panel is open.
    pub fn indicator(&self) -> String {
        let n = self.items.len();
        if n == 0 {
            return String::new();
        }
        if self.panel_open {
            format!("📌 {n} ▲")
        } else {
            format!("📌 {n}")
        }
    }

    /// Composer placeholder text advertising the stashed draft.
    pub fn placeholder_hint(&self) -> String {
        let n = self.items.len();
        if n == 0 {
            return String::new();
        }
        if n == 1 {
            format!("Ctrl+S to restore: {}", self.items[0].preview)
        } else {
            format!("Ctrl+S to browse {n} stashed drafts")
        }
    }

    // ------------------------------------------------------------ mutators

    /// Push a draft. Returns false (no-op) for a blank buffer: a buffer
    /// that is empty or whitespace-only is not worth parking and must
    /// stay a no-op, otherwise stashing on an empty composer would push a
    /// junk entry instead of triggering the restore half of the gesture.
    /// Text is stored verbatim — leading/trailing whitespace and newlines
    /// are preserved so a restore round-trips byte-for-byte.
    pub fn stash(&mut self, text: &str, images: &[String]) -> bool {
        let has_images = !images.is_empty();
        if text.trim().is_empty() && !has_images {
            return false;
        }
        let preview = build_preview(text, PREVIEW_WIDTH);
        let entry = StashEntry {
            text: text.to_string(),
            images: images.to_vec(),
            stashed_at: now_secs(),
            preview: if preview.is_empty() {
                "(images only)".to_string()
            } else {
                preview
            },
        };
        self.items.insert(0, entry);
        // Drop the oldest entries past the cap.
        self.items.truncate(self.max_items);
        // A push invalidates any open browse session.
        self.panel_open = false;
        self.panel_cursor = 0;
        true
    }

    /// Remove and return `(text, images)` at `index`, or None.
    pub fn pop(&mut self, index: usize) -> Option<(String, Vec<String>)> {
        if index >= self.items.len() {
            return None;
        }
        let entry = self.items.remove(index);
        if self.items.is_empty() {
            self.panel_open = false;
        }
        self.panel_cursor = self.clamp_cursor(self.panel_cursor);
        Some((entry.text, entry.images))
    }

    /// Return the entry at `index` without removing it.
    pub fn peek(&self, index: usize) -> Option<&StashEntry> {
        self.items.get(index)
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.panel_open = false;
        self.panel_cursor = 0;
    }

    // --------------------------------------------------------- panel state

    fn clamp_cursor(&self, value: usize) -> usize {
        if self.items.is_empty() {
            return 0;
        }
        value.min(self.items.len() - 1)
    }

    /// Open the browse panel. False when there is nothing to browse.
    pub fn open_panel(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        self.panel_open = true;
        self.panel_cursor = 0;
        true
    }

    pub fn close_panel(&mut self) {
        self.panel_open = false;
        self.panel_cursor = 0;
    }

    /// Move the panel cursor by a signed delta, clamped to the bounds.
    pub fn move_cursor(&mut self, delta: isize) -> usize {
        let current = self.panel_cursor as isize;
        let moved = if delta < 0 {
            (current + delta).max(0) as usize
        } else {
            self.clamp_cursor(current as usize + delta as usize)
        };
        self.panel_cursor = self.clamp_cursor(moved);
        self.panel_cursor
    }

    /// Delete the highlighted entry. False when there was nothing to drop.
    pub fn delete_at_cursor(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let idx = self.clamp_cursor(self.panel_cursor);
        self.items.remove(idx);
        if self.items.is_empty() {
            self.panel_open = false;
            self.panel_cursor = 0;
        } else {
            self.panel_cursor = self.clamp_cursor(idx);
        }
        true
    }

    /// Pop the highlighted entry and close the panel.
    pub fn restore_at_cursor(&mut self) -> Option<(String, Vec<String>)> {
        if self.items.is_empty() {
            return None;
        }
        let result = self.pop(self.clamp_cursor(self.panel_cursor));
        self.close_panel();
        result
    }
}

// ---------------------------------------------------------------------------
// Gesture decision table
// ---------------------------------------------------------------------------

/// Outcomes of a single Ctrl+S press (hermes ACTION_* constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashAction {
    Noop,
    Stashed,
    Restored,
    OpenPanel,
    ClosePanel,
}

/// Decide what one Ctrl+S press does (hermes `resolve_ctrl_s`).
/// `payload` carries `(text, images)` for `Restored`, else None. The
/// whole decision table in one pure function so callers stay thin.
pub fn resolve_ctrl_s(
    stash: &mut PromptStash,
    buffer_text: &str,
    images: &[String],
) -> (StashAction, Option<(String, Vec<String>)>) {
    // Panel open → Ctrl+S is the "close it" escape hatch.
    if stash.panel_open {
        stash.close_panel();
        return (StashAction::ClosePanel, None);
    }
    // Something to park → park it. Never silently clobbers an existing
    // stash: entries push onto a stack, so earlier drafts stay reachable.
    if !buffer_text.trim().is_empty() || !images.is_empty() {
        return if stash.stash(buffer_text, images) {
            (StashAction::Stashed, None)
        } else {
            (StashAction::Noop, None)
        };
    }
    // Empty buffer → restore half of the gesture.
    match stash.len() {
        0 => (StashAction::Noop, None),
        1 => (StashAction::Restored, stash.pop(0)),
        _ => {
            stash.open_panel();
            (StashAction::OpenPanel, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_collapses_multiline_and_ellipsizes() {
        assert_eq!(build_preview("", 60), "");
        assert_eq!(build_preview("a\nb\tc", 60), "a ⏎ b c");
        let long = "x".repeat(100);
        let preview = build_preview(&long, 60);
        let chars: Vec<char> = preview.chars().collect();
        assert_eq!(chars.len(), 60);
        assert_eq!(*chars.last().unwrap(), '…');
    }

    #[test]
    fn stash_is_newest_first_and_caps() {
        let mut stash = PromptStash::with_max_items(3);
        for idx in 0..5 {
            assert!(stash.stash(&format!("draft {idx}"), &[]));
        }
        assert_eq!(stash.len(), 3);
        assert_eq!(stash.peek(0).unwrap().text, "draft 4");
        assert_eq!(stash.peek(2).unwrap().text, "draft 2");
    }

    #[test]
    fn blank_buffer_is_a_noop() {
        let mut stash = PromptStash::new();
        assert!(!stash.stash("   ", &[]));
        assert!(stash.is_empty());
        // Images alone are worth parking.
        assert!(stash.stash("", &["/tmp/x.png".into()]));
        assert_eq!(stash.peek(0).unwrap().preview, "(images only)");
    }

    #[test]
    fn pop_roundtrips_verbatim() {
        let mut stash = PromptStash::new();
        let draft = "  line one\nline two  \n";
        stash.stash(draft, &["img.png".into()]);
        let (text, images) = stash.pop(0).unwrap();
        assert_eq!(text, draft);
        assert_eq!(images, vec!["img.png".to_string()]);
        assert!(stash.pop(0).is_none());
    }

    #[test]
    fn ctrl_s_gesture_decision_table() {
        let mut stash = PromptStash::new();
        // Empty buffer + empty stash → noop.
        assert_eq!(resolve_ctrl_s(&mut stash, "", &[]).0, StashAction::Noop);
        // Content → stashed.
        assert_eq!(
            resolve_ctrl_s(&mut stash, "wip", &[]).0,
            StashAction::Stashed
        );
        // Empty buffer + 1 item → restored verbatim.
        let (action, payload) = resolve_ctrl_s(&mut stash, "  ", &[]);
        assert_eq!(action, StashAction::Restored);
        assert_eq!(payload.unwrap().0, "wip");
        // Two items → panel opens; another press closes it.
        stash.stash("one", &[]);
        stash.stash("two", &[]);
        assert_eq!(
            resolve_ctrl_s(&mut stash, "", &[]).0,
            StashAction::OpenPanel
        );
        assert!(stash.panel_open);
        assert_eq!(
            resolve_ctrl_s(&mut stash, "", &[]).0,
            StashAction::ClosePanel
        );
    }

    #[test]
    fn panel_cursor_moves_clamp_and_delete() {
        let mut stash = PromptStash::new();
        stash.stash("a", &[]);
        stash.stash("b", &[]);
        stash.stash("c", &[]);
        stash.open_panel();
        assert_eq!(stash.move_cursor(1), 1);
        assert_eq!(stash.move_cursor(5), 2); // clamped
        assert_eq!(stash.move_cursor(-9), 0); // clamped
        assert!(stash.delete_at_cursor()); // drops "c" (index 0)
        assert_eq!(stash.peek(0).unwrap().text, "b");
        let restored = stash.restore_at_cursor().unwrap();
        assert_eq!(restored.0, "b");
        assert!(!stash.panel_open);
    }

    #[test]
    fn indicator_and_placeholder() {
        let mut stash = PromptStash::new();
        assert_eq!(stash.indicator(), "");
        assert_eq!(stash.placeholder_hint(), "");
        stash.stash("only draft", &[]);
        assert_eq!(stash.indicator(), "📌 1");
        assert!(stash.placeholder_hint().starts_with("Ctrl+S to restore:"));
        stash.stash("second", &[]);
        assert!(stash.placeholder_hint().contains("browse 2"));
        stash.open_panel();
        assert_eq!(stash.indicator(), "📌 2 ▲");
    }
}

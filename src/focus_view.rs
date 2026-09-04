//! Focus view — port of hermes `hermes_cli/focus_view.py`.
//!
//! `/focus` answers one question the `/verbose` cycle cannot: *"just show
//! me my prompt and the answer — and tell me what you hid."*
//!
//! `/verbose off` already silences per-tool progress lines. Focus view
//! **composes with** that machinery instead of duplicating it:
//!
//! - turning focus ON snaps the tool-progress mode to `"off"` and
//!   remembers the mode the user had configured, so the existing
//!   suppression path does the actual hiding;
//! - turning focus OFF restores that remembered mode verbatim;
//! - on top of that, focus view adds the two things `/verbose off` lacks —
//!   a per-turn count of what was hidden plus a recovery hint, and a
//!   persistent `focus` segment so the reduced mode is never invisible.
//!
//! Everything in this module is **display-only**. Nothing here reads or
//! mutates conversation history, the system prompt, tool schemas, or any
//! request payload. Flipping focus view must never change a single byte
//! of what is sent to the model.

/// Tool-progress mode focus view snaps to. Deliberately the SAME value
/// `/verbose off` uses so both features share one suppression path.
pub const FOCUS_TOOL_PROGRESS_MODE: &str = "off";

/// Modes in which the CLI commits a per-tool scrollback line (mirrors the
/// hermes renderer gate; kept here so the hidden-line counter and the
/// renderer can never drift apart).
pub const TOOL_PROGRESS_VISIBLE_MODES: &[&str] = &["new", "all", "verbose"];

/// Valid tool-progress modes (`log` is a gateway-only extra step).
pub const TOOL_PROGRESS_MODES: &[&str] = &["off", "new", "all", "verbose"];

/// Status-bar label. Short on purpose — the bar is width-constrained.
pub const FOCUS_STATUSBAR_LABEL: &str = "◉ focus";

const ON_WORDS: &[&str] = &["on", "enable", "enabled", "true", "yes", "1"];
const OFF_WORDS: &[&str] = &["off", "disable", "disabled", "false", "no", "0"];
const STATUS_WORDS: &[&str] = &["status", "show", "?"];
const TOGGLE_WORDS: &[&str] = &["", "toggle"];

pub const FOCUS_USAGE: &str = "Usage: /focus [on|off|status]";

/// Coerce a raw config value into a known tool-progress mode. YAML 1.1
/// parses a bare `off` as false and older configs stored booleans, so
/// this mirrors hermes' normalisation. Unknown values fall back to
/// `default` (hermes default: "all"; `log` is a real gateway mode).
pub fn normalize_tool_progress_mode(mode: Option<&str>, default: &str) -> String {
    let text = mode.unwrap_or("").trim().to_ascii_lowercase();
    match text.as_str() {
        "false" => return "off".to_string(),
        "true" => return "all".to_string(),
        _ => {}
    }
    if TOOL_PROGRESS_MODES.contains(&text.as_str()) {
        return text;
    }
    if text == "log" {
        return "log".to_string();
    }
    default.to_string()
}

/// Resolved `/focus` argument (hermes resolve_focus_arg).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusArg {
    /// Set focus to the given enabled-state.
    Set(bool),
    /// Print the current state.
    Status,
    /// Unrecognized argument — print usage.
    Usage,
}

/// Map a `/focus` argument onto an action, following the sibling toggles.
/// Bare `/focus` toggles, matching /footer / /battery / /timestamps.
pub fn resolve_focus_arg(arg: &str, current: bool) -> FocusArg {
    let text = arg.trim().to_ascii_lowercase();
    if STATUS_WORDS.contains(&text.as_str()) {
        return FocusArg::Status;
    }
    if ON_WORDS.contains(&text.as_str()) {
        return FocusArg::Set(true);
    }
    if OFF_WORDS.contains(&text.as_str()) {
        return FocusArg::Set(false);
    }
    if TOGGLE_WORDS.contains(&text.as_str()) {
        return FocusArg::Set(!current);
    }
    FocusArg::Usage
}

/// The tool-progress mode that should actually be in force. Focus view
/// wins while it is on (it *is* "tool progress off" plus reporting).
/// When focus is off the user's configured mode is returned untouched —
/// this is what makes `/focus off` restore `/verbose verbose` rather
/// than clobbering it to `all`.
pub fn effective_tool_progress_mode(focus_enabled: bool, configured_mode: Option<&str>) -> String {
    let normalized = normalize_tool_progress_mode(configured_mode, "all");
    if focus_enabled {
        FOCUS_TOOL_PROGRESS_MODE.to_string()
    } else {
        normalized
    }
}

/// Would the CLI have committed a scrollback line for this tool call?
/// Used to count *honestly*: if the user already had `/verbose off`,
/// focus view is hiding nothing extra and must not claim otherwise.
/// `new` mode skips consecutive repeats of the same tool, so the counter
/// skips them too.
pub fn would_display_tool_line(
    mode: Option<&str>,
    function_name: &str,
    last_tool_name: Option<&str>,
) -> bool {
    if function_name.is_empty() {
        return false;
    }
    let normalized = normalize_tool_progress_mode(mode, "all");
    if !TOOL_PROGRESS_VISIBLE_MODES.contains(&normalized.as_str()) {
        return false;
    }
    if normalized == "new" && Some(function_name) == last_tool_name {
        return false;
    }
    true
}

/// Dim post-turn recovery line, or `None` when nothing was hidden.
pub fn format_hidden_line(count: u64) -> Option<String> {
    if count == 0 {
        return None;
    }
    let noun = if count == 1 {
        "tool line"
    } else {
        "tool lines"
    };
    Some(format!("⋯ {count} {noun} hidden · /focus off to show"))
}

/// Status-bar segment text for focus view (empty when off).
pub fn focus_statusbar_segment(enabled: bool) -> String {
    if enabled {
        FOCUS_STATUSBAR_LABEL.to_string()
    } else {
        String::new()
    }
}

/// Human-readable `/focus status` body (no ANSI — callers colour it).
pub fn format_focus_status(enabled: bool, configured_mode: Option<&str>) -> String {
    let state = if enabled { "ON" } else { "OFF" };
    if enabled {
        let restore = normalize_tool_progress_mode(configured_mode, "all");
        format!(
            "Focus view: {state} — only your prompt and the final response.\n  /focus off restores tool progress: {}",
            restore.to_uppercase()
        )
    } else {
        let mode = normalize_tool_progress_mode(configured_mode, "all");
        format!(
            "Focus view: {state} — tool progress: {}",
            mode.to_uppercase()
        )
    }
}

/// Confirmation line printed when focus view is switched (no ANSI).
pub fn format_focus_toggle_message(enabled: bool, configured_mode: Option<&str>) -> String {
    if enabled {
        "Focus view enabled — just your prompt and the final response".to_string()
    } else {
        let mode = normalize_tool_progress_mode(configured_mode, "all");
        format!(
            "Focus view disabled — tool progress: {}",
            mode.to_uppercase()
        )
    }
}

/// Per-session REPL display state: the focus/verbose composition plus the
/// honest hidden-line counter (hermes cli.py `_ln_enabled` +
/// `_last_tool_name` scrollback bookkeeping).
#[derive(Debug, Clone)]
pub struct DisplayState {
    pub focus_enabled: bool,
    /// The user's configured tool-progress mode (restored verbatim when
    /// focus turns off).
    pub configured_mode: String,
    /// Last tool name committed to scrollback (drives `new` mode).
    pub last_tool_name: Option<String>,
    /// Tool lines hidden this turn by focus view.
    pub hidden_this_turn: u64,
}

impl Default for DisplayState {
    fn default() -> Self {
        Self {
            focus_enabled: false,
            configured_mode: "all".to_string(),
            last_tool_name: None,
            hidden_this_turn: 0,
        }
    }
}

impl DisplayState {
    /// The mode actually in force right now.
    pub fn effective_mode(&self) -> String {
        effective_tool_progress_mode(self.focus_enabled, Some(&self.configured_mode))
    }

    /// Account for one tool call: returns true when a scrollback line
    /// should be printed under the effective mode, otherwise bumps the
    /// honest hidden counter when focus view is what suppressed it.
    pub fn on_tool_call(&mut self, function_name: &str) -> bool {
        let display = would_display_tool_line(
            Some(&self.effective_mode()),
            function_name,
            self.last_tool_name.as_deref(),
        );
        if display {
            self.last_tool_name = Some(function_name.to_string());
            return true;
        }
        // Count honestly: only when the CONFIGURED mode would have shown
        // the line (hermes would_display_tool_line on configured mode).
        if self.focus_enabled
            && would_display_tool_line(
                Some(&self.configured_mode),
                function_name,
                self.last_tool_name.as_deref(),
            )
        {
            self.hidden_this_turn += 1;
        }
        false
    }

    /// Reset the per-turn hidden counter; returns what was accumulated.
    pub fn take_hidden_count(&mut self) -> u64 {
        std::mem::take(&mut self.hidden_this_turn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles_booleans_and_unknowns() {
        assert_eq!(normalize_tool_progress_mode(Some("false"), "all"), "off");
        assert_eq!(normalize_tool_progress_mode(Some("true"), "all"), "all");
        assert_eq!(normalize_tool_progress_mode(Some("OFF"), "all"), "off");
        assert_eq!(
            normalize_tool_progress_mode(Some("verbose"), "all"),
            "verbose"
        );
        assert_eq!(normalize_tool_progress_mode(Some("log"), "all"), "log");
        assert_eq!(normalize_tool_progress_mode(Some("bogus"), "all"), "all");
        assert_eq!(normalize_tool_progress_mode(None, "new"), "new");
    }

    #[test]
    fn focus_arg_resolution() {
        assert_eq!(resolve_focus_arg("on", false), FocusArg::Set(true));
        assert_eq!(resolve_focus_arg("OFF", true), FocusArg::Set(false));
        assert_eq!(resolve_focus_arg("status", true), FocusArg::Status);
        assert_eq!(resolve_focus_arg("?", false), FocusArg::Status);
        // Bare /focus toggles.
        assert_eq!(resolve_focus_arg("", true), FocusArg::Set(false));
        assert_eq!(resolve_focus_arg("", false), FocusArg::Set(true));
        assert_eq!(resolve_focus_arg("banana", true), FocusArg::Usage);
    }

    #[test]
    fn effective_mode_focus_wins_then_restores() {
        assert_eq!(effective_tool_progress_mode(true, Some("verbose")), "off");
        assert_eq!(
            effective_tool_progress_mode(false, Some("verbose")),
            "verbose"
        );
        assert_eq!(effective_tool_progress_mode(false, None), "all");
    }

    #[test]
    fn hidden_count_is_honest() {
        // User already had /verbose off → focus hides nothing extra.
        assert!(!would_display_tool_line(Some("off"), "shell", None));
        // new mode skips consecutive repeats.
        assert!(would_display_tool_line(Some("new"), "shell", None));
        assert!(!would_display_tool_line(
            Some("new"),
            "shell",
            Some("shell")
        ));
        assert!(would_display_tool_line(Some("all"), "shell", Some("shell")));
        assert!(!would_display_tool_line(Some("all"), "", None));
    }

    #[test]
    fn hidden_line_formats() {
        assert_eq!(format_hidden_line(0), None);
        assert_eq!(
            format_hidden_line(1).unwrap(),
            "⋯ 1 tool line hidden · /focus off to show"
        );
        assert_eq!(
            format_hidden_line(7).unwrap(),
            "⋯ 7 tool lines hidden · /focus off to show"
        );
    }

    #[test]
    fn display_state_counts_only_when_focus_hides() {
        let mut state = DisplayState::default(); // configured "all"
                                                 // Focus OFF, mode all → lines display.
        assert!(state.on_tool_call("shell"));
        assert_eq!(state.hidden_this_turn, 0);
        // Focus ON → suppressed and counted.
        state.focus_enabled = true;
        assert!(!state.on_tool_call("shell"));
        assert_eq!(state.hidden_this_turn, 1);
        assert!(!state.on_tool_call("read_file"));
        assert_eq!(state.hidden_this_turn, 2);
        assert_eq!(state.take_hidden_count(), 2);
        assert_eq!(state.take_hidden_count(), 0);
        // Focus OFF restores the configured mode verbatim.
        state.focus_enabled = false;
        assert_eq!(state.effective_mode(), "all");
    }

    #[test]
    fn display_state_respects_configured_off() {
        let mut state = DisplayState {
            configured_mode: "off".into(),
            focus_enabled: true,
            ..Default::default()
        };
        // Configured off + focus on → suppressed but NOT counted (hermes
        // honesty invariant: focus hides nothing the user hadn't hidden).
        assert!(!state.on_tool_call("shell"));
        assert_eq!(state.hidden_this_turn, 0);
        let status = format_focus_status(true, Some("off"));
        assert!(status.contains("ON"));
        assert!(status.contains("restores tool progress: OFF"));
    }

    #[test]
    fn status_and_toggle_messages() {
        let status_on = format_focus_status(true, Some("verbose"));
        assert!(status_on.contains("ON"));
        assert!(status_on.contains("VERBOSE"));
        let status_off = format_focus_status(false, None);
        assert!(status_off.contains("OFF"));
        assert!(status_off.contains("ALL"));
        assert!(format_focus_toggle_message(true, None).starts_with("Focus view enabled"));
        assert!(format_focus_toggle_message(false, Some("new")).contains("NEW"));
        assert_eq!(focus_statusbar_segment(true), "◉ focus");
        assert_eq!(focus_statusbar_segment(false), "");
    }
}

//! ANSI escape-sequence stripping — port of hermes' `tools/ansi_strip.py`.
//!
//! Used by the terminal and execute_code tools to clean command output
//! before returning it to the model. This prevents ANSI codes from
//! entering the model's context — which is the root cause of models
//! copying escape sequences into file writes.
//!
//! Covers the full ECMA-48 spec: CSI (including private-mode `?` prefix,
//! colon-separated params, intermediate bytes), OSC (BEL and ST
//! terminators), DCS/SOS/PM/APC string sequences, nF multi-byte escapes,
//! Fp/Fe/Fs single-byte escapes, and 8-bit C1 control characters.

use regex::Regex;
use std::borrow::Cow;
use std::sync::OnceLock;

fn ansi_escape_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\x1b(?:\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\][\s\S]*?(?:\x07|\x1b\\)|[PX^_][\s\S]*?(?:\x1b\\)|[\x20-\x2f]+[\x30-\x7e]|[\x30-\x7e])|\x9b[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\x9d[\s\S]*?(?:\x07|\x9c)|[\x80-\x9f]",
        )
        .expect("static regex")
    })
}

/// Fast-path check — skip the full regex when no escape-like bytes exist.
fn has_escape_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\x1b\x80-\x9f]").expect("static regex"))
}

/// C0 control characters (minus tab/newline/carriage-return, handled
/// separately) plus DEL. These survive [`strip_ansi`] — it only removes
/// well-formed escape *sequences* — but are still dangerous or garbled
/// when echoed back to a terminal.
fn control_chars_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").expect("static regex"))
}

/// Fast-path check for [`sanitize_display_text`] — any C0 control (except
/// tab/newline), CR, DEL, ESC, or C1 byte triggers the slow path.
fn has_control_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\x00-\x08\x0b-\x1f\x7f-\x9f]").expect("static regex"))
}

/// Remove ANSI escape sequences from text.
///
/// Returns the input unchanged (fast path) when no ESC or C1 bytes are
/// present. Safe to call on any string — clean text passes through with
/// negligible overhead.
pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    if text.is_empty() || !has_escape_re().is_match(text) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(ansi_escape_re().replace_all(text, "").into_owned())
}

/// Sanitize stored/untrusted text before echoing it to a terminal.
///
/// Removes ANSI/ECMA-48 escape sequences AND bare control characters,
/// preserving only newlines and tabs (carriage returns are normalized to
/// newlines so `\r`-overwrite spoofing can't hide content). Use this when
/// re-rendering conversation history or other persisted text in a terminal
/// UI: a message that arrived with embedded escapes must not be able to
/// clear the screen, retitle the window, move the cursor, or restyle
/// adjacent UI when replayed.
pub fn sanitize_display_text(text: &str) -> Cow<'_, str> {
    if text.is_empty() || !has_control_re().is_match(text) {
        return Cow::Borrowed(text);
    }
    let stripped = strip_ansi(text);
    let normalized = if stripped.contains('\r') {
        stripped.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        stripped.into_owned()
    };
    Cow::Owned(control_chars_re().replace_all(&normalized, "").into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_passes_through() {
        assert_eq!(strip_ansi("hello world"), "hello world");
        assert!(matches!(strip_ansi("plain"), Cow::Borrowed(_)));
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strips_csi_color_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
        assert_eq!(strip_ansi("\x1b[1;32mbold green\x1b[0m"), "bold green");
        // Private-mode + colon params + intermediate bytes.
        assert_eq!(strip_ansi("\x1b[?25lhidden\x1b[?25h"), "hidden");
        assert_eq!(strip_ansi("\x1b[38:2::255:0:0mrgb\x1b[0m"), "rgb");
        // \x1b[?1049$h is one complete CSI sequence (intermediate $, final h).
        assert_eq!(strip_ansi("\x1b[?1049$h\x1b[2J"), "");
    }

    #[test]
    fn strips_osc_sequences() {
        // BEL-terminated window title.
        assert_eq!(strip_ansi("\x1b]0;title\x07body"), "body");
        // ST-terminated.
        assert_eq!(
            strip_ansi("\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
    }

    #[test]
    fn strips_dcs_and_single_byte() {
        assert_eq!(strip_ansi("\x1bPq#0\x1b\\rest"), "rest");
        assert_eq!(strip_ansi("\x1b7save\x1b8"), "save");
        // nF sequence
        assert_eq!(strip_ansi("\x1b(Bascii"), "ascii");
    }

    #[test]
    fn strips_8bit_c1() {
        assert_eq!(strip_ansi("\u{9b}31mred\u{9b}0m"), "red");
        assert_eq!(strip_ansi("a\u{85}b"), "ab");
    }

    #[test]
    fn real_world_mixed_output() {
        let raw =
            "\x1b[?2004h\x1b]0;user@host: ~\x07$ ls\r\n\x1b[0m\x1b[01;34mdir\x1b[0m\r\n$ \x1b[K";
        let clean = strip_ansi(raw);
        assert!(!clean.contains('\x1b'), "got: {clean:?}");
        assert!(clean.contains("dir"));
    }

    #[test]
    fn sanitize_keeps_tab_newline_drops_rest() {
        let raw = "line1\r\nline2\ttabbed\x07bell\x1b[31mred\x1b[0m\x00nul";
        let out = sanitize_display_text(raw);
        // Control bytes drop, their surrounding text kept.
        assert_eq!(out, "line1\nline2\ttabbedbellrednul");
    }

    #[test]
    fn sanitize_cr_overwrite_spoofing() {
        // \r-overwrite spoofing gets normalized, not hidden.
        let raw = "harmless\rINJECTED";
        let out = sanitize_display_text(raw);
        assert_eq!(out, "harmless\nINJECTED");
    }

    #[test]
    fn sanitize_clean_text_is_borrowed() {
        assert!(matches!(sanitize_display_text("ok text"), Cow::Borrowed(_)));
    }
}

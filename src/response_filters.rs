//! Gateway response filtering helpers — port of hermes
//! `gateway/response_filters.py`.
//!
//! These helpers operate at the gateway boundary: they decide whether a
//! completed agent turn should be delivered to the chat, not what
//! should be persisted in the conversation history.

/// Canonical model-emitted control token for intentional silence.
pub const SILENT_REPLY_TOKEN: &str = "NO_REPLY";

/// Exact whole-response markers that mean "the agent intentionally
/// chose not to reply". Keep this list small and explicit; arbitrary
/// empty output remains an error/empty-response path, not silence.
pub const LIVE_GATEWAY_SILENT_MARKERS: &[&str] = &["[SILENT]", "SILENT", "NO_REPLY", "NO REPLY"];

/// Silence detection only applies to short control outputs (hermes
/// 64-char cap).
const MAX_SILENCE_CHARS: usize = 64;

/// Whitespace-collapse + upper-case a silence candidate (hermes
/// `_canonical_silence_candidate`).
pub fn canonical_silence_candidate(text: &str) -> String {
    text.trim()
        .split_whitespace()
        .map(|word| word.to_uppercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Unicode general-category P* test (punctuation), covering ASCII plus
/// the common punctuation blocks — hermes `unicodedata.category(c)
/// .startswith("P")` parity for model-output shapes.
fn is_punctuation_category(c: char) -> bool {
    if c.is_ascii_punctuation() {
        return true;
    }
    let cp = c as u32;
    matches!(cp,
        0x00A1 | 0x00A7 | 0x00B6 | 0x00B7 | 0x00BF
        | 0x037E | 0x0387
        | 0x055A..=0x055F | 0x0589 | 0x058A | 0x05BE | 0x05C0 | 0x05C3 | 0x05C6 | 0x05F3 | 0x05F4
        | 0x0609 | 0x060A | 0x060C | 0x060D | 0x061B | 0x061E | 0x061F | 0x066A..=0x066D | 0x06D4
        | 0x0700..=0x070D | 0x07F7..=0x07F9 | 0x0830..=0x083E | 0x085E
        | 0x0964 | 0x0965 | 0x0970
        | 0x0DF4
        | 0x0E5A | 0x0E5B
        | 0x0F04..=0x0F12 | 0x0F14 | 0x0F85
        | 0x104A..=0x104F
        | 0x1360..=0x1368
        | 0x166D | 0x166E | 0x169B | 0x169C | 0x16EB..=0x16ED
        | 0x1735 | 0x1736 | 0x17D4..=0x17D6 | 0x17D8..=0x17DA
        | 0x1800..=0x180A
        | 0x1944 | 0x1945
        | 0x2010..=0x2027 | 0x2030..=0x2043 | 0x2045..=0x2051 | 0x2053..=0x205E
        | 0x2E00..=0x2E2E | 0x2E30..=0x2E5D
        | 0x3001..=0x3003 | 0x3008..=0x3011 | 0x3014..=0x301F | 0x3030 | 0x303D | 0x30A0 | 0x30FB
        | 0xA490..=0xA4C6
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE52 | 0xFE54..=0xFE61 | 0xFE63 | 0xFE68 | 0xFE6A | 0xFE6B
        | 0xFF01..=0xFF03 | 0xFF05..=0xFF0A | 0xFF0C..=0xFF0F | 0xFF1A | 0xFF1B
        | 0xFF1F | 0xFF20 | 0xFF3B..=0xFF3D | 0xFF3F | 0xFF5B | 0xFF5D | 0xFF5F..=0xFF65
    )
}

/// Strip stray edge punctuation without erasing marker structure
/// (hermes `_strip_edge_silence_punctuation`).
///
/// Models sometimes emit `.NO_REPLY` or `*NO_REPLY*` instead of the
/// exact marker. Keep square brackets structural so malformed
/// `[SILENT` does not become `SILENT`.
pub fn strip_edge_silence_punctuation(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0usize;
    let mut end = chars.len();
    while start < end
        && chars[start] != '['
        && chars[start] != ']'
        && is_punctuation_category(chars[start])
    {
        start += 1;
    }
    while end > start
        && chars[end - 1] != '['
        && chars[end - 1] != ']'
        && is_punctuation_category(chars[end - 1])
    {
        end -= 1;
    }
    chars[start..end]
        .iter()
        .collect::<String>()
        .trim()
        .to_string()
}

/// Canonical candidates for a stripped response: the exact form plus
/// the edge-punctuation-stripped form when it differs (hermes
/// `_canonical_silence_candidates`).
fn canonical_silence_candidates(text: &str) -> Vec<String> {
    let exact = canonical_silence_candidate(text);
    let stripped = strip_edge_silence_punctuation(text.trim());
    if stripped == text.trim() {
        vec![exact]
    } else {
        vec![exact, canonical_silence_candidate(&stripped)]
    }
}

/// Return true only when `response` is exactly a silence marker
/// (hermes `is_intentional_silence_response`).
///
/// Substantive prose that merely mentions `NO_REPLY` or `[SILENT]`
/// must be delivered normally. A blank response is also not silence;
/// blank output is handled by the empty-response failure path.
pub fn is_intentional_silence_response(response: &str) -> bool {
    let stripped = response.trim();
    if stripped.is_empty() || stripped.chars().count() > MAX_SILENCE_CHARS {
        return false;
    }
    canonical_silence_candidates(stripped)
        .iter()
        .any(|candidate| LIVE_GATEWAY_SILENT_MARKERS.contains(&candidate.as_str()))
}

/// Loose silence matcher for autonomous lanes (cron, webhook) — hermes
/// `is_autonomous_silence_response`.
///
/// Autonomous lanes instruct the agent to emit `[SILENT]` when a tick
/// produced nothing worth a human's attention, and models reliably
/// bracket the marker with a short note explaining why they stayed
/// quiet. Unlike [`is_intentional_silence_response`] (the
/// interactive-chat rule, which demands the response be EXACTLY a
/// marker), this suppresses when a marker is the whole response, sits
/// on its own first or last line, or the bracketed sentinel opens the
/// response (the documented `[SILENT] No changes detected` pattern).
/// A token buried mid-sentence in a genuine report is still delivered.
///
/// Shares [`LIVE_GATEWAY_SILENT_MARKERS`] so the interactive and
/// autonomous marker sets can never drift apart.
pub fn is_autonomous_silence_response(response: &str) -> bool {
    let stripped = response.trim();
    if stripped.is_empty() {
        return false;
    }
    let is_token = |line: &str| {
        LIVE_GATEWAY_SILENT_MARKERS.contains(&canonical_silence_candidate(line).as_str())
    };
    // Whole response is exactly a token.
    if is_token(stripped) {
        return true;
    }
    // Marker on its own first or last line (leading/trailing note on a
    // separate line — e.g. "2 deals filtered\n\n[SILENT]").
    let lines: Vec<&str> = stripped
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if let (Some(first), Some(last)) = (lines.first(), lines.last()) {
        if is_token(first) || is_token(last) {
            return true;
        }
    }
    // Bracketed sentinel used as a same-line prefix — the documented
    // pattern "[SILENT] No changes detected". Restricted to the
    // bracketed form so a bare word like "Silent retry succeeded" is
    // NOT swallowed.
    stripped.to_uppercase().starts_with("[SILENT]")
}

/// Silence markers suppress delivery only for successful agent turns
/// (hermes `is_intentional_silence_agent_result`).
pub fn is_intentional_silence_agent_result(failed: bool, response: &str) -> bool {
    !failed && is_intentional_silence_response(response)
}

/// Return true while `text` could still resolve to a silence marker
/// (hermes `is_partial_silence_marker`).
///
/// The streaming path accumulates the reply delta-by-delta and must
/// decide, before the whole response is known, whether to show what it
/// has so far. A buffer whose canonical form is a non-empty *prefix*
/// of a silence marker (e.g. `"NO"` on the way to `"NO_REPLY"`, or an
/// exact marker that has not yet been terminated by stream-end) is
/// held back so a raw marker is never edited onto the screen and then
/// belatedly retracted.
///
/// Anything that has already diverged from every marker (ordinary
/// prose) — and anything longer than the marker cap — returns false so
/// normal streaming resumes immediately. This is the streaming
/// counterpart to [`is_intentional_silence_response`], sharing the
/// same marker set and canonicalization so the two never drift.
pub fn is_partial_silence_marker(text: &str) -> bool {
    let stripped = text.trim();
    if stripped.is_empty() || stripped.chars().count() > MAX_SILENCE_CHARS {
        return false;
    }
    canonical_silence_candidates(stripped)
        .iter()
        .any(|candidate| {
            !candidate.is_empty()
                && LIVE_GATEWAY_SILENT_MARKERS
                    .iter()
                    .any(|marker| marker.starts_with(candidate.as_str()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_markers_are_silence() {
        for marker in LIVE_GATEWAY_SILENT_MARKERS {
            assert!(is_intentional_silence_response(marker), "{marker}");
            assert!(
                is_intentional_silence_response(&format!("  {marker}  ")),
                "{marker}"
            );
            assert!(
                is_intentional_silence_response(&marker.to_lowercase()),
                "{marker}"
            );
        }
        // Whitespace-collapsed interior.
        assert!(is_intentional_silence_response("NO   REPLY"));
    }

    #[test]
    fn prose_mentioning_markers_is_delivered() {
        assert!(!is_intentional_silence_response(""));
        assert!(!is_intentional_silence_response("   "));
        assert!(!is_intentional_silence_response(
            "If you want me to stop, say NO_REPLY and I will."
        ));
        assert!(!is_intentional_silence_response(
            "The job returned [SILENT] which means nothing changed."
        ));
        // Over the 64-char cap → never silence.
        let long = format!("NO_REPLY {}", "x".repeat(64));
        assert!(!is_intentional_silence_response(&long));
    }

    #[test]
    fn edge_punctuation_stripped_around_markers() {
        assert!(is_intentional_silence_response(".NO_REPLY"));
        assert!(is_intentional_silence_response("*NO_REPLY*"));
        assert!(is_intentional_silence_response("…NO_REPLY…"));
        assert!(is_intentional_silence_response("。NO_REPLY。"));
        // Brackets stay structural: malformed `[SILENT` is NOT silence.
        assert!(!is_intentional_silence_response("[SILENT"));
        assert!(!is_intentional_silence_response("SILENT]"));
        // Fullwidth PUNCTUATION edges are stripped, but fullwidth
        // letters are not folded to ASCII (hermes parity — Python's
        // upper() doesn't fold them either).
        assert!(is_intentional_silence_response("，NO_REPLY，"));
        assert!(!is_intentional_silence_response("，ＳＩＬＥＮＴ，"));
    }

    #[test]
    fn failed_turns_never_silenced() {
        assert!(!is_intentional_silence_agent_result(true, "NO_REPLY"));
        assert!(is_intentional_silence_agent_result(false, "NO_REPLY"));
    }

    #[test]
    fn autonomous_matcher_suppresses_documented_patterns() {
        assert!(is_autonomous_silence_response("[SILENT]"));
        assert!(is_autonomous_silence_response("NO_REPLY"));
        assert!(is_autonomous_silence_response("  [SILENT]  "));
        assert!(is_autonomous_silence_response(
            "[SILENT] No changes detected"
        ));
        assert!(is_autonomous_silence_response(
            "2 deals filtered\n\n[SILENT]"
        ));
        assert!(is_autonomous_silence_response("[SILENT]\nnote follows"));
        // Mid-sentence token in a genuine report is delivered.
        assert!(!is_autonomous_silence_response(
            "I considered staying [SILENT] but here is the summary"
        ));
        // Bare word (unbracketed) prefix is NOT swallowed.
        assert!(!is_autonomous_silence_response(
            "Silent retry succeeded — details below"
        ));
        assert!(!is_autonomous_silence_response(""));
    }

    #[test]
    fn partial_marker_prefixes_hold_back() {
        assert!(is_partial_silence_marker("N"));
        assert!(is_partial_silence_marker("NO"));
        assert!(is_partial_silence_marker("NO_"));
        assert!(is_partial_silence_marker("NO_REPLY"));
        assert!(is_partial_silence_marker("[silent"));
        assert!(is_partial_silence_marker("  no reply "));
        // Diverged prose resumes streaming immediately.
        assert!(!is_partial_silence_marker("NOBODY"));
        assert!(!is_partial_silence_marker("Nothing to report"));
        assert!(!is_partial_silence_marker(""));
        let long = format!("NO {}", "x".repeat(70));
        assert!(!is_partial_silence_marker(&long));
    }

    #[test]
    fn edge_stripper_preserves_brackets() {
        assert_eq!(strip_edge_silence_punctuation(".NO_REPLY."), "NO_REPLY");
        assert_eq!(strip_edge_silence_punctuation("*NO_REPLY*"), "NO_REPLY");
        assert_eq!(strip_edge_silence_punctuation("[SILENT"), "[SILENT");
        assert_eq!(strip_edge_silence_punctuation("SILENT]"), "SILENT]");
        assert_eq!(strip_edge_silence_punctuation("hello"), "hello");
    }
}

//! Stateful scrubber for reasoning/thinking blocks in streamed assistant
//! text — port of hermes `agent/think_scrubber.py` (v2026.8.3).
//!
//! A per-delta regex strip destroys the state downstream consumers rely on:
//! when a model streams `<think>` / prose / `</think>` across three deltas,
//! the open tag must be remembered so the reasoning prose between the tags
//! is suppressed. This module centralises the tag-suppression state machine
//! at the upstream layer so every stream-delta consumer sees text that has
//! already had reasoning blocks removed. Partial tags at delta boundaries
//! are held back until the next delta resolves them, and end-of-stream
//! flushing surfaces any held-back prose that turned out not to be a real
//! tag.
//!
//! Tag variants handled (case-insensitive): `<think>`, `<thinking>`,
//! `<reasoning>`, `<thought>`, `<REASONING_SCRATCHPAD>`.
//!
//! Block-boundary rule for opens: an opening tag is only treated as a
//! reasoning-block opener when it appears at the start of the stream,
//! after a newline, or when only whitespace has been emitted on the
//! current line — so prose that *mentions* the tag name isn't
//! over-stripped. Closed pairs are always suppressed regardless of
//! boundary: a closed pair is an intentional, bounded construct.

const OPEN_TAG_NAMES: &[&str] = &[
    "think",
    "thinking",
    "reasoning",
    "thought",
    "REASONING_SCRATCHPAD",
];

fn open_tags() -> Vec<String> {
    OPEN_TAG_NAMES.iter().map(|n| format!("<{n}>")).collect()
}

fn close_tags() -> Vec<String> {
    OPEN_TAG_NAMES.iter().map(|n| format!("</{n}>")).collect()
}

fn max_tag_len() -> usize {
    open_tags()
        .iter()
        .chain(close_tags().iter())
        .map(|t| t.len())
        .max()
        .unwrap_or(0)
}

/// Case-insensitive (ASCII) substring search returning the byte index.
/// Needles are pure-ASCII tag strings; matching ignores ASCII case only,
/// which is exactly what tag detection needs and keeps indices valid for
/// the original buffer.
fn find_ci(haystack: &str, needle: &str, start: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < start + needle.len() {
        return None;
    }
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    let mut i = start;
    while i + nb.len() <= hb.len() {
        let mut matched = true;
        for (j, &b) in nb.iter().enumerate() {
            let a = hb[i + j];
            if a.to_ascii_lowercase() != b.to_ascii_lowercase() {
                matched = false;
                break;
            }
        }
        if matched {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Earliest (index, length) of any tag in `tags`, case-insensitive.
fn find_first_tag(buf: &str, tags: &[String]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for tag in tags {
        if let Some(idx) = find_ci(buf, tag, 0) {
            if best.map_or(true, |(bi, _)| idx < bi) {
                best = Some((idx, tag.len()));
            }
        }
    }
    best
}

/// Earliest closed `<tag>...</tag>` pair (non-greedy, case-insensitive).
/// When two tag variants could both match, the one whose open tag appears
/// earlier wins (hermes `_find_earliest_closed_pair`).
fn find_earliest_closed_pair(buf: &str) -> Option<(usize, usize)> {
    let opens = open_tags();
    let closes = close_tags();
    let mut best: Option<(usize, usize)> = None;
    for (open_tag, close_tag) in opens.iter().zip(closes.iter()) {
        let Some(open_idx) = find_ci(buf, open_tag, 0) else {
            continue;
        };
        let Some(close_idx) = find_ci(buf, close_tag, open_idx + open_tag.len()) else {
            continue;
        };
        let end_idx = close_idx + close_tag.len();
        if best.map_or(true, |(bs, _)| open_idx < bs) {
            best = Some((open_idx, end_idx));
        }
    }
    best
}

/// Longest suffix of `buf` that is a strict prefix of any tag
/// (case-insensitive) — the hold-back bound for split tags.
fn max_partial_suffix(buf: &str, tags: &[String]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let max_check = buf.len().min(max_tag_len() - 1);
    for i in (1..=max_check).rev() {
        let suffix_start = buf.len() - i;
        if !buf.is_char_boundary(suffix_start) {
            continue;
        }
        let suffix = &buf[suffix_start..];
        for tag in tags {
            if tag.len() > i && find_ci(tag, suffix, 0) == Some(0) {
                return i;
            }
        }
    }
    0
}

/// Remove orphan close tags (no matching open in the current state);
/// they're always noise. Stripped with any trailing whitespace so the
/// surrounding prose flows naturally (hermes `_strip_orphan_close_tags`).
fn strip_orphan_close_tags(text: &str) -> String {
    if !text.contains("</") {
        return text.to_string();
    }
    let closes = close_tags();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        let mut matched = false;
        if i + 1 < bytes.len() && bytes[i] == b'<' && bytes[i + 1] == b'/' {
            for tag in &closes {
                if find_ci(&text[i..], tag, 0) == Some(0) {
                    let mut j = i + tag.len();
                    while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                        j += 1;
                    }
                    i = j;
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            // Advance one UTF-8 char.
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte < 0x80 {
        1
    } else if first_byte >> 5 == 0b110 {
        2
    } else if first_byte >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// True iff position `idx` in `buf` is a block boundary (hermes
/// `_is_block_boundary`): start-of-buffer with the last emission ending in
/// a newline (or nothing emitted yet), or the text since the last newline
/// being whitespace-only (with the cross-feed newline rule when there is
/// no newline in the preceding portion).
fn is_block_boundary(
    buf: &str,
    idx: usize,
    already_emitted: &[String],
    last_nl_flag: bool,
) -> bool {
    if idx == 0 {
        if let Some(last) = already_emitted.last() {
            return last.ends_with('\n');
        }
        return last_nl_flag;
    }
    let preceding = &buf[..idx];
    match preceding.rfind('\n') {
        None => {
            let prior_newline = already_emitted
                .last()
                .map(|l| l.ends_with('\n'))
                .unwrap_or(last_nl_flag);
            prior_newline && preceding.trim().is_empty()
        }
        Some(last_nl) => preceding[last_nl + 1..].trim().is_empty(),
    }
}

/// Earliest block-boundary open tag (hermes `_find_open_at_boundary`).
fn find_open_at_boundary(
    buf: &str,
    already_emitted: &[String],
    last_nl_flag: bool,
) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for tag in open_tags() {
        let mut search_start = 0;
        while let Some(idx) = find_ci(buf, &tag, search_start) {
            if is_block_boundary(buf, idx, already_emitted, last_nl_flag) {
                if best.map_or(true, |(bi, _)| idx < bi) {
                    best = Some((idx, tag.len()));
                }
                break; // first boundary hit for this tag is enough
            }
            search_start = idx + 1;
        }
    }
    best
}

/// Stateful streaming scrubber for reasoning/thinking blocks.
#[derive(Debug, Default)]
pub struct StreamingThinkScrubber {
    /// True while inside an opened block, waiting for a close tag; all
    /// text inside is discarded.
    in_block: bool,
    /// Held-back partial-tag tail; resolved by the next feed() or flush().
    buf: String,
    /// True iff the most recent emission ended with `\n`, or nothing has
    /// been emitted yet (start-of-stream counts as a boundary).
    last_emitted_ended_newline: bool,
}

impl StreamingThinkScrubber {
    pub fn new() -> Self {
        Self {
            in_block: false,
            buf: String::new(),
            last_emitted_ended_newline: true,
        }
    }

    /// Reset all state. Call at the top of every new turn so a hung block
    /// from an interrupted prior stream cannot taint the next turn.
    pub fn reset(&mut self) {
        self.in_block = false;
        self.buf.clear();
        self.last_emitted_ended_newline = true;
    }

    /// Feed one delta; return the scrubbed visible portion. May return an
    /// empty string when the whole delta is reasoning content or is held
    /// back pending resolution of a partial tag at the boundary.
    pub fn feed(&mut self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut buf = std::mem::take(&mut self.buf);
        buf.push_str(text);
        let mut out: Vec<String> = Vec::new();
        let closes = close_tags();

        while !buf.is_empty() {
            if self.in_block {
                // Hunt for the earliest close tag.
                match find_first_tag(&buf, &closes) {
                    Some((close_idx, close_len)) => {
                        // Found close: discard block content + tag, continue.
                        buf = buf[close_idx + close_len..].to_string();
                        self.in_block = false;
                    }
                    None => {
                        // No close yet — hold back a potential partial
                        // close-tag prefix; discard everything else.
                        let held = max_partial_suffix(&buf, &closes);
                        self.buf = if held > 0 {
                            buf[buf.len() - held..].to_string()
                        } else {
                            String::new()
                        };
                        return out.join("");
                    }
                }
            } else {
                // Priority 1 — closed pair anywhere (always intentional).
                let pair = find_earliest_closed_pair(&buf);
                // Priority 2 — unterminated open tag at a block boundary.
                let open_match = find_open_at_boundary(&buf, &out, self.last_emitted_ended_newline);

                let pair_first =
                    pair.is_some() && open_match.map_or(true, |(oi, _)| pair.unwrap().0 <= oi);
                if pair_first {
                    let (start_idx, end_idx) = pair.unwrap();
                    let preceding = &buf[..start_idx];
                    if !preceding.is_empty() {
                        let stripped = strip_orphan_close_tags(preceding);
                        if !stripped.is_empty() {
                            self.last_emitted_ended_newline = stripped.ends_with('\n');
                            out.push(stripped);
                        }
                    }
                    buf = buf[end_idx..].to_string();
                    continue;
                }

                if let Some((open_idx, open_len)) = open_match {
                    // Unterminated open at boundary — emit preceding,
                    // enter block, continue with the remainder.
                    let preceding = &buf[..open_idx];
                    if !preceding.is_empty() {
                        let stripped = strip_orphan_close_tags(preceding);
                        if !stripped.is_empty() {
                            self.last_emitted_ended_newline = stripped.ends_with('\n');
                            out.push(stripped);
                        }
                    }
                    self.in_block = true;
                    buf = buf[open_idx + open_len..].to_string();
                    continue;
                }

                // No resolvable tag structure: hold back any partial-tag
                // tail so a split tag across deltas isn't missed, emit rest.
                let held_open = max_partial_suffix(&buf, &open_tags());
                let held_close = max_partial_suffix(&buf, &closes);
                let held = held_open.max(held_close);
                let emit_text = if held > 0 {
                    self.buf = buf[buf.len() - held..].to_string();
                    buf[..buf.len() - held].to_string()
                } else {
                    self.buf.clear();
                    buf.clone()
                };
                if !emit_text.is_empty() {
                    let stripped = strip_orphan_close_tags(&emit_text);
                    if !stripped.is_empty() {
                        self.last_emitted_ended_newline = stripped.ends_with('\n');
                        out.push(stripped);
                    }
                }
                return out.join("");
            }
        }

        out.join("")
    }

    /// End-of-stream flush. Inside an unterminated block, held-back
    /// content is discarded (leaking partial reasoning is worse than a
    /// truncated answer). Otherwise the held-back partial-tag tail is
    /// emitted verbatim. The next feed() is always treated as a fresh
    /// stream boundary (intra-turn retries stream again without reset).
    pub fn flush(&mut self) -> String {
        if self.in_block {
            self.buf.clear();
            self.in_block = false;
            self.last_emitted_ended_newline = true;
            return String::new();
        }
        let tail = std::mem::take(&mut self.buf);
        self.last_emitted_ended_newline = true;
        if tail.is_empty() {
            return String::new();
        }
        strip_orphan_close_tags(&tail)
    }
}

/// Strip reasoning blocks from a complete string (hermes
/// `_strip_think_blocks`): equivalent to one feed() + flush().
pub fn strip_think_blocks(text: &str) -> String {
    let mut scrubber = StreamingThinkScrubber::new();
    let mut out = scrubber.feed(text);
    out.push_str(&scrubber.flush());
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(scrubber: &mut StreamingThinkScrubber, deltas: &[&str]) -> String {
        let mut out = String::new();
        for delta in deltas {
            out.push_str(&scrubber.feed(delta));
        }
        out.push_str(&scrubber.flush());
        out
    }

    #[test]
    fn closed_pair_stripped_single_delta() {
        let mut s = StreamingThinkScrubber::new();
        assert_eq!(
            collect(&mut s, &["<think>let me reason</think>The answer is 42."]),
            "The answer is 42."
        );
    }

    #[test]
    fn cross_delta_block_suppressed() {
        // The MiniMax case from the hermes docstring: per-delta regex
        // erases the open tag and leaks the reasoning prose.
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(
            &mut s,
            &["<think>", "Let me check their config", "</think>", "Done."],
        );
        assert_eq!(visible, "Done.");
    }

    #[test]
    fn boundary_gating_protects_prose_mentions() {
        let mut s = StreamingThinkScrubber::new();
        // Mid-line mention of the tag name is NOT a block opener.
        let visible = collect(&mut s, &["You can use <think> tags here."]);
        assert_eq!(visible, "You can use <think> tags here.");
    }

    #[test]
    fn closed_pair_midline_stripped_anyway() {
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(&mut s, &["before <think>hidden</think> after"]);
        assert_eq!(visible, "before  after");
    }

    #[test]
    fn split_tags_across_deltas() {
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(&mut s, &["<thi", "nk>secret</thi", "nk>visible"]);
        assert_eq!(visible, "visible");
    }

    #[test]
    fn open_after_newline_is_boundary() {
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(&mut s, &["Intro text\n<think>reasoning</think>Answer"]);
        assert_eq!(visible, "Intro text\nAnswer");
    }

    #[test]
    fn whitespace_before_open_tag_is_boundary() {
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(&mut s, &["  <think>reasoning</think>Answer"]);
        // Closed-pair suppression emits the preceding whitespace
        // verbatim (pair match wins; preceding passes through), so the
        // two leading spaces survive — matches hermes feed().
        assert_eq!(visible, "  Answer");
    }

    #[test]
    fn unterminated_block_discarded_on_flush() {
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(
            &mut s,
            &["Visible.\n<think>partial reasoning that never closes"],
        );
        assert_eq!(visible, "Visible.\n");
    }

    #[test]
    fn held_partial_tail_flushed_verbatim() {
        let mut s = StreamingThinkScrubber::new();
        // Trailing '<' is held back, then flushed when it's not a tag.
        let visible = collect(&mut s, &["price < 100 & x > 5 <", "br> not a tag"]);
        assert_eq!(visible, "price < 100 & x > 5 <br> not a tag");
    }

    #[test]
    fn orphan_close_tags_stripped() {
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(&mut s, &["hello </think> world"]);
        assert_eq!(visible, "hello world");
    }

    #[test]
    fn case_insensitive_tags() {
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(&mut s, &["<THINK>loud reasoning</THINK>Answer"]);
        assert_eq!(visible, "Answer");
        let mut s2 = StreamingThinkScrubber::new();
        let visible = collect(&mut s2, &["<Reasoning>r</Reasoning>ok"]);
        assert_eq!(visible, "ok");
    }

    #[test]
    fn all_tag_variants() {
        for name in [
            "think",
            "thinking",
            "reasoning",
            "thought",
            "REASONING_SCRATCHPAD",
        ] {
            let mut s = StreamingThinkScrubber::new();
            let delta = format!("<{name}>hidden</{name}>shown");
            assert_eq!(collect(&mut s, &[&delta]), "shown", "variant {name}");
        }
    }

    #[test]
    fn reset_clears_hung_block() {
        let mut s = StreamingThinkScrubber::new();
        assert_eq!(collect(&mut s, &["<think>never closed"]), "");
        // collect() flushed already; simulate a hung block + explicit reset
        let mut s = StreamingThinkScrubber::new();
        let _ = s.feed("<think>hung");
        s.reset();
        assert_eq!(collect(&mut s, &["fresh turn"]), "fresh turn");
    }

    #[test]
    fn unicode_passthrough() {
        let mut s = StreamingThinkScrubber::new();
        let visible = collect(&mut s, &["你好 <think>思考中…</think>世界 🌍"]);
        assert_eq!(visible, "你好 世界 🌍");
    }

    #[test]
    fn strip_think_blocks_complete_string() {
        assert_eq!(strip_think_blocks("<think>abc</think>result"), "result");
        assert_eq!(strip_think_blocks("no tags here"), "no tags here");
        assert_eq!(strip_think_blocks("<thinking>x</thinking>"), "");
    }

    #[test]
    fn flush_resets_boundary_flag() {
        // After a flush, the next feed() is a fresh stream boundary: an
        // opening <think> must be treated as a block opener even though
        // the previous stream ended mid-line.
        let mut s = StreamingThinkScrubber::new();
        assert_eq!(s.feed("partial answer"), "partial answer");
        assert_eq!(s.flush(), "");
        assert_eq!(
            s.feed("<think>retry reasoning</think>new answer"),
            "new answer"
        );
    }
}

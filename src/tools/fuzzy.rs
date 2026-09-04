//! Fuzzy find-and-replace chain — full port of hermes' `tools/fuzzy_match.py`.
//!
//! Nine strategies, tried in order:
//!   1. exact                    — literal substring
//!   2. line_trimmed             — each line trimmed on both ends
//!   3. whitespace_normalized    — runs of spaces/tabs collapsed to one space
//!   4. indentation_flexible     — leading whitespace stripped per line
//!   5. escape_normalized        — `\n`/`\t`/`\r` escape sequences expanded
//!   6. trimmed_boundary         — only first/last pattern lines trimmed
//!   7. unicode_normalized       — smart quotes/dashes/ellipsis/space family
//!                                 mapped to ASCII (Unicode-preserving replace)
//!   8. block_anchor             — first/last line anchors + similarity middle
//!   9. context_aware            — anchored all-lines similarity (last resort)
//!
//! Similarity is an LCS-based ratio (`2*lcs/(a+b)`), the standard stand-in
//! for Python's `difflib.SequenceMatcher.ratio`.

/// Strategies whose matches are approximate; `replace_all` refuses to apply
/// them to more than one location.
pub const SIMILARITY_STRATEGIES: &[&str] = &["block_anchor", "context_aware"];

const ANCHOR_THRESHOLD: f64 = 0.80;

/// Byte-range matches plus the strategy that produced them.
#[derive(Debug, Clone)]
pub struct FuzzyHit {
    pub matches: Vec<(usize, usize)>,
    pub strategy: &'static str,
}

/// Run the strategy chain; empty `matches` when nothing worked.
pub fn fuzzy_find(content: &str, old: &str) -> FuzzyHit {
    let strategies: [(&'static str, fn(&str, &str) -> Vec<(usize, usize)>); 9] = [
        ("exact", strategy_exact),
        ("line_trimmed", strategy_line_trimmed),
        ("whitespace_normalized", strategy_whitespace_normalized),
        ("indentation_flexible", strategy_indentation_flexible),
        ("escape_normalized", strategy_escape_normalized),
        ("trimmed_boundary", strategy_trimmed_boundary),
        ("unicode_normalized", strategy_unicode_normalized),
        ("block_anchor", strategy_block_anchor),
        ("context_aware", strategy_context_aware),
    ];
    for (name, strategy) in strategies {
        let matches = strategy(content, old);
        if !matches.is_empty() {
            return FuzzyHit {
                matches,
                strategy: name,
            };
        }
    }
    FuzzyHit {
        matches: Vec::new(),
        strategy: "",
    }
}

// ---------------------------------------------------------------------------
// Strategy 1: exact
// ---------------------------------------------------------------------------

fn strategy_exact(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    if pattern.is_empty() {
        return matches;
    }
    let mut start = 0usize;
    while let Some(pos) = content[start..].find(pattern) {
        let abs = start + pos;
        matches.push((abs, abs + pattern.len()));
        // Advance past the whole match (str.replace semantics).
        start = abs + pattern.len();
    }
    matches
}

// ---------------------------------------------------------------------------
// Line-window helpers
// ---------------------------------------------------------------------------

/// Byte offsets of each line start, plus total length.
fn line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            offsets.push(idx + 1);
        }
    }
    offsets
}

/// hermes `_calculate_line_positions`: (start, end) byte positions for a
/// window of lines [start_line, end_line).
fn calculate_line_positions(
    line_starts: &[usize],
    start_line: usize,
    end_line: usize,
    content_len: usize,
) -> (usize, usize) {
    let start_pos = line_starts[start_line.min(line_starts.len() - 1)];
    let mut end_pos = if end_line < line_starts.len() {
        line_starts[end_line]
    } else {
        content_len
    };
    // Drop the trailing newline of the last line in the window.
    if end_pos > start_pos {
        end_pos -= 1;
    }
    (start_pos, end_pos.min(content_len))
}

fn find_line_windows(
    content: &str,
    content_window_lines: &[String],
    pattern_normalized: &str,
) -> Vec<(usize, usize)> {
    let pattern_line_count = pattern_normalized.split('\n').count();
    let mut matches = Vec::new();
    if content_window_lines.len() < pattern_line_count || pattern_line_count == 0 {
        return matches;
    }
    let line_starts = line_offsets(content);
    for i in 0..=(content_window_lines.len() - pattern_line_count) {
        let block = content_window_lines[i..i + pattern_line_count].join("\n");
        if block == pattern_normalized {
            matches.push(calculate_line_positions(
                &line_starts,
                i,
                i + pattern_line_count,
                content.len(),
            ));
        }
    }
    matches
}

// ---------------------------------------------------------------------------
// Strategy 2: line-trimmed
// ---------------------------------------------------------------------------

fn strategy_line_trimmed(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let pattern_normalized = pattern
        .split('\n')
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join("\n");
    let content_window_lines: Vec<String> = content
        .split('\n')
        .map(|line| line.trim().to_string())
        .collect();
    find_line_windows(content, &content_window_lines, &pattern_normalized)
}

// ---------------------------------------------------------------------------
// Strategy 3: whitespace-normalized
// ---------------------------------------------------------------------------

fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch == ' ' || ch == '\t' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn strategy_whitespace_normalized(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let norm_pattern = collapse_spaces(pattern);
    let norm_content = collapse_spaces(content);
    let norm_matches = strategy_exact(&norm_content, &norm_pattern);
    if norm_matches.is_empty() {
        return Vec::new();
    }
    map_normalized_positions(content, &norm_content, &norm_matches)
}

/// hermes `_map_normalized_positions`: walk original/normalized char streams
/// together to map normalized ranges back to original byte ranges.
fn map_normalized_positions(
    original: &str,
    normalized: &str,
    normalized_matches: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let orig_chars: Vec<char> = original.chars().collect();
    let norm_chars: Vec<char> = normalized.chars().collect();

    // orig_to_norm[i] = normalized char index for original char i.
    let mut orig_to_norm: Vec<usize> = Vec::with_capacity(orig_chars.len());
    let mut norm_idx = 0usize;
    let mut orig_idx = 0usize;
    while orig_idx < orig_chars.len() && norm_idx < norm_chars.len() {
        if orig_chars[orig_idx] == norm_chars[norm_idx] {
            orig_to_norm.push(norm_idx);
            orig_idx += 1;
            norm_idx += 1;
        } else if (orig_chars[orig_idx] == ' ' || orig_chars[orig_idx] == '\t')
            && norm_chars[norm_idx] == ' '
        {
            orig_to_norm.push(norm_idx);
            orig_idx += 1;
            if orig_idx < orig_chars.len()
                && orig_chars[orig_idx] != ' '
                && orig_chars[orig_idx] != '\t'
            {
                norm_idx += 1;
            }
        } else if orig_chars[orig_idx] == ' ' || orig_chars[orig_idx] == '\t' {
            orig_to_norm.push(norm_idx);
            orig_idx += 1;
        } else {
            orig_to_norm.push(norm_idx);
            orig_idx += 1;
        }
    }
    while orig_idx < orig_chars.len() {
        orig_to_norm.push(norm_chars.len());
        orig_idx += 1;
    }

    // Invert: first/last original char for each normalized position.
    let mut norm_to_orig_start: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    let mut norm_to_orig_end: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for (orig_pos, &norm_pos) in orig_to_norm.iter().enumerate() {
        norm_to_orig_start.entry(norm_pos).or_insert(orig_pos);
        norm_to_orig_end.insert(norm_pos, orig_pos);
    }

    let char_to_byte = |chars: &[char], idx: usize| -> usize {
        chars.iter().take(idx).map(|c| c.len_utf8()).sum()
    };

    let mut out = Vec::new();
    for &(norm_start, norm_end) in normalized_matches {
        let orig_start_char = if let Some(&pos) = norm_to_orig_start.get(&norm_start) {
            pos
        } else {
            orig_to_norm
                .iter()
                .position(|&n| n >= norm_start)
                .unwrap_or(orig_chars.len())
        };
        let mut orig_end_char =
            if let Some(&pos) = norm_to_orig_end.get(&(norm_end.saturating_sub(1))) {
                pos + 1
            } else {
                orig_start_char + (norm_end - norm_start)
            };
        // Expand trailing whitespace only when the normalized match itself
        // ended with a space (hermes issue #52491).
        if norm_end < norm_chars.len() && norm_end > 0 && norm_chars[norm_end - 1] == ' ' {
            while orig_end_char < orig_chars.len()
                && (orig_chars[orig_end_char] == ' ' || orig_chars[orig_end_char] == '\t')
            {
                orig_end_char += 1;
            }
        }
        let start_b = char_to_byte(&orig_chars, orig_start_char);
        let end_b = char_to_byte(&orig_chars, orig_end_char.min(orig_chars.len()));
        out.push((start_b, end_b.min(original.len())));
    }
    out
}

// ---------------------------------------------------------------------------
// Strategy 4: indentation-flexible
// ---------------------------------------------------------------------------

fn strategy_indentation_flexible(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let pattern_normalized = pattern
        .split('\n')
        .map(|line| line.trim_start())
        .collect::<Vec<_>>()
        .join("\n");
    let content_window_lines: Vec<String> = content
        .split('\n')
        .map(|line| line.trim_start().to_string())
        .collect();
    find_line_windows(content, &content_window_lines, &pattern_normalized)
}

// ---------------------------------------------------------------------------
// Strategy 5: escape-normalized
// ---------------------------------------------------------------------------

fn strategy_escape_normalized(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let unescaped = pattern
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r");
    if unescaped == pattern {
        return Vec::new();
    }
    strategy_exact(content, &unescaped)
}

// ---------------------------------------------------------------------------
// Strategy 6: trimmed-boundary
// ---------------------------------------------------------------------------

fn strategy_trimmed_boundary(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let mut pattern_lines: Vec<&str> = pattern.split('\n').collect();
    if pattern_lines.is_empty() {
        return Vec::new();
    }
    let first_owned = pattern_lines[0].trim().to_string();
    let last_owned = pattern_lines[pattern_lines.len() - 1].trim().to_string();
    pattern_lines[0] = &first_owned;
    if pattern_lines.len() > 1 {
        let last = pattern_lines.len() - 1;
        pattern_lines[last] = &last_owned;
    }
    let modified_pattern = pattern_lines.join("\n");

    let content_lines: Vec<&str> = content.split('\n').collect();
    let pattern_line_count = pattern_lines.len();
    let mut matches = Vec::new();
    if content_lines.len() < pattern_line_count {
        return matches;
    }
    let line_starts = line_offsets(content);
    for i in 0..=(content_lines.len() - pattern_line_count) {
        let mut check: Vec<String> = content_lines[i..i + pattern_line_count]
            .iter()
            .map(|l| l.to_string())
            .collect();
        check[0] = check[0].trim().to_string();
        if check.len() > 1 {
            let last = check.len() - 1;
            check[last] = check[last].trim().to_string();
        }
        if check.join("\n") == modified_pattern {
            matches.push(calculate_line_positions(
                &line_starts,
                i,
                i + pattern_line_count,
                content.len(),
            ));
        }
    }
    matches
}

// ---------------------------------------------------------------------------
// Strategy 7: unicode-normalized
// ---------------------------------------------------------------------------

const UNICODE_MAP: &[(char, &str)] = &[
    ('\u{201c}', "\""),
    ('\u{201d}', "\""),
    ('\u{2018}', "'"),
    ('\u{2019}', "'"),
    ('\u{2014}', "--"),
    ('\u{2013}', "-"),
    ('\u{2026}', "..."),
    ('\u{00a0}', " "),
    ('\u{2212}', "-"),
    ('\u{2000}', " "),
    ('\u{2001}', " "),
    ('\u{2002}', " "),
    ('\u{2003}', " "),
    ('\u{2004}', " "),
    ('\u{2005}', " "),
    ('\u{2006}', " "),
    ('\u{2007}', " "),
    ('\u{2008}', " "),
    ('\u{2009}', " "),
    ('\u{200a}', " "),
    ('\u{202f}', " "),
    ('\u{205f}', " "),
    ('\u{3000}', " "),
];

fn unicode_normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match UNICODE_MAP.iter().find(|(c, _)| *c == ch) {
            Some((_, repl)) => out.push_str(repl),
            None => out.push(ch),
        }
    }
    out
}

/// Map original char index -> normalized char index (replacements can expand).
fn build_orig_to_norm_map(original: &str) -> Vec<usize> {
    let mut result = Vec::with_capacity(original.chars().count() + 1);
    let mut norm_pos = 0usize;
    for ch in original.chars() {
        result.push(norm_pos);
        match UNICODE_MAP.iter().find(|(c, _)| *c == ch) {
            Some((_, repl)) => norm_pos += repl.chars().count(),
            None => norm_pos += 1,
        }
    }
    result.push(norm_pos);
    result
}

fn map_norm_matches_to_orig(
    orig_to_norm: &[usize],
    norm_matches: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let mut norm_to_orig_start: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for (orig_pos, &norm_pos) in orig_to_norm[..orig_to_norm.len() - 1].iter().enumerate() {
        norm_to_orig_start.entry(norm_pos).or_insert(orig_pos);
    }
    let orig_len = orig_to_norm.len() - 1;
    let mut out = Vec::new();
    for &(norm_start, norm_end) in norm_matches {
        let Some(&orig_start) = norm_to_orig_start.get(&norm_start) else {
            continue;
        };
        let mut orig_end = orig_start;
        while orig_end < orig_len && orig_to_norm[orig_end] < norm_end {
            orig_end += 1;
        }
        out.push((orig_start, orig_end));
    }
    out
}

fn strategy_unicode_normalized(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let norm_pattern = unicode_normalize(pattern);
    if norm_pattern == pattern {
        // Pattern has no typographic chars — this strategy can only fire when
        // the FILE side carries them; still try (content gets normalized).
    }
    let norm_content = unicode_normalize(content);
    let norm_matches = strategy_exact(&norm_content, &norm_pattern);
    if norm_matches.is_empty() {
        return Vec::new();
    }
    // Char-space ranges in original, then to bytes.
    let orig_to_norm = build_orig_to_norm_map(content);
    let char_ranges = map_norm_matches_to_orig(&orig_to_norm, &norm_matches);
    let orig_chars: Vec<char> = content.chars().collect();
    char_ranges
        .into_iter()
        .map(|(start_char, end_char)| {
            let start_b: usize = orig_chars[..start_char.min(orig_chars.len())]
                .iter()
                .map(|c| c.len_utf8())
                .sum();
            let end_b: usize = orig_chars[..end_char.min(orig_chars.len())]
                .iter()
                .map(|c| c.len_utf8())
                .sum();
            (start_b, end_b)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Similarity (difflib.ratio stand-in)
// ---------------------------------------------------------------------------

fn lcs_len(a: &[char], b: &[char]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut curr = vec![0usize; b.len() + 1];
    for ca in a.iter() {
        for (j, cb) in b.iter().enumerate() {
            curr[j + 1] = if ca == cb {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
        for slot in curr.iter_mut() {
            *slot = 0;
        }
    }
    prev[b.len()]
}

pub fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let total = a_chars.len() + b_chars.len();
    if total == 0 {
        return 1.0;
    }
    2.0 * lcs_len(&a_chars, &b_chars) as f64 / total as f64
}

// ---------------------------------------------------------------------------
// Strategy 8: block-anchor
// ---------------------------------------------------------------------------

fn strategy_block_anchor(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let norm_pattern = unicode_normalize(pattern);
    let norm_content = unicode_normalize(content);

    let pattern_lines: Vec<&str> = norm_pattern.split('\n').collect();
    if pattern_lines.len() < 2 {
        return Vec::new();
    }
    let first_line = pattern_lines[0].trim();
    let last_line = pattern_lines[pattern_lines.len() - 1].trim();

    let norm_content_lines: Vec<&str> = norm_content.split('\n').collect();
    let pattern_line_count = pattern_lines.len();
    if norm_content_lines.len() < pattern_line_count {
        return Vec::new();
    }

    let mut potential = Vec::new();
    for i in 0..=(norm_content_lines.len() - pattern_line_count) {
        if norm_content_lines[i].trim() == first_line
            && norm_content_lines[i + pattern_line_count - 1].trim() == last_line
        {
            potential.push(i);
        }
    }

    // 0.50 for a unique candidate, 0.70 when several compete.
    let threshold = if potential.len() == 1 { 0.50 } else { 0.70 };

    // Positions must be computed on ORIGINAL lines; unicode normalization
    // never adds/removes newlines, so indices align.
    let line_starts = line_offsets(content);
    let content_len = content.len();
    let mut matches = Vec::new();
    for i in potential {
        let sim = if pattern_line_count <= 2 {
            1.0
        } else {
            let content_middle = norm_content_lines[i + 1..i + pattern_line_count - 1].join("\n");
            let pattern_middle = pattern_lines[1..pattern_line_count - 1].join("\n");
            similarity(&content_middle, &pattern_middle)
        };
        if sim >= threshold {
            matches.push(calculate_line_positions(
                &line_starts,
                i,
                i + pattern_line_count,
                content_len,
            ));
        }
    }
    matches
}

// ---------------------------------------------------------------------------
// Strategy 9: context-aware
// ---------------------------------------------------------------------------

fn strategy_context_aware(content: &str, pattern: &str) -> Vec<(usize, usize)> {
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();
    if pattern_lines.is_empty() || pattern_lines.len() > content_lines.len() {
        return Vec::new();
    }
    let pattern_line_count = pattern_lines.len();
    let first_pat = pattern_lines[0].trim();
    let last_pat = pattern_lines[pattern_line_count - 1].trim();

    let line_starts = line_offsets(content);
    let mut matches = Vec::new();
    for i in 0..=(content_lines.len() - pattern_line_count) {
        let block = &content_lines[i..i + pattern_line_count];
        if similarity(first_pat, block[0].trim()) < ANCHOR_THRESHOLD {
            continue;
        }
        if similarity(last_pat, block[pattern_line_count - 1].trim()) < ANCHOR_THRESHOLD {
            continue;
        }
        let mut all_match = true;
        for (p_line, c_line) in pattern_lines.iter().zip(block.iter()) {
            let p_stripped = p_line.trim();
            if p_stripped.is_empty() {
                continue;
            }
            if similarity(p_stripped, c_line.trim()) < ANCHOR_THRESHOLD {
                all_match = false;
                break;
            }
        }
        if all_match {
            matches.push(calculate_line_positions(
                &line_starts,
                i,
                i + pattern_line_count,
                content.len(),
            ));
        }
    }
    matches
}

// ---------------------------------------------------------------------------
// Replacement helpers
// ---------------------------------------------------------------------------

fn leading_whitespace(line: &str) -> &str {
    let end = line
        .char_indices()
        .find(|(_, c)| *c != ' ' && *c != '\t')
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    &line[..end]
}

fn first_meaningful_line(text: &str) -> Option<&str> {
    text.split('\n').find(|line| !line.trim().is_empty())
}

/// Adjust `new_string` indentation to the file's actual base indent after a
/// non-exact fuzzy match (hermes `_reindent_replacement`).
pub fn reindent_replacement(file_region: &str, old_string: &str, new_string: &str) -> String {
    if new_string.is_empty() {
        return new_string.to_string();
    }
    let Some(old_first) = first_meaningful_line(old_string) else {
        return new_string.to_string();
    };
    let Some(file_first) = first_meaningful_line(file_region) else {
        return new_string.to_string();
    };
    let old_indent = leading_whitespace(old_first);
    let file_indent = leading_whitespace(file_first);
    if old_indent == file_indent {
        return new_string.to_string();
    }
    let mut out = Vec::new();
    for line in new_string.split('\n') {
        if line.trim().is_empty() {
            out.push(line.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix(old_indent) {
            out.push(format!("{}{}", file_indent, rest));
        } else {
            out.push(format!("{}{}", file_indent, line.trim_start()));
        }
    }
    out.join("\n")
}

/// Conditionally unescape `\\t`/`\\r` in `new_string` — only when the matched
/// file region actually contains the real control character (hermes
/// `_maybe_unescape_new_string`).
pub fn maybe_unescape_new_string(
    new_string: &str,
    content: &str,
    matches: &[(usize, usize)],
) -> String {
    if !new_string.contains("\\t") && !new_string.contains("\\r") {
        return new_string.to_string();
    }
    let matched_regions: String = matches
        .iter()
        .map(|&(start, end)| &content[start..end])
        .collect();
    let mut out = new_string.to_string();
    if out.contains("\\t") && matched_regions.contains('\t') {
        out = out.replace("\\t", "\t");
    }
    if out.contains("\\r") && matched_regions.contains('\r') {
        out = out.replace("\\r", "\r");
    }
    out
}

/// Render up to `cap` match positions as `L<line>: <snippet>` rows (hermes
/// `_format_match_locations`).
pub fn format_match_locations(content: &str, matches: &[(usize, usize)], cap: usize) -> String {
    let mut rows = Vec::new();
    for &(start, _end) in matches.iter().take(cap) {
        let line_no = content[..start].matches('\n').count() + 1;
        let line_start = content[..start].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let line_end = content[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(content.len());
        let mut snippet = content[line_start..line_end].trim().to_string();
        if snippet.chars().count() > 80 {
            let truncated: String = snippet.chars().take(77).collect();
            snippet = format!("{}...", truncated);
        }
        rows.push(format!("  L{}: {}", line_no, snippet));
    }
    if matches.len() > cap {
        rows.push(format!("  ... and {} more", matches.len() - cap));
    }
    rows.join("\n")
}

/// Detect tool-call escape-drift artifacts (`\'` / `\"`) in `new_string`
/// (hermes `_detect_escape_drift`). Returns an error message when detected.
pub fn detect_escape_drift(
    content: &str,
    matches: &[(usize, usize)],
    old_string: &str,
    new_string: &str,
) -> Option<String> {
    if !new_string.contains("\\'") && !new_string.contains("\\\"") {
        return None;
    }
    let matched_regions: String = matches
        .iter()
        .map(|&(start, end)| &content[start..end])
        .collect();
    for suspect in ["\\'", "\\\""] {
        if new_string.contains(suspect)
            && old_string.contains(suspect)
            && !matched_regions.contains(suspect)
        {
            let plain = &suspect[1..];
            return Some(format!(
                "Escape-drift detected: old_string and new_string contain the literal sequence {:?} \
                 but the matched region of the file does not. This is almost always a tool-call \
                 serialization artifact where an apostrophe or quote got prefixed with a spurious \
                 backslash. Re-read the file with read_file and pass old_string/new_string without \
                 backslash-escaping {} characters.",
                suspect, plain
            ));
        }
    }
    None
}

/// Unicode-preserving replacement for strategy 7: diff norm(old) -> new and
/// apply edits onto the original file region, keeping untouched Unicode
/// spans intact.
pub fn apply_unicode_preserving_replacement(
    content: &str,
    matches: &[(usize, usize)],
    old_string: &str,
    new_string: &str,
) -> String {
    let file_region: String = matches
        .iter()
        .map(|&(start, end)| &content[start..end])
        .collect();
    let norm_old = unicode_normalize(old_string);
    let norm_file = unicode_normalize(&file_region);
    if norm_old != norm_file {
        return new_string.to_string();
    }

    let file_orig_to_norm = build_orig_to_norm_map(&file_region);
    let mut file_norm_to_orig: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    for (orig_pos, &norm_pos) in file_orig_to_norm[..file_orig_to_norm.len() - 1]
        .iter()
        .enumerate()
    {
        file_norm_to_orig.entry(norm_pos).or_insert(orig_pos);
    }

    let opcodes = diff_opcodes(&norm_old, new_string);
    let file_chars: Vec<char> = file_region.chars().collect();
    let mut result = String::new();
    for (tag, i1, i2, j1, j2) in opcodes {
        match tag.as_str() {
            "equal" => {
                let orig_start = file_norm_to_orig.get(&i1).copied().unwrap_or(0);
                let mut orig_end = orig_start;
                while orig_end < file_chars.len() && file_orig_to_norm[orig_end] < i2 {
                    orig_end += 1;
                }
                result.extend(&file_chars[orig_start..orig_end]);
            }
            "replace" | "insert" => {
                let new_chars: Vec<char> = new_string.chars().collect();
                result.extend(&new_chars[j1..j2]);
            }
            _ => {}
        }
    }
    result
}

/// LCS-based diff opcodes (equal/replace/delete/insert) over char sequences —
/// the difflib.SequenceMatcher.get_opcodes stand-in.
fn diff_opcodes(a: &str, b: &str) -> Vec<(String, usize, usize, usize, usize)> {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();
    // Full LCS table for backtracking (patterns are small; cap for safety).
    if n * m > 4_000_000 {
        return vec![("replace".to_string(), 0, n, 0, m)];
    }
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if a_chars[i] == b_chars[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    // Backtrack into edit ops, then merge consecutive ops into opcodes.
    let mut ops: Vec<(char, usize, usize)> = Vec::new(); // (kind, a_idx, b_idx)
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a_chars[i] == b_chars[j] {
            ops.push(('e', i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            ops.push(('d', i, j));
            i += 1;
        } else {
            ops.push(('i', i, j));
            j += 1;
        }
    }
    while i < n {
        ops.push(('d', i, j));
        i += 1;
    }
    while j < m {
        ops.push(('i', i, j));
        j += 1;
    }

    let mut opcodes: Vec<(String, usize, usize, usize, usize)> = Vec::new();
    let mut ai = 0usize;
    let mut bj = 0usize;
    let mut idx = 0usize;
    while idx < ops.len() {
        let (kind, _, _) = ops[idx];
        if kind == 'e' {
            let start_ai = ai;
            let start_bj = bj;
            while idx < ops.len() && ops[idx].0 == 'e' {
                ai += 1;
                bj += 1;
                idx += 1;
            }
            opcodes.push(("equal".to_string(), start_ai, ai, start_bj, bj));
        } else {
            let start_ai = ai;
            let start_bj = bj;
            while idx < ops.len() && ops[idx].0 != 'e' {
                match ops[idx].0 {
                    'd' => ai += 1,
                    'i' => bj += 1,
                    _ => {}
                }
                idx += 1;
            }
            let tag = if ai > start_ai && bj > start_bj {
                "replace"
            } else if ai > start_ai {
                "delete"
            } else {
                "insert"
            };
            opcodes.push((tag.to_string(), start_ai, ai, start_bj, bj));
        }
    }
    opcodes
}

// ---------------------------------------------------------------------------
// "Did you mean?" feedback
// ---------------------------------------------------------------------------

fn visualize_whitespace(line: &str) -> String {
    let mut out = String::new();
    let mut rest_start = line.len();
    for (idx, ch) in line.char_indices() {
        if ch == ' ' || ch == '\t' {
            out.push(if ch == '\t' { '→' } else { '·' });
        } else {
            rest_start = idx;
            break;
        }
    }
    out.push_str(&line[rest_start..]);
    out
}

/// hermes `find_closest_lines`: context-rich suggestions for no-match errors.
pub fn find_closest_lines(
    old_string: &str,
    content: &str,
    context_lines: usize,
    max_results: usize,
) -> String {
    if old_string.is_empty() || content.is_empty() {
        return String::new();
    }
    let old_lines: Vec<&str> = old_string.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();
    if old_lines.is_empty() || content_lines.is_empty() {
        return String::new();
    }
    let anchor = old_lines
        .iter()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if anchor.is_empty() {
        return String::new();
    }

    let mut scored: Vec<(f64, usize)> = Vec::new();
    for (i, line) in content_lines.iter().enumerate() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let ratio = similarity(anchor, stripped);
        if ratio > 0.3 {
            scored.push((ratio, i));
        }
    }
    if scored.is_empty() {
        return String::new();
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<(f64, usize)> = scored.into_iter().take(max_results).collect();

    let mut parts: Vec<String> = Vec::new();
    let mut seen: Vec<(usize, usize)> = Vec::new();
    for &(_, line_idx) in &top {
        let start = line_idx.saturating_sub(context_lines);
        let end = (line_idx + old_lines.len() + context_lines).min(content_lines.len());
        if seen.contains(&(start, end)) {
            continue;
        }
        seen.push((start, end));
        let snippet = (start..end)
            .map(|j| format!("{:4}| {}", j + 1, content_lines[j]))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(snippet);
    }
    if parts.is_empty() {
        return String::new();
    }
    let mut result = parts.join("\n---\n");

    let best_line = content_lines[top[0].1];
    if best_line.trim() == anchor && best_line != old_lines[0] {
        result.push_str(&format!(
            "\n\nWhitespace difference detected (→ = tab, · = space):\n  file has: {}\n  you sent: {}\nUse the exact whitespace shown in 'file has'.",
            visualize_whitespace(best_line),
            visualize_whitespace(old_lines[0])
        ));
    }
    result
}

/// Append a "Did you mean..." hint to plain no-match errors.
pub fn format_no_match_hint(
    error: &str,
    match_count: usize,
    old_string: &str,
    content: &str,
) -> String {
    if match_count != 0 || !error.starts_with("Could not find") {
        return String::new();
    }
    let hint = find_closest_lines(old_string, content, 2, 3);
    if hint.is_empty() {
        String::new()
    } else {
        format!("\n\nDid you mean one of these sections?\n{}", hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(content: &str, old: &str) -> FuzzyHit {
        fuzzy_find(content, old)
    }

    #[test]
    fn test_exact() {
        let hit = find(
            "fn main() {\n    println!(\"hi\");\n}\n",
            "println!(\"hi\");",
        );
        assert_eq!(hit.strategy, "exact");
        assert_eq!(hit.matches.len(), 1);
    }

    #[test]
    fn test_line_trimmed() {
        let content = "alpha\n  beta  \n gamma\n";
        let hit = find(content, "beta\ngamma");
        assert_eq!(hit.strategy, "line_trimmed");
        let (start, end) = hit.matches[0];
        assert_eq!(&content[start..end], "  beta  \n gamma");
    }

    #[test]
    fn test_whitespace_normalized() {
        let content = "let x =  a   + b;\n";
        let hit = find(content, "let x = a + b;");
        assert_eq!(hit.strategy, "whitespace_normalized");
        let (start, end) = hit.matches[0];
        assert_eq!(&content[start..end], "let x =  a   + b;");
    }

    #[test]
    fn test_indentation_flexible() {
        let content = "fn a() {\n        let x = 1;\n        x\n}\n";
        // The chain's line_trimmed strategy subsumes this case; verify the
        // dedicated strategy directly.
        let matches = strategy_indentation_flexible(content, "fn a() {\n  let x = 1;\n  x\n}");
        assert_eq!(matches.len(), 1);
        let hit = find(content, "fn a() {\n  let x = 1;\n  x\n}");
        assert_eq!(hit.matches.len(), 1);
    }

    #[test]
    fn test_escape_normalized() {
        let content = "line one\nline two\n";
        let hit = find(content, "line one\\nline two");
        assert_eq!(hit.strategy, "escape_normalized");
        let (start, end) = hit.matches[0];
        assert_eq!(&content[start..end], "line one\nline two");
    }

    #[test]
    fn test_trimmed_boundary() {
        let content = "keep\n    middle line\nkeep end\n";
        // Verified directly: line_trimmed subsumes this case in the chain.
        let matches = strategy_trimmed_boundary(content, "  keep\n    middle line\nkeep end  ");
        assert_eq!(matches.len(), 1);
        let (start, end) = matches[0];
        assert_eq!(&content[start..end], "keep\n    middle line\nkeep end");
    }

    #[test]
    fn test_unicode_normalized() {
        let content = "value \u{2014} the em dash\n";
        let hit = find(content, "value -- the em dash");
        assert_eq!(hit.strategy, "unicode_normalized");
        let (start, end) = hit.matches[0];
        assert_eq!(&content[start..end], "value \u{2014} the em dash");
    }

    #[test]
    fn test_block_anchor() {
        let content = "start\naaa\nbbb\nccc\nend\nother\n";
        let hit = find(content, "start\naaa\nbbb\nXcX\nend");
        assert_eq!(hit.strategy, "block_anchor");
        let (start, end) = hit.matches[0];
        assert_eq!(&content[start..end], "start\naaa\nbbb\nccc\nend");
    }

    #[test]
    fn test_context_aware() {
        let content = "fn compute(x: i64) -> i64 {\n    let y = x * 2;\n    y + 1\n}\n";
        // First line differs slightly so the exact-anchor block_anchor
        // strategy cannot fire; context_aware matches by similarity.
        let hit = find(
            content,
            "fn compute(x : i64) -> i64 {\n    let y = x * 3;\n    y + 1\n}",
        );
        assert_eq!(hit.strategy, "context_aware");
        assert_eq!(hit.matches.len(), 1);
    }

    #[test]
    fn test_reindent_replacement() {
        let file_region = "    let x = 1;\n    let y = 2;";
        let old = "  let x = 1;\n  let y = 2;";
        let new = "  let x = 10;\n  if x {\n    y\n  }";
        let out = reindent_replacement(file_region, old, new);
        assert_eq!(out, "    let x = 10;\n    if x {\n      y\n    }");
    }

    #[test]
    fn test_escape_drift_guard() {
        let content = "it's fine\n";
        // Both old and new carry the spurious backslash; the file region does not.
        let drift = detect_escape_drift(content, &[(0, 9)], "it\\'s fine", "it\\'s great");
        assert!(drift.is_some());
        let clean = detect_escape_drift(content, &[(0, 9)], "it's fine", "it's great");
        assert!(clean.is_none());
    }

    #[test]
    fn test_unicode_preserving_replacement() {
        let content = "a \u{2014} b";
        let hit = find(content, "a -- b");
        assert_eq!(hit.strategy, "unicode_normalized");
        let replaced =
            apply_unicode_preserving_replacement(content, &hit.matches, "a -- b", "a -- c");
        // The em dash must survive the replacement.
        assert_eq!(replaced, "a \u{2014} c");
    }

    #[test]
    fn test_no_match_hint_whitespace() {
        let content = "fn main() {\n\tlet x = 1;\n}\n";
        let hint = find_closest_lines("  let x = 1;", content, 1, 1);
        assert!(hint.contains("Whitespace difference detected"));
    }
}

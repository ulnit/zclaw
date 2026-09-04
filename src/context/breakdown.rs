//! Live session context-window breakdown (hermes `agent/context_breakdown.py`
//! port) — estimates how the next provider request is composed: system prompt
//! tiers, tool schemas, MCP schemas, persistent memory, and conversation
//! history. Uses the same rough chars/4 heuristic as
//! [`ContextCompressor::estimate_tokens`] so numbers align with compression
//! thresholds — not exact tokenizer counts.

use serde::Serialize;

/// One labelled slice of the context window (hermes `categories` entry).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContextCategory {
    pub id: String,
    pub label: String,
    pub tokens: usize,
}

/// Full breakdown payload (hermes `ContextBreakdown` shape — the desktop
/// and `/context` render over the same fields).
#[derive(Debug, Clone, Serialize)]
pub struct ContextBreakdown {
    pub categories: Vec<ContextCategory>,
    pub context_max: usize,
    pub context_percent: usize,
    pub context_used: usize,
    pub estimated_total: usize,
    pub model: String,
}

/// The live pieces the gateway needs to compute a breakdown (supplied by
/// `Agent::context_breakdown_parts`).
#[derive(Debug, Clone, Default)]
pub struct BreakdownParts {
    /// System prompt minus the memory block (base + environment + volatile).
    pub system_prompt: String,
    /// Persistent memory block (`## Persistent memory\n...`), empty when none.
    pub memory_block: String,
    /// JSON serialization of the builtin tool schemas.
    pub builtin_tools_json: String,
    /// JSON serialization of the `mcp__*` tool schemas.
    pub mcp_tools_json: String,
    pub model: String,
}

fn chars_to_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        (text.len() + 3) / 4
    }
}

/// Token cost of the static per-call payload (system prompt, tool/MCP
/// schemas, memory — everything except the conversation history). Lets
/// list endpoints reuse one parts snapshot across many sessions.
pub fn static_tokens(parts: &BreakdownParts) -> usize {
    chars_to_tokens(&parts.system_prompt)
        + chars_to_tokens(&parts.builtin_tools_json)
        + chars_to_tokens(&parts.mcp_tools_json)
        + chars_to_tokens(&parts.memory_block)
}

/// Rounded used/budget percentage capped at 100 (0 when budget is 0).
pub fn percent_of(used: usize, budget: usize) -> usize {
    if budget == 0 {
        return 0;
    }
    ((used * 100 + budget / 2) / budget).min(100)
}

/// Compose the breakdown (hermes `compute_session_context_breakdown`).
/// Categories keep the hermes declaration order; zero-cost ones are dropped.
pub fn compute(
    parts: &BreakdownParts,
    conversation_tokens: usize,
    context_max: usize,
) -> ContextBreakdown {
    let raw: [(&str, &str, usize); 5] = [
        (
            "system_prompt",
            "System prompt",
            chars_to_tokens(&parts.system_prompt),
        ),
        (
            "tool_definitions",
            "Tool definitions",
            chars_to_tokens(&parts.builtin_tools_json),
        ),
        ("mcp", "MCP", chars_to_tokens(&parts.mcp_tools_json)),
        ("memory", "Memory", chars_to_tokens(&parts.memory_block)),
        ("conversation", "Conversation", conversation_tokens),
    ];
    let categories: Vec<ContextCategory> = raw
        .iter()
        .filter(|(_, _, tokens)| *tokens > 0)
        .map(|(id, label, tokens)| ContextCategory {
            id: (*id).to_string(),
            label: (*label).to_string(),
            tokens: *tokens,
        })
        .collect();
    let estimated_total: usize = categories.iter().map(|c| c.tokens).sum();
    // ulnclaw has no measured last-prompt-token gauge yet, so the estimate
    // stands in (hermes falls back the same way).
    let context_used = estimated_total;
    let context_percent = percent_of(context_used, context_max);
    ContextBreakdown {
        categories,
        context_max,
        context_percent,
        context_used,
        estimated_total,
        model: parts.model.clone(),
    }
}

// ── /context rendering (hermes render_* parity) ────────────────────────────
//
// The CLI shows a glyph block-grid plus a category table; messaging surfaces
// can drop the grid (proportional monospace is not guaranteed there).

const CATEGORY_GLYPHS: [(&str, char); 5] = [
    ("system_prompt", '\u{25A0}'),    // ■
    ("tool_definitions", '\u{25A3}'), // ▣
    ("mcp", '\u{25A5}'),              // ▥
    ("memory", '\u{25A7}'),           // ▧
    ("conversation", '\u{25A8}'),     // ▨
];
const FREE_GLYPH: char = '\u{00B7}'; // ·
const GRID_COLUMNS: usize = 20;
const GRID_ROWS: usize = 5; // 100 cells → one cell per percent

fn glyph_for(id: &str) -> char {
    CATEGORY_GLYPHS
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, glyph)| *glyph)
        .unwrap_or('\u{25AA}') // ▪
}

/// Render the payload as a Claude Code-style glyph block grid: 100 cells
/// (5×20), each one percent of the model context window. Categories fill in
/// declaration order; the remainder renders as free space.
pub fn render_grid(payload: &ContextBreakdown) -> Vec<String> {
    let total_cells = GRID_COLUMNS * GRID_ROWS;
    let mut cells: Vec<char> = Vec::new();
    if payload.context_max > 0 {
        for category in &payload.categories {
            let mut count = ((category.tokens as f64 / payload.context_max as f64)
                * total_cells as f64)
                .round() as usize;
            if category.tokens > 0 && count == 0 {
                count = 1; // never render a nonzero category as invisible
            }
            cells.extend(std::iter::repeat(glyph_for(&category.id)).take(count));
        }
        cells.truncate(total_cells);
    }
    cells.extend(std::iter::repeat(FREE_GLYPH).take(total_cells - cells.len()));
    (0..GRID_ROWS)
        .map(|row| {
            cells[row * GRID_COLUMNS..(row + 1) * GRID_COLUMNS]
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// Render the "Estimated usage by category" table as plain-text lines.
pub fn render_category_lines(payload: &ContextBreakdown) -> Vec<String> {
    let mut lines = vec!["Estimated usage by category".to_string()];
    if payload.categories.is_empty() {
        lines.push("  (no data yet — send a message first)".to_string());
        return lines;
    }
    let denom = if payload.context_max > 0 {
        payload.context_max
    } else {
        payload.estimated_total
    };
    let width = payload
        .categories
        .iter()
        .map(|c| c.label.len())
        .max()
        .unwrap_or(0)
        .max("Free space".len());
    for category in &payload.categories {
        let pct = if denom > 0 {
            category.tokens as f64 / denom as f64 * 100.0
        } else {
            0.0
        };
        lines.push(format!(
            "{} {:<width$} {:>9} tokens {:>5.1}%",
            glyph_for(&category.id),
            category.label,
            format_thousands(category.tokens),
            pct,
            width = width
        ));
    }
    if payload.context_max > 0 {
        let free = payload.context_max.saturating_sub(payload.estimated_total);
        let pct = free as f64 / payload.context_max as f64 * 100.0;
        lines.push(format!(
            "{} {:<width$} {:>9} tokens {:>5.1}%",
            FREE_GLYPH,
            "Free space",
            format_thousands(free),
            pct,
            width = width
        ));
    }
    lines
}

/// The full `/context` view (hermes `render_context_breakdown_lines`):
/// optional glyph grid + category table + window summary line.
pub fn render_breakdown_lines(payload: &ContextBreakdown, grid: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if grid {
        lines.extend(render_grid(payload));
        lines.push(String::new());
    }
    lines.extend(render_category_lines(payload));
    if payload.context_max > 0 {
        lines.push(String::new());
        lines.push(format!(
            "Context window: {} / {} tokens ({}%)",
            format_thousands(payload.context_used),
            format_thousands(payload.context_max),
            payload.context_percent
        ));
    }
    lines
}

fn format_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> BreakdownParts {
        BreakdownParts {
            system_prompt: "x".repeat(4000),      // ~1000 tokens
            memory_block: "m".repeat(2000),       // ~500 tokens
            builtin_tools_json: "t".repeat(8000), // ~2000 tokens
            mcp_tools_json: String::new(),
            model: "test-model".to_string(),
        }
    }

    #[test]
    fn test_compute_categories_and_percent() {
        let payload = compute(&parts(), 500, 10_000);
        let ids: Vec<&str> = payload.categories.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "system_prompt",
                "tool_definitions",
                "memory",
                "conversation"
            ]
        );
        assert_eq!(payload.estimated_total, 4000);
        assert_eq!(payload.context_used, 4000);
        assert_eq!(payload.context_percent, 40);
        assert_eq!(payload.context_max, 10_000);
        assert_eq!(payload.model, "test-model");
    }

    #[test]
    fn test_compute_zero_max_and_empty() {
        let empty = BreakdownParts::default();
        let payload = compute(&empty, 0, 0);
        assert!(payload.categories.is_empty());
        assert_eq!(payload.context_percent, 0);
        assert_eq!(payload.estimated_total, 0);
    }

    #[test]
    fn test_percent_caps_at_100() {
        let payload = compute(&parts(), 100_000, 10_000);
        assert_eq!(payload.context_percent, 100);
    }

    #[test]
    fn test_render_grid_has_100_cells() {
        let payload = compute(&parts(), 500, 10_000);
        let grid = render_grid(&payload);
        assert_eq!(grid.len(), GRID_ROWS);
        for row in &grid {
            assert_eq!(row.split(' ').count(), GRID_COLUMNS);
        }
        // 40% used → 40 glyph cells + 60 free cells.
        let all: String = grid.join("");
        let free = all.chars().filter(|c| *c == FREE_GLYPH).count();
        assert_eq!(free, 60);
    }

    #[test]
    fn test_render_grid_empty_when_no_max() {
        let payload = compute(&BreakdownParts::default(), 0, 0);
        let grid = render_grid(&payload);
        let all: String = grid.join("");
        assert_eq!(all.chars().filter(|c| *c == FREE_GLYPH).count(), 100);
    }

    #[test]
    fn test_render_category_lines_includes_free_space() {
        let payload = compute(&parts(), 500, 10_000);
        let lines = render_category_lines(&payload);
        assert_eq!(lines[0], "Estimated usage by category");
        assert!(lines.iter().any(|l| l.contains("System prompt")));
        assert!(lines.iter().any(|l| l.contains("Conversation")));
        assert!(lines
            .iter()
            .any(|l| l.contains("Free space") && l.contains("6,000")));
    }

    #[test]
    fn test_render_breakdown_lines_with_and_without_grid() {
        let payload = compute(&parts(), 500, 10_000);
        let with_grid = render_breakdown_lines(&payload, true);
        assert!(with_grid
            .iter()
            .any(|l| l.contains("Context window: 4,000 / 10,000 tokens (40%)")));
        let without = render_breakdown_lines(&payload, false);
        assert_eq!(without.len(), with_grid.len() - GRID_ROWS - 1);
    }

    #[test]
    fn test_static_tokens_and_percent_of() {
        let parts = parts();
        // 4000 + 8000 + 0 + 2000 chars → 1000 + 2000 + 0 + 500 tokens.
        assert_eq!(static_tokens(&parts), 3500);
        assert_eq!(percent_of(50, 100), 50);
        assert_eq!(percent_of(1, 3), 33);
        assert_eq!(percent_of(2, 3), 67);
        assert_eq!(percent_of(500, 100), 100);
        assert_eq!(percent_of(10, 0), 0);
    }

    #[test]
    fn test_format_thousands() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1000), "1,000");
        assert_eq!(format_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn test_render_empty_categories_hint() {
        let payload = compute(&BreakdownParts::default(), 0, 10_000);
        let lines = render_category_lines(&payload);
        assert!(lines[1].contains("no data yet"));
    }
}

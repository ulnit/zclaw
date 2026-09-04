//! File tools — port of hermes' tools/file_tools.py
//!
//! Tools: read_file, write_file, patch (replace + V4A patch), search_files.

use crate::error::Result;
use crate::tools::{tool, ToolContext, ToolRegistry};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_READ_CHARS: usize = 100_000;
const DEFAULT_SEARCH_LIMIT: usize = 50;

/// Directories skipped by search_files (mirrors hermes' ignore set).
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    ".idea",
    ".next",
];

pub fn register(registry: &mut ToolRegistry) {
    registry.register(read_file_tool());
    registry.register(write_file_tool());
    registry.register(patch_tool());
    registry.register(search_files_tool());
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

fn read_file_tool() -> crate::tools::Tool {
    tool("read_file")
        .description(
            "Read a text file with line numbers and pagination. Use this instead of cat/head/tail \
             in terminal. Output format: 'LINE_NUM|CONTENT'. Suggests similar filenames if not \
             found. Use offset and limit for large files. Reads exceeding ~100K characters are \
             truncated on a line boundary and return a next_offset; continue with offset to read \
             the rest.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read (absolute, relative, or ~/path)"},
                "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed, default: 1)", "default": 1, "minimum": 1},
                "limit": {"type": "integer", "description": "Maximum number of lines to read (default and max come from [tool_output] max_lines, normally 2000)", "default": 2000}
            },
            "required": ["path"]
        }))
        .handler(|args, ctx| async move {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
            let max_lines = ctx.config.tool_output.resolved().max_lines as u64;
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(max_lines)
                .min(max_lines) as usize;
            read_file_impl(&ctx, &path, offset, limit)
        })
        .toolset("file")
        .emoji("📄")
        .build()
        .expect("read_file builds")
}

fn suggest_similar(dir: &Path, filename: &str) -> Vec<String> {
    let mut suggestions = Vec::new();
    let needle = filename.to_lowercase();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return suggestions;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let lower = name.to_lowercase();
        if lower.contains(&needle) || needle.contains(&lower) || levenshtein_close(&lower, &needle)
        {
            suggestions.push(name);
        }
        if suggestions.len() >= 5 {
            break;
        }
    }
    suggestions
}

fn levenshtein_close(a: &str, b: &str) -> bool {
    // Cheap closeness check: equal length ±2 and >=60% shared chars in order.
    if a.len().abs_diff(b.len()) > 2 {
        return false;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut matches = 0usize;
    let mut j = 0usize;
    for ch in &a {
        if let Some(pos) = b[j..].iter().position(|c| c == ch) {
            matches += 1;
            j += pos + 1;
        }
    }
    let max_len = a.len().max(b.len()).max(1);
    matches * 10 >= max_len * 6
}

fn read_file_impl(
    ctx: &Arc<ToolContext>,
    raw_path: &str,
    offset: usize,
    limit: usize,
) -> Result<serde_json::Value> {
    if raw_path.is_empty() {
        return Ok(json!({"success": false, "error": "read_file: missing required field 'path'"}));
    }
    let path = ctx.resolve_path(raw_path);
    if !path.exists() {
        let filename = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let suggestions = suggest_similar(dir, &filename);
        let hint = if suggestions.is_empty() {
            String::new()
        } else {
            format!(
                " Similar files in {}: {}",
                dir.display(),
                suggestions.join(", ")
            )
        };
        return Ok(json!({
            "success": false,
            "error": format!("File not found: {}{}", path.display(), hint)
        }));
    }
    if path.is_dir() {
        return Ok(json!({
            "success": false,
            "error": format!("'{}' is a directory — use search_files with target='files' to list it", path.display())
        }));
    }
    // Binary file guard (hermes binary_extensions): block by extension, no I/O.
    if crate::binary_ext::has_binary_extension(&path.display().to_string()) {
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or_default();
        return Ok(json!({
            "success": false,
            "error": format!(
                "Cannot read binary file '{}' ({}). Use vision_analyze for images, or terminal to inspect binary files.",
                raw_path, ext
            )
        }));
    }

    let content = match std::fs::read(&path) {
        Ok(bytes) => {
            if bytes.iter().take(8192).any(|b| *b == 0) {
                return Ok(json!({
                    "success": false,
                    "error": "Cannot read binary files — use vision_analyze for images."
                }));
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }
        Err(e) => {
            return Ok(json!({"success": false, "error": format!("read error: {}", e)}));
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = offset.saturating_sub(1);
    if start > total {
        return Ok(json!({
            "success": true,
            "content": "",
            "total_lines": total,
            "note": format!("offset {} beyond end of file ({} lines)", offset, total)
        }));
    }

    let limits = ctx.config.tool_output.resolved();
    let mut out = String::new();
    let mut char_count = 0usize;
    let mut last_line = start;
    for (idx, line) in lines.iter().enumerate().skip(start).take(limit) {
        // Long lines get clamped with a marker (hermes file_operations).
        let display: String = if line.chars().count() > limits.max_line_length {
            line.chars()
                .take(limits.max_line_length)
                .collect::<String>()
                + "... [truncated]"
        } else {
            (*line).to_string()
        };
        let formatted = format!("{}|{}\n", idx + 1, display);
        char_count += formatted.len();
        if char_count > MAX_READ_CHARS {
            break;
        }
        out.push_str(&formatted);
        last_line = idx + 1;
    }

    // Secrets in file content become non-reusable sentinels so the agent
    // never sees (or writes back) real credential bytes (agent/redact.py,
    // file_read semantics).
    let out = crate::redact::redact_sensitive_text(
        &out,
        crate::redact::RedactOpts {
            file_read: true,
            ..Default::default()
        },
    );
    let mut result = json!({
        "success": true,
        "content": out,
        "total_lines": total,
    });
    if last_line < total {
        result["next_offset"] = json!(last_line + 1);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

fn write_file_tool() -> crate::tools::Tool {
    tool("write_file")
        .description(
            "Write content to a file, completely replacing existing content. Use this instead of \
             echo/cat heredoc in terminal. Creates parent directories automatically. OVERWRITES \
             the entire file — use 'patch' for targeted edits. The result's verified:true means \
             the on-disk content hash was confirmed — do NOT re-read the file to check the write \
             landed.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write (will be created if it doesn't exist, overwritten if it does)"},
                "content": {"type": "string", "description": "Complete content to write to the file"}
            },
            "required": ["path", "content"]
        }))
        .handler(|args, ctx| async move {
            let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "write_file: missing required field 'path'"}));
            };
            let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "write_file: missing required field 'content'. Re-emit the tool call with both 'path' and 'content' set."}));
            };
            write_file_impl(&ctx, path, content)
        })
        .toolset("file")
        .emoji("✍️")
        .build()
        .expect("write_file builds")
}

fn write_file_impl(
    ctx: &Arc<ToolContext>,
    raw_path: &str,
    content: &str,
) -> Result<serde_json::Value> {
    let path = ctx.resolve_path(raw_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Err(e) = std::fs::write(&path, content) {
        return Ok(json!({"success": false, "error": format!("write failed: {}", e)}));
    }
    // Verify by hashing what landed on disk (hermes verified:true contract).
    let verified = std::fs::read(&path)
        .map(|bytes| {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let disk = format!("{:x}", hasher.finalize());
            let mut hasher = Sha256::new();
            hasher.update(content.as_bytes());
            disk == format!("{:x}", hasher.finalize())
        })
        .unwrap_or(false);
    Ok(json!({
        "success": true,
        "path": path.display().to_string(),
        "bytes_written": content.len(),
        "verified": verified,
    }))
}

// ---------------------------------------------------------------------------
// patch — replace mode with fuzzy matching + V4A multi-file patches
// ---------------------------------------------------------------------------

fn patch_tool() -> crate::tools::Tool {
    tool("patch")
        .description(
            "Targeted find-and-replace edits in files. Use this instead of sed/awk in terminal. \
             Uses fuzzy matching so minor whitespace/indentation differences won't break it. \
             Returns a unified diff.\n\n\
             REPLACE MODE (mode='replace', default): find a unique string and replace it. \
             REQUIRED PARAMETERS: mode, path, old_string, new_string.\n\
             PATCH MODE (mode='patch'): apply V4A multi-file patches for bulk changes. \
             REQUIRED PARAMETERS: mode, patch.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["replace", "patch"], "description": "Edit mode. 'replace' (default): requires path + old_string + new_string. 'patch': requires patch content only.", "default": "replace"},
                "path": {"type": "string", "description": "REQUIRED when mode='replace'. File path to edit."},
                "old_string": {"type": "string", "description": "REQUIRED when mode='replace'. Exact text to find and replace. Must be unique in the file unless replace_all=true."},
                "new_string": {"type": "string", "description": "REQUIRED when mode='replace'. Replacement text. Pass empty string '' to delete the matched text."},
                "replace_all": {"type": "boolean", "description": "Replace all occurrences instead of requiring a unique match (default: false)", "default": false},
                "patch": {"type": "string", "description": "REQUIRED when mode='patch'. V4A format patch content. Format:\n*** Begin Patch\n*** Update File: path/to/file\n@@ context hint @@\n context line\n-removed line\n+added line\n*** End Patch"}
            },
            "required": ["mode"]
        }))
        .handler(|args, ctx| async move {
            let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("replace");
            match mode {
                "patch" => {
                    let Some(patch) = args.get("patch").and_then(|v| v.as_str()) else {
                        return Ok(json!({"success": false, "error": "patch: mode='patch' requires the 'patch' parameter"}));
                    };
                    apply_v4a_patch(&ctx, patch)
                }
                _ => {
                    let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
                        return Ok(json!({"success": false, "error": "patch: mode='replace' requires 'path'"}));
                    };
                    let Some(old_string) = args.get("old_string").and_then(|v| v.as_str()) else {
                        return Ok(json!({"success": false, "error": "patch: mode='replace' requires 'old_string'"}));
                    };
                    let new_string = args.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                    let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
                    patch_replace(&ctx, path, old_string, new_string, replace_all)
                }
            }
        })
        .toolset("file")
        .emoji("🩹")
        .build()
        .expect("patch builds")
}

/// Fuzzy find-and-replace — port of the hermes strategy chain (exact →
/// line-trimmed → whitespace-normalized → indentation-flexible →
/// boundary-trimmed).
/// Check whether an edit was already applied (hermes is_already_applied).
fn already_applied(content: &str, old: &str, new: &str) -> bool {
    !new.is_empty()
        && !crate::tools::fuzzy::fuzzy_find(content, new)
            .matches
            .is_empty()
        && crate::tools::fuzzy::fuzzy_find(content, old)
            .matches
            .is_empty()
}

fn make_diff(path: &str, before: &str, after: &str) -> String {
    let mut diff = format!("--- a/{}\n+++ b/{}\n", path, path);
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    // Simple LCS-free diff: find common prefix/suffix, emit the middle.
    let mut prefix = 0usize;
    while prefix < before_lines.len()
        && prefix < after_lines.len()
        && before_lines[prefix] == after_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < before_lines.len() - prefix
        && suffix < after_lines.len() - prefix
        && before_lines[before_lines.len() - 1 - suffix]
            == after_lines[after_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let ctx_start = prefix.saturating_sub(3);
    for i in ctx_start..prefix {
        diff.push_str(&format!(" {}\n", before_lines[i]));
    }
    for line in &before_lines[prefix..before_lines.len() - suffix] {
        diff.push_str(&format!("-{}\n", line));
    }
    for line in &after_lines[prefix..after_lines.len() - suffix] {
        diff.push_str(&format!("+{}\n", line));
    }
    let ctx_end = (before_lines.len() - suffix + 3).min(before_lines.len());
    for i in (before_lines.len() - suffix)..ctx_end {
        diff.push_str(&format!(" {}\n", before_lines[i]));
    }
    diff
}

fn patch_replace(
    ctx: &Arc<ToolContext>,
    raw_path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<serde_json::Value> {
    let path = ctx.resolve_path(raw_path);
    if !path.exists() {
        return Ok(
            json!({"success": false, "error": format!("File not found: {}", path.display())}),
        );
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| crate::error::AgentError::tool(format!("read failed: {}", e)))?;

    if already_applied(&content, old_string, new_string) {
        return Ok(json!({
            "success": true,
            "note": "Edit appears to be already applied — no changes made.",
            "path": path.display().to_string(),
        }));
    }

    let hit = crate::tools::fuzzy::fuzzy_find(&content, old_string);
    if hit.matches.is_empty() {
        let hint = crate::tools::fuzzy::format_no_match_hint(
            "Could not find a match for old_string in the file",
            0,
            old_string,
            &content,
        );
        return Ok(json!({
            "success": false,
            "error": format!(
                "old_string not found in {} (even with fuzzy matching). Read the file to check exact content.{}",
                path.display(),
                hint
            )
        }));
    }
    if hit.matches.len() > 1 && !replace_all {
        let locations = crate::tools::fuzzy::format_match_locations(&content, &hit.matches, 5);
        return Ok(json!({
            "success": false,
            "error": format!(
                "Found {} matches for old_string in {}. Provide more context to make it unique, or use replace_all=true. Matches:\n{}",
                hit.matches.len(),
                path.display(),
                locations
            )
        }));
    }
    // Similarity-based strategies must never rewrite multiple approximate
    // regions under replace_all.
    if replace_all
        && hit.matches.len() > 1
        && crate::tools::fuzzy::SIMILARITY_STRATEGIES.contains(&hit.strategy)
    {
        return Ok(json!({
            "success": false,
            "error": format!(
                "Found {} approximate matches via the '{}' strategy; replace_all only applies to exact matches. Provide the precise text (whitespace included) so an exact/line-trimmed match can be made.",
                hit.matches.len(),
                hit.strategy
            )
        }));
    }
    // Escape-drift guard on any non-exact match.
    if hit.strategy != "exact" {
        if let Some(drift) =
            crate::tools::fuzzy::detect_escape_drift(&content, &hit.matches, old_string, new_string)
        {
            return Ok(json!({"success": false, "error": drift}));
        }
    }

    // Effective replacement: conditional \t/\r unescape, Unicode
    // preservation for strategy 7, re-indentation for all non-exact matches.
    let mut effective_new =
        crate::tools::fuzzy::maybe_unescape_new_string(new_string, &content, &hit.matches);
    if hit.strategy == "unicode_normalized" {
        effective_new = crate::tools::fuzzy::apply_unicode_preserving_replacement(
            &content,
            &hit.matches,
            old_string,
            &effective_new,
        );
    }
    // Apply replacements right-to-left to keep positions valid.
    let mut updated = content.clone();
    for &(start, end) in hit.matches.iter().rev() {
        let replacement = if hit.strategy == "exact" || hit.strategy == "unicode_normalized" {
            effective_new.clone()
        } else {
            crate::tools::fuzzy::reindent_replacement(
                &content[start..end],
                old_string,
                &effective_new,
            )
        };
        updated.replace_range(start..end, &replacement);
    }
    let matches = &hit.matches;
    let diff = make_diff(raw_path, &content, &updated);
    std::fs::write(&path, &updated)
        .map_err(|e| crate::error::AgentError::tool(format!("write failed: {}", e)))?;
    Ok(json!({
        "success": true,
        "path": path.display().to_string(),
        "replacements": matches.len(),
        "diff": diff,
    }))
}

/// V4A patch application — port of tools/patch_parser.py (Update/Add/Delete).
fn apply_v4a_patch(ctx: &Arc<ToolContext>, patch: &str) -> Result<serde_json::Value> {
    #[derive(Debug)]
    struct Op {
        kind: String,
        path: String,
        lines: Vec<String>,
    }
    let mut ops: Vec<Op> = Vec::new();
    let mut current: Option<Op> = None;
    let mut in_patch = false;
    for line in patch.lines() {
        if line.starts_with("*** Begin Patch") {
            in_patch = true;
            continue;
        }
        if line.starts_with("*** End Patch") {
            if let Some(op) = current.take() {
                ops.push(op);
            }
            break;
        }
        if !in_patch {
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Update File: ") {
            if let Some(op) = current.take() {
                ops.push(op);
            }
            current = Some(Op {
                kind: "update".into(),
                path: rest.trim().to_string(),
                lines: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("*** Add File: ") {
            if let Some(op) = current.take() {
                ops.push(op);
            }
            current = Some(Op {
                kind: "add".into(),
                path: rest.trim().to_string(),
                lines: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            if let Some(op) = current.take() {
                ops.push(op);
            }
            current = Some(Op {
                kind: "delete".into(),
                path: rest.trim().to_string(),
                lines: Vec::new(),
            });
        } else if line.starts_with("@@") {
            continue; // hunk header / context hint
        } else if let Some(ref mut op) = current {
            op.lines.push(line.to_string());
        }
    }
    if let Some(op) = current.take() {
        ops.push(op);
    }
    if ops.is_empty() {
        return Ok(
            json!({"success": false, "error": "No operations found in patch. Expected '*** Begin Patch' ... '*** End Patch' with '*** Update File:' sections."}),
        );
    }

    let mut applied = Vec::new();
    for op in &ops {
        let path = ctx.resolve_path(&op.path);
        match op.kind.as_str() {
            "add" => {
                if path.exists() {
                    return Ok(
                        json!({"success": false, "error": format!("Add File failed: {} already exists", op.path)}),
                    );
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let content = op
                    .lines
                    .iter()
                    .map(|l| l.strip_prefix('+').unwrap_or(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(&path, content)
                    .map_err(|e| crate::error::AgentError::tool(format!("add file: {}", e)))?;
                applied.push(format!("added {}", op.path));
            }
            "delete" => {
                if !path.exists() {
                    return Ok(
                        json!({"success": false, "error": format!("Delete File failed: {} not found", op.path)}),
                    );
                }
                std::fs::remove_file(&path)
                    .map_err(|e| crate::error::AgentError::tool(format!("delete file: {}", e)))?;
                applied.push(format!("deleted {}", op.path));
            }
            _ => {
                // update: hunks of context(-/+/ ) lines
                if !path.exists() {
                    return Ok(
                        json!({"success": false, "error": format!("Update File failed: {} not found", op.path)}),
                    );
                }
                let mut content = std::fs::read_to_string(&path)
                    .map_err(|e| crate::error::AgentError::tool(format!("read: {}", e)))?;
                let mut i = 0usize;
                let lines = &op.lines;
                while i < lines.len() {
                    // Collect one hunk: run of ' '/'-' lines followed by '+' insertions.
                    let mut old_block: Vec<String> = Vec::new();
                    let mut new_block: Vec<String> = Vec::new();
                    while i < lines.len() {
                        let line = &lines[i];
                        if let Some(rest) = line.strip_prefix(' ') {
                            old_block.push(rest.to_string());
                            new_block.push(rest.to_string());
                        } else if let Some(rest) = line.strip_prefix('-') {
                            old_block.push(rest.to_string());
                        } else if let Some(rest) = line.strip_prefix('+') {
                            new_block.push(rest.to_string());
                        } else if line.is_empty() {
                            // Blank context line (some models drop the space).
                            old_block.push(String::new());
                            new_block.push(String::new());
                        } else {
                            break;
                        }
                        i += 1;
                    }
                    if old_block.is_empty() && new_block.is_empty() {
                        i += 1;
                        continue;
                    }
                    let old_text = old_block.join("\n");
                    let new_text = new_block.join("\n");
                    let matches = crate::tools::fuzzy::fuzzy_find(&content, &old_text).matches;
                    if matches.is_empty() {
                        return Ok(json!({
                            "success": false,
                            "error": format!("Hunk failed in {}: context not found:\n{}", op.path, old_text)
                        }));
                    }
                    if matches.len() > 1 {
                        return Ok(json!({
                            "success": false,
                            "error": format!("Hunk ambiguous in {}: {} matches for context", op.path, matches.len())
                        }));
                    }
                    let (start, end) = matches[0];
                    content.replace_range(start..end, &new_text);
                }
                std::fs::write(&path, &content)
                    .map_err(|e| crate::error::AgentError::tool(format!("write: {}", e)))?;
                applied.push(format!("updated {}", op.path));
            }
        }
    }
    Ok(json!({
        "success": true,
        "applied": applied,
    }))
}

// ---------------------------------------------------------------------------
// search_files
// ---------------------------------------------------------------------------

fn search_files_tool() -> crate::tools::Tool {
    tool("search_files")
        .description(
            "Search file contents or find files by name. Use this instead of grep/rg/find/ls in \
             terminal.\n\nContent search (target='content'): Regex search inside files. Output \
             modes: full matches with line numbers, file paths only, or match counts.\n\nFile \
             search (target='files'): Find files by glob pattern (e.g., '*.py', '*config*'). \
             Also use this instead of ls — results sorted by modification time.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern for content search, or glob pattern (e.g., '*.py') for file search"},
                "target": {"type": "string", "enum": ["content", "files"], "description": "'content' searches inside file contents, 'files' searches for files by name", "default": "content"},
                "path": {"type": "string", "description": "Directory or file to search in (default: current working directory)", "default": "."},
                "file_glob": {"type": "string", "description": "Filter files by pattern in grep mode (e.g., '*.py' to only search Python files)"},
                "limit": {"type": "integer", "description": "Maximum number of results to return (default: 50)", "default": 50},
                "offset": {"type": "integer", "description": "Skip first N results for pagination (default: 0)", "default": 0},
                "output_mode": {"type": "string", "enum": ["content", "files_only", "count"], "description": "Output format for grep mode", "default": "content"},
                "context": {"type": "integer", "description": "Number of context lines before and after each match (grep mode only)", "default": 0}
            },
            "required": ["pattern"]
        }))
        .handler(|args, ctx| async move {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if pattern.is_empty() {
                return Ok(json!({"success": false, "error": "search_files: 'pattern' is required"}));
            }
            let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("content");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".").to_string();
            let file_glob = args.get("file_glob").and_then(|v| v.as_str()).map(String::from);
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_SEARCH_LIMIT as u64) as usize;
            let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let output_mode = args.get("output_mode").and_then(|v| v.as_str()).unwrap_or("content");
            let context_lines = args.get("context").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            search_files_impl(&ctx, &pattern, target, &path, file_glob, limit, offset, output_mode, context_lines)
        })
        .toolset("file")
        .emoji("🔍")
        .build()
        .expect("search_files builds")
}

#[allow(clippy::too_many_arguments)]
fn search_files_impl(
    ctx: &Arc<ToolContext>,
    pattern: &str,
    target: &str,
    raw_path: &str,
    file_glob: Option<String>,
    limit: usize,
    offset: usize,
    output_mode: &str,
    context_lines: usize,
) -> Result<serde_json::Value> {
    let root = ctx.resolve_path(raw_path);
    if !root.exists() {
        return Ok(
            json!({"success": false, "error": format!("Path not found: {}", root.display())}),
        );
    }

    if target == "files" {
        return file_search(&root, pattern, limit, offset);
    }

    let re = match regex::Regex::new(pattern) {
        Ok(re) => re,
        Err(e) => {
            return Ok(json!({"success": false, "error": format!("Invalid regex: {}", e)}));
        }
    };

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut total_matches = 0usize;

    if root.is_file() {
        collect_file_matches(
            &root,
            &root,
            &re,
            output_mode,
            context_lines,
            limit,
            &mut results,
            &mut total_matches,
        );
    } else {
        'walk: for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !(e.depth() > 0 && e.file_type().is_dir() && IGNORED_DIRS.contains(&name.as_ref()))
            })
        {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if let Some(ref glob_pat) = file_glob {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Ok(pat) = glob::Pattern::new(glob_pat) {
                    if !pat.matches(&name) {
                        continue;
                    }
                }
            }
            // Skip binaries quickly.
            if let Ok(head) = std::fs::read(path) {
                if head.iter().take(8192).any(|b| *b == 0) {
                    continue;
                }
            }
            collect_file_matches(
                &root,
                path,
                &re,
                output_mode,
                context_lines,
                limit,
                &mut results,
                &mut total_matches,
            );
            if results.len() >= limit + offset {
                break 'walk;
            }
        }
    }

    let paged: Vec<serde_json::Value> = results.into_iter().skip(offset).take(limit).collect();
    Ok(json!({
        "success": true,
        "results": paged,
        "total_matches": total_matches,
        "truncated": total_matches > offset + paged.len(),
    }))
}

fn collect_file_matches(
    root: &Path,
    path: &Path,
    re: &regex::Regex,
    output_mode: &str,
    context_lines: usize,
    limit: usize,
    results: &mut Vec<serde_json::Value>,
    total_matches: &mut usize,
) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    let lines: Vec<&str> = content.lines().collect();
    let mut file_match_count = 0usize;
    let mut matched_lines: Vec<usize> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            file_match_count += 1;
            matched_lines.push(idx);
            *total_matches += 1;
            if output_mode == "content" && results.len() < limit {
                let mut block = Vec::new();
                let start = idx.saturating_sub(context_lines);
                let end = (idx + context_lines + 1).min(lines.len());
                for i in start..end {
                    block.push(format!(
                        "{}{}|{}",
                        if i == idx { ">" } else { " " },
                        i + 1,
                        lines[i]
                    ));
                }
                results.push(json!({
                    "file": rel,
                    "line": idx + 1,
                    "match": block.join("\n"),
                }));
            }
        }
    }
    if file_match_count > 0 {
        if output_mode == "files_only" && results.len() < limit {
            results.push(json!({"file": rel, "matches": file_match_count}));
        } else if output_mode == "count" {
            results.push(json!({"file": rel, "count": file_match_count}));
        }
    }
    let _ = matched_lines;
}

fn file_search(
    root: &Path,
    pattern: &str,
    limit: usize,
    offset: usize,
) -> Result<serde_json::Value> {
    let pat = match glob::Pattern::new(pattern) {
        Ok(p) => p,
        Err(e) => return Ok(json!({"success": false, "error": format!("Invalid glob: {}", e)})),
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.depth() > 0 && e.file_type().is_dir() && IGNORED_DIRS.contains(&name.as_ref()))
        })
    {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if pat.matches(&name) || pat.matches(&entry.path().display().to_string()) {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            found.push((mtime, entry.path().to_path_buf()));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0)); // newest first (hermes convention)
    let total = found.len();
    let paged: Vec<String> = found
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, p)| p.strip_prefix(root).unwrap_or(&p).display().to_string())
        .collect();
    Ok(json!({
        "success": true,
        "files": paged,
        "total": total,
        "truncated": total > offset + paged.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_find_exact_and_ws() {
        let content = "fn main() {\n    println!(\"hi\");\n}\n";
        let hit = crate::tools::fuzzy::fuzzy_find(content, "println!(\"hi\");");
        assert_eq!(hit.strategy, "exact");
        assert_eq!(hit.matches.len(), 1);
        // whitespace-normalized match
        let hit = crate::tools::fuzzy::fuzzy_find(content, "println!(   \"hi\"   );");
        assert!(!hit.matches.is_empty());
    }

    #[test]
    fn test_v4a_patch_roundtrip() {
        let ctx = Arc::new(ToolContext::new().with_workdir(std::env::temp_dir()));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sample.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n alpha\n-beta\n+BETA\n gamma\n*** End Patch",
            file.display()
        );
        let result = apply_v4a_patch(&ctx, &patch).unwrap();
        assert_eq!(result["success"], json!(true));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[test]
    fn test_diff_output() {
        let diff = make_diff("f.txt", "a\nb\nc", "a\nB\nc");
        assert!(diff.contains("-b"));
        assert!(diff.contains("+B"));
    }

    #[test]
    fn test_read_file_rejects_binary_extension() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("logo.png");
        std::fs::write(&png, b"\x89PNG fake bytes").unwrap();
        let ctx = Arc::new(ToolContext::new().with_workdir(dir.path()));
        let result = read_file_impl(&ctx, "logo.png", 1, 100).unwrap();
        assert_eq!(result["success"], json!(false));
        let err = result["error"].as_str().unwrap();
        assert!(err.contains("Cannot read binary file"), "got: {err}");
        assert!(err.contains("vision_analyze"), "got: {err}");
    }
}

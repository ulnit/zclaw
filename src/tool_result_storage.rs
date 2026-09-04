//! Tool result persistence — preserves large outputs instead of
//! truncating them. Port of hermes `tools/tool_result_storage.py` +
//! `tools/budget_config.py` (v2026.8.3).
//!
//! Defense against context-window overflow operates at three levels:
//!
//! 1. **Per-tool output cap** — each tool pre-truncates its own output
//!    (already in place across the tool surface).
//! 2. **Per-result persistence** ([`maybe_persist_tool_result`]) — after
//!    a tool returns, if its serialized output exceeds the tool's
//!    threshold (default 100K chars, `read_file` pinned to infinity so a
//!    persist→read→persist loop cannot form), the full output is written
//!    to `<tmp>/ulnclaw-results/<tool_call_id>.txt` THROUGH the terminal
//!    backend (so the file is reachable on local, docker and ssh alike)
//!    and the in-context content is replaced with a `<persisted-output>`
//!    preview (1500 chars) + file path; the model reads the full output
//!    via `read_file`.
//! 3. **Per-turn aggregate budget** ([`enforce_turn_budget`]) — after all
//!    tool results of one assistant turn are collected, if the total
//!    exceeds the turn budget (200K chars), the largest non-persisted
//!    results are spilled until the aggregate fits.
//!
//! When the backend write fails, the result degrades to an inline
//! truncation notice (hermes fallback) — never a silent loss.

use crate::environments::TerminalBackend;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const PERSISTED_OUTPUT_TAG: &str = "<persisted-output>";
pub const PERSISTED_OUTPUT_CLOSING_TAG: &str = "</persisted-output>";

/// hermes `DEFAULT_RESULT_SIZE_CHARS`.
pub const DEFAULT_RESULT_SIZE_CHARS: usize = 100_000;
/// hermes `DEFAULT_TURN_BUDGET_CHARS`.
pub const DEFAULT_TURN_BUDGET_CHARS: usize = 200_000;
/// hermes `DEFAULT_PREVIEW_SIZE_CHARS`.
pub const DEFAULT_PREVIEW_SIZE_CHARS: usize = 1_500;

const UNSAFE_FILENAME_CHARS: &[char] = &[
    ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '*', '+', ',', '/', ':', ';', '<', '=', '>',
    '?', '@', '[', '\\', ']', '^', '`', '{', '|', '}', '~', '\n', '\t',
];
const MAX_RESULT_FILENAME_STEM: usize = 120;

/// Immutable budget constants (hermes `BudgetConfig`).
#[derive(Debug, Clone)]
pub struct BudgetConfig {
    pub default_result_size: usize,
    pub turn_budget: usize,
    pub preview_size: usize,
    pub tool_overrides: HashMap<String, usize>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            default_result_size: DEFAULT_RESULT_SIZE_CHARS,
            turn_budget: DEFAULT_TURN_BUDGET_CHARS,
            preview_size: DEFAULT_PREVIEW_SIZE_CHARS,
            tool_overrides: HashMap::new(),
        }
    }
}

impl BudgetConfig {
    /// Resolve the persistence threshold for a tool (hermes
    /// `resolve_threshold`): pinned > overrides > default. `None` means
    /// infinity — `read_file` is pinned so persist→read→persist loops
    /// cannot form (hermes `PINNED_THRESHOLDS`).
    pub fn resolve_threshold(&self, tool_name: &str) -> Option<usize> {
        if tool_name == "read_file" {
            return None;
        }
        if let Some(value) = self.tool_overrides.get(tool_name) {
            return Some(*value);
        }
        Some(self.default_result_size)
    }
}

/// Truncate at the last newline within `max_chars` (hermes
/// `generate_preview`). Returns `(preview, has_more)`.
pub fn generate_preview(content: &str, max_chars: usize) -> (String, bool) {
    if content.len() <= max_chars {
        return (content.to_string(), false);
    }
    let mut truncated = &content[..max_chars];
    if let Some(last_nl) = truncated.rfind('\n') {
        if last_nl > max_chars / 2 {
            truncated = &truncated[..last_nl + 1];
        }
    }
    (truncated.to_string(), true)
}

/// One safe filename for a tool-call id (hermes `_safe_result_filename`):
/// unsafe characters collapse to `_`; a changed or over-long stem gains a
/// 12-hex sha256 suffix so distinct ids never collide.
pub fn safe_result_filename(tool_call_id: &str) -> String {
    let raw = if tool_call_id.is_empty() {
        "tool_result"
    } else {
        tool_call_id
    };
    let mut stem: String = raw
        .chars()
        .map(|c| {
            if UNSAFE_FILENAME_CHARS.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let changed = stem != raw;
    stem = stem.trim_matches(&['.', '_', '-'][..]).to_string();
    let mut changed = changed || stem != raw.trim_matches(&['.', '_', '-'][..]);
    if stem.is_empty() {
        stem = "tool_result".to_string();
        changed = true;
    }
    if changed || stem.len() > MAX_RESULT_FILENAME_STEM {
        let digest = Sha256::digest(raw.as_bytes());
        let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        let short: String = stem.chars().take(MAX_RESULT_FILENAME_STEM).collect();
        let short = short.trim_matches(&['.', '_', '-'][..]);
        let short = if short.is_empty() {
            "tool_result"
        } else {
            short
        };
        stem = format!("{short}_{hex}");
    }
    format!("{stem}.txt")
}

/// Best temp-backed storage dir for the backend (hermes
/// `_resolve_storage_dir`): the LOCAL temp dir for the local backend,
/// `/tmp/ulnclaw-results` inside the container/remote otherwise. An
/// explicit override wins (test hook).
pub fn resolve_storage_dir(
    backend: Option<&TerminalBackend>,
    override_dir: Option<&Path>,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    match backend {
        None | Some(TerminalBackend::Local) => std::env::temp_dir().join("ulnclaw-results"),
        _ => PathBuf::from("/tmp/ulnclaw-results"),
    }
}

/// Build the `<persisted-output>` replacement block (hermes
/// `_build_persisted_message`).
pub fn build_persisted_message(
    preview: &str,
    has_more: bool,
    original_size: usize,
    file_path: &str,
) -> String {
    let size_kb = original_size as f64 / 1024.0;
    let size_str = if size_kb >= 1024.0 {
        format!("{:.1} MB", size_kb / 1024.0)
    } else {
        format!("{size_kb:.1} KB")
    };
    let mut msg = format!("{PERSISTED_OUTPUT_TAG}\n");
    msg.push_str(&format!(
        "This tool result was too large ({} characters, {size_str}).\n",
        original_size
    ));
    msg.push_str(&format!("Full output saved to: {file_path}\n"));
    msg.push_str(
        "Use the read_file tool with offset and limit to access specific sections of this output.\n\n",
    );
    msg.push_str(&format!("Preview (first {} chars):\n", preview.len()));
    msg.push_str(preview);
    if has_more {
        msg.push_str("\n...");
    }
    msg.push_str(&format!("\n{PERSISTED_OUTPUT_CLOSING_TAG}"));
    msg
}

/// Write `content` to `path` through the backend (hermes
/// `_write_to_sandbox`): local writes go straight to the filesystem;
/// docker/ssh pipe the content into `mkdir -p … && cat > path` over
/// stdin (the command string never carries the payload — argv length
/// caps would silently drop large results).
async fn write_to_sandbox(content: &str, path: &Path, backend: Option<&TerminalBackend>) -> bool {
    match backend {
        None | Some(TerminalBackend::Local) => {
            if let Some(parent) = path.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    return false;
                }
            }
            std::fs::write(path, content).is_ok()
        }
        Some(remote) => {
            let dir = path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "/tmp".to_string());
            let command = format!(
                "mkdir -p {} && cat > {}",
                crate::environments::shell_quote(&dir),
                crate::environments::shell_quote(&path.to_string_lossy()),
            );
            let wrapped = crate::environments::wrap_command(remote, &command, None);
            let mut cmd = if cfg!(windows) {
                let mut c = tokio::process::Command::new("cmd");
                c.args(["/C", &wrapped]);
                c
            } else {
                let mut c = tokio::process::Command::new("/bin/bash");
                c.args(["-c", &wrapped]);
                c
            };
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(_) => return false,
            };
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                if stdin.write_all(content.as_bytes()).await.is_err() {
                    return false;
                }
            }
            child.wait().await.map(|s| s.success()).unwrap_or(false)
        }
    }
}

/// Layer 2: persist an oversized result, return preview + path (hermes
/// `maybe_persist_tool_result`). Returns the original content when it is
/// small enough or the tool is pinned to infinity; falls back to an
/// inline truncation notice when no write succeeds.
pub async fn maybe_persist_tool_result(
    content: &str,
    tool_name: &str,
    tool_call_id: &str,
    backend: Option<&TerminalBackend>,
    config: &BudgetConfig,
    threshold_override: Option<usize>,
    storage_override: Option<&Path>,
) -> String {
    let threshold = threshold_override.or_else(|| config.resolve_threshold(tool_name));
    let Some(threshold) = threshold else {
        return content.to_string(); // pinned infinity
    };
    if content.len() <= threshold {
        return content.to_string();
    }
    let storage_dir = resolve_storage_dir(backend, storage_override);
    let remote_path = storage_dir.join(safe_result_filename(tool_call_id));
    let (preview, has_more) = generate_preview(content, config.preview_size);
    if write_to_sandbox(content, &remote_path, backend).await {
        tracing::info!(
            "Persisted large tool result: {tool_name} ({tool_call_id}, {} chars -> {})",
            content.len(),
            remote_path.display()
        );
        return build_persisted_message(
            &preview,
            has_more,
            content.len(),
            &remote_path.to_string_lossy(),
        );
    }
    tracing::info!(
        "Inline-truncating large tool result: {tool_name} ({} chars, no sandbox write)",
        content.len()
    );
    format!(
        "{preview}\n\n[Truncated: tool response was {} chars. Full output could not be saved to sandbox.]",
        content.len()
    )
}

/// Layer 3: enforce the aggregate turn budget across all tool results of
/// one assistant turn (hermes `enforce_turn_budget`). Spills the largest
/// non-persisted results first until the total fits. `contents[i]` pairs
/// with `tool_call_ids[i]`; already-persisted entries are skipped.
pub async fn enforce_turn_budget(
    contents: &mut [String],
    tool_call_ids: &[String],
    backend: Option<&TerminalBackend>,
    config: &BudgetConfig,
    storage_override: Option<&Path>,
) {
    let total: usize = contents.iter().map(|c| c.len()).sum();
    if total <= config.turn_budget {
        return;
    }
    let mut candidates: Vec<(usize, usize)> = contents
        .iter()
        .enumerate()
        .filter(|(_, content)| !content.contains(PERSISTED_OUTPUT_TAG))
        .map(|(i, content)| (i, content.len()))
        .collect();
    candidates.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    let mut total_size = total;
    for (idx, size) in candidates {
        if total_size <= config.turn_budget {
            break;
        }
        let tool_call_id = tool_call_ids
            .get(idx)
            .cloned()
            .unwrap_or_else(|| format!("budget_{idx}"));
        let replacement = maybe_persist_tool_result(
            &contents[idx].clone(),
            "__budget_enforcement__",
            &tool_call_id,
            backend,
            config,
            Some(0),
            storage_override,
        )
        .await;
        if replacement != contents[idx] {
            total_size -= size;
            total_size += replacement.len();
            contents[idx] = replacement;
            tracing::info!(
                "Budget enforcement: persisted tool result {tool_call_id} ({size} chars)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_truncates_at_last_newline() {
        let content = "line one\nline two\nline three";
        let (preview, has_more) = generate_preview(content, 20);
        assert!(has_more);
        assert!(preview.ends_with('\n'), "cut at newline: {preview:?}");
        assert!(preview.len() <= 20);
        let (whole, more) = generate_preview("short", 100);
        assert_eq!(whole, "short");
        assert!(!more);
    }

    #[test]
    fn filename_sanitization_and_digest_suffix() {
        assert_eq!(safe_result_filename("call_abc123"), "call_abc123.txt");
        let spaced = safe_result_filename("call with spaces/id");
        assert!(spaced.starts_with("call_with_spaces_id_"));
        assert!(spaced.ends_with(".txt"));
        // Distinct unsafe ids keep distinct digests.
        let a = safe_result_filename("a/b");
        let b = safe_result_filename("a:b");
        assert_ne!(a, b);
        assert_eq!(safe_result_filename(""), "tool_result.txt");
        let long = safe_result_filename(&"x".repeat(300));
        assert!(long.len() <= MAX_RESULT_FILENAME_STEM + 1 + 12 + 4);
    }

    #[test]
    fn threshold_resolution_pins_read_file() {
        let config = BudgetConfig::default();
        assert_eq!(config.resolve_threshold("read_file"), None);
        assert_eq!(
            config.resolve_threshold("terminal"),
            Some(DEFAULT_RESULT_SIZE_CHARS)
        );
        let mut overrides = BudgetConfig::default();
        overrides.tool_overrides.insert("web_extract".into(), 5_000);
        assert_eq!(overrides.resolve_threshold("web_extract"), Some(5_000));
    }

    #[tokio::test]
    async fn persist_oversized_result_writes_file_and_previews() {
        let dir = tempfile::tempdir().unwrap();
        let content = "x".repeat(2_000);
        let config = BudgetConfig {
            default_result_size: 1_000,
            preview_size: 100,
            ..BudgetConfig::default()
        };
        let out = maybe_persist_tool_result(
            &content,
            "terminal",
            "call_42",
            None,
            &config,
            None,
            Some(dir.path()),
        )
        .await;
        assert!(out.contains(PERSISTED_OUTPUT_TAG));
        assert!(out.contains("call_42.txt"));
        assert!(out.contains("Preview (first 100 chars)"));
        let saved = std::fs::read_to_string(dir.path().join("call_42.txt")).unwrap();
        assert_eq!(saved.len(), 2_000);
    }

    #[tokio::test]
    async fn small_and_pinned_results_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        let config = BudgetConfig {
            default_result_size: 1_000,
            ..BudgetConfig::default()
        };
        let small = maybe_persist_tool_result(
            "tiny",
            "terminal",
            "c1",
            None,
            &config,
            None,
            Some(dir.path()),
        )
        .await;
        assert_eq!(small, "tiny");
        // read_file is pinned to infinity: never persisted.
        let big = "y".repeat(5_000);
        let out = maybe_persist_tool_result(
            &big,
            "read_file",
            "c2",
            None,
            &config,
            None,
            Some(dir.path()),
        )
        .await;
        assert_eq!(out.len(), 5_000);
    }

    #[tokio::test]
    async fn turn_budget_spills_largest_first() {
        let dir = tempfile::tempdir().unwrap();
        let config = BudgetConfig {
            default_result_size: 100,
            turn_budget: 1_500,
            preview_size: 50,
            ..BudgetConfig::default()
        };
        let mut contents = vec!["a".repeat(1_200), "b".repeat(600), "c".repeat(200)];
        let ids: Vec<String> = vec!["t1".into(), "t2".into(), "t3".into()];
        enforce_turn_budget(&mut contents, &ids, None, &config, Some(dir.path())).await;
        assert!(
            contents[0].contains(PERSISTED_OUTPUT_TAG),
            "largest result must spill"
        );
        assert_eq!(contents[2], "c".repeat(200), "smallest stays inline");
        let total: usize = contents.iter().map(|c| c.len()).sum();
        assert!(total <= config.turn_budget, "aggregate under budget");
    }
}

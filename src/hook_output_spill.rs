//! Spill oversized hook-injected context to disk with a preview
//! placeholder. Port of hermes `tools/hook_output_spill.py` (v2026.8.3,
//! itself ported from openai/codex PR #21069).
//!
//! Shell hooks and plugin hooks can return `{"context": "..."}` which is
//! concatenated into the current turn's user message on EVERY subsequent
//! API call. A hook that emits a large blob (debug dump, full file,
//! runaway script) inflates every turn of the session and busts the
//! prompt-cache prefix. Spilling bounds it: the full content goes to
//! `<home>/hook_outputs/<session>/<uuid>.txt` and the prompt carries a
//! head/tail preview + the file path.
//!
//! Design invariants (hermes):
//! - behavior-preserving when disabled or under the cap — input returned
//!   unchanged;
//! - never fails the turn: any I/O error falls back to a bounded preview
//!   marked "spill write failed";
//! - spill files are grouped by session so a `/new` session doesn't grow
//!   one directory forever.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// hermes `DEFAULT_MAX_CHARS`.
pub const DEFAULT_MAX_CHARS: usize = 10_000;
/// hermes `DEFAULT_PREVIEW_HEAD`.
pub const DEFAULT_PREVIEW_HEAD: usize = 500;
/// hermes `DEFAULT_PREVIEW_TAIL`.
pub const DEFAULT_PREVIEW_TAIL: usize = 500;

fn default_true() -> bool {
    true
}
fn default_max_chars() -> usize {
    DEFAULT_MAX_CHARS
}
fn default_preview_head() -> usize {
    DEFAULT_PREVIEW_HEAD
}
fn default_preview_tail() -> usize {
    DEFAULT_PREVIEW_TAIL
}

/// `[hooks.output_spill]` config (hermes `hooks.output_spill`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpillConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
    #[serde(default = "default_preview_head")]
    pub preview_head: usize,
    #[serde(default = "default_preview_tail")]
    pub preview_tail: usize,
    /// Override spill directory (default `<home>/hook_outputs`).
    #[serde(default)]
    pub directory: Option<String>,
}

impl Default for SpillConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chars: DEFAULT_MAX_CHARS,
            preview_head: DEFAULT_PREVIEW_HEAD,
            preview_tail: DEFAULT_PREVIEW_TAIL,
            directory: None,
        }
    }
}

impl SpillConfig {
    /// True when every field matches the default (skip serialization).
    pub fn is_default(&self) -> bool {
        self.enabled
            && self.max_chars == DEFAULT_MAX_CHARS
            && self.preview_head == DEFAULT_PREVIEW_HEAD
            && self.preview_tail == DEFAULT_PREVIEW_TAIL
            && self.directory.is_none()
    }
}

fn sanitize_session_segment(session_id: &str) -> String {
    session_id
        .replace('/', "_")
        .replace('\\', "_")
        .replace("..", "_")
}

fn resolve_spill_dir(
    config: &SpillConfig,
    home: &std::path::Path,
    session_id: Option<&str>,
) -> PathBuf {
    let base = match &config.directory {
        Some(dir) => {
            let expanded = if let Some(rest) = dir.strip_prefix("~/") {
                match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
                    Some(home_dir) => PathBuf::from(home_dir).join(rest),
                    None => PathBuf::from(dir),
                }
            } else {
                PathBuf::from(dir)
            };
            expanded
        }
        None => home.join("hook_outputs"),
    };
    let segment = sanitize_session_segment(session_id.unwrap_or("no-session"));
    base.join(segment)
}

/// Head/tail preview that stays inside the prompt (char-based, UTF-8
/// safe — hermes slices code points).
fn build_preview(
    text: &str,
    head: usize,
    tail: usize,
    saved_path: Option<&str>,
    source: &str,
) -> String {
    let total = text.chars().count();
    let head_chunk: String = text.chars().take(head).collect();
    let tail_chunk: String = if tail > 0 && total > head {
        text.chars()
            .rev()
            .take(tail)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else {
        String::new()
    };
    let status = match saved_path {
        Some(path) => format!("full content saved to {path}]"),
        None => "unavailable — spill write failed]".to_string(),
    };
    let mut parts = vec![format!(
        "[{source} output truncated — {total} chars; {status}"
    )];
    if !head_chunk.is_empty() {
        parts.push("--- head ---".to_string());
        parts.push(head_chunk);
    }
    if !tail_chunk.is_empty() {
        parts.push("--- tail ---".to_string());
        parts.push(tail_chunk);
    }
    parts.join("\n")
}

/// Spill `text` to disk when it exceeds the configured cap (hermes
/// `spill_if_oversized`). Returns the input unchanged when disabled,
/// empty, or under the cap; otherwise a bounded preview pointing at the
/// spill file (or marked failed when the write did not succeed).
pub fn spill_if_oversized(
    text: &str,
    session_id: Option<&str>,
    source: &str,
    config: &SpillConfig,
    home: &std::path::Path,
) -> String {
    if text.is_empty() || !config.enabled {
        return text.to_string();
    }
    let max_chars = config.max_chars.max(1);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let spill_dir = resolve_spill_dir(config, home, session_id);
    let mut saved_path: Option<String> = None;
    match std::fs::create_dir_all(&spill_dir) {
        Ok(()) => {
            let filename = format!("{}.txt", uuid::Uuid::new_v4().simple());
            let spill_path = spill_dir.join(&filename);
            let body = if text.ends_with('\n') {
                text.to_string()
            } else {
                format!("{text}\n")
            };
            match std::fs::write(&spill_path, body) {
                Ok(()) => saved_path = Some(spill_path.to_string_lossy().to_string()),
                Err(e) => tracing::warn!("hook output spill failed: {e}"),
            }
        }
        Err(e) => tracing::warn!("hook output spill failed: {e}"),
    }
    build_preview(
        text,
        config.preview_head,
        config.preview_tail,
        saved_path.as_deref(),
        source,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cap_and_disabled_pass_through() {
        let dir = tempfile::tempdir().unwrap();
        let config = SpillConfig::default();
        assert_eq!(
            spill_if_oversized("small", Some("s1"), "hook", &config, dir.path()),
            "small"
        );
        let mut disabled = SpillConfig::default();
        disabled.enabled = false;
        disabled.max_chars = 5;
        let big = "x".repeat(50);
        assert_eq!(
            spill_if_oversized(&big, Some("s1"), "hook", &disabled, dir.path()),
            big
        );
    }

    #[test]
    fn oversized_spills_to_session_dir_with_preview() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = SpillConfig::default();
        config.max_chars = 100;
        config.preview_head = 10;
        config.preview_tail = 10;
        let content = format!("{}MIDDLE{}", "a".repeat(200), "z".repeat(200));
        let out = spill_if_oversized(
            &content,
            Some("sess/one"),
            "shell hook",
            &config,
            dir.path(),
        );
        assert!(out.contains("[shell hook output truncated"));
        assert!(out.contains("full content saved to"));
        assert!(out.contains("--- head ---"));
        assert!(out.contains("aaaaaaaaaa"));
        assert!(out.contains("--- tail ---"));
        assert!(out.contains("zzzzzzzzzz"));
        // Session segment sanitized; exactly one spill file with the full
        // content (+ trailing newline).
        let session_dir = dir.path().join("hook_outputs").join("sess_one");
        let files: Vec<_> = std::fs::read_dir(&session_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let saved = std::fs::read_to_string(files[0].path()).unwrap();
        assert_eq!(saved, format!("{content}\n"));
    }

    #[test]
    fn write_failure_degrades_to_bounded_preview() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = SpillConfig::default();
        config.max_chars = 10;
        config.preview_head = 5;
        config.preview_tail = 5;
        // A directory override under a regular file cannot be created.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "file").unwrap();
        config.directory = Some(blocker.join("sub").to_string_lossy().to_string());
        let out = spill_if_oversized(&"y".repeat(100_000), None, "hook", &config, dir.path());
        assert!(out.contains("spill write failed"), "{out}");
        assert!(out.contains("--- head ---"));
        assert!(
            out.len() < 5_000,
            "preview must be bounded, got {}",
            out.len()
        );
    }

    #[test]
    fn preview_is_utf8_safe() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = SpillConfig::default();
        config.max_chars = 10;
        config.preview_head = 4;
        config.preview_tail = 4;
        let content: String = "汉字".repeat(25); // 50 code points
        let out = spill_if_oversized(&content, Some("s"), "hook", &config, dir.path());
        assert!(out.contains("汉"), "head chunk keeps whole code points");
        assert!(out.contains("字"), "tail chunk keeps whole code points");
        assert!(out.contains("50 chars"));
    }

    #[test]
    fn session_segment_sanitization() {
        assert_eq!(sanitize_session_segment("a/b\\c..d"), "a_b_c_d");
        assert_eq!(sanitize_session_segment("plain-123"), "plain-123");
    }
}

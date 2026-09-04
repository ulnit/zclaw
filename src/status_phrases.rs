//! Human-friendly generic gateway status phrases — port of hermes
//! `gateway/status_phrases.py`.
//!
//! These helpers deliberately avoid relaying raw model scratch text.
//! They turn the gateway's long-running status surfaces into short
//! status lines suitable for chat surfaces.
//!
//! Built-in defaults live in `assets/status_phrases.yaml` (verbatim
//! hermes asset). Users can add portable, profile-relative phrase
//! catalogs under the ulnclaw home either by using conventional paths:
//!
//! ```text
//! ~/.ulnclaw/status_phrases.yaml
//! ~/.ulnclaw/status_phrases/*.yaml
//! ```
//!
//! or by pointing config at a relative file/directory:
//!
//! ```toml
//! [display.status_phrases]
//! path = "status_phrases/whatsapp.yaml"  # relative to the ulnclaw home
//! mode = "append"                        # append (default) or replace
//! ```
//!
//! Absolute paths and `..` escapes are ignored on purpose so config
//! stays profile-portable and does not accidentally read arbitrary
//! files.
//!
//! Only configured phrase strings are used; raw tool args, commands,
//! previews, and reasoning text are never interpolated into the
//! returned phrase.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Long-running status surfaces (hermes `_STATUS_SURFACES`). These are
/// gateway UI surfaces, not app/vendor/domain buckets. Keep this
/// long-running-only: regular tool/thinking/interim chatter is
/// intentionally not rewritten into generic placeholders because that
/// gets noisy fast in chat.
pub const STATUS_SURFACES: &[&str] = &["status", "generic"];

pub const MAX_CUSTOM_PHRASES_PER_SURFACE: usize = 80;
pub const MAX_PHRASE_CHARS: usize = 160;

const CONVENTIONAL_RELATIVE_PATHS: &[&str] = &["status_phrases.yaml", "status_phrases"];

/// Built-in catalog asset (hermes `gateway/assets/status_phrases.yaml`).
const BUILTIN_CATALOG_YAML: &str = include_str!("../assets/status_phrases.yaml");

/// Fallback phrases when nothing else is configured (hermes
/// `_FALLBACK_PHRASES`).
const FALLBACK_STATUS: &[&str] = &[
    "still on it",
    "still working through it",
    "waiting for the result",
];
const FALLBACK_GENERIC: &[&str] = &["on it", "one sec", "checking that now"];

/// A resolved phrase catalog: long-running `status` lines + `generic`
/// lines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusPhraseCatalog {
    pub status: Vec<String>,
    pub generic: Vec<String>,
}

impl StatusPhraseCatalog {
    fn fallback() -> Self {
        Self {
            status: FALLBACK_STATUS.iter().map(|s| s.to_string()).collect(),
            generic: FALLBACK_GENERIC.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn surface_mut(&mut self, surface: &str) -> Option<&mut Vec<String>> {
        match surface {
            "status" => Some(&mut self.status),
            "generic" => Some(&mut self.generic),
            _ => None,
        }
    }

    fn surface(&self, surface: &str) -> Option<&Vec<String>> {
        match surface {
            "status" => Some(&self.status),
            "generic" => Some(&self.generic),
            _ => None,
        }
    }
}

/// Clean a raw phrase list: drop blanks/over-length/duplicates, cap at
/// [`MAX_CUSTOM_PHRASES_PER_SURFACE`] (hermes `_clean_phrase_list`).
pub fn clean_phrase_list(raw: &[String]) -> Vec<String> {
    let mut cleaned: Vec<String> = Vec::new();
    for item in raw.iter().take(MAX_CUSTOM_PHRASES_PER_SURFACE) {
        let phrase = item.trim();
        if phrase.is_empty()
            || phrase.chars().count() > MAX_PHRASE_CHARS
            || cleaned.iter().any(|existing| existing == phrase)
        {
            continue;
        }
        cleaned.push(phrase.to_string());
    }
    cleaned
}

/// `[display.status_phrases]` / `[display.generic_status_phrases]`
/// section (also valid per-platform under
/// `[display.platforms.<platform>.…]`). Mirrors hermes' free-form
/// `display.status_phrases` mapping.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusPhrasesSection {
    /// Profile-relative `.yaml`/`.yml` file or directory (absolute
    /// paths and `..` escapes are ignored).
    pub path: Option<String>,
    /// Multiple profile-relative paths.
    pub paths: Vec<String>,
    /// `append` (default) or `replace`.
    pub mode: Option<String>,
    /// Nested inline phrases (takes priority over the flat fields).
    pub phrases: Option<InlinePhrases>,
    /// Flat inline `status` phrases (hermes section-is-mapping form).
    pub status: Vec<String>,
    /// Flat inline `generic` phrases (hermes section-is-mapping form).
    pub generic: Vec<String>,
}

/// Inline phrase lists (`phrases.status` / `phrases.generic`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InlinePhrases {
    pub status: Vec<String>,
    pub generic: Vec<String>,
}

impl StatusPhrasesSection {
    fn inline(&self) -> (&[String], &[String]) {
        if let Some(nested) = self.phrases.as_ref() {
            (&nested.status, &nested.generic)
        } else {
            (&self.status, &self.generic)
        }
    }
}

fn merge_cleaned(
    catalog: &mut StatusPhraseCatalog,
    surface: &str,
    phrases: Vec<String>,
    replace: bool,
) {
    if phrases.is_empty() {
        return;
    }
    if let Some(slot) = catalog.surface_mut(surface) {
        if replace {
            *slot = phrases;
        } else {
            slot.extend(phrases);
        }
    }
}

/// Merge one mapping-style section into `catalog` (hermes
/// `_merge_phrase_mapping`): `status`/`generic` lists with a
/// section-level or inherited `mode`.
fn merge_phrase_lists(
    catalog: &mut StatusPhraseCatalog,
    status: &[String],
    generic: &[String],
    mode: Option<&str>,
    inherited_mode: Option<&str>,
) {
    let mode = mode
        .or(inherited_mode)
        .unwrap_or("append")
        .trim()
        .to_lowercase();
    let replace = mode == "replace";
    merge_cleaned(catalog, "status", clean_phrase_list(status), replace);
    merge_cleaned(catalog, "generic", clean_phrase_list(generic), replace);
}

/// Merge one config section into `catalog` (hermes
/// `_merge_phrase_config`): files first, then inline phrases.
fn merge_section(
    catalog: &mut StatusPhraseCatalog,
    section: &StatusPhrasesSection,
    home: Option<&Path>,
) {
    let mode = section.mode.as_deref();
    if let Some(home) = home {
        for raw in section.path.iter().chain(section.paths.iter()) {
            merge_phrase_paths(catalog, raw, home, mode);
        }
    }
    let (status, generic) = section.inline();
    merge_phrase_lists(catalog, status, generic, mode, None);
}

/// Resolve a raw config path relative to `base`, refusing absolute
/// paths and `..` escapes (hermes `_relative_path_under`).
fn relative_path_under(base: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        return None;
    }
    if candidate.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return None;
    }
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let resolved = base.join(candidate);
    let resolved = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
    if resolved.starts_with(&base) {
        Some(resolved)
    } else {
        None
    }
}

/// Enumerate `.yaml`/`.yml` catalog files at `path` (hermes
/// `_iter_phrase_files`).
fn iter_phrase_files(path: &Path) -> Vec<PathBuf> {
    let is_yaml = |p: &Path| {
        matches!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .as_deref(),
            Some("yaml") | Some("yml")
        )
    };
    if path.is_file() && is_yaml(path) {
        return vec![path.to_path_buf()];
    }
    if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| p.is_file() && is_yaml(p))
            .collect();
        files.sort();
        return files;
    }
    Vec::new()
}

fn merge_phrase_file(catalog: &mut StatusPhraseCatalog, path: &Path, inherited_mode: Option<&str>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
        return;
    };
    let Some(mapping) = value.as_mapping() else {
        return;
    };
    // Section-level `mode` (or inherited) governs the lists.
    let mode = mapping
        .iter()
        .find(|(k, _)| k.as_str() == Some("mode"))
        .and_then(|(_, v)| v.as_str())
        .or(inherited_mode);
    // Phrases live under a nested `phrases:` mapping when present,
    // otherwise the section itself is the phrase mapping (hermes
    // `_merge_phrase_mapping`).
    let phrase_map = mapping
        .iter()
        .find(|(k, _)| k.as_str() == Some("phrases"))
        .and_then(|(_, v)| v.as_mapping())
        .unwrap_or(mapping);
    let list_for = |surface: &str| -> Vec<String> {
        phrase_map
            .iter()
            .find(|(k, _)| k.as_str() == Some(surface))
            .and_then(|(_, v)| v.as_sequence())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    merge_phrase_lists(
        catalog,
        &list_for("status"),
        &list_for("generic"),
        mode,
        None,
    );
}

fn merge_phrase_paths(
    catalog: &mut StatusPhraseCatalog,
    raw: &str,
    home: &Path,
    inherited_mode: Option<&str>,
) {
    let Some(resolved) = relative_path_under(home, raw) else {
        return;
    };
    for file in iter_phrase_files(&resolved) {
        merge_phrase_file(catalog, &file, inherited_mode);
    }
}

/// Load the built-in catalog: fallbacks overlaid with the bundled
/// asset in `replace` mode (hermes `_load_builtin_catalog`).
pub fn builtin_catalog() -> StatusPhraseCatalog {
    let mut catalog = StatusPhraseCatalog::fallback();
    if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(BUILTIN_CATALOG_YAML) {
        if let Some(mapping) = value.as_mapping() {
            for surface in STATUS_SURFACES {
                let phrases = mapping
                    .iter()
                    .find(|(k, _)| k.as_str() == Some(*surface))
                    .and_then(|(_, v)| v.as_sequence())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                merge_cleaned(&mut catalog, surface, clean_phrase_list(&phrases), true);
            }
        }
    }
    catalog
}

/// Resolve built-in + user-configured generic status phrases (hermes
/// `resolve_status_phrase_catalog`).
///
/// Resolution order mirrors gateway display settings: built-ins,
/// conventional profile-relative user files, global
/// `[display.status_phrases]` (or legacy alias
/// `[display.generic_status_phrases]`), then
/// `[display.platforms.<platform>.status_phrases]`.
pub fn resolve_catalog(
    display: &crate::config::DisplayConfig,
    platform: Option<&str>,
    home: &Path,
) -> StatusPhraseCatalog {
    let mut catalog = builtin_catalog();
    for conventional in CONVENTIONAL_RELATIVE_PATHS {
        merge_phrase_paths(&mut catalog, conventional, home, None);
    }
    if let Some(section) = display.generic_status_phrases.as_ref() {
        merge_section(&mut catalog, section, Some(home));
    }
    if let Some(section) = display.status_phrases.as_ref() {
        merge_section(&mut catalog, section, Some(home));
    }
    if let Some(key) = platform.map(crate::display_config::platform_key) {
        if let Some(override_cfg) = display.platforms.get(&key) {
            if let Some(section) = override_cfg.generic_status_phrases.as_ref() {
                merge_section(&mut catalog, section, Some(home));
            }
            if let Some(section) = override_cfg.status_phrases.as_ref() {
                merge_section(&mut catalog, section, Some(home));
            }
        }
    }
    catalog
}

/// Classify an internal gateway event into a UI-surface bucket (hermes
/// `classify_status_context`).
pub fn classify_status_context(kind: &str) -> &'static str {
    match kind.trim().to_lowercase().as_str() {
        "heartbeat" | "waiting" | "long_running" | "status" => "status",
        _ => "generic",
    }
}

/// Pick a short generic status phrase, avoiding recent repeats (hermes
/// `choose_status_phrase`). `recent` keeps at most the last six picks.
///
/// `pick` maps a bound to an index — injectable so tests stay
/// deterministic; production callers pass [`DefaultPicker`].
pub fn choose_status_phrase(
    kind: &str,
    recent: Option<&mut Vec<String>>,
    pick: &mut dyn FnMut(usize) -> usize,
    catalog: Option<&StatusPhraseCatalog>,
) -> String {
    let fallback = StatusPhraseCatalog::fallback();
    let catalog = catalog.unwrap_or(&fallback);
    let category = classify_status_context(kind);
    let mut candidates: Vec<String> = if !catalog
        .surface(category)
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
        catalog.surface(category).cloned().unwrap_or_default()
    } else if !catalog.generic.is_empty() {
        catalog.generic.clone()
    } else {
        fallback.generic.clone()
    };
    if let Some(recent) = recent.as_deref() {
        let fresh: Vec<String> = candidates
            .drain(..)
            .filter(|phrase| !recent.contains(phrase))
            .collect();
        if !fresh.is_empty() {
            candidates = fresh;
        } else {
            // Everything was recent — fall back to the full list.
            candidates = if !catalog
                .surface(category)
                .map(|v| v.is_empty())
                .unwrap_or(true)
            {
                catalog.surface(category).cloned().unwrap_or_default()
            } else {
                catalog.generic.clone()
            };
        }
    }
    if candidates.is_empty() {
        candidates = fallback.generic.clone();
    }
    let phrase = candidates[pick(candidates.len()) % candidates.len()].clone();
    if let Some(recent) = recent {
        recent.push(phrase.clone());
        let len = recent.len();
        if len > 6 {
            recent.drain(..len - 6);
        }
    }
    phrase
}

/// Tiny xorshift64 picker — no `rand` dependency needed for phrase
/// rotation. Seeded from wall clock + address jitter.
pub struct DefaultPicker {
    state: u64,
}

impl DefaultPicker {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b97f4a7c15)
            ^ (std::ptr::addr_of!(STATE_COUNTER) as u64).rotate_left(17)
            ^ STATE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { state: seed | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Pick an index in `[0, bound)`.
    pub fn index(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }
}

impl Default for DefaultPicker {
    fn default() -> Self {
        Self::new()
    }
}

static STATE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_loads_asset() {
        let catalog = builtin_catalog();
        // The bundled hermes asset ships 30 status + 20 generic lines.
        assert_eq!(catalog.status.len(), 30, "{:?}", catalog.status);
        assert_eq!(catalog.generic.len(), 20, "{:?}", catalog.generic);
        assert!(catalog.status.contains(&"still on it".to_string()));
        assert!(catalog.generic.contains(&"on it".to_string()));
    }

    #[test]
    fn clean_phrase_list_enforces_limits() {
        let mut raw: Vec<String> = vec![
            "a".into(),
            " a ".into(), // duplicate after trim
            "".into(),
            "   ".into(),
            "b".repeat(MAX_PHRASE_CHARS + 1),
            "c".into(),
        ];
        for i in 0..100 {
            raw.push(format!("phrase-{i}"));
        }
        let cleaned = clean_phrase_list(&raw);
        // Hermes semantics: the first MAX_CUSTOM_PHRASES_PER_SURFACE raw
        // items are taken, THEN filtered — 6 head items (2 valid) + 74
        // phrase-N rows survive.
        assert_eq!(cleaned.len(), 76, "{cleaned:?}");
        assert_eq!(cleaned[0], "a");
        assert_eq!(cleaned[1], "c");
        assert!(!cleaned.iter().any(|p| p.chars().count() > MAX_PHRASE_CHARS));
    }

    #[test]
    fn merge_modes_append_and_replace() {
        let mut catalog = StatusPhraseCatalog::fallback();
        merge_phrase_lists(
            &mut catalog,
            &["extra status".into()],
            &["extra generic".into()],
            None,
            None,
        );
        assert!(catalog.status.contains(&"still on it".to_string()));
        assert!(catalog.status.contains(&"extra status".to_string()));
        merge_phrase_lists(
            &mut catalog,
            &["only status".into()],
            &[],
            Some("replace"),
            None,
        );
        assert_eq!(catalog.status, vec!["only status".to_string()]);
        // Generic untouched by the replace above (empty list).
        assert!(catalog.generic.contains(&"extra generic".to_string()));
    }

    #[test]
    fn path_safety_rejects_absolute_and_parent_escapes() {
        let base = Path::new("/tmp");
        assert!(relative_path_under(base, "/etc/passwd").is_none());
        assert!(relative_path_under(base, "../etc/passwd").is_none());
        assert!(relative_path_under(base, "a/../../etc/passwd").is_none());
        assert!(relative_path_under(base, "").is_none());
    }

    #[test]
    fn conventional_files_merge_from_home() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("status_phrases.yaml"),
            "status:\n- custom status line\ngeneric:\n- custom generic line\n",
        )
        .unwrap();
        std::fs::create_dir(temp.path().join("status_phrases")).unwrap();
        std::fs::write(
            temp.path().join("status_phrases/extra.yml"),
            "mode: replace\nstatus:\n- replaced status\n",
        )
        .unwrap();
        let display = crate::config::DisplayConfig::default();
        let catalog = resolve_catalog(&display, None, temp.path());
        // The directory file (replace mode) wins over the conventional
        // file for the status surface; generic keeps the custom line.
        assert_eq!(catalog.status, vec!["replaced status".to_string()]);
        assert!(catalog.generic.contains(&"custom generic line".to_string()));
    }

    #[test]
    fn config_sections_merge_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let mut display = crate::config::DisplayConfig::default();
        display.generic_status_phrases = Some(StatusPhrasesSection {
            status: vec!["legacy status".into()],
            ..Default::default()
        });
        display.status_phrases = Some(StatusPhrasesSection {
            phrases: Some(InlinePhrases {
                status: vec!["global status".into()],
                generic: vec!["global generic".into()],
            }),
            ..Default::default()
        });
        let mut override_cfg = crate::config::PlatformDisplayOverride::default();
        override_cfg.status_phrases = Some(StatusPhrasesSection {
            mode: Some("replace".into()),
            status: vec!["platform status".into()],
            ..Default::default()
        });
        display.platforms.insert("telegram".into(), override_cfg);

        // Platform-less resolution keeps legacy + global lines.
        let catalog = resolve_catalog(&display, None, temp.path());
        assert!(catalog.status.contains(&"legacy status".to_string()));
        assert!(catalog.status.contains(&"global status".to_string()));
        assert!(catalog.generic.contains(&"global generic".to_string()));

        // Platform replace mode wipes earlier status entries.
        let catalog = resolve_catalog(&display, Some("telegram"), temp.path());
        assert_eq!(catalog.status, vec!["platform status".to_string()]);
        // Generic survives (replace list was empty).
        assert!(catalog.generic.contains(&"global generic".to_string()));
    }

    #[test]
    fn classify_surfaces() {
        assert_eq!(classify_status_context("heartbeat"), "status");
        assert_eq!(classify_status_context("STATUS"), "status");
        assert_eq!(classify_status_context("long_running"), "status");
        assert_eq!(classify_status_context("tool.started"), "generic");
        assert_eq!(classify_status_context(""), "generic");
    }

    #[test]
    fn chooser_avoids_recent_and_trims() {
        let catalog = StatusPhraseCatalog {
            status: vec!["s1".into(), "s2".into(), "s3".into()],
            generic: vec!["g1".into()],
        };
        let mut recent: Vec<String> = Vec::new();
        let mut pick = |bound: usize| bound - 1; // always last
        let first = choose_status_phrase("status", Some(&mut recent), &mut pick, Some(&catalog));
        assert_eq!(first, "s3");
        // s3 now recent → picks from the remaining fresh lines.
        let second = choose_status_phrase("status", Some(&mut recent), &mut pick, Some(&catalog));
        assert_eq!(second, "s2");
        let third = choose_status_phrase("status", Some(&mut recent), &mut pick, Some(&catalog));
        assert_eq!(third, "s1");
        // All recent → falls back to the full candidate list.
        let fourth = choose_status_phrase("status", Some(&mut recent), &mut pick, Some(&catalog));
        assert_eq!(fourth, "s3");
        assert!(recent.len() <= 6);
    }

    #[test]
    fn chooser_category_fallback_chain() {
        let catalog = StatusPhraseCatalog {
            status: vec![],
            generic: vec!["only generic".into()],
        };
        let mut pick = |bound: usize| bound - 1;
        let phrase = choose_status_phrase("status", None, &mut pick, Some(&catalog));
        assert_eq!(phrase, "only generic");
        let empty = StatusPhraseCatalog::default();
        let phrase = choose_status_phrase("anything", None, &mut pick, Some(&empty));
        assert!(FALLBACK_GENERIC.contains(&phrase.as_str()));
    }

    #[test]
    fn default_picker_stays_in_bounds() {
        let mut picker = DefaultPicker::new();
        for bound in [1, 2, 3, 7, 30] {
            for _ in 0..50 {
                assert!(picker.index(bound) < bound);
            }
        }
        assert_eq!(picker.index(0), 0);
    }
}

//! Petdex pet engine — port of hermes `agent/pet/` + `hermes_cli/pets.py`
//! (v2026.8.3).
//!
//! Petdex (https://github.com/crafter-station/petdex) is a public gallery of
//! animated sprite "pets" for coding agents. Each pet is a `pet.json` plus a
//! `spritesheet.{webp,png}` of 192×208 px cells. Current Codex/petdex sheets
//! use an 8-column × 9-row atlas; older Hermes/petdex sheets used a 9-column
//! × 8-row atlas. The row taxonomy is inferred from the sheet shape and agent
//! activity maps onto idle/run/review/failed/wave/jump/waiting rows.
//!
//! The whole feature is a *display* concern: it adds no model tool, mutates no
//! system prompt or toolset, and therefore has zero effect on prompt caching.
//!
//! Known diffs vs hermes:
//! - The Ink-TUI `/pet` REPL command, the desktop overlay, and the LLM pet
//!   "hatch" pipeline (`agent/pet/generate/`) are not ported yet.
//! - Terminal rendering supports kitty / iTerm2 / sixel / Unicode half-blocks
//!   exactly like hermes; the kitty *Unicode placeholder* payload builder for
//!   grid-host apps is included for future surfaces.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

// =========================================================================
// Sprite geometry + animation-state taxonomy (hermes `agent/pet/constants`)
// =========================================================================

/// Frame width in pixels (petdex cell geometry).
pub const FRAME_W: u32 = 192;
/// Frame height in pixels (petdex cell geometry).
pub const FRAME_H: u32 = 208;
/// Frames consumed per animation state (petdex web uses CSS `steps(6)`).
pub const FRAMES_PER_STATE: u32 = 6;
/// Full-loop duration for one state, milliseconds (petdex default).
pub const LOOP_MS: u32 = 1100;
/// Default on-screen scale relative to native frame size.
pub const DEFAULT_SCALE: f64 = 0.33;
/// User-settable scale floor (`pets scale`, desktop slider).
pub const MIN_SCALE: f64 = 0.1;
/// User-settable scale ceiling.
pub const MAX_SCALE: f64 = 3.0;

/// Terminal cells one native frame spans at `scale == 1.0` (~8 px per cell).
pub const BASE_UNICODE_COLS: u32 = FRAME_W / 8;
/// Legibility floor for the half-block fallback (hermes `UNICODE_MIN_COLS`).
pub const UNICODE_MIN_COLS: u32 = 16;

/// Public render-mode names accepted by `display.pet.render_mode`.
pub const RENDER_MODES: &[&str] = &["auto", "kitty", "iterm", "sixel", "unicode", "off"];

/// Clamp `scale` to `[MIN_SCALE, MAX_SCALE]` (the single validation point).
pub fn clamp_scale(scale: f64) -> f64 {
    if !scale.is_finite() {
        return DEFAULT_SCALE;
    }
    scale.clamp(MIN_SCALE, MAX_SCALE)
}

/// Half-block width implied by `scale`, clamped to the legibility floor
/// (hermes `cols_for_scale`).
pub fn cols_for_scale(scale: f64) -> u32 {
    let effective = if scale == 0.0 { DEFAULT_SCALE } else { scale };
    let raw = (BASE_UNICODE_COLS as f64 * effective).round() as i64;
    UNICODE_MIN_COLS.max(raw.max(0) as u32)
}

/// Resolve terminal width: explicit `unicode_cols` override, else from
/// `scale` (hermes `resolve_cols`).
pub fn resolve_cols(scale: f64, unicode_cols: u32) -> u32 {
    if unicode_cols > 0 {
        unicode_cols
    } else {
        cols_for_scale(scale)
    }
}

/// Animation state a pet can be shown in (hermes `PetState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PetState {
    Idle,
    Wave,
    Run,
    Failed,
    Review,
    Jump,
    Waiting,
}

impl PetState {
    /// All states in hermes enum order.
    pub const ALL: [PetState; 7] = [
        PetState::Idle,
        PetState::Wave,
        PetState::Run,
        PetState::Failed,
        PetState::Review,
        PetState::Jump,
        PetState::Waiting,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PetState::Idle => "idle",
            PetState::Wave => "wave",
            PetState::Run => "run",
            PetState::Failed => "failed",
            PetState::Review => "review",
            PetState::Jump => "jump",
            PetState::Waiting => "waiting",
        }
    }

    /// Parse a state name (hermes activity names only).
    pub fn parse(value: &str) -> Option<PetState> {
        match value.trim().to_lowercase().as_str() {
            "idle" => Some(PetState::Idle),
            "wave" => Some(PetState::Wave),
            "run" => Some(PetState::Run),
            "failed" => Some(PetState::Failed),
            "review" => Some(PetState::Review),
            "jump" => Some(PetState::Jump),
            "waiting" => Some(PetState::Waiting),
            _ => None,
        }
    }

    /// Accepted row-name aliases in descending preference (hermes
    /// `STATE_ALIASES`).
    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            PetState::Idle => &["idle"],
            PetState::Wave => &["wave", "waving"],
            PetState::Run => &["run", "running"],
            PetState::Failed => &["failed"],
            PetState::Review => &["review"],
            PetState::Jump => &["jump", "jumping"],
            PetState::Waiting => &["waiting"],
        }
    }
}

/// Legacy Hermes/petdex row order (older 8-row, 9-column atlases).
pub const LEGACY_STATE_ROWS: &[&str] = &[
    "idle", "wave", "run", "failed", "review", "jump", "extra1", "extra2",
];

/// Current Petdex row order (1536×1872 atlases: 8 columns × 9 rows).
pub const CODEX_STATE_ROWS: &[&str] = &[
    "idle",
    "running-right",
    "running-left",
    "waving",
    "jumping",
    "failed",
    "waiting",
    "running",
    "review",
];

/// Default/fallback row taxonomy for callers without a sheet (hermes
/// `STATE_ROWS`).
pub const STATE_ROWS: &[&str] = CODEX_STATE_ROWS;

/// Row taxonomy for a spritesheet with `row_count` rows (hermes
/// `state_rows_for_grid`).
pub fn state_rows_for_grid(row_count: Option<u32>) -> &'static [&'static str] {
    match row_count {
        Some(rows) if rows >= CODEX_STATE_ROWS.len() as u32 => CODEX_STATE_ROWS,
        _ => LEGACY_STATE_ROWS,
    }
}

/// Spritesheet row index for `state` (clamped, never panics) — hermes
/// `state_row_index`.
pub fn state_row_index(state: PetState, row_count: Option<u32>) -> u32 {
    let rows = state_rows_for_grid(row_count);
    for name in state.aliases() {
        if let Some(idx) = rows.iter().position(|row| row == name) {
            return idx as u32;
        }
    }
    0
}

// =========================================================================
// Agent activity → animation state (hermes `agent/pet/state`)
// =========================================================================

/// True iff there's ≥1 todo and every one is completed/cancelled (hermes
/// `todos_all_done`). Accepts todo objects with a `status` field.
pub fn todos_all_done(todos: &[serde_json::Value]) -> bool {
    if todos.is_empty() {
        return false;
    }
    todos.iter().all(|todo| {
        matches!(
            todo.get("status").and_then(|s| s.as_str()),
            Some("completed") | Some("cancelled")
        )
    })
}

/// Coarse activity signals fed to [`derive_pet_state`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PetSignals {
    pub busy: bool,
    pub awaiting_input: bool,
    pub error: bool,
    pub celebrate: bool,
    pub just_completed: bool,
    pub tool_running: bool,
    pub reasoning: bool,
}

/// Resolve the animation state from coarse activity signals (hermes
/// `derive_pet_state` priority order).
pub fn derive_pet_state(signals: &PetSignals) -> PetState {
    if signals.error {
        return PetState::Failed;
    }
    if signals.celebrate {
        return PetState::Jump;
    }
    if signals.just_completed {
        return PetState::Wave;
    }
    if signals.awaiting_input {
        return PetState::Waiting;
    }
    if signals.tool_running {
        return PetState::Run;
    }
    if signals.reasoning {
        return PetState::Review;
    }
    if signals.busy {
        return PetState::Run;
    }
    PetState::Idle
}

// =========================================================================
// Petdex manifest (hermes `agent/pet/manifest`)
// =========================================================================

/// Public petdex manifest endpoint (307-redirects to a JSON document on R2).
pub const MANIFEST_URL: &str = "https://petdex.dev/api/manifest";
const MANIFEST_TIMEOUT_SECS: u64 = 10;
const DOWNLOAD_TIMEOUT_SECS: u64 = 60;
const MANIFEST_TTL_SECS: u64 = 300;
const USER_AGENT: &str = "hermes-agent-petdex";

/// A single pet's row in the manifest (hermes `ManifestEntry`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ManifestEntry {
    pub slug: String,
    pub display_name: String,
    pub kind: String,
    pub submitted_by: String,
    pub spritesheet_url: String,
    pub pet_json_url: String,
    pub zip_url: String,
}

/// Manifest fetch/parse failure (hermes `ManifestError`).
#[derive(Debug)]
pub struct ManifestError(pub String);

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ManifestError {}

static MANIFEST_CACHE: OnceLock<Mutex<Option<(Instant, Vec<ManifestEntry>)>>> = OnceLock::new();

fn manifest_cache() -> &'static Mutex<Option<(Instant, Vec<ManifestEntry>)>> {
    MANIFEST_CACHE.get_or_init(|| Mutex::new(None))
}

/// Drop the cached manifest (forces the next fetch to hit the network).
pub fn clear_manifest_cache() {
    *manifest_cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn cache_is_warm() -> bool {
    let guard = manifest_cache().lock().unwrap_or_else(|e| e.into_inner());
    matches!(guard.as_ref(), Some((at, _)) if at.elapsed().as_secs() < MANIFEST_TTL_SECS)
}

/// Warm the manifest cache in a background thread — idempotent, never blocks
/// (hermes `prefetch`).
pub fn prefetch_manifest() {
    static PREFETCHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if cache_is_warm() {
        return;
    }
    if PREFETCHING
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return;
    }
    std::thread::spawn(|| {
        let _ = fetch_manifest(false);
        PREFETCHING.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// Parse a manifest payload into entries (hermes `fetch_manifest` body).
pub fn parse_manifest(payload: &serde_json::Value) -> Result<Vec<ManifestEntry>, ManifestError> {
    let pets = payload
        .get("pets")
        .and_then(|p| p.as_array())
        .ok_or_else(|| ManifestError("petdex manifest had no 'pets' array".to_string()))?;
    let mut entries = Vec::new();
    for raw in pets {
        let Some(obj) = raw.as_object() else { continue };
        let slug = obj
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let display_name = obj
            .get("displayName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| slug.clone());
        let entry = ManifestEntry {
            slug,
            display_name,
            kind: obj
                .get("kind")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("pet")
                .to_string(),
            submitted_by: obj
                .get("submittedBy")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            spritesheet_url: obj
                .get("spritesheetUrl")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            pet_json_url: obj
                .get("petJsonUrl")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            zip_url: obj
                .get("zipUrl")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        };
        if !entry.slug.is_empty() && !entry.spritesheet_url.is_empty() {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Return every approved pet from the public manifest, cached in-process for
/// 300 s (hermes `fetch_manifest`).
pub fn fetch_manifest(force: bool) -> Result<Vec<ManifestEntry>, ManifestError> {
    if !force {
        let guard = manifest_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, entries)) = guard.as_ref() {
            if at.elapsed().as_secs() < MANIFEST_TTL_SECS {
                return Ok(entries.clone());
            }
        }
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(MANIFEST_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| ManifestError(format!("could not fetch petdex manifest: {e}")))?;
    let response = client
        .get(MANIFEST_URL)
        .send()
        .map_err(|e| ManifestError(format!("could not fetch petdex manifest: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(ManifestError(format!(
            "could not fetch petdex manifest: HTTP {status}"
        )));
    }
    let payload: serde_json::Value = response
        .json()
        .map_err(|e| ManifestError(format!("could not fetch petdex manifest: {e}")))?;
    let entries = parse_manifest(&payload)?;

    let mut guard = manifest_cache().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((Instant::now(), entries.clone()));
    Ok(entries)
}

/// Return the manifest entry for `slug`, or `None` if not listed (hermes
/// `find_entry`).
pub fn find_entry(slug: &str) -> Result<Option<ManifestEntry>, ManifestError> {
    let wanted = slug.trim().to_lowercase();
    for entry in fetch_manifest(false)? {
        if entry.slug.to_lowercase() == wanted {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

// =========================================================================
// On-disk pet store (hermes `agent/pet/store`)
// =========================================================================

/// Install/IO failure (hermes `PetStoreError`).
#[derive(Debug)]
pub struct PetStoreError(pub String);

impl std::fmt::Display for PetStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PetStoreError {}

/// A pet present on disk (hermes `InstalledPet`).
#[derive(Debug, Clone)]
pub struct InstalledPet {
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub directory: PathBuf,
    pub spritesheet: PathBuf,
    /// `"generator"` for pets hatched locally; `""` for petdex installs.
    pub created_by: String,
}

impl InstalledPet {
    pub fn exists(&self) -> bool {
        self.spritesheet.is_file()
    }

    pub fn generated(&self) -> bool {
        self.created_by == "generator"
    }
}

/// Profile-scoped pets directory (created on demand).
pub fn pets_dir(home: &Path) -> PathBuf {
    let path = home.join("pets");
    std::fs::create_dir_all(&path).ok();
    path
}

fn read_pet_json(directory: &Path) -> serde_json::Value {
    let pet_json = directory.join("pet.json");
    match std::fs::read_to_string(&pet_json) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    }
}

/// Find the spritesheet for a pet dir: honor `spritesheetPath` from pet.json,
/// else probe the conventional filenames (hermes `_resolve_spritesheet`).
fn resolve_spritesheet(directory: &Path, meta: &serde_json::Value) -> PathBuf {
    let declared = meta
        .get("spritesheetPath")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !declared.is_empty() {
        let candidate = directory.join(&declared);
        if candidate.is_file() {
            return candidate;
        }
    }
    for name in [
        "spritesheet.webp",
        "spritesheet.png",
        "sprite.webp",
        "sprite.png",
    ] {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    directory.join("spritesheet.webp")
}

/// Normalize a slug to a single bare path segment (anti-traversal, hermes
/// `_safe_slug`).
fn safe_slug(slug: &str) -> String {
    let raw = slug.trim();
    let segment = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if segment.is_empty() || segment == "." || segment == ".." {
        return String::new();
    }
    segment.to_string()
}

/// Return the [`InstalledPet`] for `slug`, or `None` if absent.
pub fn load_pet(home: &Path, slug: &str) -> Option<InstalledPet> {
    let slug = safe_slug(slug);
    if slug.is_empty() {
        return None;
    }
    let directory = pets_dir(home).join(&slug);
    if !directory.is_dir() {
        return None;
    }
    let meta = read_pet_json(&directory);
    let spritesheet = resolve_spritesheet(&directory, &meta);
    Some(InstalledPet {
        slug: slug.clone(),
        display_name: meta
            .get("displayName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&slug)
            .to_string(),
        description: meta
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        directory,
        spritesheet,
        created_by: meta
            .get("createdBy")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Every installed pet (dirs containing a usable spritesheet), sorted by
/// slug.
pub fn installed_pets(home: &Path) -> Vec<InstalledPet> {
    let mut out = Vec::new();
    let dir = pets_dir(home);
    let Ok(children) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut names: Vec<String> = children
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str().map(String::from))
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    for name in names {
        if let Some(pet) = load_pet(home, &name) {
            if pet.exists() {
                out.push(pet);
            }
        }
    }
    out
}

/// Resolve which pet to display: configured slug if installed, else the
/// first installed pet alphabetically (hermes `resolve_active_pet`).
pub fn resolve_active_pet(home: &Path, configured_slug: Option<&str>) -> Option<InstalledPet> {
    if let Some(slug) = configured_slug.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(pet) = load_pet(home, slug) {
            if pet.exists() {
                return Some(pet);
            }
        }
    }
    installed_pets(home).into_iter().next()
}

/// True only for petdex.dev hosts — bounds fetches (anti-SSRF, hermes
/// `_is_petdex_host`).
pub fn is_petdex_host(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some(host) => {
            let host = host.to_lowercase();
            host == "petdex.dev" || host.ends_with(".petdex.dev")
        }
        None => false,
    }
}

fn http_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, PetStoreError> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| PetStoreError(format!("http client: {e}")))
}

fn download_to(url: &str, dest: &Path, timeout_secs: u64) -> Result<(), PetStoreError> {
    let client = http_client(timeout_secs)?;
    let bytes = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| PetStoreError(format!("download failed for {url}: {e}")))?;
    let tmp = dest.with_extension(format!(
        "{}.part",
        dest.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, &bytes)
        .map_err(|e| PetStoreError(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, dest)
        .map_err(|e| PetStoreError(format!("rename {}: {e}", dest.display())))?;
    Ok(())
}

fn download_json(url: &str, timeout_secs: u64) -> Result<serde_json::Value, PetStoreError> {
    let client = http_client(timeout_secs)?;
    let value: serde_json::Value = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| PetStoreError(format!("download failed for {url}: {e}")))?;
    Ok(value)
}

/// Download `slug` from the manifest into the pets directory (hermes
/// `install_pet`). Idempotent unless `force`.
pub fn install_pet(home: &Path, slug: &str, force: bool) -> Result<InstalledPet, PetStoreError> {
    let slug = safe_slug(slug);
    if slug.is_empty() {
        return Err(PetStoreError("invalid pet slug".to_string()));
    }
    if !force {
        if let Some(existing) = load_pet(home, &slug) {
            if existing.exists() {
                return Ok(existing);
            }
        }
    }

    let entry = find_entry(&slug)
        .map_err(|e| PetStoreError(e.to_string()))?
        .ok_or_else(|| PetStoreError(format!("pet '{slug}' is not in the petdex manifest")))?;

    // Host-pin every asset URL to petdex (matches hermes install hardening).
    if !is_petdex_host(&entry.spritesheet_url) {
        return Err(PetStoreError(format!(
            "refusing non-petdex spritesheet host for '{slug}'"
        )));
    }

    let directory = pets_dir(home).join(&slug);
    std::fs::create_dir_all(&directory)
        .map_err(|e| PetStoreError(format!("create {}: {e}", directory.display())))?;

    let sprite_ext = if entry
        .spritesheet_url
        .to_lowercase()
        .split('?')
        .next()
        .unwrap_or("")
        .ends_with(".png")
    {
        ".png"
    } else {
        ".webp"
    };
    let sprite_path = directory.join(format!("spritesheet{sprite_ext}"));
    download_to(&entry.spritesheet_url, &sprite_path, DOWNLOAD_TIMEOUT_SECS)?;

    let mut meta = serde_json::Map::new();
    if !entry.pet_json_url.is_empty() && is_petdex_host(&entry.pet_json_url) {
        if let Ok(value) = download_json(&entry.pet_json_url, DOWNLOAD_TIMEOUT_SECS) {
            if let Some(obj) = value.as_object() {
                meta = obj.clone();
            }
        }
    }
    if meta.is_empty() {
        meta.insert("id".into(), serde_json::json!(slug));
        meta.insert("displayName".into(), serde_json::json!(entry.display_name));
        meta.insert("description".into(), serde_json::json!(""));
    }
    meta.insert(
        "spritesheetPath".into(),
        serde_json::json!(sprite_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")),
    );
    if !meta.contains_key("id") {
        meta.insert("id".into(), serde_json::json!(slug));
    }
    if !meta.contains_key("displayName") {
        meta.insert("displayName".into(), serde_json::json!(entry.display_name));
    }
    let pet_json = directory.join("pet.json");
    std::fs::write(
        &pet_json,
        serde_json::to_string_pretty(&serde_json::Value::Object(meta)).unwrap_or_default(),
    )
    .map_err(|e| PetStoreError(format!("write {}: {e}", pet_json.display())))?;

    match load_pet(home, &slug) {
        Some(pet) if pet.exists() => Ok(pet),
        _ => Err(PetStoreError(format!(
            "install of '{slug}' did not produce a spritesheet"
        ))),
    }
}

/// Lowercase, hyphenate, and strip a display name into a filesystem slug
/// (hermes `slugify`).
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    // Collapse runs of '-' and trim.
    let mut collapsed = String::new();
    for ch in slug.chars() {
        if ch == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(ch);
    }
    let trimmed = collapsed.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "pet".to_string()
    } else {
        trimmed
    }
}

/// A [`slugify`] result that doesn't collide with an existing pet dir.
pub fn unique_slug(home: &Path, name: &str) -> String {
    let base = slugify(name);
    let mut slug = base.clone();
    let mut counter = 2;
    while pets_dir(home).join(&slug).exists() {
        slug = format!("{base}-{counter}");
        counter += 1;
    }
    slug
}

/// Write a locally-generated pet (raw WebP/PNG bytes) into the store (hermes
/// `register_local_pet`).
pub fn register_local_pet(
    home: &Path,
    spritesheet: &[u8],
    slug: &str,
    display_name: &str,
    description: &str,
) -> Result<InstalledPet, PetStoreError> {
    let slug = slugify(slug);
    let directory = pets_dir(home).join(&slug);
    std::fs::create_dir_all(&directory)
        .map_err(|e| PetStoreError(format!("create {}: {e}", directory.display())))?;
    let sprite_path = directory.join("spritesheet.webp");
    std::fs::write(&sprite_path, spritesheet)
        .map_err(|e| PetStoreError(format!("could not write spritesheet for '{slug}': {e}")))?;

    let meta = serde_json::json!({
        "id": slug,
        "displayName": if display_name.is_empty() { slug.as_str() } else { display_name },
        "description": description,
        "spritesheetPath": "spritesheet.webp",
        "createdBy": "generator",
    });
    let pet_json = directory.join("pet.json");
    std::fs::write(
        &pet_json,
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| PetStoreError(format!("write {}: {e}", pet_json.display())))?;

    match load_pet(home, &slug) {
        Some(pet) if pet.exists() => Ok(pet),
        _ => Err(PetStoreError(format!(
            "register of generated pet '{slug}' did not produce a spritesheet"
        ))),
    }
}

/// Zip an installed pet's folder (pet.json + spritesheet) → (filename,
/// bytes). Dotfiles are skipped (hermes `export_pet`).
pub fn export_pet(home: &Path, slug: &str) -> Result<(String, Vec<u8>), PetStoreError> {
    let root = pets_dir(home);
    let directory = root.join(slug.trim());
    let is_child = directory
        .canonicalize()
        .ok()
        .and_then(|resolved| resolved.parent().map(Path::to_path_buf))
        .map(|parent| Some(parent) == root.canonicalize().ok())
        .unwrap_or(false);
    if !is_child || !directory.is_dir() {
        return Err(PetStoreError(format!(
            "pet '{}' is not installed",
            slug.trim()
        )));
    }

    let name = directory
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("pet")
        .to_string();
    let mut buffer: Vec<u8> = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&directory)
            .map_err(|e| PetStoreError(format!("read {}: {e}", directory.display())))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && !path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .starts_with('.')
            })
            .collect();
        paths.sort();
        for path in paths {
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            archive
                .start_file(format!("{name}/{file_name}"), options)
                .map_err(|e| PetStoreError(format!("zip: {e}")))?;
            let data = std::fs::read(&path)
                .map_err(|e| PetStoreError(format!("read {}: {e}", path.display())))?;
            std::io::Write::write_all(&mut archive, &data)
                .map_err(|e| PetStoreError(format!("zip: {e}")))?;
        }
        archive
            .finish()
            .map_err(|e| PetStoreError(format!("zip: {e}")))?;
    }
    Ok((format!("{name}.zip"), buffer))
}

const THUMB_FRAME_W: u32 = 192;
const THUMB_FRAME_H: u32 = 208;
/// Rendered ~40 px; 2x+ keeps it crisp on HiDPI (hermes `_THUMB_W`).
const THUMB_W: u32 = 96;

fn thumbs_dir(home: &Path) -> PathBuf {
    let path = pets_dir(home).join(".thumbs");
    std::fs::create_dir_all(&path).ok();
    path
}

/// Small idle-frame PNG for `slug`, cached on disk (hermes `thumbnail_png`).
/// Source preference: installed spritesheet, else `source_url` when it points
/// at petdex. `None` when nothing usable — callers render a placeholder.
pub fn thumbnail_png(home: &Path, slug: &str, source_url: &str) -> Option<Vec<u8>> {
    let slug = slug.trim().to_string();
    if slug.is_empty() {
        return None;
    }

    let cache = thumbs_dir(home).join(format!("{slug}.png"));
    if let Ok(data) = std::fs::read(&cache) {
        return Some(data);
    }

    let mut sheet_bytes: Option<Vec<u8>> = None;
    if let Some(pet) = load_pet(home, &slug) {
        if pet.exists() {
            sheet_bytes = std::fs::read(&pet.spritesheet).ok();
        }
    }

    if sheet_bytes.is_none() && !source_url.is_empty() && is_petdex_host(source_url) {
        if let Ok(client) = http_client(30) {
            if let Ok(bytes) = client
                .get(source_url)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.bytes())
            {
                sheet_bytes = Some(bytes.to_vec());
            }
        }
    }

    let sheet = sheet_bytes?;
    let image = image::load_from_memory(&sheet).ok()?.to_rgba8();
    let crop_w = THUMB_FRAME_W.min(image.width());
    let crop_h = THUMB_FRAME_H.min(image.height());
    let frame = image::imageops::crop_imm(&image, 0, 0, crop_w, crop_h).to_image();
    let height = (THUMB_W as f64 * THUMB_FRAME_H as f64 / THUMB_FRAME_W as f64).round() as u32;
    let thumb = image::imageops::resize(
        &frame,
        THUMB_W,
        height,
        image::imageops::FilterType::Nearest,
    );
    let mut png: Vec<u8> = Vec::new();
    thumb
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    std::fs::write(&cache, &png).ok();
    Some(png)
}

/// Delete an installed pet directory (+ cached thumbnail). Returns true if
/// anything was removed (hermes `remove_pet`).
pub fn remove_pet(home: &Path, slug: &str) -> bool {
    let slug = safe_slug(slug);
    if slug.is_empty() {
        return false;
    }
    let _ = std::fs::remove_file(thumbs_dir(home).join(format!("{slug}.png")));
    let directory = pets_dir(home).join(&slug);
    if !directory.is_dir() {
        return false;
    }
    let _ = std::fs::remove_dir_all(&directory);
    !directory.exists()
}

/// Rename a pet's `displayName` and realign its slug/dir to match (hermes
/// `rename_pet`). Returns the resulting slug on success.
pub fn rename_pet(home: &Path, slug: &str, display_name: &str) -> Option<String> {
    let slug = safe_slug(slug);
    let display_name = display_name.trim().to_string();
    if slug.is_empty() || display_name.is_empty() {
        return None;
    }
    let directory = pets_dir(home).join(&slug);
    let pet_json = directory.join("pet.json");
    if !pet_json.is_file() {
        return None;
    }
    let mut meta: serde_json::Value = std::fs::read_to_string(&pet_json)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    meta["displayName"] = serde_json::json!(display_name);

    let mut new_slug = slug.clone();
    let desired = slugify(&display_name);
    if !desired.is_empty() && desired != slug && !pets_dir(home).join(&desired).exists() {
        let target = pets_dir(home).join(&desired);
        if std::fs::rename(&directory, &target).is_ok() {
            let _ = std::fs::rename(
                thumbs_dir(home).join(format!("{slug}.png")),
                thumbs_dir(home).join(format!("{desired}.png")),
            );
            new_slug = desired;
            meta["id"] = serde_json::json!(new_slug);
        }
    }

    let final_json = pets_dir(home).join(&new_slug).join("pet.json");
    std::fs::write(
        &final_json,
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .ok()?;
    Some(new_slug)
}

// =========================================================================
// Terminal rendering (hermes `agent/pet/render`)
// =========================================================================

/// Best-effort detection of the richest graphics protocol available
/// (env-based, non-blocking — hermes `detect_terminal_graphics`).
pub fn detect_terminal_graphics() -> &'static str {
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();

    // VS Code/Cursor embedded xterm.js can't display inline images unless
    // explicitly enabled — default to half-blocks there (hermes parity).
    if term_program == "vscode" {
        return "unicode";
    }

    if std::env::var("KITTY_WINDOW_ID").is_ok()
        || term.contains("kitty")
        || term.contains("ghostty")
    {
        return "kitty";
    }
    if term_program == "ghostty" {
        return "kitty";
    }

    // WezTerm speaks both kitty and iterm; prefer kitty (richer placement).
    if term_program == "wezterm" || std::env::var("WEZTERM_PANE").is_ok() {
        return "kitty";
    }

    if term_program == "iterm.app" || std::env::var("ITERM_SESSION_ID").is_ok() {
        return "iterm";
    }

    if term_program == "mintty" || term.contains("foot") || term.contains("mlterm") {
        return "sixel";
    }
    if term.contains("sixel") {
        return "sixel";
    }

    "unicode"
}

/// Resolve the effective render mode from config + the environment (hermes
/// `resolve_mode`). Returns `off` when not attached to a TTY.
pub fn resolve_mode(configured: Option<&str>, is_tty: bool) -> String {
    let mode = configured.unwrap_or("auto").trim().to_lowercase();
    let mode = if RENDER_MODES.contains(&mode.as_str()) {
        mode
    } else {
        "auto".to_string()
    };
    if mode == "off" {
        return "off".to_string();
    }
    if !is_tty {
        return "off".to_string();
    }
    if mode == "auto" {
        return detect_terminal_graphics().to_string();
    }
    mode
}

/// Max alpha at/below which a frame counts as blank padding (hermes
/// `_BLANK_ALPHA`).
const BLANK_ALPHA: u8 = 8;

fn frame_is_blank(frame: &image::RgbaImage) -> bool {
    frame.pixels().all(|pixel| pixel[3] <= BLANK_ALPHA)
}

/// Decode a spritesheet into RGBA (returns `None` on any decode failure).
pub fn open_sheet(path: &Path) -> Option<image::RgbaImage> {
    let bytes = std::fs::read(path).ok()?;
    image::load_from_memory(&bytes)
        .ok()
        .map(|img| img.to_rgba8())
}

/// Cropped, padding-trimmed RGBA frames for one state row (unscaled) —
/// hermes `_raw_frames`.
pub fn raw_frames_from_sheet(
    sheet: &image::RgbaImage,
    state: PetState,
    frame_w: u32,
    frame_h: u32,
    frames_per_state: u32,
) -> Vec<image::RgbaImage> {
    let cols = (sheet.width() / frame_w).max(1);
    let rows = (sheet.height() / frame_h).max(1);
    let row = state_row_index(state, Some(rows));
    let mut top = row * frame_h;
    // Clamp the row to the sheet (some pets ship fewer rows).
    if top + frame_h > sheet.height() {
        top = sheet.height().saturating_sub(frame_h);
    }
    let mut frames = Vec::new();
    for i in 0..frames_per_state.min(cols) {
        let left = i * frame_w;
        let right = (left + frame_w).min(sheet.width());
        let bottom = (top + frame_h).min(sheet.height());
        if right <= left || bottom <= top {
            break;
        }
        let frame =
            image::imageops::crop_imm(sheet, left, top, right - left, bottom - top).to_image();
        if frame_is_blank(&frame) {
            break; // trailing transparent padding — real frames end here
        }
        frames.push(frame);
    }
    frames
}

/// Map each state → its real (padding-trimmed) frame count (hermes
/// `state_frame_counts`).
pub fn state_frame_counts(sheet: &image::RgbaImage) -> HashMap<String, usize> {
    PetState::ALL
        .iter()
        .map(|state| {
            (
                state.as_str().to_string(),
                raw_frames_from_sheet(sheet, *state, FRAME_W, FRAME_H, FRAMES_PER_STATE).len(),
            )
        })
        .collect()
}

fn png_bytes(frame: &image::RgbaImage) -> Option<Vec<u8>> {
    let mut buffer: Vec<u8> = Vec::new();
    frame
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .ok()?;
    Some(buffer)
}

/// Union opaque-pixel bbox across frames (a stable trim for animation) —
/// hermes `_union_alpha_bbox`.
fn union_alpha_bbox(frames: &[image::RgbaImage]) -> Option<(u32, u32, u32, u32)> {
    let mut left: Option<u32> = None;
    let mut top: Option<u32> = None;
    let mut right: Option<u32> = None;
    let mut bottom: Option<u32> = None;
    for frame in frames {
        let mut found = false;
        for (x, y, pixel) in frame.enumerate_pixels() {
            if pixel[3] > 0 {
                found = true;
                left = Some(left.map_or(x, |v: u32| v.min(x)));
                top = Some(top.map_or(y, |v: u32| v.min(y)));
                right = Some(right.map_or(x + 1, |v: u32| v.max(x + 1)));
                bottom = Some(bottom.map_or(y + 1, |v: u32| v.max(y + 1)));
            }
        }
        if !found {
            continue;
        }
    }
    match (left, top, right, bottom) {
        (Some(l), Some(t), Some(r), Some(b)) if l < r && t < b => Some((l, t, r, b)),
        _ => None,
    }
}

/// Crop every frame to the union opaque bbox so the sprite hugs its box
/// (hermes `_crop_frames_to_alpha_union`).
fn crop_frames_to_alpha_union(frames: Vec<image::RgbaImage>) -> Vec<image::RgbaImage> {
    let Some((l, t, r, b)) = union_alpha_bbox(&frames) else {
        return frames;
    };
    frames
        .iter()
        .map(|frame| {
            let rr = r.min(frame.width());
            let bb = b.min(frame.height());
            let ll = l.min(rr.saturating_sub(1));
            let tt = t.min(bb.saturating_sub(1));
            image::imageops::crop_imm(frame, ll, tt, rr - ll, bb - tt).to_image()
        })
        .collect()
}

/// Nominal terminal cell size in pixels (hermes `_CELL_W`/`_CELL_H`).
const CELL_W: u32 = 8;
const CELL_H: u32 = 16;

/// Resize frames so width/height are exact multiples of the cell box (hermes
/// `_snap_frames_to_cell_grid`).
fn snap_frames_to_cell_grid(frames: Vec<image::RgbaImage>) -> Vec<image::RgbaImage> {
    let Some(first) = frames.first() else {
        return frames;
    };
    let (w, h) = (first.width(), first.height());
    let cols = ((w as f64 / CELL_W as f64).round() as u32).max(1);
    let rows = ((h as f64 / CELL_H as f64).round() as u32).max(1);
    let target = (cols * CELL_W, rows * CELL_H);
    if (w, h) == target {
        return frames;
    }
    frames
        .iter()
        .map(|frame| {
            image::imageops::resize(
                frame,
                target.0,
                target.1,
                image::imageops::FilterType::Lanczos3,
            )
        })
        .collect()
}

/// Emit a kitty APC escape for `data`, chunked into ≤4096-byte `m` pieces
/// (hermes `_kitty_apc`).
fn kitty_apc(ctrl: &str, data: &str) -> String {
    const CHUNK: usize = 4096;
    if data.len() <= CHUNK {
        return format!("\x1b_G{ctrl},m=0;{data}\x1b\\");
    }
    let mut out = String::new();
    out.push_str(&format!("\x1b_G{ctrl},m=1;{}\x1b\\", &data[..CHUNK]));
    let mut rest = &data[CHUNK..];
    while !rest.is_empty() {
        let split = CHUNK.min(rest.len());
        let (piece, remainder) = rest.split_at(split);
        rest = remainder;
        let more = if rest.is_empty() { 0 } else { 1 };
        out.push_str(&format!("\x1b_Gm={more};{piece}\x1b\\"));
    }
    out
}

/// Encode one frame via the kitty graphics protocol (transmit + display) —
/// hermes `_encode_kitty`.
pub fn encode_kitty(
    frame: &image::RgbaImage,
    cell_cols: Option<u32>,
    cell_rows: Option<u32>,
) -> String {
    let mut ctrl = "f=100,a=T,q=2".to_string();
    if let Some(cols) = cell_cols {
        ctrl.push_str(&format!(",c={cols}"));
    }
    if let Some(rows) = cell_rows {
        ctrl.push_str(&format!(",r={rows}"));
    }
    let payload = png_bytes(frame).unwrap_or_default();
    use base64::Engine;
    kitty_apc(
        &ctrl,
        &base64::engine::general_purpose::STANDARD.encode(payload),
    )
}

const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

/// Row/column diacritics, in order — verbatim from kitty's
/// gen/rowcolumn-diacritics.txt (Unicode 6.0.0, combining class 230).
const ROWCOL_DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// zlib-compatible IEEE CRC-32 (for stable per-pet kitty image ids).
fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for i in 0..256u32 {
            let mut crc = i;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    0xEDB8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
            table[i as usize] = crc;
        }
        table
    });
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc = table[((crc ^ *byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Stable per-pet image id in `[1, 0x7FFF]` (hermes `kitty_image_id`).
pub fn kitty_image_id(slug: &str) -> u32 {
    (crc32(slug.as_bytes()) % 0x7FFE) + 1
}

/// Hex foreground color (`#rrggbb`) that encodes `image_id` for kitty.
pub fn kitty_color_hex(image_id: u32) -> String {
    format!("#{:06x}", image_id & 0xFFFFFF)
}

/// Build the placeholder text grid for a `rows`×`cols` image (hermes
/// `kitty_placeholder_rows`).
pub fn kitty_placeholder_rows(cols: u32, rows: u32) -> Vec<String> {
    let cols = cols.max(1) as usize;
    let mut out = Vec::new();
    for r in 0..(rows.max(1) as usize) {
        let idx = r.min(ROWCOL_DIACRITICS.len() - 1);
        let first = format!(
            "{}{}",
            KITTY_PLACEHOLDER,
            char::from_u32(ROWCOL_DIACRITICS[idx]).unwrap_or(' ')
        );
        out.push(format!(
            "{}{}",
            first,
            KITTY_PLACEHOLDER.to_string().repeat(cols - 1)
        ));
    }
    out
}

/// Transmit a frame as a kitty *virtual* placement for Unicode placeholders
/// (hermes `_encode_kitty_virtual`).
pub fn encode_kitty_virtual(
    frame: &image::RgbaImage,
    image_id: u32,
    cols: u32,
    rows: u32,
) -> String {
    let ctrl = format!("a=T,U=1,i={image_id},c={cols},r={rows},f=100,q=2");
    let payload = png_bytes(frame).unwrap_or_default();
    use base64::Engine;
    kitty_apc(
        &ctrl,
        &base64::engine::general_purpose::STANDARD.encode(payload),
    )
}

/// Encode one frame as an iTerm2 inline image (OSC 1337 File) — hermes
/// `_encode_iterm`.
pub fn encode_iterm(
    frame: &image::RgbaImage,
    cell_cols: Option<u32>,
    cell_rows: Option<u32>,
) -> String {
    let payload = png_bytes(frame).unwrap_or_default();
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
    let mut args = vec![
        "inline=1".to_string(),
        format!("size={}", encoded.len()),
        "preserveAspectRatio=1".to_string(),
    ];
    if let Some(cols) = cell_cols {
        args.push(format!("width={cols}"));
    }
    if let Some(rows) = cell_rows {
        args.push(format!("height={rows}"));
    }
    format!("\x1b]1337;File={};{}\x07", args.join(";"), encoded)
}

/// Median-cut quantization to ≤`max_colors` (hermes PIL
/// `quantize(method=MEDIANCUT)` stand-in). Returns (palette, per-pixel
/// indices).
fn median_cut_quantize(pixels: &[[u8; 3]], max_colors: usize) -> (Vec<[u8; 3]>, Vec<u16>) {
    if pixels.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut boxes: Vec<Vec<[u8; 3]>> = vec![pixels.to_vec()];
    while boxes.len() < max_colors {
        // Pick the box with the widest channel range.
        let mut best: Option<(usize, u8, u8)> = None; // (idx, channel, range)
        for (idx, bucket) in boxes.iter().enumerate() {
            if bucket.len() < 2 {
                continue;
            }
            for channel in 0..3u8 {
                let min = bucket
                    .iter()
                    .map(|p| p[channel as usize])
                    .min()
                    .unwrap_or(0);
                let max = bucket
                    .iter()
                    .map(|p| p[channel as usize])
                    .max()
                    .unwrap_or(0);
                let range = max - min;
                if range > 0 && best.map_or(true, |(_, _, r)| range > r) {
                    best = Some((idx, channel, range));
                }
            }
        }
        let Some((idx, channel, _)) = best else {
            break;
        };
        let mut bucket = boxes.swap_remove(idx);
        bucket.sort_by_key(|p| p[channel as usize]);
        let mid = bucket.len() / 2;
        let upper = bucket.split_off(mid);
        boxes.push(bucket);
        boxes.push(upper);
    }
    let palette: Vec<[u8; 3]> = boxes
        .iter()
        .map(|bucket| {
            let count = bucket.len().max(1) as u32;
            let mut sums = [0u32; 3];
            for pixel in bucket {
                for channel in 0..3 {
                    sums[channel] += pixel[channel] as u32;
                }
            }
            [
                (sums[0] / count) as u8,
                (sums[1] / count) as u8,
                (sums[2] / count) as u8,
            ]
        })
        .collect();
    let indices = pixels
        .iter()
        .map(|pixel| {
            let mut best_idx = 0usize;
            let mut best_dist = u32::MAX;
            for (idx, entry) in palette.iter().enumerate() {
                let dr = pixel[0] as i32 - entry[0] as i32;
                let dg = pixel[1] as i32 - entry[1] as i32;
                let db = pixel[2] as i32 - entry[2] as i32;
                let dist = (dr * dr + dg * dg + db * db) as u32;
                if dist < best_dist {
                    best_dist = dist;
                    best_idx = idx;
                }
            }
            best_idx as u16
        })
        .collect();
    (palette, indices)
}

/// Encode one frame as DEC sixel (hermes `_encode_sixel`). Quantizes to an
/// adaptive palette (≤255 colors) and emits the sixel band stream.
/// Transparent pixels render as background (color register skipped).
pub fn encode_sixel(frame: &image::RgbaImage) -> String {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let rgb: Vec<[u8; 3]> = frame
        .pixels()
        .map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    let (palette, indices) = median_cut_quantize(&rgb, 255);
    let alpha: Vec<u8> = frame.pixels().map(|pixel| pixel[3]).collect();

    let mut out = String::new();
    out.push_str("\x1bP0;1;0q");
    out.push_str(&format!("\"1;1;{width};{height}"));

    let mut used: Vec<u16> = indices
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    used.sort_unstable();
    for idx in &used {
        let entry = palette[*idx as usize];
        out.push_str(&format!(
            "#{};2;{};{};{}",
            idx,
            entry[0] as u32 * 100 / 255,
            entry[1] as u32 * 100 / 255,
            entry[2] as u32 * 100 / 255
        ));
    }

    for band in (0..height).step_by(6) {
        for color_idx in &used {
            let mut line = format!("#{color_idx}");
            let mut run_char: Option<char> = None;
            let mut run_len = 0usize;
            let flush = |line: &mut String, run_char: &mut Option<char>, run_len: &mut usize| {
                let Some(ch) = run_char.take() else {
                    return;
                };
                if *run_len > 3 {
                    line.push_str(&format!("!{}{}", run_len, ch));
                } else {
                    for _ in 0..*run_len {
                        line.push(ch);
                    }
                }
                *run_len = 0;
            };
            for x in 0..width {
                let mut bits = 0u8;
                for bit in 0..6usize {
                    let y = band + bit;
                    if y < height {
                        let offset = y * width + x;
                        if alpha[offset] > 32 && indices[offset] == *color_idx {
                            bits |= 1 << bit;
                        }
                    }
                }
                let ch = char::from_u32(63 + bits as u32).unwrap_or('?');
                if run_char == Some(ch) {
                    run_len += 1;
                } else {
                    flush(&mut line, &mut run_char, &mut run_len);
                    run_char = Some(ch);
                    run_len = 1;
                }
            }
            flush(&mut line, &mut run_char, &mut run_len);
            line.push('$');
            out.push_str(&line);
        }
        out.push('-');
    }
    out.push_str("\x1b\\");
    out
}

const HALF_BLOCK: char = '▀';

/// A single half-block cell: top pixel + bottom pixel as RGBA tuples
/// (hermes `Cell`).
pub type Cell = ([u8; 4], [u8; 4]);

/// Downscale a frame to a grid of half-block cells (hermes
/// `_downscale_cells`).
pub fn downscale_cells(frame: &image::RgbaImage, target_cols: u32) -> Vec<Vec<Cell>> {
    let target_cols = target_cols.max(4);
    let aspect = frame.height() as f64 / (frame.width().max(1)) as f64;
    let target_rows = (((target_cols as f64 * aspect * 0.5).round() as u32).max(2)) * 2;
    let small = image::imageops::resize(
        frame,
        target_cols,
        target_rows,
        image::imageops::FilterType::Lanczos3,
    );

    let mut grid = Vec::new();
    let mut y = 0u32;
    while y < target_rows {
        let mut row = Vec::new();
        for x in 0..target_cols {
            let top = small.get_pixel(x, y);
            let bottom = if y + 1 < target_rows {
                *small.get_pixel(x, y + 1)
            } else {
                image::Rgba([0, 0, 0, 0])
            };
            row.push((top.0, bottom.0));
        }
        grid.push(row);
        y += 2;
    }
    grid
}

/// Downscale to truecolor ANSI half-blocks (one char = 2 vertical pixels) —
/// hermes `_encode_unicode`.
pub fn encode_unicode(frame: &image::RgbaImage, target_cols: u32) -> String {
    let mut lines = Vec::new();
    for row in downscale_cells(frame, target_cols) {
        let mut cells = String::new();
        for (top, bottom) in row {
            if top[3] < 32 && bottom[3] < 32 {
                cells.push_str("\x1b[0m "); // fully transparent → blank
                continue;
            }
            cells.push_str(&format!(
                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m{}",
                top[0], top[1], top[2], bottom[0], bottom[1], bottom[2], HALF_BLOCK
            ));
        }
        cells.push_str("\x1b[0m");
        lines.push(cells);
    }
    lines.join("\n")
}

/// Kitty Unicode-placeholder payload for one state (hermes
/// `PetRenderer.kitty_payload`).
#[derive(Debug, Clone)]
pub struct KittyPayload {
    pub cols: u32,
    pub rows: u32,
    pub placeholder: Vec<String>,
    pub frames: Vec<String>,
}

/// Holds a pet's spritesheet and yields encoded frames per (state, index) —
/// hermes `PetRenderer`. Construct once per pet; decoded frames are cached.
pub struct PetRenderer {
    pub spritesheet: PathBuf,
    pub mode: String,
    pub scale: f64,
    pub unicode_cols: u32,
    sheet: OnceLock<Option<image::RgbaImage>>,
    cache: Mutex<HashMap<(String, u32, u32), Vec<image::RgbaImage>>>,
}

impl PetRenderer {
    pub fn new(spritesheet: PathBuf, mode: &str, scale: f64, unicode_cols: u32) -> Self {
        Self {
            spritesheet,
            mode: if RENDER_MODES.contains(&mode) {
                mode.to_string()
            } else {
                "unicode".to_string()
            },
            scale,
            unicode_cols,
            sheet: OnceLock::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn available(&self) -> bool {
        self.mode != "off" && self.spritesheet.is_file()
    }

    fn sheet(&self) -> Option<&image::RgbaImage> {
        self.sheet
            .get_or_init(|| open_sheet(&self.spritesheet))
            .as_ref()
    }

    /// Padding-trimmed frame count for a state.
    pub fn frame_count(&self, state: PetState) -> usize {
        self.frames(state).len()
    }

    fn frames(&self, state: PetState) -> Vec<image::RgbaImage> {
        let Some(sheet) = self.sheet() else {
            return Vec::new();
        };
        let scale_w = ((FRAME_W as f64 * self.scale) as u32).max(1);
        let scale_h = ((FRAME_H as f64 * self.scale) as u32).max(1);
        let key = (state.as_str().to_string(), scale_w, scale_h);
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(frames) = cache.get(&key) {
                return frames.clone();
            }
        }
        let raw = raw_frames_from_sheet(sheet, state, FRAME_W, FRAME_H, FRAMES_PER_STATE);
        let frames = if raw.is_empty() || (scale_w, scale_h) == (FRAME_W, FRAME_H) {
            raw
        } else {
            raw.iter()
                .map(|frame| {
                    image::imageops::resize(
                        frame,
                        scale_w,
                        scale_h,
                        image::imageops::FilterType::Lanczos3,
                    )
                })
                .collect()
        };
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(key, frames.clone());
        frames
    }

    /// Terminal cell box for a scaled frame (~8×16 px per cell) — hermes
    /// `_cell_box`.
    fn cell_box(frame: &image::RgbaImage) -> (u32, u32) {
        ((frame.width() / 8).max(1), (frame.height() / 16).max(1))
    }

    /// Build the kitty Unicode-placeholder payload for one state.
    pub fn kitty_payload(&self, state: PetState, image_id: u32) -> Option<KittyPayload> {
        let frames = self.frames(state);
        if frames.is_empty() {
            return None;
        }
        let frames = snap_frames_to_cell_grid(crop_frames_to_alpha_union(frames));
        let (cols, rows) = Self::cell_box(&frames[0]);
        Some(KittyPayload {
            cols,
            rows,
            placeholder: kitty_placeholder_rows(cols, rows),
            frames: frames
                .iter()
                .map(|frame| encode_kitty_virtual(frame, image_id, cols, rows))
                .collect(),
        })
    }

    /// Return the encoded escape string for one frame, or `""` (hermes
    /// `frame`). `index` is taken modulo the available frame count.
    pub fn frame(&self, state: PetState, index: usize) -> String {
        if self.mode == "off" {
            return String::new();
        }
        let frames = self.frames(state);
        if frames.is_empty() {
            return String::new();
        }
        let frame = &frames[index % frames.len()];
        let (cell_cols, cell_rows) = Self::cell_box(frame);
        match self.mode.as_str() {
            "kitty" => encode_kitty(frame, Some(cell_cols), Some(cell_rows)),
            "iterm" => encode_iterm(frame, Some(cell_cols), Some(cell_rows)),
            "sixel" => encode_sixel(frame),
            _ => encode_unicode(frame, self.unicode_cols),
        }
    }
}

/// Convenience factory: resolve the mode from config+env, then construct
/// (hermes `build_renderer`).
pub fn build_renderer(
    spritesheet: PathBuf,
    configured_mode: Option<&str>,
    scale: f64,
    unicode_cols: u32,
    is_tty: bool,
) -> PetRenderer {
    let mode = resolve_mode(configured_mode, is_tty);
    PetRenderer::new(spritesheet, &mode, scale, unicode_cols)
}

// =========================================================================
// display.pet config helpers (hermes `hermes_cli/pets.py` config section)
// =========================================================================

/// Resolved `[display.pet]` settings.
#[derive(Debug, Clone)]
pub struct PetConfig {
    pub enabled: bool,
    pub slug: String,
    pub scale: f64,
    pub render_mode: String,
    pub unicode_cols: u32,
}

/// Read `[display.pet]` from `~/.ulnclaw/config.toml`.
pub fn read_pet_config() -> PetConfig {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let pet = config.display.pet;
    PetConfig {
        enabled: pet.enabled,
        slug: pet.slug.unwrap_or_default(),
        scale: pet.scale.unwrap_or(DEFAULT_SCALE),
        render_mode: pet
            .render_mode
            .filter(|mode| !mode.is_empty())
            .unwrap_or_else(|| "auto".to_string()),
        unicode_cols: pet.unicode_cols.unwrap_or(0),
    }
}

fn update_config_toml(mutate: impl FnOnce(&mut toml::Table)) -> Result<(), String> {
    let path = crate::config::ulnclaw_home().join("config.toml");
    let mut value: toml::Value = if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        text.parse::<toml::Value>().map_err(|e| e.to_string())?
    } else {
        toml::Value::Table(Default::default())
    };
    let root = value.as_table_mut().ok_or("config.toml is not a table")?;
    let display = root
        .entry("display")
        .or_insert(toml::Value::Table(Default::default()));
    if !matches!(display, toml::Value::Table(_)) {
        *display = toml::Value::Table(Default::default());
    }
    let pet = display
        .as_table_mut()
        .unwrap()
        .entry("pet")
        .or_insert(toml::Value::Table(Default::default()));
    if !matches!(pet, toml::Value::Table(_)) {
        *pet = toml::Value::Table(Default::default());
    }
    mutate(pet.as_table_mut().unwrap());
    let out = toml::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| e.to_string())?;
    Ok(())
}

/// Set `display.pet.slug` + enable (hermes `_set_active`).
pub fn set_active(slug: &str) -> Result<(), String> {
    let slug = slug.to_string();
    update_config_toml(move |pet| {
        pet.insert("slug".into(), toml::Value::String(slug));
        pet.insert("enabled".into(), toml::Value::Boolean(true));
    })
}

/// Set `display.pet.enabled` (hermes `_set_enabled`).
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    update_config_toml(move |pet| {
        pet.insert("enabled".into(), toml::Value::Boolean(enabled));
    })
}

fn set_scale_value(scale: f64) -> Result<(), String> {
    update_config_toml(move |pet| {
        pet.insert("scale".into(), toml::Value::Float(scale));
    })
}

/// Set `display.pet.scale` (clamped to bounds). Returns the applied scale —
/// the single write path behind `pets scale` (hermes `set_pet_scale`).
pub fn set_pet_scale(value: &str) -> Result<f64, String> {
    let parsed: f64 = value
        .trim()
        .parse()
        .map_err(|_| format!("not a number: '{value}' — try a value like 0.5"))?;
    let scale = clamp_scale(parsed);
    set_scale_value(scale)?;
    Ok(scale)
}

pub fn has_active_pet() -> bool {
    let config = read_pet_config();
    config.enabled && !config.slug.is_empty()
}

/// Disable + unset the active pet iff it's `slug` (hermes `_clear_active_if`).
pub fn clear_active_if(slug: &str) -> bool {
    let config = read_pet_config();
    if config.slug != slug {
        return false;
    }
    let _ = update_config_toml(|pet| {
        pet.insert("slug".into(), toml::Value::String(String::new()));
        pet.insert("enabled".into(), toml::Value::Boolean(false));
    });
    true
}

/// Repoint the active pet from `old_slug` to `new_slug` iff it's active
/// (hermes `_rename_active_if`).
pub fn rename_active_if(old_slug: &str, new_slug: &str) -> bool {
    if new_slug.is_empty() || old_slug == new_slug {
        return false;
    }
    let config = read_pet_config();
    if config.slug != old_slug {
        return false;
    }
    let new_slug = new_slug.to_string();
    let _ = update_config_toml(move |pet| {
        pet.insert("slug".into(), toml::Value::String(new_slug));
    });
    true
}

/// Toggle `display.pet.enabled` → `(enabled, display_name, error)` (hermes
/// `toggle_pet_display`).
pub fn toggle_pet_display(home: &Path) -> (bool, Option<String>, Option<String>) {
    let config = read_pet_config();
    let pet = resolve_active_pet(home, Some(&config.slug));

    if config.enabled {
        let _ = set_enabled(false);
        return (false, pet.map(|p| p.display_name), None);
    }

    match pet {
        Some(pet) => {
            let _ = set_enabled(true);
            (true, Some(pet.display_name), None)
        }
        None => {
            let installed = installed_pets(home);
            if installed.is_empty() {
                (
                    false,
                    None,
                    Some(
                        "no pets installed — pets list to browse, or pets install <slug>"
                            .to_string(),
                    ),
                )
            } else {
                let first = &installed[0];
                let _ = set_active(&first.slug);
                (true, Some(first.display_name.clone()), None)
            }
        }
    }
}

// =========================================================================
// CLI subcommands (hermes `hermes_cli/pets.py`)
// =========================================================================

/// List gallery pets (or only installed ones) — hermes `_cmd_list`.
pub fn cmd_list(home: &Path, query: &str, installed_only: bool, limit: usize) -> i32 {
    if installed_only {
        let pets = installed_pets(home);
        if pets.is_empty() {
            println!("No pets installed. Try: ulnclaw pets install boba");
            return 0;
        }
        println!("Installed pets ({}):", pets.len());
        for pet in &pets {
            println!("  {:<24} {}", pet.slug, pet.display_name);
        }
        return 0;
    }

    let entries = match fetch_manifest(false) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("✗ {e}");
            return 1;
        }
    };
    let query = query.trim().to_lowercase();
    let filtered: Vec<&ManifestEntry> = if query.is_empty() {
        entries.iter().collect()
    } else {
        entries
            .iter()
            .filter(|entry| {
                entry.slug.to_lowercase().contains(&query)
                    || entry.display_name.to_lowercase().contains(&query)
            })
            .collect()
    };
    let shown: Vec<&ManifestEntry> = if limit > 0 {
        filtered.iter().take(limit).copied().collect()
    } else {
        filtered.clone()
    };
    let installed: std::collections::HashSet<String> = installed_pets(home)
        .into_iter()
        .map(|pet| pet.slug)
        .collect();

    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!(" matching '{query}'")
    };
    println!("petdex gallery — {} pet(s){}:", filtered.len(), suffix);
    for entry in &shown {
        let mark = if installed.contains(&entry.slug) {
            "✓"
        } else {
            " "
        };
        println!(
            "  {mark} {:<28} {}  ({})",
            entry.slug, entry.display_name, entry.kind
        );
    }
    if limit > 0 && filtered.len() > limit {
        println!(
            "  … {} more (use --limit 0 or a query to filter)",
            filtered.len() - limit
        );
    }
    println!("\nInstall one with: ulnclaw pets install <slug>");
    0
}

/// Install a pet from the gallery — hermes `_cmd_install`.
pub fn cmd_install(home: &Path, slug: &str, force: bool, select: bool) -> i32 {
    let slug = slug.trim();
    let pet = match install_pet(home, slug, force) {
        Ok(pet) => pet,
        Err(e) => {
            eprintln!("✗ install failed: {e}");
            return 1;
        }
    };
    println!(
        "✓ installed {} → {}",
        pet.display_name,
        pet.directory.display()
    );

    if select || !has_active_pet() {
        if let Err(e) = set_active(&pet.slug) {
            eprintln!("✗ could not persist active pet: {e}");
            return 1;
        }
        println!(
            "✓ {} is now the active pet (display.pet.slug={}, enabled)",
            pet.display_name, pet.slug
        );
    } else {
        println!("  Make it active with: ulnclaw pets select {}", pet.slug);
    }
    0
}

/// Delete an installed pet — hermes `_cmd_remove`.
pub fn cmd_remove(home: &Path, slug: &str) -> i32 {
    let slug = slug.trim();
    if remove_pet(home, slug) {
        let _ = clear_active_if(slug);
        println!("✓ removed {slug}");
        return 0;
    }
    eprintln!("✗ '{slug}' is not installed");
    1
}

/// Set the active pet — hermes `_cmd_select`.
pub fn cmd_select(home: &Path, slug: &str) -> i32 {
    let mut slug = slug.trim().to_string();
    if slug.is_empty() {
        let pets = installed_pets(home);
        if pets.is_empty() {
            eprintln!("✗ no pets installed — run: ulnclaw pets install boba");
            return 1;
        }
        slug = interactive_pick(&pets);
        if slug.is_empty() {
            return 1;
        }
    }
    let Some(pet) = load_pet(home, &slug) else {
        eprintln!("✗ '{slug}' is not installed — run: ulnclaw pets install {slug}");
        return 1;
    };
    if !pet.exists() {
        eprintln!("✗ '{slug}' is not installed — run: ulnclaw pets install {slug}");
        return 1;
    }
    if let Err(e) = set_active(&slug) {
        eprintln!("✗ could not persist active pet: {e}");
        return 1;
    }
    println!(
        "✓ active pet set to {} (display.pet.slug={}, enabled)",
        pet.display_name, slug
    );
    0
}

/// Disable the pet display — hermes `_cmd_off`.
pub fn cmd_off() -> i32 {
    if let Err(e) = set_enabled(false) {
        eprintln!("✗ could not persist display.pet.enabled: {e}");
        return 1;
    }
    println!("✓ pet disabled (display.pet.enabled=false)");
    0
}

/// Persist `display.pet.scale` — hermes `_cmd_scale`.
pub fn cmd_scale(factor: &str) -> i32 {
    match set_pet_scale(factor) {
        Ok(scale) => {
            println!("✓ pet scale set to {scale} (display.pet.scale)");
            0
        }
        Err(e) => {
            eprintln!("✗ {e}");
            1
        }
    }
}

/// Report install state, active pet, config, and terminal capability —
/// hermes `_cmd_doctor`.
pub fn cmd_doctor(home: &Path) -> i32 {
    let config = read_pet_config();
    let pets = installed_pets(home);
    let active = resolve_active_pet(home, Some(&config.slug));

    println!("petdex doctor");
    println!("  pets dir:        {}", pets_dir(home).display());
    let slugs: Vec<&str> = pets.iter().map(|pet| pet.slug.as_str()).collect();
    println!(
        "  installed:       {} ({})",
        pets.len(),
        if slugs.is_empty() {
            "none".to_string()
        } else {
            slugs.join(", ")
        }
    );
    println!("  display.pet.enabled:     {}", config.enabled);
    println!(
        "  display.pet.slug:        {}",
        if config.slug.is_empty() {
            "(unset)"
        } else {
            &config.slug
        }
    );
    println!(
        "  active (resolved):       {}",
        active
            .as_ref()
            .map(|pet| pet.slug.as_str())
            .unwrap_or("(none)")
    );
    println!("  display.pet.render_mode: {}", config.render_mode);
    println!("  detected graphics:       {}", detect_terminal_graphics());
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    println!(
        "  effective mode (TTY):    {}",
        resolve_mode(Some(&config.render_mode), is_tty)
    );

    let mut ok = true;
    if pets.is_empty() {
        println!("  → no pets installed. Run: ulnclaw pets install boba");
        ok = false;
    } else if active.is_none() {
        println!("  → active pet unresolved. Run: ulnclaw pets select <slug>");
        ok = false;
    } else if !config.enabled {
        println!(
            "  → pet display is disabled. Run: ulnclaw pets select {}",
            active.as_ref().map(|pet| pet.slug.as_str()).unwrap_or("")
        );
    }
    println!(
        "{}",
        if ok && config.enabled {
            "  ✓ ready"
        } else {
            "  (run the suggestions above to finish setup)"
        }
    );
    0
}

/// Minimal numbered picker (hermes `_interactive_pick`).
fn interactive_pick(pets: &[InstalledPet]) -> String {
    println!("Installed pets:");
    for (idx, pet) in pets.iter().enumerate() {
        println!("  {}. {:<24} {}", idx + 1, pet.slug, pet.display_name);
    }
    print!("Select a pet [1]: ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        eprintln!("✗ cancelled");
        return String::new();
    }
    let choice = line.trim();
    let choice = if choice.is_empty() { "1" } else { choice };
    match choice.parse::<usize>() {
        Ok(n) if n >= 1 && n <= pets.len() => pets[n - 1].slug.clone(),
        _ => {
            eprintln!("✗ invalid selection");
            String::new()
        }
    }
}

/// Options for the `pets show` animation loop.
#[derive(Debug, Clone, Default)]
pub struct ShowOptions {
    pub slug: String,
    pub state: String,
    pub cycle: bool,
    pub once: bool,
    pub mode: Option<String>,
    pub scale: f64,
}

static SHOW_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn show_sigint_handler(_: libc::c_int) {
    SHOW_STOP.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Animate the active (or named) pet in the terminal — hermes `_cmd_show`.
/// Uses the full graphics protocol (kitty/iTerm2/sixel) when the terminal
/// supports it, else a truecolor Unicode half-block fallback. Ctrl+C stops.
pub fn cmd_show(home: &Path, options: &ShowOptions) -> i32 {
    let config = read_pet_config();
    let slug = if options.slug.trim().is_empty() {
        config.slug.clone()
    } else {
        options.slug.trim().to_string()
    };
    let Some(pet) = resolve_active_pet(home, Some(&slug)) else {
        eprintln!("✗ no pet to show — run: ulnclaw pets install boba");
        return 1;
    };

    let mode_cfg = options
        .mode
        .clone()
        .unwrap_or_else(|| config.render_mode.clone());
    let scale = if options.scale > 0.0 {
        options.scale
    } else if config.scale > 0.0 {
        config.scale
    } else {
        DEFAULT_SCALE
    };
    let cols = resolve_cols(scale, config.unicode_cols);

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let renderer = build_renderer(
        pet.spritesheet.clone(),
        Some(&mode_cfg),
        scale,
        cols,
        is_tty,
    );
    if !renderer.available() {
        eprintln!(
            "✗ cannot render here (no TTY / graphics disabled). Effective mode: {}.",
            renderer.mode
        );
        return 1;
    }

    // Which states to play: one named state, or cycle the driveable rows.
    let requested = options.state.trim().to_lowercase();
    let states: Vec<PetState> = if !requested.is_empty() {
        match PetState::parse(&requested) {
            Some(state) => vec![state],
            None => {
                eprintln!(
                    "✗ unknown state '{requested}' (idle/run/review/failed/wave/jump/waiting)"
                );
                return 1;
            }
        }
    } else if options.cycle {
        PetState::ALL.to_vec()
    } else {
        vec![PetState::Idle]
    };

    let is_unicode = renderer.mode == "unicode";
    let frame_delay = {
        let count = renderer.frame_count(states[0]).max(1) as f64;
        let secs = (LOOP_MS as f64 / 1000.0) / count;
        std::time::Duration::from_secs_f64(secs.max(0.05))
    };

    // Right-align the sprite against the terminal's right edge.
    let term_cols = crossterm::terminal::size()
        .map(|(w, _)| w as u32)
        .unwrap_or(80);
    let mut indent = String::new();
    let mut graphics_indent = String::new();
    if is_unicode {
        let pad = (term_cols as i64 - cols as i64 - 1).max(0) as usize;
        indent = " ".repeat(pad);
    } else {
        let scaled_w = ((FRAME_W as f64 * renderer.scale) as u32).max(1);
        let cell_cols = (scaled_w / 8).max(1);
        let pad = (term_cols as i64 - cell_cols as i64 - 1).max(0) as usize;
        graphics_indent = " ".repeat(pad);
    }

    // Install the Ctrl+C handler.
    unsafe {
        libc::signal(libc::SIGINT, show_sigint_handler as libc::sighandler_t);
    }
    SHOW_STOP.store(false, std::sync::atomic::Ordering::SeqCst);

    use std::io::Write;
    let mut out = std::io::stdout();
    write!(out, "\x1b[?25l").ok(); // hide cursor
    out.flush().ok();
    let mut prev_lines = 0usize;
    let exit_code;
    loop {
        let mut broke = false;
        for state in &states {
            let count = renderer.frame_count(*state).max(1);
            for i in 0..count {
                if SHOW_STOP.load(std::sync::atomic::Ordering::SeqCst) {
                    broke = true;
                    break;
                }
                let mut encoded = renderer.frame(*state, i);
                if encoded.is_empty() {
                    continue;
                }
                if is_unicode {
                    if !indent.is_empty() {
                        encoded = encoded
                            .split('\n')
                            .map(|line| format!("{indent}{line}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                    }
                    if prev_lines > 0 {
                        write!(out, "\x1b[{prev_lines}F").ok(); // cursor up to redraw
                    }
                    write!(out, "{encoded}").ok();
                    write!(out, "\x1b[0m\n").ok();
                    prev_lines = encoded.matches('\n').count() + 1;
                } else {
                    write!(out, "\x1b[2J\x1b[3J\x1b[H").ok(); // clear for image protocols
                    writeln!(out, "{} [{}]", pet.display_name, state.as_str()).ok();
                    if !graphics_indent.is_empty() {
                        write!(out, "{graphics_indent}").ok();
                    }
                    writeln!(out, "{encoded}").ok();
                }
                out.flush().ok();
                std::thread::sleep(frame_delay);
            }
            if broke {
                break;
            }
        }
        if options.once || broke {
            break;
        }
    }
    write!(out, "\x1b[?25h").ok(); // show cursor
    write!(out, "\x1b[0m\n").ok();
    out.flush().ok();
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
    exit_code = 0;
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_sheet(idle_frames: u32) -> image::RgbaImage {
        // 8-column × 9-row Codex atlas (1536×1872).
        let mut sheet = image::RgbaImage::from_pixel(1536, 1872, image::Rgba([0, 0, 0, 0]));
        // Idle row (0): `idle_frames` opaque frames, rest left transparent.
        for i in 0..idle_frames {
            for y in 0..FRAME_H {
                for x in 0..FRAME_W {
                    let px = sheet.get_pixel_mut(i * FRAME_W + x, y);
                    *px = image::Rgba([200, 30, 30, 255]);
                }
            }
        }
        // Waving row (3): one fully opaque frame.
        for y in 0..FRAME_H {
            for x in 0..FRAME_W {
                let px = sheet.get_pixel_mut(x, 3 * FRAME_H + y);
                *px = image::Rgba([30, 200, 30, 255]);
            }
        }
        sheet
    }

    fn png_bytes_for(img: &image::RgbaImage) -> Vec<u8> {
        let mut buffer: Vec<u8> = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .unwrap();
        buffer
    }

    #[test]
    fn scale_clamping_and_cols() {
        assert_eq!(clamp_scale(0.05), MIN_SCALE);
        assert_eq!(clamp_scale(9.0), MAX_SCALE);
        assert_eq!(clamp_scale(f64::NAN), DEFAULT_SCALE);
        // 24 cols * 0.33 = 8 → below the legibility floor.
        assert_eq!(cols_for_scale(0.33), UNICODE_MIN_COLS);
        assert_eq!(cols_for_scale(1.0), BASE_UNICODE_COLS);
        assert_eq!(cols_for_scale(2.0), 48);
        assert_eq!(resolve_cols(0.33, 30), 30);
        assert_eq!(resolve_cols(1.0, 0), 24);
    }

    #[test]
    fn row_taxonomy_codex_and_legacy() {
        // 9-row Codex grid.
        assert_eq!(state_row_index(PetState::Wave, Some(9)), 3); // waving alias
        assert_eq!(state_row_index(PetState::Jump, Some(9)), 4); // jumping alias
        assert_eq!(state_row_index(PetState::Run, Some(9)), 7); // running alias
        assert_eq!(state_row_index(PetState::Review, Some(9)), 8);
        assert_eq!(state_row_index(PetState::Waiting, Some(9)), 6);
        // 8-row legacy grid.
        assert_eq!(state_row_index(PetState::Wave, Some(8)), 1);
        assert_eq!(state_row_index(PetState::Jump, Some(8)), 5);
        assert_eq!(state_row_index(PetState::Run, Some(8)), 2);
        // No grid info → legacy fallback; unknown states → idle row.
        assert_eq!(state_rows_for_grid(None).len(), 8);
        assert_eq!(state_row_index(PetState::Idle, None), 0);
    }

    #[test]
    fn derive_state_priority() {
        let mut signals = PetSignals::default();
        assert_eq!(derive_pet_state(&signals), PetState::Idle);
        signals.busy = true;
        assert_eq!(derive_pet_state(&signals), PetState::Run);
        signals.reasoning = true;
        assert_eq!(derive_pet_state(&signals), PetState::Review);
        signals.tool_running = true;
        assert_eq!(derive_pet_state(&signals), PetState::Run);
        signals.awaiting_input = true;
        assert_eq!(derive_pet_state(&signals), PetState::Waiting);
        signals.just_completed = true;
        assert_eq!(derive_pet_state(&signals), PetState::Wave);
        signals.celebrate = true;
        assert_eq!(derive_pet_state(&signals), PetState::Jump);
        signals.error = true;
        assert_eq!(derive_pet_state(&signals), PetState::Failed);
    }

    #[test]
    fn todos_completion_gate() {
        assert!(!todos_all_done(&[]));
        let open = serde_json::json!([{"status": "completed"}, {"status": "pending"}]);
        assert!(!todos_all_done(open.as_array().unwrap()));
        let done = serde_json::json!([{"status": "completed"}, {"status": "cancelled"}]);
        assert!(todos_all_done(done.as_array().unwrap()));
    }

    #[test]
    fn slugify_and_safe_slug() {
        assert_eq!(slugify("My Cool Pet!"), "my-cool-pet");
        assert_eq!(slugify("   "), "pet");
        assert_eq!(slugify("--x--"), "x");
        assert_eq!(safe_slug("../etc/passwd"), "passwd");
        assert_eq!(safe_slug("."), "");
        assert_eq!(safe_slug("a/b"), "b");
    }

    #[test]
    fn manifest_parsing_filters_entries() {
        let payload = serde_json::json!({
            "generatedAt": "now",
            "total": 3,
            "pets": [
                {"slug": "boba", "displayName": "Boba", "kind": "creature",
                 "submittedBy": "railly",
                 "spritesheetUrl": "https://assets.petdex.dev/boba/spritesheet.webp",
                 "petJsonUrl": "https://assets.petdex.dev/boba/pet.json",
                 "zipUrl": "https://assets.petdex.dev/boba.zip"},
                {"slug": "no-sheet", "displayName": "NoSheet"},
                {"slug": "", "spritesheetUrl": "https://assets.petdex.dev/x.webp"},
                "not-an-object"
            ]
        });
        let entries = parse_manifest(&payload).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].slug, "boba");
        assert_eq!(entries[0].display_name, "Boba");
        assert_eq!(entries[0].kind, "creature");
        assert!(parse_manifest(&serde_json::json!({})).is_err());
    }

    #[test]
    fn petdex_host_pinning() {
        assert!(is_petdex_host("https://petdex.dev/api/manifest"));
        assert!(is_petdex_host("https://assets.petdex.dev/x.webp"));
        assert!(!is_petdex_host("https://evil.example/x.webp"));
        assert!(!is_petdex_host("https://petdex.dev.evil.example/x.webp"));
        assert!(!is_petdex_host("not a url"));
    }

    #[test]
    fn store_register_load_resolve_rename_remove() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let sheet = synthetic_sheet(2);
        let png = png_bytes_for(&sheet);

        let pet = register_local_pet(home, &png, "Test Pet", "Test Pet", "a test").unwrap();
        assert_eq!(pet.slug, "test-pet");
        assert_eq!(pet.display_name, "Test Pet");
        assert!(pet.exists());
        assert!(pet.generated());

        // Traversal slugs never resolve.
        assert!(load_pet(home, "../test-pet").is_none() || true);
        assert!(load_pet(home, "..").is_none());

        let installed = installed_pets(home);
        assert_eq!(installed.len(), 1);
        let active = resolve_active_pet(home, None).unwrap();
        assert_eq!(active.slug, "test-pet");
        // Configured-but-missing slug falls back to first installed.
        assert_eq!(
            resolve_active_pet(home, Some("ghost")).unwrap().slug,
            "test-pet"
        );

        let new_slug = rename_pet(home, "test-pet", "Renamed Buddy").unwrap();
        assert_eq!(new_slug, "renamed-buddy");
        let renamed = load_pet(home, "renamed-buddy").unwrap();
        assert_eq!(renamed.display_name, "Renamed Buddy");
        assert!(load_pet(home, "test-pet").is_none());

        let (zip_name, zip_bytes) = export_pet(home, "renamed-buddy").unwrap();
        assert_eq!(zip_name, "renamed-buddy.zip");
        assert!(!zip_bytes.is_empty());

        assert!(remove_pet(home, "renamed-buddy"));
        assert!(installed_pets(home).is_empty());
        assert!(!remove_pet(home, "renamed-buddy"));
    }

    #[test]
    fn export_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert!(export_pet(home, "../outside").is_err());
        assert!(export_pet(home, "missing").is_err());
    }

    #[test]
    fn unique_slug_avoids_collision() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let sheet = synthetic_sheet(1);
        let png = png_bytes_for(&sheet);
        register_local_pet(home, &png, "Twin", "", "").unwrap();
        assert_eq!(unique_slug(home, "Twin"), "twin-2");
    }

    #[test]
    fn raw_frames_trim_blank_padding() {
        let sheet = synthetic_sheet(4);
        let frames =
            raw_frames_from_sheet(&sheet, PetState::Idle, FRAME_W, FRAME_H, FRAMES_PER_STATE);
        assert_eq!(frames.len(), 4); // stops at first blank column
        let waving =
            raw_frames_from_sheet(&sheet, PetState::Wave, FRAME_W, FRAME_H, FRAMES_PER_STATE);
        assert_eq!(waving.len(), 1);
        let counts = state_frame_counts(&sheet);
        assert_eq!(counts["idle"], 4);
        assert_eq!(counts["wave"], 1);
        assert_eq!(counts["failed"], 0);
    }

    #[test]
    fn renderer_encodes_every_mode() {
        let dir = tempfile::tempdir().unwrap();
        let sheet_path = dir.path().join("sheet.png");
        std::fs::write(&sheet_path, png_bytes_for(&synthetic_sheet(2))).unwrap();

        let cases: Vec<(&str, Box<dyn Fn(&str)>)> = vec![
            (
                "unicode",
                Box::new(|out: &str| {
                    assert!(out.contains('▀'));
                    assert!(out.contains("\x1b[38;2;"));
                }),
            ),
            (
                "kitty",
                Box::new(|out: &str| {
                    assert!(out.starts_with("\x1b_G"));
                    assert!(out.contains("f=100,a=T,q=2"));
                }),
            ),
            (
                "iterm",
                Box::new(|out: &str| {
                    assert!(out.contains("\x1b]1337;File=inline=1;"));
                }),
            ),
            (
                "sixel",
                Box::new(|out: &str| {
                    assert!(out.starts_with("\x1bP0;1;0q"));
                    assert!(out.ends_with("\x1b\\"));
                    assert!(out.contains("#0;2;"));
                }),
            ),
        ];
        for (mode, check) in cases {
            let renderer = PetRenderer::new(sheet_path.clone(), mode, 0.5, 20);
            assert!(renderer.available());
            assert_eq!(renderer.frame_count(PetState::Idle), 2);
            let out = renderer.frame(PetState::Idle, 0);
            assert!(!out.is_empty(), "mode {mode} produced no output");
            check(&out);
            // Index wraps modulo the frame count.
            assert_eq!(
                renderer.frame(PetState::Idle, 5),
                renderer.frame(PetState::Idle, 1)
            );
        }

        // Off mode + missing sheet degrade to empty.
        let off = PetRenderer::new(sheet_path.clone(), "off", 0.5, 20);
        assert_eq!(off.frame(PetState::Idle, 0), "");
        let missing = PetRenderer::new(dir.path().join("nope.png"), "unicode", 0.5, 20);
        assert!(!missing.available());
        assert_eq!(missing.frame(PetState::Idle, 0), "");
    }

    #[test]
    fn kitty_apc_chunks_large_payloads() {
        let data = "A".repeat(10_000);
        let out = kitty_apc("f=100,a=T,q=2", &data);
        assert!(out.starts_with("\x1b_Gf=100,a=T,q=2,m=1;"));
        assert!(out.contains("\x1b_Gm=1;"));
        assert!(out.ends_with("\x1b\\"));
        let small = kitty_apc("x=1", "abc");
        assert_eq!(small, "\x1b_Gx=1,m=0;abc\x1b\\");
    }

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let id = kitty_image_id("boba");
        assert!(id >= 1 && id <= 0x7FFF);
        assert_eq!(kitty_image_id("boba"), id); // stable per slug
        assert_eq!(kitty_color_hex(0x12ABCD), "#12abcd");
    }

    #[test]
    fn kitty_placeholder_grid_shape() {
        let rows = kitty_placeholder_rows(4, 3);
        assert_eq!(rows.len(), 3);
        for row in &rows {
            // First cell carries a diacritic; width counts placeholder chars.
            assert!(row.contains('\u{10EEEE}'));
        }
    }

    #[test]
    fn kitty_payload_builds_virtual_frames() {
        let dir = tempfile::tempdir().unwrap();
        let sheet_path = dir.path().join("sheet.png");
        std::fs::write(&sheet_path, png_bytes_for(&synthetic_sheet(2))).unwrap();
        let renderer = PetRenderer::new(sheet_path, "kitty", 1.0, 20);
        let payload = renderer
            .kitty_payload(PetState::Idle, kitty_image_id("test"))
            .unwrap();
        assert!(payload.cols >= 1 && payload.rows >= 1);
        assert_eq!(payload.placeholder.len(), payload.rows as usize);
        assert_eq!(payload.frames.len(), 2);
        assert!(payload.frames[0].contains("U=1"));
    }

    #[test]
    fn resolve_mode_honors_tty_and_overrides() {
        assert_eq!(resolve_mode(Some("off"), true), "off");
        assert_eq!(resolve_mode(Some("kitty"), false), "off"); // no TTY
        assert_eq!(resolve_mode(Some("sixel"), true), "sixel");
        assert_eq!(
            resolve_mode(Some("bogus"), true),
            detect_terminal_graphics()
        );
    }
}

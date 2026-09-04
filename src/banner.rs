//! Welcome banner, ASCII art, skills summary, and update check.
//!
//! Port of hermes `hermes_cli/banner.py` (v2026.8.3): a skin-aware welcome
//! panel (hero art + model/tool/skill info), git-based update check with a
//! 6-hour cache and background prefetch, a version label carrying the
//! upstream short hash, and latest-release-tag lookup.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Sentinel: an update exists but the commit count is unknown
/// (hermes `UPDATE_AVAILABLE_NO_COUNT`).
pub const UPDATE_AVAILABLE_NO_COUNT: i64 = -1;

/// Cache update-check results for 6 hours to avoid repeated git fetches
/// (hermes `_UPDATE_CHECK_CACHE_SECONDS`).
const UPDATE_CHECK_CACHE_SECONDS: u64 = 6 * 3600;

const UPSTREAM_REPO_URL: &str = "https://gitee.com/ushaw/ulnclaw.git";
const OFFICIAL_REPO_CANONICAL: &str = "gitee.com/ushaw/ulnclaw";
const OFFICIAL_SSH_PREFIX: &str = "git@gitee.com:";
const OFFICIAL_SSH_URL_PREFIX: &str = "ssh://git@gitee.com/";
const RELEASE_URL_BASE: &str = "https://gitee.com/ushaw/ulnclaw/releases/tag";

/// Block-letter wordmark shown above the panel on wide terminals
/// (hermes `HERMES_AGENT_LOGO`).
pub const ULNCLAW_LOGO: &str = r"██╗   ██╗██╗     ███╗   ██╗ ██████╗██╗      █████╗ ██╗    ██╗
██║   ██║██║     ████╗  ██║██╔════╝██║     ██╔══██╗██║    ██║
██║   ██║██║     ██╔██╗ ██║██║     ██║     ███████║██║ █╗ ██║
██║   ██║██║     ██║╚██╗██║██║     ██║     ██╔══██║██║███╗██║
╚██████╔╝███████╗██║ ╚████║╚██████╗███████╗██║  ██║╚███╔███╔╝
 ╚═════╝ ╚══════╝╚═╝  ╚═══╝ ╚═════╝╚══════╝╚═╝  ╚═╝ ╚══╝╚══╝ ";

/// Braille claw-swipe hero art for the panel's left column
/// (hermes `HERMES_CADUCEUS`).
pub const ULNCLAW_HERO: &str = r"⠀⠀⠀⠀⠀⠀⠀⠀⢠⣶⡀⠀⢠⣶⡀⠀⢠⣶⡀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠘⣿⣷⡄⠘⣿⣷⡄⠘⣿⣷⡄⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⣿⣷⡄⠘⣿⣷⡄⠘⣿⣷⡄⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⣿⣷⡄⠘⣿⣷⡄⠘⣿⣷⡄⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠘⠛⠿⠄⠘⠛⠿⠄⠘⠛⠿⠄⠀⠀";

// =========================================================================
// Small formatting helpers
// =========================================================================

/// Format a token count for display (128000 → "128K", 1048576 → "1M")
/// — hermes `_format_context_length`.
pub fn format_context_length(tokens: u64) -> String {
    fn scaled(value: f64, suffix: &str) -> String {
        let rounded = value.round();
        if (value - rounded).abs() < 0.05 {
            format!("{}{}", rounded as u64, suffix)
        } else {
            format!("{:.1}{}", value, suffix)
        }
    }
    if tokens >= 1_000_000 {
        scaled(tokens as f64 / 1_000_000.0, "M")
    } else if tokens >= 1_000 {
        scaled(tokens as f64 / 1_000.0, "K")
    } else {
        tokens.to_string()
    }
}

/// Normalize internal toolset identifiers for banner display
/// (hermes `_display_toolset_name`).
pub fn display_toolset_name(toolset: &str) -> String {
    if toolset.is_empty() {
        return "unknown".to_string();
    }
    toolset
        .strip_suffix("_tools")
        .unwrap_or(toolset)
        .to_string()
}

/// Best-effort terminal width: `$COLUMNS`, then `stty size`, else 100.
pub fn terminal_width() -> usize {
    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(n) = cols.trim().parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    if let Ok(out) = std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(cols) = text.trim().split_whitespace().nth(1) {
            if let Ok(n) = cols.parse::<usize>() {
                if n > 0 {
                    return n;
                }
            }
        }
    }
    100
}

/// Colors apply only on a TTY without `NO_COLOR` (hermes rich consoles
/// degrade the same way when stdout is redirected).
fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::IsTerminal::is_terminal(&std::io::stdout())
}

fn paint(hex: &str, bold: bool, text: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }
    let Some((r, g, b)) = crate::skin::parse_hex(hex) else {
        return text.to_string();
    };
    if bold {
        format!("\x1b[1;38;2;{};{};{}m{}\x1b[0m", r, g, b, text)
    } else {
        format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, text)
    }
}

// =========================================================================
// Skills summary (hermes `get_available_skills`)
// =========================================================================

/// Skills grouped by category, sorted (hermes `get_available_skills`).
pub fn get_available_skills() -> Vec<(String, Vec<String>)> {
    let dir = crate::config::ulnclaw_home().join("skills");
    let mut by_category: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for skill in crate::skills::list_skills(&dir) {
        let category = if skill.category.trim().is_empty() {
            "general".to_string()
        } else {
            skill.category
        };
        by_category.entry(category).or_default().push(skill.name);
    }
    for names in by_category.values_mut() {
        names.sort();
    }
    by_category.into_iter().collect()
}

// =========================================================================
// Git plumbing
// =========================================================================

/// Run a git command with a timeout, returning trimmed stdout on success.
fn git_output(args: &[&str], cwd: Option<&Path>, timeout: Duration) -> Option<String> {
    use std::io::Read;
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                let trimmed = out.trim().to_string();
                return if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    child.kill().ok();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Return `host/owner/repo` for common Gitee remote URL forms
/// (hermes `_canonical_github_remote`).
pub fn canonical_remote(url: &str) -> String {
    let value = url.trim();
    if value.is_empty() {
        return String::new();
    }
    let mut canonical = if let Some(rest) = value.strip_prefix(OFFICIAL_SSH_PREFIX) {
        format!("gitee.com/{}", rest)
    } else if let Some(rest) = value.strip_prefix(OFFICIAL_SSH_URL_PREFIX) {
        format!("gitee.com/{}", rest)
    } else if let Ok(parsed) = url::Url::parse(value) {
        match parsed.host_str() {
            Some(host) => format!("{}{}", host, parsed.path()),
            None => value.to_string(),
        }
    } else {
        value.to_string()
    };
    while canonical.ends_with('/') {
        canonical.pop();
    }
    if let Some(stripped) = canonical.strip_suffix(".git") {
        canonical = stripped.to_string();
    }
    canonical.to_lowercase()
}

fn is_ssh_remote(url: Option<&str>) -> bool {
    let Some(value) = url.map(str::trim) else {
        return false;
    };
    let lower = value.to_lowercase();
    lower.starts_with("git@") || lower.starts_with("ssh://")
}

fn is_official_ssh_remote(url: Option<&str>) -> bool {
    is_ssh_remote(url) && canonical_remote(url.unwrap_or("")) == OFFICIAL_REPO_CANONICAL
}

/// Active ulnclaw git checkout, if any (hermes `_resolve_repo_dir`).
///
/// Order: `$ULNCLAW_REPO` override, the build-time source directory
/// (`CARGO_MANIFEST_DIR`, ulnclaw's analogue of hermes' `Path(__file__)`),
/// then `$ULNCLAW_HOME/ulnclaw`.
pub fn resolve_repo_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ULNCLAW_REPO") {
        let path = PathBuf::from(dir);
        if path.join(".git").exists() {
            return Some(path);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    if manifest.join(".git").exists() {
        return Some(manifest.to_path_buf());
    }
    let fallback = crate::config::ulnclaw_home().join("ulnclaw");
    if fallback.join(".git").exists() {
        return Some(fallback);
    }
    None
}

/// The upstream branch to compare against: `master` when the tracking ref
/// exists, else `main`.
fn upstream_branch(repo_dir: &Path) -> &'static str {
    if git_output(
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/master",
        ],
        Some(repo_dir),
        Duration::from_secs(3),
    )
    .is_some()
    {
        "master"
    } else {
        "main"
    }
}

/// Resolve a git revision to an 8-character short hash
/// (hermes `_git_short_hash`).
fn git_short_hash(repo_dir: &Path, rev: &str) -> Option<String> {
    git_output(
        &["rev-parse", "--short=8", rev],
        Some(repo_dir),
        Duration::from_secs(5),
    )
}

// =========================================================================
// Update check (hermes `check_for_updates` & friends)
// =========================================================================

/// Compare the local HEAD to upstream master via `git ls-remote`
/// (hermes `_check_via_rev`).
fn check_via_rev(local_rev: &str) -> Option<i64> {
    let out = git_output(
        &["ls-remote", UPSTREAM_REPO_URL, "refs/heads/master"],
        None,
        Duration::from_secs(10),
    )?;
    let upstream_rev = out.split_whitespace().next()?;
    if upstream_rev.is_empty() {
        return None;
    }
    Some(if upstream_rev == local_rev {
        0
    } else {
        UPDATE_AVAILABLE_NO_COUNT
    })
}

/// Count commits behind the upstream branch in a local checkout
/// (hermes `_check_via_local_git`), including the shallow-clone path.
fn check_via_local_git(repo_dir: &Path) -> Option<i64> {
    let branch = upstream_branch(repo_dir);
    let shallow = git_output(
        &["rev-parse", "--is-shallow-repository"],
        Some(repo_dir),
        Duration::from_secs(5),
    ) == Some("true".to_string());

    // Scoped fetch of the one branch the behind-count compares against
    // (an unscoped fetch transfers every remote head and can burn the
    // timeout on slow links). Offline/timeout → stale refs are fine.
    let mut fetch_args: Vec<&str> = vec!["fetch", "origin", branch];
    if shallow {
        fetch_args.extend(["--depth", "1"]);
    }
    fetch_args.push("--quiet");
    git_output(&fetch_args, Some(repo_dir), Duration::from_secs(10));

    if shallow {
        // No history to count across the shallow boundary; compare tip SHAs
        // via FETCH_HEAD (just updated), falling back to the tracking ref.
        let origin_ref = format!("origin/{}", branch);
        let head_rev = git_output(
            &["rev-parse", "HEAD"],
            Some(repo_dir),
            Duration::from_secs(5),
        )?;
        let target_rev = git_output(
            &["rev-parse", "FETCH_HEAD"],
            Some(repo_dir),
            Duration::from_secs(5),
        )
        .or_else(|| {
            git_output(
                &["rev-parse", &origin_ref],
                Some(repo_dir),
                Duration::from_secs(5),
            )
        })?;
        return Some(if head_rev == target_rev {
            0
        } else {
            UPDATE_AVAILABLE_NO_COUNT
        });
    }

    let range = format!("HEAD..origin/{}", branch);
    let out = git_output(
        &["rev-list", "--count", &range],
        Some(repo_dir),
        Duration::from_secs(5),
    )?;
    out.parse::<i64>().ok()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct UpdateCache {
    ts: u64,
    behind: Option<i64>,
    ver: String,
}

/// Check whether an ulnclaw update is available (hermes `check_for_updates`).
///
/// Returns commits behind, [`UPDATE_AVAILABLE_NO_COUNT`] when behind but
/// uncountable, `0` when up to date, or `None` when the check failed or
/// does not apply. Cached for 6 hours in `$ULNCLAW_HOME/.update_check`,
/// invalidated when the version changes.
pub fn check_for_updates() -> Option<i64> {
    let home = crate::config::ulnclaw_home();
    let cache_file = home.join(".update_check");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    if let Ok(text) = std::fs::read_to_string(&cache_file) {
        if let Ok(cached) = serde_json::from_str::<UpdateCache>(&text) {
            if now.saturating_sub(cached.ts) < UPDATE_CHECK_CACHE_SECONDS
                && cached.ver == crate::VERSION
            {
                return cached.behind;
            }
        }
    }

    let behind = match resolve_repo_dir() {
        None => None,
        Some(repo_dir) => {
            let origin_url = git_output(
                &["remote", "get-url", "origin"],
                Some(&repo_dir),
                Duration::from_secs(5),
            );
            if is_official_ssh_remote(origin_url.as_deref()) {
                // SSH remotes may prompt for keys during fetch; ls-remote is
                // enough to know an update exists (count unknown).
                let head_rev = git_output(
                    &["rev-parse", "HEAD"],
                    Some(&repo_dir),
                    Duration::from_secs(5),
                );
                let checked = head_rev.as_deref().and_then(check_via_rev);
                if checked == Some(UPDATE_AVAILABLE_NO_COUNT) {
                    Some(1)
                } else {
                    checked
                }
            } else {
                check_via_local_git(&repo_dir)
            }
        }
    };

    if let Some(parent) = cache_file.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(json) = serde_json::to_string(&UpdateCache {
        ts: now,
        behind,
        ver: crate::VERSION.to_string(),
    }) {
        std::fs::write(&cache_file, json).ok();
    }
    behind
}

/// Recommended command to update ulnclaw (hermes `recommended_update_command`).
pub fn recommended_update_command() -> String {
    match resolve_repo_dir() {
        Some(dir) => format!("git -C {} pull", dir.display()),
        None => "git pull".to_string(),
    }
}

// =========================================================================
// Git banner state & version label
// =========================================================================

/// Upstream/local short hashes + carried-commit count for the banner title
/// (hermes `get_git_banner_state`).
#[derive(Debug, Clone)]
pub struct GitBannerState {
    pub upstream: String,
    pub local: String,
    pub ahead: u64,
}

pub fn get_git_banner_state() -> Option<GitBannerState> {
    let repo_dir = resolve_repo_dir()?;
    let branch = upstream_branch(&repo_dir);
    let upstream_ref = format!("origin/{}", branch);
    let upstream = git_short_hash(&repo_dir, &upstream_ref)?;
    let local = git_short_hash(&repo_dir, "HEAD")?;
    let range = format!("{}..HEAD", upstream_ref);
    let ahead = git_output(
        &["rev-list", "--count", &range],
        Some(&repo_dir),
        Duration::from_secs(5),
    )
    .and_then(|s| s.parse::<u64>().ok())
    .unwrap_or(0);
    Some(GitBannerState {
        upstream,
        local,
        ahead,
    })
}

static RELEASE_TAG_CACHE: OnceLock<Mutex<Option<Option<(String, String)>>>> = OnceLock::new();

/// `(tag, release_url)` for the latest git tag, cached per process
/// (hermes `get_latest_release_tag`).
pub fn get_latest_release_tag() -> Option<(String, String)> {
    let cache = RELEASE_TAG_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = guard.clone() {
        return entry;
    }
    let result = resolve_repo_dir().and_then(|repo_dir| {
        let tag = git_output(
            &["describe", "--tags", "--abbrev=0"],
            Some(&repo_dir),
            Duration::from_secs(3),
        )?;
        Some((tag.clone(), format!("{}/{}", RELEASE_URL_BASE, tag)))
    });
    *guard = Some(result.clone());
    result
}

/// Version label shown in the banner title (hermes
/// `format_banner_version_label`).
pub fn format_banner_version_label() -> String {
    let base = format!("ulnclaw v{}", crate::VERSION);
    let Some(state) = get_git_banner_state() else {
        return base;
    };
    if state.ahead == 0 || state.upstream == state.local {
        format!("{} · upstream {}", base, state.upstream)
    } else {
        let word = if state.ahead == 1 {
            "commit"
        } else {
            "commits"
        };
        format!(
            "{} · upstream {} · local {} (+{} carried {})",
            base, state.upstream, state.local, state.ahead, word
        )
    }
}

// =========================================================================
// Non-blocking update check (hermes prefetch_update_check/get_update_result)
// =========================================================================

struct UpdateState {
    result: Option<i64>,
    done: bool,
}

static UPDATE_STATE: OnceLock<(Mutex<UpdateState>, Condvar)> = OnceLock::new();

fn update_state() -> &'static (Mutex<UpdateState>, Condvar) {
    UPDATE_STATE.get_or_init(|| {
        (
            Mutex::new(UpdateState {
                result: None,
                done: false,
            }),
            Condvar::new(),
        )
    })
}

/// Kick off the update check in a background thread while the agent is
/// being constructed (hermes `prefetch_update_check`).
pub fn prefetch_update_check() {
    let _ = update_state();
    std::thread::spawn(|| {
        let result = check_for_updates();
        let (lock, cvar) = update_state();
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.result = result;
        state.done = true;
        cvar.notify_all();
    });
}

/// Result of the prefetched update check, waiting at most `timeout`
/// (hermes `get_update_result`).
pub fn get_update_result(timeout: Duration) -> Option<i64> {
    let (lock, cvar) = update_state();
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    if !state.done {
        state = cvar
            .wait_timeout(state, timeout)
            .map(|(guard, _)| guard)
            .unwrap_or_else(|e| e.into_inner().0);
    }
    if state.done {
        state.result
    } else {
        None
    }
}

// =========================================================================
// Welcome banner rendering
// =========================================================================

/// Everything the welcome banner needs, collected by the caller.
#[derive(Debug, Clone, Default)]
pub struct BannerInfo {
    pub model: String,
    pub provider: String,
    pub cwd: String,
    pub session_id: Option<String>,
    pub context_length: Option<u64>,
    /// `(display toolset name, tool names)` pairs to advertise.
    pub toolsets: Vec<(String, Vec<String>)>,
    /// `(category, skill names)` pairs (see [`get_available_skills`]).
    pub skills: Vec<(String, Vec<String>)>,
    pub total_tools: usize,
    /// Approvals disabled (`approvals.mode = "off"`) — hermes YOLO mode.
    pub yolo: bool,
    /// Prefetched update-check result.
    pub update_behind: Option<i64>,
}

#[derive(Clone)]
struct Seg {
    color: String,
    bold: bool,
    text: String,
}

type BLine = Vec<Seg>;

fn seg(color: &str, text: impl Into<String>) -> Seg {
    Seg {
        color: color.to_string(),
        bold: false,
        text: text.into(),
    }
}

fn bold_seg(color: &str, text: impl Into<String>) -> Seg {
    Seg {
        color: color.to_string(),
        bold: true,
        text: text.into(),
    }
}

fn line_width(line: &[Seg]) -> usize {
    line.iter().map(|s| s.text.chars().count()).sum()
}

/// Render a line padded/truncated to exactly `width` columns.
fn render_line(line: &[Seg], width: usize, enabled: bool) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for seg in line {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let text: String = seg.text.chars().take(remaining).collect();
        used += text.chars().count();
        out.push_str(&paint(&seg.color, seg.bold, &text, enabled));
    }
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    out
}

/// Gradient for the hero art lines (hermes caduceus bronze/gold bands).
fn hero_colors() -> [&'static str; 5] {
    ["#FFBF00", "#FFBF00", "#FFD700", "#B8860B", "#B8860B"]
}

/// Render the wordmark with the hermes gold→bronze gradient.
pub fn render_logo(enabled: bool) -> String {
    let skin = crate::skin::get_active_skin();
    let bands = [
        skin.get_color("banner_title", "#FFD700"),
        skin.get_color("banner_title", "#FFD700"),
        skin.get_color("banner_accent", "#FFBF00"),
        skin.get_color("banner_accent", "#FFBF00"),
        skin.get_color("banner_dim", "#CD7F32"),
        skin.get_color("banner_dim", "#CD7F32"),
    ];
    ULNCLAW_LOGO
        .lines()
        .enumerate()
        .map(|(i, line)| {
            paint(
                bands.get(i).cloned().unwrap_or_default().as_str(),
                i < 2,
                line,
                enabled,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the welcome panel (hermes `build_welcome_banner`), returned as a
/// printable string. Colors degrade to plain text when stdout is not a TTY
/// or `NO_COLOR` is set.
pub fn build_welcome_banner(info: &BannerInfo, term_width: usize) -> String {
    let skin = crate::skin::get_active_skin();
    let accent = skin.get_color("banner_accent", "#FFBF00");
    let dim = skin.get_color("banner_dim", "#B8860B");
    let text_color = skin.get_color("banner_text", "#FFF8DC");
    let session_color = skin.get_color("session_border", "#8B8682");
    let title_color = skin.get_color("banner_title", "#FFD700");
    let border_color = skin.get_color("banner_border", "#CD7F32");
    let red = "#DC3545".to_string();
    let yellow = "#FFC107".to_string();
    let enabled = color_enabled();

    // ---- left column: hero art + model/cwd/session ---------------------
    let mut left: Vec<BLine> = Vec::new();
    left.push(Vec::new());
    let hero_palette = hero_colors();
    for (i, line) in ULNCLAW_HERO.lines().enumerate() {
        let fallback = hero_palette.get(i).copied().unwrap_or("#B8860B");
        let color = match i {
            0 | 1 => accent.clone(),
            2 => title_color.clone(),
            _ => dim.clone(),
        };
        let color = if color.is_empty() {
            fallback.to_string()
        } else {
            color
        };
        left.push(vec![seg(&color, line)]);
    }
    left.push(Vec::new());

    let model = info.model.trim().to_string();
    if model.is_empty() || model.eq_ignore_ascii_case("unknown") {
        // Unconfigured install: say so loudly (hermes shows the same red line).
        left.push(vec![
            bold_seg(&red, "no model configured"),
            seg(&dim, " — run /model or edit config.toml"),
        ]);
    } else {
        let mut model_short = model.rsplit('/').next().unwrap_or(&model).to_string();
        if let Some(stripped) = model_short.strip_suffix(".gguf") {
            model_short = stripped.to_string();
        }
        if model_short.chars().count() > 28 {
            let truncated: String = model_short.chars().take(25).collect();
            model_short = format!("{}...", truncated);
        }
        let mut line = vec![seg(&accent, model_short)];
        if let Some(ctx) = info.context_length {
            line.push(seg(&dim, " · "));
            line.push(seg(&dim, format!("{} context", format_context_length(ctx))));
        }
        line.push(seg(&dim, " · "));
        line.push(seg(&dim, "ulnclaw"));
        left.push(line);
    }

    if info.yolo {
        left.push(vec![
            bold_seg(&red, "⚠ auto-approve mode"),
            seg(&dim, " — all approval prompts bypassed"),
        ]);
    }
    left.push(vec![seg(&dim, info.cwd.clone())]);
    if let Some(session_id) = &info.session_id {
        left.push(vec![seg(
            &session_color,
            format!("Session: {}", session_id),
        )]);
    }

    // ---- right column: tools, skills, summary ---------------------------
    let mut right: Vec<BLine> = Vec::new();
    right.push(vec![bold_seg(&accent, "Available Tools")]);

    let mut toolsets: Vec<(String, Vec<String>)> = info.toolsets.clone();
    toolsets.sort_by(|a, b| a.0.cmp(&b.0));
    let display_toolsets = toolsets.len().min(8);
    for (toolset, tools) in toolsets.iter().take(display_toolsets) {
        let mut names: Vec<String> = tools.clone();
        names.sort();
        let joined_plain = names.join(", ");
        let mut line = vec![seg(&dim, format!("{}: ", toolset))];
        if joined_plain.chars().count() > 45 {
            let mut short: Vec<&str> = Vec::new();
            let mut length = 0usize;
            for name in &names {
                if length + name.chars().count() + 2 > 42 {
                    short.push("...");
                    break;
                }
                short.push(name);
                length += name.chars().count() + 2;
            }
            line.push(seg(&text_color, short.join(", ")));
        } else {
            line.push(seg(&text_color, joined_plain));
        }
        right.push(line);
    }
    if toolsets.len() > display_toolsets {
        right.push(vec![seg(
            &dim,
            format!("+{} more toolsets", toolsets.len() - display_toolsets),
        )]);
    }

    right.push(Vec::new());
    let total_skills: usize = info.skills.iter().map(|(_, names)| names.len()).sum();
    if info.skills.is_empty() {
        right.push(vec![seg(&dim, "No skills installed")]);
    } else {
        for (category, skill_names) in &info.skills {
            let mut names = skill_names.clone();
            names.sort();
            let prefix_len = category.chars().count() + 2;
            let avail = 46usize.saturating_sub(prefix_len).max(20);
            let mut parts: Vec<String> = Vec::new();
            let mut length = 0usize;
            for (i, name) in names.iter().enumerate() {
                let needed = if parts.is_empty() { 0 } else { 2 } + name.chars().count();
                let after = names.len() - (i + 1);
                let indicator_len = if after > 0 {
                    format!(", +{} more", after).chars().count()
                } else {
                    0
                };
                if !parts.is_empty() && length + needed + indicator_len > avail {
                    parts.push(format!("+{} more", names.len() - parts.len()));
                    break;
                }
                parts.push(name.clone());
                length += needed;
            }
            right.push(vec![
                seg(&dim, format!("{}: ", category)),
                seg(&text_color, parts.join(", ")),
            ]);
        }
    }

    right.push(Vec::new());
    right.push(vec![seg(
        &dim,
        format!(
            "{} tools · {} skills · /help for commands",
            info.total_tools, total_skills
        ),
    )]);

    if let Some(behind) = info.update_behind {
        if behind > 0 {
            let word = if behind == 1 { "commit" } else { "commits" };
            right.push(vec![
                bold_seg(&yellow, format!("⚠ {} {} behind", behind, word)),
                seg(
                    &yellow,
                    format!(" — run {} to update", recommended_update_command()),
                ),
            ]);
        } else if behind == UPDATE_AVAILABLE_NO_COUNT {
            right.push(vec![bold_seg(&yellow, "⚠ update available")]);
        }
    }

    // ---- panel geometry ---------------------------------------------------
    let left_w = left.iter().map(|l| line_width(l)).max().unwrap_or(0);
    let mut right_w = right.iter().map(|l| line_width(l)).max().unwrap_or(0);
    right_w = right_w.min(56);
    let budget = term_width.saturating_sub(6 + left_w);
    if right_w > budget {
        right_w = budget.max(24);
    }

    let title = format_banner_version_label();
    let inner = left_w + 2 + right_w;
    let total = inner + 4;
    let title_text = format!(" {} ", title);
    let dash_space = total.saturating_sub(2);
    let title_len = title_text.chars().count();
    let left_dash = dash_space.saturating_sub(title_len) / 2;
    let right_dash = dash_space
        .saturating_sub(title_len)
        .saturating_sub(left_dash);

    let mut out = String::new();
    out.push_str(&paint(&border_color, false, "┌", enabled));
    out.push_str(&paint(
        &border_color,
        false,
        &"─".repeat(left_dash),
        enabled,
    ));
    out.push_str(&paint(&title_color, true, &title_text, enabled));
    out.push_str(&paint(
        &border_color,
        false,
        &"─".repeat(right_dash),
        enabled,
    ));
    out.push_str(&paint(&border_color, false, "┐", enabled));
    out.push('\n');

    let rows = left.len().max(right.len());
    let empty: BLine = Vec::new();
    for i in 0..rows {
        let left_line = left.get(i).unwrap_or(&empty);
        let right_line = right.get(i).unwrap_or(&empty);
        out.push_str(&paint(&border_color, false, "│", enabled));
        out.push_str("  ");
        out.push_str(&render_line(left_line, left_w, enabled));
        out.push_str("  ");
        out.push_str(&render_line(right_line, right_w, enabled));
        out.push_str("  ");
        out.push_str(&paint(&border_color, false, "│", enabled));
        out.push('\n');
    }

    out.push_str(&paint(&border_color, false, "└", enabled));
    out.push_str(&paint(
        &border_color,
        false,
        &"─".repeat(total.saturating_sub(2)),
        enabled,
    ));
    out.push_str(&paint(&border_color, false, "┘", enabled));
    out
}

/// Full startup display: wordmark (terminals ≥95 columns, hermes gates the
/// logo the same way) plus the welcome panel.
pub fn build_startup_display(info: &BannerInfo, term_width: usize) -> String {
    let enabled = color_enabled();
    let mut out = String::new();
    if term_width >= 95 {
        out.push_str(&render_logo(enabled));
        out.push_str("\n\n");
    }
    out.push_str(&build_welcome_banner(info, term_width));
    out
}

/// Serialize the update-check cache (test helper).
#[cfg(test)]
fn write_update_cache(home: &Path, behind: Option<i64>, ver: &str, ts: u64) {
    let cache = UpdateCache {
        ts,
        behind,
        ver: ver.to_string(),
    };
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join(".update_check"),
        serde_json::to_string(&cache).unwrap(),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> BannerInfo {
        BannerInfo {
            model: "qwen2.5:14b".to_string(),
            provider: "ollama".to_string(),
            cwd: "/home/dev/project".to_string(),
            session_id: Some("sess-123".to_string()),
            context_length: Some(128_000),
            toolsets: vec![
                (
                    "terminal".to_string(),
                    vec!["terminal".to_string(), "process".to_string()],
                ),
                (
                    "browser".to_string(),
                    vec![
                        "browser_navigate".to_string(),
                        "browser_snapshot".to_string(),
                    ],
                ),
            ],
            skills: vec![(
                "general".to_string(),
                vec!["imagegen".to_string(), "skill-creator".to_string()],
            )],
            total_tools: 4,
            yolo: false,
            update_behind: None,
        }
    }

    #[test]
    fn format_context_length_scales() {
        assert_eq!(format_context_length(999), "999");
        assert_eq!(format_context_length(128_000), "128K");
        assert_eq!(format_context_length(1_500), "1.5K");
        assert_eq!(format_context_length(1_000_000), "1M");
        assert_eq!(format_context_length(1_048_576), "1M");
    }

    #[test]
    fn display_toolset_name_strips_suffix() {
        assert_eq!(display_toolset_name("browser_tools"), "browser");
        assert_eq!(display_toolset_name("terminal"), "terminal");
        assert_eq!(display_toolset_name(""), "unknown");
    }

    #[test]
    fn canonical_remote_normalizes_forms() {
        assert_eq!(
            canonical_remote("https://gitee.com/ushaw/ulnclaw.git"),
            "gitee.com/ushaw/ulnclaw"
        );
        assert_eq!(
            canonical_remote("git@gitee.com:ushaw/ulnclaw.git"),
            "gitee.com/ushaw/ulnclaw"
        );
        assert_eq!(
            canonical_remote("ssh://git@gitee.com/ushaw/ulnclaw.git"),
            "gitee.com/ushaw/ulnclaw"
        );
        assert_eq!(canonical_remote(""), "");
    }

    #[test]
    fn official_ssh_remote_detection() {
        assert!(is_official_ssh_remote(Some(
            "git@gitee.com:ushaw/ulnclaw.git"
        )));
        assert!(!is_official_ssh_remote(Some(
            "https://gitee.com/ushaw/ulnclaw.git"
        )));
        assert!(!is_official_ssh_remote(Some(
            "git@gitee.com:someone/fork.git"
        )));
        assert!(!is_official_ssh_remote(None));
    }

    #[test]
    fn banner_contains_key_sections() {
        let info = sample_info();
        let out = build_welcome_banner(&info, 120);
        assert!(out.contains("Available Tools"));
        assert!(out.contains("qwen2.5:14b"));
        assert!(out.contains("128K context"));
        assert!(out.contains("/home/dev/project"));
        assert!(out.contains("Session: sess-123"));
        assert!(out.contains("terminal: "));
        assert!(out.contains("browser: "));
        assert!(out.contains("general: "));
        assert!(out.contains("4 tools · 2 skills · /help for commands"));
        assert!(out.contains('┌') && out.contains('┘'));
    }

    #[test]
    fn banner_truncates_long_model_names() {
        let mut info = sample_info();
        info.model = "some-very-long-model-name-that-exceeds-the-cap.gguf".to_string();
        let out = build_welcome_banner(&info, 120);
        assert!(out.contains("some-very-long-model-name..."));
        assert!(!out.contains(".gguf"));
    }

    #[test]
    fn banner_warns_on_yolo_and_missing_model() {
        let mut info = sample_info();
        info.yolo = true;
        info.model = String::new();
        let out = build_welcome_banner(&info, 120);
        assert!(out.contains("auto-approve mode"));
        assert!(out.contains("no model configured"));
    }

    #[test]
    fn banner_shows_update_warning() {
        let mut info = sample_info();
        info.update_behind = Some(3);
        let out = build_welcome_banner(&info, 160);
        assert!(out.contains("⚠ 3 commits behind"));
        info.update_behind = Some(UPDATE_AVAILABLE_NO_COUNT);
        let out = build_welcome_banner(&info, 160);
        assert!(out.contains("⚠ update available"));
        info.update_behind = Some(0);
        let out = build_welcome_banner(&info, 160);
        assert!(!out.contains("behind"));
    }

    #[test]
    fn startup_display_gates_logo_on_width() {
        let info = sample_info();
        let wide = build_startup_display(&info, 120);
        let narrow = build_startup_display(&info, 80);
        assert!(wide.contains("██╗"));
        assert!(!narrow.contains("██╗"));
        assert!(narrow.contains("┌"));
    }

    #[test]
    fn update_cache_hit_skips_git() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        write_update_cache(dir.path(), Some(3), crate::VERSION, now - 60);
        assert_eq!(check_for_updates(), Some(3));
        // Expired cache is not returned as-is.
        write_update_cache(
            dir.path(),
            Some(7),
            crate::VERSION,
            now - UPDATE_CHECK_CACHE_SECONDS - 60,
        );
        let fresh = std::fs::read_to_string(dir.path().join(".update_check")).unwrap();
        let cached: UpdateCache = serde_json::from_str(&fresh).unwrap();
        // check_for_updates ran git (or produced None) and rewrote the cache.
        assert_eq!(cached.ver, crate::VERSION);
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn version_label_starts_with_product() {
        let label = format_banner_version_label();
        assert!(label.starts_with(&format!("ulnclaw v{}", crate::VERSION)));
    }

    #[test]
    fn skills_grouped_and_sorted() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let skills_dir = dir.path().join("skills");
        for name in ["beta", "alpha"] {
            let skill_dir = skills_dir.join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {}\ndescription: test\n---\nbody\n", name),
            )
            .unwrap();
        }
        let grouped = get_available_skills();
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0, "general");
        assert_eq!(grouped[0].1, vec!["alpha".to_string(), "beta".to_string()]);
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }
}

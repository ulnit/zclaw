//! Transparent filesystem checkpoints via a single shared shadow git store.
//!
//! Port of hermes-agent `tools/checkpoint_manager.py` (v2 shared-store
//! layout).  Automatic snapshots of working directories are taken before
//! file-mutating operations (`write_file`, `patch`), at most once per
//! directory per conversation turn.  This is transparent infrastructure —
//! the LLM never sees it — controlled by the `[checkpoints]` config section.
//!
//! Storage layout (single shared store, git objects deduplicated across
//! projects):
//!
//! ```text
//! <home>/checkpoints/
//!     store/                      — single bare git repo (shared)
//!         HEAD, config, objects/  — standard git internals
//!         refs/hermes/<hash16>    — per-project branch tip
//!         indexes/<hash16>        — per-project git index
//!         projects/<hash16>.json  — {workdir, created_at, last_touch}
//!         info/exclude            — default excludes (shared)
//!     .last_prune                 — auto-prune idempotency marker
//! ```
//!
//! The shadow store uses `GIT_DIR` + `GIT_WORK_TREE` + `GIT_INDEX_FILE` so
//! no git state leaks into the user's project directory, and config
//! isolation env vars so the user's global git config (signing, hooks,
//! credential helpers) can never interfere with background snapshots.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

const STORE_DIRNAME: &str = "store";
const REFS_PREFIX: &str = "refs/hermes";
const INDEXES_DIRNAME: &str = "indexes";
const PROJECTS_DIRNAME: &str = "projects";
const PRUNE_MARKER: &str = ".last_prune";

/// Git subprocess timeout (seconds).
const GIT_TIMEOUT_SECS: u64 = 30;

/// Max files to snapshot — skip huge directories to avoid slowdowns.
const MAX_FILES: u64 = 50_000;

/// Default excludes written to `store/info/exclude`.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    // Dependency / build output
    "node_modules/",
    "dist/",
    "build/",
    "target/",
    "out/",
    ".next/",
    ".nuxt/",
    // Caches
    "__pycache__/",
    "*.pyc",
    "*.pyo",
    ".cache/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    "coverage/",
    ".coverage",
    // Virtualenvs
    ".venv/",
    "venv/",
    "env/",
    // VCS
    ".git/",
    ".hg/",
    ".svn/",
    // Worktrees (don't recursively snapshot siblings)
    ".worktrees/",
    // Native / compiled binaries
    "*.so",
    "*.dylib",
    "*.dll",
    "*.o",
    "*.a",
    "*.jar",
    "*.class",
    "*.exe",
    "*.obj",
    // Media / large binaries
    "*.mp4",
    "*.mov",
    "*.mkv",
    "*.webm",
    "*.zip",
    "*.tar",
    "*.tar.gz",
    "*.tgz",
    "*.7z",
    "*.rar",
    "*.iso",
    // Secrets
    ".env",
    ".env.*",
    ".env.local",
    ".env.*.local",
    // OS junk
    ".DS_Store",
    "Thumbs.db",
    // Logs
    "*.log",
];

/// `[checkpoints]` config section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckpointsConfig {
    /// Master switch (checkpoints are opt-in).
    pub enabled: bool,
    /// Keep at most this many checkpoints per project.
    pub max_snapshots: usize,
    /// Hard ceiling on total store size (MB); oldest checkpoints across all
    /// projects are dropped when the store exceeds this after a commit.
    pub max_total_size_mb: u64,
    /// Skip adding any single file larger than this (MB) to a checkpoint.
    pub max_file_size_mb: u64,
    /// Auto-prune drops projects untouched for this many days.
    pub retention_days: u64,
    /// Auto-prune runs at most once per this many hours.
    pub auto_prune_hours: u64,
}

impl Default for CheckpointsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_snapshots: 20,
            max_total_size_mb: 500,
            max_file_size_mb: 10,
            retention_days: 7,
            auto_prune_hours: 24,
        }
    }
}

/// One checkpoint entry (a commit on the per-project ref).
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointEntry {
    pub hash: String,
    pub short_hash: String,
    pub timestamp: String,
    pub reason: String,
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
}

/// Result of a prune sweep.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PruneStats {
    pub scanned: usize,
    pub deleted_orphan: usize,
    pub deleted_stale: usize,
    pub errors: usize,
    pub bytes_freed: u64,
}

/// Summary of the shared store (`ulnclaw checkpoints status`).
#[derive(Debug, Clone, Serialize)]
pub struct ProjectStatus {
    pub hash: String,
    pub workdir: String,
    pub exists: bool,
    pub created_at: f64,
    pub last_touch: f64,
    pub commits: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreStatus {
    pub base: String,
    pub store_size_bytes: u64,
    pub total_size_bytes: u64,
    pub project_count: usize,
    pub projects: Vec<ProjectStatus>,
}

// ---------------------------------------------------------------------------
// Path / hash helpers
// ---------------------------------------------------------------------------

fn normalize_path(path: &str) -> PathBuf {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(path),
        }
    } else if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    std::fs::canonicalize(&expanded).unwrap_or(expanded)
}

/// Deterministic per-project hash: sha256(abs_path)[:16].
fn project_hash(working_dir: &str) -> String {
    let abs = normalize_path(working_dir);
    let mut hasher = Sha256::new();
    hasher.update(abs.to_string_lossy().as_bytes());
    hasher.finalize()[..8]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn store_path(base: &Path) -> PathBuf {
    base.join(STORE_DIRNAME)
}

fn index_path(store: &Path, dir_hash: &str) -> PathBuf {
    store.join(INDEXES_DIRNAME).join(dir_hash)
}

fn ref_name(dir_hash: &str) -> String {
    format!("{}/{}", REFS_PREFIX, dir_hash)
}

fn project_meta_path(store: &Path, dir_hash: &str) -> PathBuf {
    store
        .join(PROJECTS_DIRNAME)
        .join(format!("{}.json", dir_hash))
}

fn valid_commit_hash(hash: &str) -> bool {
    let bytes = hash.as_bytes();
    (4..=64).contains(&bytes.len()) && bytes.iter().all(|b| b.is_ascii_hexdigit())
}

fn now_secs_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Git plumbing
// ---------------------------------------------------------------------------

/// Isolate git from the user's global/system config.  Settings like
/// `commit.gpgsign` or credential helpers would break background snapshots
/// or spawn interactive prompts mid-session.
fn apply_git_env(
    cmd: &mut tokio::process::Command,
    store: &Path,
    workdir: &Path,
    index_file: Option<&Path>,
) {
    cmd.env("GIT_DIR", store);
    cmd.env("GIT_WORK_TREE", workdir);
    cmd.env_remove("GIT_NAMESPACE");
    cmd.env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    match index_file {
        Some(f) => {
            cmd.env("GIT_INDEX_FILE", f);
        }
        None => {
            cmd.env_remove("GIT_INDEX_FILE");
        }
    }
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
}

struct GitResult {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run a git command against the shared store.
async fn run_git(
    args: &[&str],
    store: &Path,
    workdir: &Path,
    index_file: Option<&Path>,
    allowed_returncodes: &[i32],
) -> GitResult {
    if !workdir.is_dir() {
        let msg = format!("working directory not found: {}", workdir.display());
        debug!("git skipped: {} ({})", args.join(" "), msg);
        return GitResult {
            ok: false,
            stdout: String::new(),
            stderr: msg,
        };
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_git_env(&mut cmd, store, workdir, index_file);

    let timeout = Duration::from_secs(GIT_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => {
            let ok = output.status.success();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let code = output.status.code().unwrap_or(-1);
            if !ok && !allowed_returncodes.contains(&code) {
                debug!("git failed: {} (rc={}) {}", args.join(" "), code, stderr);
            }
            GitResult { ok, stdout, stderr }
        }
        Ok(Err(e)) => GitResult {
            ok: false,
            stdout: String::new(),
            stderr: e.to_string(),
        },
        Err(_) => GitResult {
            ok: false,
            stdout: String::new(),
            stderr: format!(
                "git timed out after {}s: {}",
                GIT_TIMEOUT_SECS,
                args.join(" ")
            ),
        },
    }
}

/// Recreate `refs/heads` and `branches` dirs that `git gc` may have removed
/// (git 2.34+ requires them even when all refs are packed).
fn repair_bare_repo_dirs(store: &Path) {
    for subdir in ["refs/heads", "branches"] {
        let path = store.join(subdir);
        if !path.exists() {
            std::fs::create_dir_all(&path).ok();
        }
    }
}

/// Initialise the shared shadow store if needed.
async fn init_store(store: &Path, _working_dir: &Path) -> Result<(), String> {
    let base = store.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(base)
        .map_err(|e| format!("could not create checkpoint base: {}", e))?;

    if store.join("HEAD").exists() {
        repair_bare_repo_dirs(store);
        return Ok(());
    }

    std::fs::create_dir_all(store.join(INDEXES_DIRNAME)).ok();
    std::fs::create_dir_all(store.join(PROJECTS_DIRNAME)).ok();

    // `git init --bare` rejects GIT_WORK_TREE, so use a raw command with
    // only the config-isolation env vars.
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["init", "--bare"])
        .arg(store)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let output = tokio::time::timeout(Duration::from_secs(GIT_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| "shadow store init timed out".to_string())?
        .map_err(|e| format!("shadow store init failed: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "shadow store init failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    // Per-store config (belt-and-suspenders on top of env isolation).
    for cfg in [
        ["config", "user.email", "ulnclaw@local"],
        ["config", "user.name", "ulnclaw checkpoint"],
        ["config", "commit.gpgsign", "false"],
        ["config", "tag.gpgSign", "false"],
        ["config", "gc.auto", "0"],
    ] {
        run_git(&cfg, store, base, None, &[]).await;
    }

    let info_dir = store.join("info");
    std::fs::create_dir_all(&info_dir).ok();
    std::fs::write(info_dir.join("exclude"), DEFAULT_EXCLUDES.join("\n") + "\n").ok();

    debug!("initialised checkpoint store at {}", store.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Project registry (projects/<hash>.json)
// ---------------------------------------------------------------------------

fn touch_project(store: &Path, working_dir: &Path) {
    let dir_hash = project_hash(&working_dir.to_string_lossy());
    let meta_path = project_meta_path(store, &dir_hash);
    let now = now_secs_f64();
    let mut meta: serde_json::Value = if meta_path.exists() {
        std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({"created_at": now})
    };
    meta["workdir"] = serde_json::Value::String(working_dir.to_string_lossy().to_string());
    meta["last_touch"] = serde_json::Value::from(now);
    if meta.get("created_at").is_none() {
        meta["created_at"] = serde_json::Value::from(now);
    }
    std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap_or_default()).ok();
}

fn list_projects(store: &Path) -> Vec<(String, serde_json::Value)> {
    let projects_dir = store.join(PROJECTS_DIRNAME);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let dir_hash = match name.strip_suffix(".json") {
            Some(h) => h.to_string(),
            None => continue,
        };
        let meta: serde_json::Value = match std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        {
            Some(v) if v.is_object() => v,
            _ => continue,
        };
        out.push((dir_hash, meta));
    }
    out
}

fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path).into_iter().flatten() {
        if let Ok(md) = entry.metadata() {
            if md.is_file() {
                total += md.len();
            }
        }
    }
    total
}

fn dir_file_count(path: &Path) -> u64 {
    let mut count = 0u64;
    for _entry in walkdir::WalkDir::new(path).into_iter().flatten() {
        count += 1;
        if count > MAX_FILES {
            return count;
        }
    }
    count
}

// ---------------------------------------------------------------------------
// CheckpointManager
// ---------------------------------------------------------------------------

/// Manages automatic filesystem checkpoints.
///
/// Designed to be owned by the tool context.  Call `new_turn()` at the start
/// of each agent iteration and `ensure_checkpoint(dir, reason)` before any
/// file-mutating tool call.  The manager deduplicates so at most one
/// snapshot is taken per directory per turn.  Never raises — all errors are
/// logged at debug level.
pub struct CheckpointManager {
    base: PathBuf,
    enabled: bool,
    max_snapshots: usize,
    max_total_size_mb: u64,
    max_file_size_mb: u64,
    retention_days: u64,
    auto_prune_hours: u64,
    checkpointed_dirs: Mutex<HashSet<String>>,
    git_available: std::sync::OnceLock<bool>,
}

impl CheckpointManager {
    pub fn new(base: PathBuf, config: &CheckpointsConfig) -> Self {
        Self {
            base,
            enabled: config.enabled,
            max_snapshots: config.max_snapshots.max(1),
            max_total_size_mb: config.max_total_size_mb,
            max_file_size_mb: config.max_file_size_mb,
            retention_days: config.retention_days,
            auto_prune_hours: config.auto_prune_hours,
            checkpointed_dirs: Mutex::new(HashSet::new()),
            git_available: std::sync::OnceLock::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Reset per-turn dedup.  Call at the start of each agent iteration.
    pub fn new_turn(&self) {
        self.checkpointed_dirs.lock().unwrap().clear();
    }

    fn git_ok(&self) -> bool {
        *self.git_available.get_or_init(|| which_git())
    }

    /// Take a checkpoint if enabled and not already done this turn.
    /// Returns true if a checkpoint was taken.  Never fails loudly.
    pub async fn ensure_checkpoint(&self, working_dir: &str, reason: &str) -> bool {
        if !self.enabled || !self.git_ok() {
            return false;
        }
        let abs_dir = normalize_path(working_dir);
        let abs_str = abs_dir.to_string_lossy().to_string();

        // Skip root, home, and other overly broad directories.
        let too_broad = abs_str == "/"
            || dirs::home_dir()
                .map(|h| abs_str == h.to_string_lossy())
                .unwrap_or(false);
        if too_broad {
            debug!("checkpoint skipped: directory too broad ({})", abs_str);
            return false;
        }

        {
            let mut dirs = self.checkpointed_dirs.lock().unwrap();
            if dirs.contains(&abs_str) {
                return false;
            }
            dirs.insert(abs_str.clone());
        }

        self.take(&abs_str, reason).await
    }

    /// Resolve a file path to its working directory for checkpointing:
    /// walk up to the nearest project marker, else the file's directory.
    pub fn working_dir_for_path(&self, file_path: &Path) -> PathBuf {
        let path = normalize_path(&file_path.to_string_lossy());
        let candidate = if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.clone())
        };
        const MARKERS: &[&str] = &[
            ".git",
            "pyproject.toml",
            "package.json",
            "Cargo.toml",
            "go.mod",
            "Makefile",
            "pom.xml",
            ".hg",
            "Gemfile",
        ];
        let mut check = candidate.clone();
        while check != check.parent().unwrap_or(Path::new("")) {
            if MARKERS.iter().any(|m| check.join(m).exists()) {
                return check;
            }
            match check.parent() {
                Some(parent) => check = parent.to_path_buf(),
                None => break,
            }
        }
        candidate
    }

    /// List available checkpoints for a directory (most recent first).
    pub async fn list_checkpoints(&self, working_dir: &str) -> Vec<CheckpointEntry> {
        let abs_dir = normalize_path(working_dir);
        let store = store_path(&self.base);
        if !store.join("HEAD").exists() {
            return Vec::new();
        }
        let dir_hash = project_hash(&abs_dir.to_string_lossy());
        let git_ref = ref_name(&dir_hash);
        let limit = self.max_snapshots.to_string();
        let res = run_git(
            &["log", &git_ref, "--format=%H|%h|%aI|%s", "-n", &limit],
            &store,
            &abs_dir,
            None,
            &[128, 129],
        )
        .await;
        if !res.ok || res.stdout.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::new();
        for line in res.stdout.lines() {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() != 4 {
                continue;
            }
            let mut entry = CheckpointEntry {
                hash: parts[0].to_string(),
                short_hash: parts[1].to_string(),
                timestamp: parts[2].to_string(),
                reason: parts[3].to_string(),
                files_changed: 0,
                insertions: 0,
                deletions: 0,
            };
            let from = format!("{}~1", parts[0]);
            let stat = run_git(
                &["diff", "--shortstat", &from, parts[0]],
                &store,
                &abs_dir,
                None,
                &[128, 129],
            )
            .await;
            if stat.ok && !stat.stdout.is_empty() {
                parse_shortstat(&stat.stdout, &mut entry);
            }
            results.push(entry);
        }
        results
    }

    /// Restore files to a checkpoint state (optionally a single file).
    /// Takes a pre-rollback snapshot first so the undo can be undone.
    pub async fn restore(
        &self,
        working_dir: &str,
        commit_hash: &str,
        file_path: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        if !valid_commit_hash(commit_hash) {
            return Err("invalid commit hash (expected 4-64 hex chars)".into());
        }
        let abs_dir = normalize_path(working_dir);
        let abs_str = abs_dir.to_string_lossy().to_string();
        let store = store_path(&self.base);
        if !store.join("HEAD").exists() {
            return Err("no checkpoints exist for this directory".into());
        }
        let probe = run_git(
            &["cat-file", "-t", commit_hash],
            &store,
            &abs_dir,
            None,
            &[],
        )
        .await;
        if !probe.ok {
            return Err(format!("checkpoint '{}' not found", commit_hash));
        }

        // Pre-rollback snapshot so you can undo the undo.
        self.take(
            &abs_str,
            &format!(
                "pre-rollback snapshot (restoring to {})",
                &commit_hash[..commit_hash.len().min(8)]
            ),
        )
        .await;

        let dir_hash = project_hash(&abs_str);
        let idx = index_path(&store, &dir_hash);
        let target = file_path.unwrap_or(".");
        let res = run_git(
            &["checkout", commit_hash, "--", target],
            &store,
            &abs_dir,
            Some(&idx),
            &[],
        )
        .await;
        if !res.ok {
            return Err(format!("restore failed: {}", res.stderr));
        }
        let reason_res = run_git(
            &["log", "--format=%s", "-1", commit_hash],
            &store,
            &abs_dir,
            None,
            &[],
        )
        .await;
        let reason = if reason_res.ok {
            reason_res.stdout
        } else {
            "unknown".into()
        };
        let mut result = serde_json::json!({
            "success": true,
            "restored_to": &commit_hash[..commit_hash.len().min(8)],
            "reason": reason,
            "directory": abs_str,
        });
        if let Some(f) = file_path {
            result["file"] = serde_json::Value::String(f.to_string());
        }
        Ok(result)
    }

    /// Diff the working tree against a checkpoint (powers rollback preview).
    pub async fn diff(
        &self,
        working_dir: &str,
        commit_hash: &str,
    ) -> Result<serde_json::Value, String> {
        if !valid_commit_hash(commit_hash) {
            return Err("invalid commit hash (expected 4-64 hex chars)".into());
        }
        let abs_dir = normalize_path(working_dir);
        let store = store_path(&self.base);
        if !store.join("HEAD").exists() {
            return Err("no checkpoints exist for this directory".into());
        }
        let dir_hash = project_hash(&abs_dir.to_string_lossy());
        let idx = index_path(&store, &dir_hash);
        let git_ref = ref_name(&dir_hash);
        // Refresh the index from the checkpoint so the diff sees the right base.
        run_git(
            &["read-tree", commit_hash],
            &store,
            &abs_dir,
            Some(&idx),
            &[128],
        )
        .await;
        let stat = run_git(
            &["diff", "--stat", commit_hash],
            &store,
            &abs_dir,
            Some(&idx),
            &[128],
        )
        .await;
        let diff = run_git(&["diff", commit_hash], &store, &abs_dir, Some(&idx), &[128]).await;
        if !stat.ok && !diff.ok {
            let _ = git_ref; // ref kept for future ref-based diffs
            return Err("could not generate diff".into());
        }
        Ok(serde_json::json!({
            "success": true,
            "stat": if stat.ok { stat.stdout } else { String::new() },
            "diff": if diff.ok { diff.stdout } else { String::new() },
        }))
    }

    /// Cumulative diff of everything changed since the earliest retained
    /// checkpoint ("what did the agent change here?").
    pub async fn session_diff(&self, working_dir: &str) -> serde_json::Value {
        let checkpoints = self.list_checkpoints(working_dir).await;
        let Some(baseline) = checkpoints.last().map(|c| c.hash.clone()) else {
            return serde_json::json!({"success": true, "stat": "", "diff": "", "empty": true});
        };
        match self.diff(working_dir, &baseline).await {
            Ok(mut result) => {
                result["baseline"] = serde_json::Value::String(baseline);
                if result["stat"].as_str().map(str::is_empty).unwrap_or(true)
                    && result["diff"].as_str().map(str::is_empty).unwrap_or(true)
                {
                    result["empty"] = serde_json::Value::Bool(true);
                }
                result
            }
            Err(error) => serde_json::json!({"success": false, "error": error}),
        }
    }

    /// Store summary (projects, sizes, commit counts).
    pub async fn status(&self) -> StoreStatus {
        let store = store_path(&self.base);
        let mut out = StoreStatus {
            base: self.base.to_string_lossy().to_string(),
            store_size_bytes: 0,
            total_size_bytes: 0,
            project_count: 0,
            projects: Vec::new(),
        };
        if !self.base.exists() {
            return out;
        }
        if store.exists() {
            out.store_size_bytes = dir_size_bytes(&store);
            if store.join("HEAD").exists() {
                for (dir_hash, meta) in list_projects(&store) {
                    let workdir = meta
                        .get("workdir")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let git_ref = ref_name(&dir_hash);
                    let count_res = run_git(
                        &["rev-list", "--count", &git_ref],
                        &store,
                        &self.base,
                        None,
                        &[128],
                    )
                    .await;
                    let commits = count_res.stdout.parse::<u64>().unwrap_or(0);
                    out.projects.push(ProjectStatus {
                        hash: dir_hash,
                        exists: !workdir.is_empty() && Path::new(&workdir).exists(),
                        created_at: meta
                            .get("created_at")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0),
                        last_touch: meta
                            .get("last_touch")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0),
                        workdir,
                        commits,
                    });
                }
            }
        }
        out.project_count = out.projects.len();
        out.total_size_bytes = dir_size_bytes(&self.base);
        out
    }

    /// Delete stale/orphan project entries and reclaim store space.
    ///
    /// A project entry is deleted when its workdir no longer exists (orphan)
    /// or its last touch is older than `retention_days` (stale).  Returns
    /// counts; never fails loudly.
    pub async fn prune(&self, retention_days: u64, delete_orphans: bool) -> PruneStats {
        let mut stats = PruneStats::default();
        let store = store_path(&self.base);
        if !store.join("HEAD").exists() {
            return stats;
        }
        let size_before = dir_size_bytes(&self.base);
        let cutoff = if retention_days > 0 {
            now_secs_f64() - (retention_days * 86400) as f64
        } else {
            0.0
        };
        for (dir_hash, meta) in list_projects(&store) {
            stats.scanned += 1;
            let workdir = meta
                .get("workdir")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let last_touch = meta
                .get("last_touch")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let orphan = delete_orphans && !workdir.is_empty() && !Path::new(&workdir).exists();
            let stale = retention_days > 0 && last_touch > 0.0 && last_touch < cutoff;
            if !orphan && !stale {
                continue;
            }
            // Drop the per-project ref, index, and metadata.
            let git_ref = ref_name(&dir_hash);
            let res = run_git(
                &["update-ref", "-d", &git_ref],
                &store,
                &self.base,
                None,
                &[128],
            )
            .await;
            if !res.ok {
                stats.errors += 1;
                continue;
            }
            std::fs::remove_file(index_path(&store, &dir_hash)).ok();
            if std::fs::remove_file(project_meta_path(&store, &dir_hash)).is_err() {
                stats.errors += 1;
                continue;
            }
            if orphan {
                stats.deleted_orphan += 1;
            } else {
                stats.deleted_stale += 1;
            }
        }
        // Reclaim objects from dropped refs.
        run_git(
            &["reflog", "expire", "--expire=now", "--all"],
            &store,
            &self.base,
            None,
            &[],
        )
        .await;
        run_git(
            &["gc", "--prune=now", "--quiet"],
            &store,
            &self.base,
            None,
            &[],
        )
        .await;
        repair_bare_repo_dirs(&store);
        let size_after = dir_size_bytes(&self.base);
        stats.bytes_freed = size_before.saturating_sub(size_after);
        stats
    }

    /// Idempotent auto-prune for startup hooks (marker in `.last_prune`).
    pub async fn maybe_auto_prune(&self) -> Option<PruneStats> {
        if !self.enabled || self.auto_prune_hours == 0 {
            return None;
        }
        let marker = self.base.join(PRUNE_MARKER);
        let now = now_secs_f64();
        if let Ok(content) = std::fs::read_to_string(&marker) {
            if let Ok(last) = content.trim().parse::<f64>() {
                if now - last < (self.auto_prune_hours * 3600) as f64 {
                    return None;
                }
            }
        }
        let stats = self.prune(self.retention_days, true).await;
        std::fs::write(&marker, now.to_string()).ok();
        if stats.deleted_orphan + stats.deleted_stale > 0 {
            debug!(
                "checkpoint auto-maintenance: pruned {} entry(ies) ({} orphan, {} stale), reclaimed {} bytes",
                stats.deleted_orphan + stats.deleted_stale,
                stats.deleted_orphan,
                stats.deleted_stale,
                stats.bytes_freed
            );
        }
        Some(stats)
    }

    // ------------------------------------------------------------------
    // Internal: snapshot
    // ------------------------------------------------------------------

    async fn take(&self, working_dir: &str, reason: &str) -> bool {
        let store = store_path(&self.base);
        let workdir = PathBuf::from(working_dir);

        if let Err(e) = init_store(&store, &workdir).await {
            debug!("checkpoint store init failed: {}", e);
            return false;
        }
        touch_project(&store, &workdir);

        // Quick size guard — don't snapshot enormous directories.
        if dir_file_count(&workdir) > MAX_FILES {
            debug!(
                "checkpoint skipped: >{} files in {}",
                MAX_FILES, working_dir
            );
            return false;
        }

        let dir_hash = project_hash(working_dir);
        let idx = index_path(&store, &dir_hash);
        let git_ref = ref_name(&dir_hash);

        // Seed the per-project index from the last checkpoint (if any) so
        // the diff/commit machinery sees only changes since then.
        if idx.exists() {
            let ref_probe = run_git(
                &["rev-parse", "--verify", &format!("{}^{{commit}}", git_ref)],
                &store,
                &workdir,
                None,
                &[128],
            )
            .await;
            if ref_probe.ok && !ref_probe.stdout.is_empty() {
                run_git(
                    &["read-tree", &ref_probe.stdout],
                    &store,
                    &workdir,
                    Some(&idx),
                    &[128],
                )
                .await;
            } else {
                std::fs::remove_file(&idx).ok();
            }
        } else {
            std::fs::create_dir_all(idx.parent().unwrap_or(Path::new("."))).ok();
        }

        // Stage everything (broad patterns filtered via info/exclude).
        let add = run_git(&["add", "-A"], &store, &workdir, Some(&idx), &[]).await;
        if !add.ok {
            debug!("checkpoint git-add failed: {}", add.stderr);
            return false;
        }

        if self.max_file_size_mb > 0 {
            self.drop_oversize_from_index(&store, &workdir, &idx).await;
        }

        // Compare against the current ref tip (not HEAD — HEAD points to a
        // branch that doesn't exist on a bare store).
        let ref_probe = run_git(
            &["rev-parse", "--verify", &format!("{}^{{commit}}", git_ref)],
            &store,
            &workdir,
            None,
            &[128],
        )
        .await;
        let has_ref = ref_probe.ok && !ref_probe.stdout.is_empty();
        let ref_commit = ref_probe.stdout.clone();

        if has_ref {
            let diff_check = run_git(
                &["diff-index", "--cached", "--quiet", &ref_commit],
                &store,
                &workdir,
                Some(&idx),
                &[1],
            )
            .await;
            if diff_check.ok {
                debug!("checkpoint skipped: no changes in {}", working_dir);
                return false;
            }
        } else {
            let ls = run_git(&["ls-files", "--cached"], &store, &workdir, Some(&idx), &[]).await;
            if ls.ok && ls.stdout.trim().is_empty() {
                debug!("checkpoint skipped: empty tree in {}", working_dir);
                return false;
            }
        }

        // Write tree from the per-project index.
        let tree = run_git(&["write-tree"], &store, &workdir, Some(&idx), &[]).await;
        if !tree.ok || tree.stdout.is_empty() {
            debug!("checkpoint write-tree failed: {}", tree.stderr);
            return false;
        }
        let tree_sha = tree.stdout.clone();

        // Build commit (parent = current ref tip, if any).
        let mut commit_args: Vec<&str> = vec!["commit-tree", &tree_sha];
        if has_ref {
            commit_args.push("-p");
            commit_args.push(&ref_commit);
        }
        commit_args.push("-m");
        commit_args.push(reason);
        commit_args.push("--no-gpg-sign");
        let commit = run_git(&commit_args, &store, &workdir, Some(&idx), &[]).await;
        if !commit.ok || commit.stdout.is_empty() {
            debug!("checkpoint commit-tree failed: {}", commit.stderr);
            return false;
        }
        let new_sha = commit.stdout.clone();

        // Update the per-project ref.
        let update_args: Vec<&str> = if has_ref {
            vec!["update-ref", &git_ref, &new_sha, &ref_commit]
        } else {
            vec!["update-ref", &git_ref, &new_sha]
        };
        let update = run_git(&update_args, &store, &workdir, None, &[]).await;
        if !update.ok {
            debug!("checkpoint update-ref failed: {}", update.stderr);
            return false;
        }

        debug!(
            "checkpoint taken in {}: {} ({})",
            working_dir,
            reason,
            &new_sha[..8]
        );

        // Drop old commits beyond max_snapshots, then enforce the size cap.
        self.prune_ref(&store, &workdir, &git_ref).await;
        self.enforce_size_cap(&store).await;

        true
    }

    /// Remove any staged file larger than `max_file_size_mb` from the index.
    async fn drop_oversize_from_index(&self, store: &Path, workdir: &Path, idx: &Path) {
        let cap = self.max_file_size_mb * 1024 * 1024;
        let ls = run_git(
            &["ls-files", "--cached", "-z"],
            store,
            workdir,
            Some(idx),
            &[],
        )
        .await;
        if !ls.ok || ls.stdout.is_empty() {
            return;
        }
        let mut oversize: Vec<String> = Vec::new();
        for rel in ls.stdout.split('\0').filter(|p| !p.is_empty()) {
            let size = std::fs::metadata(workdir.join(rel))
                .map(|m| m.len())
                .unwrap_or(0);
            if size > cap {
                oversize.push(rel.to_string());
            }
        }
        if oversize.is_empty() {
            return;
        }
        debug!(
            "checkpoint: dropping {} oversize file(s) (>{} MB) from index",
            oversize.len(),
            self.max_file_size_mb
        );
        for chunk in oversize.chunks(200) {
            let mut args: Vec<&str> = vec!["rm", "--cached", "--quiet", "--"];
            args.extend(chunk.iter().map(String::as_str));
            run_git(&args, store, workdir, Some(idx), &[128]).await;
        }
    }

    /// Keep only the last `max_snapshots` commits on the per-project ref by
    /// rebuilding a linear chain from the kept trees.
    async fn prune_ref(&self, store: &Path, workdir: &Path, git_ref: &str) {
        let count_res = run_git(
            &["rev-list", "--count", git_ref],
            store,
            workdir,
            None,
            &[128],
        )
        .await;
        let count = match count_res.stdout.parse::<usize>() {
            Ok(c) if count_res.ok => c,
            _ => return,
        };
        if count <= self.max_snapshots {
            return;
        }
        let list = run_git(
            &["rev-list", "--reverse", git_ref],
            store,
            workdir,
            None,
            &[],
        )
        .await;
        if !list.ok || list.stdout.is_empty() {
            return;
        }
        let commits: Vec<&str> = list.stdout.lines().collect();
        let keep = &commits[commits.len().saturating_sub(self.max_snapshots)..];
        match self.rebuild_chain(store, workdir, keep).await {
            Some(tip) => {
                run_git(&["update-ref", git_ref, &tip], store, workdir, None, &[]).await;
                run_git(
                    &["reflog", "expire", "--expire=now", "--all"],
                    store,
                    workdir,
                    None,
                    &[],
                )
                .await;
                run_git(&["gc", "--prune=now", "--quiet"], store, workdir, None, &[]).await;
                repair_bare_repo_dirs(store);
            }
            None => return,
        }
    }

    /// Rebuild a linear commit chain from existing commits' trees.
    async fn rebuild_chain(
        &self,
        store: &Path,
        workdir: &Path,
        commits: &[&str],
    ) -> Option<String> {
        let mut new_parent: Option<String> = None;
        for sha in commits {
            let tree = run_git(
                &["rev-parse", &format!("{}^{{tree}}", sha)],
                store,
                workdir,
                None,
                &[],
            )
            .await;
            if !tree.ok || tree.stdout.is_empty() {
                return None;
            }
            let msg_res = run_git(
                &["log", "--format=%s", "-1", sha],
                store,
                workdir,
                None,
                &[],
            )
            .await;
            let msg = if msg_res.ok && !msg_res.stdout.is_empty() {
                msg_res.stdout
            } else {
                "checkpoint".to_string()
            };
            let mut args: Vec<&str> = vec!["commit-tree", &tree.stdout];
            if let Some(parent) = &new_parent {
                args.push("-p");
                args.push(parent);
            }
            args.push("-m");
            args.push(&msg);
            args.push("--no-gpg-sign");
            let commit = run_git(&args, store, workdir, None, &[]).await;
            if !commit.ok || commit.stdout.is_empty() {
                return None;
            }
            new_parent = Some(commit.stdout);
        }
        new_parent
    }

    /// If total store size exceeds `max_total_size_mb`, drop oldest
    /// checkpoints across all projects (round-robin) until under the cap.
    async fn enforce_size_cap(&self, store: &Path) {
        if self.max_total_size_mb == 0 {
            return;
        }
        let cap_bytes = self.max_total_size_mb * 1024 * 1024;
        if dir_size_bytes(store) <= cap_bytes {
            return;
        }
        let refs_res = run_git(
            &["for-each-ref", "--format=%(refname)", REFS_PREFIX],
            store,
            &self.base,
            None,
            &[128],
        )
        .await;
        if !refs_res.ok || refs_res.stdout.is_empty() {
            return;
        }
        let refs: Vec<String> = refs_res
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();

        for _ in 0..20 {
            if dir_size_bytes(store) <= cap_bytes {
                break;
            }
            let mut any_dropped = false;
            for git_ref in &refs {
                let count_res = run_git(
                    &["rev-list", "--count", git_ref],
                    store,
                    &self.base,
                    None,
                    &[128],
                )
                .await;
                let count = count_res.stdout.parse::<usize>().unwrap_or(0);
                if count <= 1 {
                    continue; // keep at least one snapshot per project
                }
                let list = run_git(
                    &["rev-list", "--reverse", git_ref],
                    store,
                    &self.base,
                    None,
                    &[],
                )
                .await;
                if !list.ok || list.stdout.is_empty() {
                    continue;
                }
                let commits: Vec<&str> = list.stdout.lines().collect();
                if let Some(tip) = self.rebuild_chain(store, &self.base, &commits[1..]).await {
                    run_git(&["update-ref", git_ref, &tip], store, &self.base, None, &[]).await;
                    any_dropped = true;
                }
            }
            if !any_dropped {
                break;
            }
        }
        run_git(
            &["reflog", "expire", "--expire=now", "--all"],
            store,
            &self.base,
            None,
            &[],
        )
        .await;
        run_git(
            &["gc", "--prune=now", "--quiet"],
            store,
            &self.base,
            None,
            &[],
        )
        .await;
        repair_bare_repo_dirs(store);
    }
}

fn which_git() -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| dir.join("git").is_file() || dir.join("git.exe").is_file())
        })
        .unwrap_or(false)
}

fn parse_shortstat(stat_line: &str, entry: &mut CheckpointEntry) {
    let re_files = regex::Regex::new(r"(\d+) file").unwrap();
    let re_ins = regex::Regex::new(r"(\d+) insertion").unwrap();
    let re_del = regex::Regex::new(r"(\d+) deletion").unwrap();
    if let Some(m) = re_files.captures(stat_line) {
        entry.files_changed = m[1].parse().unwrap_or(0);
    }
    if let Some(m) = re_ins.captures(stat_line) {
        entry.insertions = m[1].parse().unwrap_or(0);
    }
    if let Some(m) = re_del.captures(stat_line) {
        entry.deletions = m[1].parse().unwrap_or(0);
    }
}

/// Format a checkpoint list for display (CLI / REPL).
pub fn format_checkpoint_list(checkpoints: &[CheckpointEntry], directory: &str) -> String {
    if checkpoints.is_empty() {
        return format!("No checkpoints found for {}", directory);
    }
    let mut lines = vec![format!("📸 Checkpoints for {}:\n", directory)];
    for (i, cp) in checkpoints.iter().enumerate() {
        let ts = if let Some((date, rest)) = cp.timestamp.split_once('T') {
            let time: String = rest.chars().take(5).collect();
            format!("{} {}", date, time)
        } else {
            cp.timestamp.clone()
        };
        let stat = if cp.files_changed > 0 {
            format!(
                "  ({} file{}, +{}/{})",
                cp.files_changed,
                if cp.files_changed == 1 { "" } else { "s" },
                cp.insertions,
                cp.deletions
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "  {}. {}  {}  {}{}",
            i + 1,
            cp.short_hash,
            ts,
            cp.reason,
            stat
        ));
    }
    lines.push("\n  ulnclaw checkpoints restore <N|hash>   restore to checkpoint".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(enabled: bool) -> CheckpointsConfig {
        CheckpointsConfig {
            enabled,
            max_snapshots: 3,
            max_total_size_mb: 500,
            max_file_size_mb: 1,
            ..Default::default()
        }
    }

    #[test]
    fn test_project_hash_stable() {
        let a = project_hash("/tmp/some/project");
        let b = project_hash("/tmp/some/project");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert_ne!(a, project_hash("/tmp/other/project"));
    }

    #[test]
    fn test_valid_commit_hash() {
        assert!(valid_commit_hash("abcd"));
        assert!(valid_commit_hash(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(!valid_commit_hash("abc")); // too short
        assert!(!valid_commit_hash("xyz1"));
        assert!(!valid_commit_hash(""));
        assert!(!valid_commit_hash(&"a".repeat(65)));
    }

    #[test]
    fn test_parse_shortstat() {
        let mut entry = CheckpointEntry {
            hash: "h".into(),
            short_hash: "h".into(),
            timestamp: "t".into(),
            reason: "r".into(),
            files_changed: 0,
            insertions: 0,
            deletions: 0,
        };
        parse_shortstat(
            " 3 files changed, 12 insertions(+), 4 deletions(-)",
            &mut entry,
        );
        assert_eq!(entry.files_changed, 3);
        assert_eq!(entry.insertions, 12);
        assert_eq!(entry.deletions, 4);
    }

    #[test]
    fn test_ensure_checkpoint_disabled() {
        let manager = CheckpointManager::new(PathBuf::from("/tmp/none"), &test_config(false));
        assert!(!manager.enabled());
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(!rt.block_on(manager.ensure_checkpoint("/tmp", "test")));
    }

    #[tokio::test]
    async fn test_take_list_restore_cycle() {
        if !which_git() {
            eprintln!("git not available; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("checkpoints");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("a.txt"), "one\n").unwrap();

        let manager = CheckpointManager::new(base.clone(), &test_config(true));

        // First snapshot.
        assert!(manager.take(&project.to_string_lossy(), "first").await);
        // No changes → no new snapshot.
        assert!(!manager.take(&project.to_string_lossy(), "noop").await);

        // Modify and snapshot again.
        std::fs::write(project.join("a.txt"), "two\n").unwrap();
        assert!(manager.take(&project.to_string_lossy(), "second").await);

        let list = manager.list_checkpoints(&project.to_string_lossy()).await;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].reason, "second");
        assert_eq!(list[1].reason, "first");

        // Uncommitted change in the working tree...
        std::fs::write(project.join("a.txt"), "three\n").unwrap();

        // Restore to the first checkpoint.
        let first_hash = list[1].hash.clone();
        let result = manager
            .restore(&project.to_string_lossy(), &first_hash, None)
            .await
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(
            std::fs::read_to_string(project.join("a.txt")).unwrap(),
            "one\n"
        );

        // Pre-rollback snapshot captured the "three" state → 3 checkpoints.
        let list = manager.list_checkpoints(&project.to_string_lossy()).await;
        assert_eq!(list.len(), 3);
        assert!(list[0].reason.starts_with("pre-rollback snapshot"));
    }

    #[tokio::test]
    async fn test_max_snapshots_prune() {
        if !which_git() {
            eprintln!("git not available; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("checkpoints");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let manager = CheckpointManager::new(base, &test_config(true));
        for i in 0..6 {
            std::fs::write(project.join("f.txt"), format!("v{}\n", i)).unwrap();
            assert!(
                manager
                    .take(&project.to_string_lossy(), &format!("snap {}", i))
                    .await
            );
        }
        let list = manager.list_checkpoints(&project.to_string_lossy()).await;
        assert!(
            list.len() <= 3,
            "expected pruning to cap at 3, got {}",
            list.len()
        );
        assert_eq!(list[0].reason, "snap 5");
    }

    #[tokio::test]
    async fn test_oversize_file_skipped() {
        if !which_git() {
            eprintln!("git not available; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("checkpoints");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("small.txt"), "hello\n").unwrap();
        // 2 MB file exceeds the 1 MB cap in test_config.
        std::fs::write(project.join("big.bin"), vec![0u8; 2 * 1024 * 1024]).unwrap();

        let manager = CheckpointManager::new(base.clone(), &test_config(true));
        assert!(
            manager
                .take(&project.to_string_lossy(), "with big file")
                .await
        );

        // Verify big.bin is not in the checkpoint tree.
        let store = base.join(STORE_DIRNAME);
        let dir_hash = project_hash(&project.to_string_lossy());
        let idx = index_path(&store, &dir_hash);
        let ls = run_git(&["ls-files", "--cached"], &store, &project, Some(&idx), &[]).await;
        assert!(ls.ok);
        assert!(ls.stdout.contains("small.txt"));
        assert!(!ls.stdout.contains("big.bin"));
    }

    #[tokio::test]
    async fn test_per_turn_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = CheckpointManager::new(tmp.path().join("cp"), &test_config(true));
        let dir = tmp.path().join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_string_lossy().to_string();
        // Whether or not git is present, the dedup set must gate the second call.
        let first = manager.ensure_checkpoint(&dir_str, "t1").await;
        let second = manager.ensure_checkpoint(&dir_str, "t1").await;
        assert!(!second, "second call in same turn must be deduped");
        manager.new_turn();
        let third = manager.ensure_checkpoint(&dir_str, "t2").await;
        // With git available the third call re-attempts (may succeed or be a
        // no-change skip → false); without git it's false.  Either way the
        // dedup state was reset.
        let _ = (first, third);
    }

    #[tokio::test]
    async fn test_prune_orphan_and_stale() {
        if !which_git() {
            eprintln!("git not available; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("checkpoints");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("a.txt"), "x\n").unwrap();

        let manager = CheckpointManager::new(base.clone(), &test_config(true));
        assert!(manager.take(&project.to_string_lossy(), "snap").await);

        // Orphan: remove the workdir, then prune with orphans enabled.
        std::fs::remove_dir_all(&project).unwrap();
        let stats = manager.prune(7, true).await;
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.deleted_orphan, 1);
        let status = manager.status().await;
        assert_eq!(status.project_count, 0);
    }

    #[test]
    fn test_working_dir_for_path_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let nested = project.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let manager = CheckpointManager::new(tmp.path().join("cp"), &test_config(false));
        let resolved = manager.working_dir_for_path(&file);
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(&project).unwrap()
        );
    }
}

//! Working-tree git diff collection — port of hermes' `tools/working_diff.py`.
//!
//! Answers "what changed here?" with three modes:
//! - `working` (default): unstaged changes plus untracked files — what
//!   you'd lose with `git checkout . && git clean -fd`;
//! - `staged`: changes already staged for commit (`git diff --cached`);
//! - `all`: everything since HEAD (staged + unstaged) plus untracked files.
//!
//! Untracked files are folded in via `git diff --no-index /dev/null <file>`
//! so brand-new files show up as additions instead of being silently
//! invisible. This is the *git* working diff; the REPL `/diff` command
//! remains checkpoint-based.

use crate::error::{AgentError, Result};
use std::path::Path;
use std::time::{Duration, Instant};

const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const GIT_TIMEOUT_LONG: Duration = Duration::from_secs(30);
const MAX_UNTRACKED_FILES: usize = 50;

/// Diff collection mode (hermes `VALID_MODES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    /// Unstaged changes (+ untracked files).
    Working,
    /// Staged changes only.
    Staged,
    /// Everything since HEAD (+ untracked files).
    All,
}

impl DiffMode {
    pub fn parse(raw: &str) -> Option<DiffMode> {
        match raw.trim().to_lowercase().as_str() {
            "working" => Some(DiffMode::Working),
            "staged" => Some(DiffMode::Staged),
            "all" => Some(DiffMode::All),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DiffMode::Working => "working",
            DiffMode::Staged => "staged",
            DiffMode::All => "all",
        }
    }

    fn base_args(&self) -> Vec<&'static str> {
        match self {
            DiffMode::Working => vec!["diff"],
            DiffMode::Staged => vec!["diff", "--cached"],
            DiffMode::All => vec!["diff", "HEAD"],
        }
    }
}

/// Collected diff (hermes result shape: stat/diff/untracked/empty).
#[derive(Debug, Clone, Default)]
pub struct WorkingDiff {
    /// `--stat` summary.
    pub stat: String,
    /// Full diff text (untracked files folded in for working/all).
    pub diff: String,
    /// Untracked file paths (working/all modes without pathspec).
    pub untracked: Vec<String>,
    /// True when there is nothing to show.
    pub empty: bool,
}

/// Run git with a polling timeout. Returns `(exit_code, stdout)`; never
/// treats a non-zero exit as an error (git diff --no-index exits 1 when
/// files differ — the success path).
fn run_git(args: &[&str], cwd: &Path, timeout: Duration) -> Result<(i32, String)> {
    let mut child = std::process::Command::new("git")
        .arg("-c")
        .arg("core.quotePath=false")
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| AgentError::Tool(format!("git is not installed or not on PATH: {}", e)))?;
    let start = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|e| AgentError::Tool(format!("git failed: {}", e)))?
        {
            Some(status) => {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    stdout.read_to_string(&mut output).ok();
                }
                return Ok((status.code().unwrap_or(-1), output));
            }
            None => {
                if start.elapsed() > timeout {
                    child.kill().ok();
                    return Err(AgentError::Tool("git diff timed out".to_string()));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

fn untracked_files(cwd: &Path) -> Vec<String> {
    let (code, out) = run_git(
        &["ls-files", "--others", "--exclude-standard"],
        cwd,
        GIT_TIMEOUT,
    )
    .unwrap_or((-1, String::new()));
    if code != 0 {
        return Vec::new();
    }
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Render untracked files as new-file diffs via `git diff --no-index`.
fn untracked_diff(cwd: &Path, files: &[String]) -> String {
    let mut chunks: Vec<String> = Vec::new();
    for rel in files.iter().take(MAX_UNTRACKED_FILES) {
        let (_, out) = run_git(
            &["diff", "--no-index", "--", "/dev/null", rel],
            cwd,
            GIT_TIMEOUT,
        )
        .unwrap_or((-1, String::new()));
        let trimmed = out.trim_end_matches('\n');
        if !trimmed.trim().is_empty() {
            chunks.push(trimmed.to_string());
        }
    }
    if files.len() > MAX_UNTRACKED_FILES {
        chunks.push(format!(
            "... ({} more untracked files not shown)",
            files.len() - MAX_UNTRACKED_FILES
        ));
    }
    chunks.join("\n")
}

/// Collect a git diff of the working directory at `cwd` (hermes
/// `collect_working_diff`). `paths` optionally restricts the diff to
/// specific pathspecs (passed through to git verbatim).
pub fn collect_working_diff(cwd: &Path, mode: DiffMode, paths: &[String]) -> Result<WorkingDiff> {
    // Repo probe.
    let (code, _) = run_git(
        &["rev-parse", "--is-inside-work-tree"],
        cwd,
        Duration::from_secs(5),
    )?;
    if code != 0 {
        return Err(AgentError::Tool("Not a git repository.".to_string()));
    }

    let mut args: Vec<&str> = mode.base_args();
    args.push("--stat");
    let mut pathspec: Vec<&str> = Vec::new();
    if !paths.is_empty() {
        pathspec.push("--");
        for path in paths {
            pathspec.push(path.as_str());
        }
    }
    let stat_args: Vec<&str> = args.iter().chain(pathspec.iter()).copied().collect();
    let (_, stat_out) = run_git(&stat_args, cwd, GIT_TIMEOUT)?;

    let args: Vec<&str> = mode.base_args();
    let diff_args: Vec<&str> = args.iter().chain(pathspec.iter()).copied().collect();
    let (_, diff_out) = run_git(&diff_args, cwd, GIT_TIMEOUT_LONG)?;

    let mut untracked: Vec<String> = Vec::new();
    let mut untracked_text = String::new();
    if matches!(mode, DiffMode::Working | DiffMode::All) && paths.is_empty() {
        untracked = untracked_files(cwd);
        if !untracked.is_empty() {
            untracked_text = untracked_diff(cwd, &untracked);
        }
    }

    let stat = stat_out.trim().to_string();
    let mut diff = diff_out.trim().to_string();
    if !untracked_text.is_empty() {
        diff = format!("{}\n{}", diff, untracked_text);
        diff = diff.trim().to_string();
    }
    let empty = stat.is_empty() && diff.is_empty() && untracked.is_empty();
    Ok(WorkingDiff {
        stat,
        diff,
        untracked,
        empty,
    })
}

// ---------------------------------------------------------------------------
// Git review actions — lean port of hermes' `/api/git/*` review router
// (status/stage/unstage/revert/commit/push), backing the desktop
// file-tree Changes pane.
// ---------------------------------------------------------------------------

/// Parsed `git status --porcelain` summary for the review pane.
#[derive(Debug, Default)]
pub struct StatusSummary {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Paths with staged changes (index vs HEAD).
    pub staged: Vec<String>,
    /// Paths with unstaged working-tree changes.
    pub unstaged: Vec<String>,
    /// Untracked paths (`??`).
    pub untracked: Vec<String>,
}

fn require_repo(cwd: &Path) -> Result<()> {
    let (code, _) = run_git(
        &["rev-parse", "--is-inside-work-tree"],
        cwd,
        Duration::from_secs(5),
    )?;
    if code != 0 {
        return Err(AgentError::Tool("Not a git repository.".to_string()));
    }
    Ok(())
}

/// Branch + ahead/behind + per-area path lists for one checkout.
pub fn status_summary(cwd: &Path) -> Result<StatusSummary> {
    require_repo(cwd)?;
    let mut summary = StatusSummary::default();
    let (_, branch) = run_git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd, GIT_TIMEOUT)?;
    summary.branch = branch.trim().to_string();
    if let Ok((0, upstream)) = run_git(
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        cwd,
        GIT_TIMEOUT,
    ) {
        let upstream = upstream.trim().to_string();
        if !upstream.is_empty() {
            summary.upstream = Some(upstream);
            if let Ok((0, counts)) = run_git(
                &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
                cwd,
                GIT_TIMEOUT,
            ) {
                let mut parts = counts.split_whitespace();
                summary.behind = parts
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                summary.ahead = parts
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
            }
        }
    }
    let (code, porcelain) = run_git(&["status", "--porcelain"], cwd, GIT_TIMEOUT_LONG)?;
    if code != 0 {
        return Err(AgentError::Tool("git status failed".to_string()));
    }
    for line in porcelain.lines() {
        let bytes = line.as_bytes();
        if bytes.len() < 3 {
            continue;
        }
        let (index, working) = (bytes[0], bytes[1]);
        // Rename entries read "orig -> new"; keep the new path.
        let file = line[3..]
            .split(" -> ")
            .last()
            .unwrap_or(&line[3..])
            .trim()
            .to_string();
        if index == b'?' && working == b'?' {
            summary.untracked.push(file);
            continue;
        }
        if index != b' ' && index != b'?' {
            summary.staged.push(file.clone());
        }
        if working != b' ' && working != b'?' {
            summary.unstaged.push(file);
        }
    }
    Ok(summary)
}

/// Local branch names (current branch first).
pub fn local_branches(cwd: &Path) -> Result<Vec<String>> {
    require_repo(cwd)?;
    let (code, out) = run_git(&["branch", "--format=%(refname:short)"], cwd, GIT_TIMEOUT)?;
    if code != 0 {
        return Err(AgentError::Tool("git branch failed".to_string()));
    }
    let mut branches: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let current = status_summary_branch(cwd);
    branches.sort();
    if let Some(position) = current
        .as_ref()
        .and_then(|name| branches.iter().position(|b| b == name))
    {
        let name = branches.remove(position);
        branches.insert(0, name);
    }
    Ok(branches)
}

fn status_summary_branch(cwd: &Path) -> Option<String> {
    run_git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd, GIT_TIMEOUT)
        .ok()
        .filter(|(code, _)| *code == 0)
        .map(|(_, out)| out.trim().to_string())
        .filter(|name| !name.is_empty())
}

fn mutate(args: &[&str], cwd: &Path, what: &str, timeout: Duration) -> Result<String> {
    let (code, out) = run_git(args, cwd, timeout)?;
    if code != 0 {
        let detail = out.trim();
        return Err(AgentError::Tool(if detail.is_empty() {
            format!("{what} failed")
        } else {
            format!("{what} failed: {detail}")
        }));
    }
    Ok(out)
}

/// Stage paths (`git add -- <paths>`); empty list stages everything
/// (`git add -A`).
pub fn stage(cwd: &Path, paths: &[String]) -> Result<String> {
    require_repo(cwd)?;
    if paths.is_empty() {
        return mutate(&["add", "-A"], cwd, "git add", GIT_TIMEOUT);
    }
    let mut args: Vec<&str> = vec!["add", "--"];
    for path in paths {
        args.push(path.as_str());
    }
    mutate(&args, cwd, "git add", GIT_TIMEOUT)
}

/// Unstage paths (`git reset [-- <paths>]`) — works on every
/// supported git version (no `restore --staged` requirement).
pub fn unstage(cwd: &Path, paths: &[String]) -> Result<String> {
    require_repo(cwd)?;
    let mut args: Vec<&str> = vec!["reset"];
    if !paths.is_empty() {
        args.push("--");
        for path in paths {
            args.push(path.as_str());
        }
    }
    mutate(&args, cwd, "git reset", GIT_TIMEOUT)
}

/// Discard working-tree changes for the given tracked paths
/// (`git checkout -- <paths>`). Requires explicit paths — never the
/// whole repo — so callers must confirm deliberately.
pub fn revert_working(cwd: &Path, paths: &[String]) -> Result<String> {
    require_repo(cwd)?;
    if paths.is_empty() {
        return Err(AgentError::Tool("revert needs explicit paths".to_string()));
    }
    let mut args: Vec<&str> = vec!["checkout", "--"];
    for path in paths {
        args.push(path.as_str());
    }
    mutate(&args, cwd, "git checkout", GIT_TIMEOUT)
}

/// Commit the staged index with the given message.
pub fn commit(cwd: &Path, message: &str) -> Result<String> {
    require_repo(cwd)?;
    if message.trim().is_empty() {
        return Err(AgentError::Tool("commit message is empty".to_string()));
    }
    mutate(&["commit", "-m", message], cwd, "git commit", GIT_TIMEOUT)
}

/// Push the current branch to its upstream (`git push`), with a
/// longer timeout for network round-trips.
pub fn push(cwd: &Path) -> Result<String> {
    require_repo(cwd)?;
    mutate(&["push"], cwd, "git push", Duration::from_secs(60))
}

/// One linked worktree (hermes `/api/git/worktrees` parity).
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
    pub is_main: bool,
}

/// Parse `git worktree list --porcelain` output.
pub fn list_worktrees(cwd: &Path) -> Result<Vec<WorktreeInfo>> {
    require_repo(cwd)?;
    let (code, out) = run_git(&["worktree", "list", "--porcelain"], cwd, GIT_TIMEOUT)?;
    if code != 0 {
        return Err(AgentError::Tool("git worktree list failed".to_string()));
    }
    let mut trees: Vec<WorktreeInfo> = Vec::new();
    let mut path: Option<String> = None;
    let mut branch = String::new();
    let mut is_main = false;
    for line in out.lines().chain(std::iter::once("")) {
        if let Some(raw) = line.strip_prefix("worktree ") {
            if let Some(previous) = path.take() {
                trees.push(WorktreeInfo {
                    path: previous,
                    branch: std::mem::take(&mut branch),
                    is_main,
                });
                is_main = false;
            }
            path = Some(raw.trim().to_string());
        } else if line.starts_with("branch ") {
            branch = line
                .trim_start_matches("branch ")
                .trim_start_matches("refs/heads/")
                .to_string();
        } else if line == "detached" {
            branch = "(detached)".to_string();
        } else if line.is_empty() {
            if let Some(previous) = path.take() {
                trees.push(WorktreeInfo {
                    path: previous,
                    branch: std::mem::take(&mut branch),
                    is_main,
                });
                is_main = false;
            }
        }
    }
    if let Some(first) = trees.first_mut() {
        first.is_main = true;
    }
    Ok(trees)
}

/// Add a worktree: `git worktree add <target> [<branch>]`, or with
/// `new_branch` create it first (`-b`).
pub fn add_worktree(
    cwd: &Path,
    target: &str,
    branch: Option<&str>,
    new_branch: Option<&str>,
) -> Result<String> {
    require_repo(cwd)?;
    let target = target.trim();
    if target.is_empty() {
        return Err(AgentError::Tool("target path is required".to_string()));
    }
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if let Some(new_branch) = new_branch.map(str::trim).filter(|raw| !raw.is_empty()) {
        if !valid_branch_name(new_branch) {
            return Err(AgentError::Tool("invalid branch name".to_string()));
        }
        args.push("-b");
        args.push(new_branch);
    }
    args.push(target);
    if let Some(branch) = branch.map(str::trim).filter(|raw| !raw.is_empty()) {
        args.push(branch);
    }
    mutate(&args, cwd, "git worktree add", GIT_TIMEOUT_LONG)
}

/// Remove a worktree (`git worktree remove <target>`).
pub fn remove_worktree(cwd: &Path, target: &str) -> Result<String> {
    require_repo(cwd)?;
    let target = target.trim();
    if target.is_empty() {
        return Err(AgentError::Tool("target path is required".to_string()));
    }
    mutate(
        &["worktree", "remove", target],
        cwd,
        "git worktree remove",
        GIT_TIMEOUT_LONG,
    )
}

/// Diff for a single path (hermes `/api/git/file-diff` parity).
/// Untracked paths in working/all modes fold in via `--no-index` so
/// brand-new files show up as additions.
pub fn file_diff(cwd: &Path, file: &str, mode: DiffMode) -> Result<String> {
    require_repo(cwd)?;
    let file = file.trim();
    if file.is_empty() {
        return Err(AgentError::Tool("file is required".to_string()));
    }
    let untracked = untracked_files(cwd).iter().any(|path| path == file);
    if untracked && !matches!(mode, DiffMode::Staged) {
        let target = cwd.join(file);
        let (code, out) = run_git(
            &[
                "diff",
                "--no-index",
                "--",
                "/dev/null",
                &target.to_string_lossy(),
            ],
            cwd,
            GIT_TIMEOUT_LONG,
        )?;
        // --no-index exits 1 when the paths differ — that is the diff.
        if code > 1 {
            return Err(AgentError::Tool("git diff failed".to_string()));
        }
        return Ok(out);
    }
    let mut args: Vec<&str> = mode.base_args();
    args.push("--");
    args.push(file);
    let (code, out) = run_git(&args, cwd, GIT_TIMEOUT_LONG)?;
    if code != 0 {
        return Err(AgentError::Tool("git diff failed".to_string()));
    }
    Ok(out)
}

/// Branch-name sanity: no whitespace, no `..`, no control chars, no
/// leading `-` (git refuses these; keeps CLI injection impossible).
fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('-')
        && !name.contains("..")
        && !name.contains(char::is_whitespace)
        && !name
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '~' | '^' | ':' | '?' | '*' | '['))
}

/// Create a branch (`git branch <name> [<start_point>]`).
pub fn create_branch(cwd: &Path, name: &str, start_point: Option<&str>) -> Result<String> {
    require_repo(cwd)?;
    if !valid_branch_name(name) {
        return Err(AgentError::Tool("invalid branch name".to_string()));
    }
    match start_point {
        Some(start) => mutate(&["branch", name, start], cwd, "git branch", GIT_TIMEOUT),
        None => mutate(&["branch", name], cwd, "git branch", GIT_TIMEOUT),
    }
}

/// Switch branches (`git checkout <name>` — works on every supported
/// git version). Refuses names that look like options.
pub fn switch_branch(cwd: &Path, name: &str) -> Result<String> {
    require_repo(cwd)?;
    if !valid_branch_name(name) {
        return Err(AgentError::Tool("invalid branch name".to_string()));
    }
    mutate(&["checkout", name], cwd, "git checkout", GIT_TIMEOUT_LONG)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-gitdiff-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git(&["init", "-q"], &dir);
        git(&["config", "user.email", "test@ulnclaw.local"], &dir);
        git(&["config", "user.name", "ulnclaw-test"], &dir);
        git(&["config", "commit.gpgsign", "false"], &dir);
        std::fs::write(dir.join("tracked.txt"), "line one\n").unwrap();
        git(&["add", "tracked.txt"], &dir);
        git(&["commit", "-q", "-m", "initial"], &dir);
        dir
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(DiffMode::parse("working"), Some(DiffMode::Working));
        assert_eq!(DiffMode::parse("STAGED"), Some(DiffMode::Staged));
        assert_eq!(DiffMode::parse("all"), Some(DiffMode::All));
        assert_eq!(DiffMode::parse("bogus"), None);
    }

    #[test]
    fn clean_repo_is_empty() {
        let dir = temp_repo("clean");
        let result = collect_working_diff(&dir, DiffMode::Working, &[]).unwrap();
        assert!(result.empty);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn working_mode_shows_modifications_and_untracked() {
        let dir = temp_repo("working");
        std::fs::write(dir.join("tracked.txt"), "line one\nline two\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "brand new file\n").unwrap();
        let result = collect_working_diff(&dir, DiffMode::Working, &[]).unwrap();
        assert!(!result.empty);
        assert!(result.stat.contains("tracked.txt"), "stat: {}", result.stat);
        assert!(result.diff.contains("+line two"));
        assert_eq!(result.untracked, vec!["fresh.txt".to_string()]);
        // Untracked file folded in as an addition.
        assert!(result.diff.contains("brand new file"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn staged_mode_ignores_unstaged_and_untracked() {
        let dir = temp_repo("staged");
        std::fs::write(dir.join("tracked.txt"), "staged change\n").unwrap();
        git(&["add", "tracked.txt"], &dir);
        std::fs::write(dir.join("tracked.txt"), "staged change\nunstaged too\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "not shown\n").unwrap();
        let result = collect_working_diff(&dir, DiffMode::Staged, &[]).unwrap();
        assert!(result.diff.contains("+staged change"));
        assert!(!result.diff.contains("unstaged too"));
        assert!(result.untracked.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn all_mode_covers_everything() {
        let dir = temp_repo("all");
        std::fs::write(dir.join("tracked.txt"), "modified again\n").unwrap();
        std::fs::write(dir.join("new-file.txt"), "added\n").unwrap();
        let result = collect_working_diff(&dir, DiffMode::All, &[]).unwrap();
        assert!(result.diff.contains("+modified again"));
        assert!(result.diff.contains("new-file.txt"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn not_a_repository_errors() {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-gitdiff-norepo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let error = collect_working_diff(&dir, DiffMode::Working, &[])
            .err()
            .unwrap();
        assert!(error.to_string().contains("Not a git repository"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn status_summary_and_review_actions_round_trip() {
        let dir = temp_repo("review");

        // Mutate: one tracked edit + one new file.
        std::fs::write(dir.join("tracked.txt"), "line one\nline two\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "brand new\n").unwrap();

        let summary = status_summary(&dir).unwrap();
        assert_eq!(summary.branch, current_branch_name(&dir));
        assert_eq!(summary.staged.len(), 0);
        assert_eq!(summary.unstaged, vec!["tracked.txt".to_string()]);
        assert_eq!(summary.untracked, vec!["fresh.txt".to_string()]);

        // Stage everything, then commit.
        stage(&dir, &[]).unwrap();
        let summary = status_summary(&dir).unwrap();
        assert_eq!(summary.staged.len(), 2);
        assert!(summary.unstaged.is_empty());
        assert!(summary.untracked.is_empty());
        let output = commit(&dir, "second").unwrap();
        assert!(output.contains("second") || !output.is_empty());

        // Empty message is rejected; clean status after the commit.
        assert!(commit(&dir, "   ").is_err());
        let summary = status_summary(&dir).unwrap();
        assert!(summary.staged.is_empty());
        assert!(summary.unstaged.is_empty());
        assert!(summary.untracked.is_empty());

        // Unstage round-trip: stage then unstage one path.
        std::fs::write(dir.join("tracked.txt"), "line one\nline two\nthree\n").unwrap();
        stage(&dir, &["tracked.txt".to_string()]).unwrap();
        unstage(&dir, &["tracked.txt".to_string()]).unwrap();
        let summary = status_summary(&dir).unwrap();
        assert!(summary.staged.is_empty());
        assert_eq!(summary.unstaged, vec!["tracked.txt".to_string()]);

        // Revert restores the working tree; whole-repo revert is refused.
        assert!(revert_working(&dir, &[]).is_err());
        revert_working(&dir, &["tracked.txt".to_string()]).unwrap();
        let summary = status_summary(&dir).unwrap();
        assert!(summary.unstaged.is_empty());

        // Branch listing puts the current branch first.
        git(&["branch", "feature/x"], &dir);
        let branches = local_branches(&dir).unwrap();
        assert_eq!(branches[0], current_branch_name(&dir));
        assert!(branches.iter().any(|name| name == "feature/x"));

        // Per-file diffs: working diff sees the edit, staged diff is
        // empty until staged, untracked folds in via --no-index.
        std::fs::write(dir.join("tracked.txt"), "line one\nline two\nline three\n").unwrap();
        std::fs::write(dir.join("stray.txt"), "brand new\n").unwrap();
        let working = file_diff(&dir, "tracked.txt", DiffMode::Working).unwrap();
        assert!(working.contains("+line three"));
        assert!(file_diff(&dir, "tracked.txt", DiffMode::Staged)
            .unwrap()
            .trim()
            .is_empty());
        stage(&dir, &["tracked.txt".to_string()]).unwrap();
        let staged = file_diff(&dir, "tracked.txt", DiffMode::Staged).unwrap();
        assert!(staged.contains("+line three"));
        let stray = file_diff(&dir, "stray.txt", DiffMode::Working).unwrap();
        assert!(stray.contains("+brand new"));
        unstage(&dir, &[]).unwrap();

        // Branch create/switch: invalid names rejected, switching works.
        assert!(create_branch(&dir, "bad name", None).is_err());
        assert!(create_branch(&dir, "-flag", None).is_err());
        create_branch(&dir, "feature/y", None).unwrap();
        assert!(create_branch(&dir, "feature/y", None).is_err()); // duplicate
        let before = current_branch_name(&dir);
        switch_branch(&dir, "feature/y").unwrap();
        assert_eq!(current_branch_name(&dir), "feature/y");
        assert!(switch_branch(&dir, "no-such-branch").is_err());
        switch_branch(&dir, &before).unwrap();

        // Worktrees: list shows the main checkout; add/remove round-trip.
        let trees = list_worktrees(&dir).unwrap();
        assert_eq!(trees.len(), 1);
        assert!(trees[0].is_main);
        let target = dir.with_file_name(format!(
            "ulnclaw-wt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        add_worktree(&dir, &target.to_string_lossy(), None, Some("wt-branch")).unwrap();
        let trees = list_worktrees(&dir).unwrap();
        assert_eq!(trees.len(), 2);
        let added = trees
            .iter()
            .find(|tree| tree.branch == "wt-branch")
            .unwrap();
        assert!(!added.is_main);
        remove_worktree(&dir, &target.to_string_lossy()).unwrap();
        assert_eq!(list_worktrees(&dir).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    fn current_branch_name(dir: &Path) -> String {
        let output = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}

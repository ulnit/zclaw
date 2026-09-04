//! Self-updater — port of `hermes update` (`hermes_cli/update_cmd.py`,
//! `hermes_cli/subcommands/update.py`), adapted to a Rust git checkout:
//! fetch/compare for `--check`, stash → fast-forward pull → stash restore
//! → `cargo build --release` for the apply path.
//!
//! Hermes-specific machinery that has no ulnclaw counterpart (venv/pip
//! refresh, npm lockfiles, Windows exe locking, desktop (Electron) hand-off,
//! docker/nix install methods, systemd gateway restarts) is intentionally
//! out of scope; the git core is ported faithfully:
//! - upstream-preferred fetch for the default branch, origin otherwise
//! - shallow-checkout awareness (`--depth 1` fetch, presence-only report)
//! - fetch error classification (network / auth / generic)
//! - compare-ref verification before counting
//! - auto-stash with untracked files + unmerged-index cleanup
//! - fork detection via origin URL

use std::path::{Path, PathBuf};
use std::process::Command;

/// Official ulnclaw repository (hermes `OFFICIAL_REPO_URL`).
pub const OFFICIAL_REPO_URL: &str = "https://gitee.com/ushaw/ulnclaw.git";

/// Options for `ulnclaw update` (hermes `cmd_update` args minus the
/// Python/Windows-only flags).
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// Only report whether an update is available.
    pub check: bool,
    /// Update against this branch instead of the current one.
    pub branch: Option<String>,
    /// Assume yes for interactive prompts (accepted for hermes parity;
    /// the ulnclaw flow is non-interactive).
    pub yes: bool,
}

struct GitOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn git(cwd: &Path, args: &[&str]) -> GitOutput {
    let output = Command::new("git").args(args).current_dir(cwd).output();
    match output {
        Ok(o) => GitOutput {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
        },
        Err(e) => GitOutput {
            ok: false,
            stdout: String::new(),
            stderr: e.to_string(),
        },
    }
}

/// Locate the ulnclaw git checkout: probe the executable's directory first
/// (developer trees run the binary from `target/...` inside the repo),
/// then the current working directory.
pub fn find_repo_root() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd);
    }
    for candidate in candidates {
        let out = git(&candidate, &["rev-parse", "--show-toplevel"]);
        if out.ok && !out.stdout.is_empty() {
            return Some(PathBuf::from(out.stdout));
        }
    }
    None
}

fn is_shallow(root: &Path) -> bool {
    git(root, &["rev-parse", "--is-shallow-repository"]).stdout == "true"
}

fn has_upstream_remote(root: &Path) -> bool {
    git(root, &["remote", "get-url", "upstream"]).ok
}

fn get_origin_url(root: &Path) -> Option<String> {
    let out = git(root, &["remote", "get-url", "origin"]);
    if out.ok && !out.stdout.is_empty() {
        Some(out.stdout)
    } else {
        None
    }
}

/// True when origin does not point at the official repo (hermes `_is_fork`).
pub fn is_fork(origin_url: Option<&str>) -> bool {
    let Some(url) = origin_url else { return false };
    let normalize = |u: &str| {
        let mut n = u.trim_end_matches('/').to_string();
        if n.ends_with(".git") {
            n.truncate(n.len() - 4);
        }
        n.to_lowercase()
    };
    normalize(url) != normalize(OFFICIAL_REPO_URL)
}

fn capture_head_sha(root: &Path) -> Option<String> {
    let out = git(root, &["rev-parse", "HEAD"]);
    if out.ok && !out.stdout.is_empty() {
        Some(out.stdout)
    } else {
        None
    }
}

fn count_commits_between(root: &Path, base: &str, head: &str) -> i64 {
    let range = format!("{base}..{head}");
    let out = git(root, &["rev-list", "--count", &range]);
    if out.ok {
        out.stdout.parse().unwrap_or(-1)
    } else {
        -1
    }
}

fn current_branch(root: &Path) -> Option<String> {
    let out = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if out.ok && !out.stdout.is_empty() && out.stdout != "HEAD" {
        Some(out.stdout)
    } else {
        None
    }
}

/// Normalize the target branch (hermes `_resolve_update_branch`): explicit
/// `--branch` wins; otherwise use the checkout's current branch so forks and
/// non-default layouts update in place; fall back to `master`.
pub fn resolve_update_branch(root: &Path, opts: &UpdateOptions) -> String {
    if let Some(branch) = &opts.branch {
        let trimmed = branch.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    current_branch(root).unwrap_or_else(|| "master".to_string())
}

fn fetch(root: &Path, remote: &str, branch: &str, shallow: bool) -> GitOutput {
    if shallow {
        git(root, &["fetch", "--depth", "1", remote, branch])
    } else {
        git(root, &["fetch", remote, branch])
    }
}

fn classify_fetch_error(stderr: &str) -> String {
    if stderr.contains("Could not resolve host") || stderr.contains("unable to access") {
        "✗ Network error — cannot reach the remote repository.".to_string()
    } else if stderr.contains("Authentication failed") || stderr.contains("could not read Username")
    {
        "✗ Authentication failed — check your git credentials or SSH key.".to_string()
    } else {
        let first = stderr.lines().next().unwrap_or("");
        if first.is_empty() {
            "✗ Failed to fetch.".to_string()
        } else {
            format!("✗ Failed to fetch.\n  {first}")
        }
    }
}

/// Pick the canonical comparison ref: prefer the `upstream` remote for the
/// default branch when it exists (hermes check-path semantics); otherwise
/// origin. Returns `(remote, compare_ref)`.
fn pick_compare_remote(root: &Path, branch: &str, default_branch: &str) -> (&'static str, String) {
    if branch == default_branch && has_upstream_remote(root) {
        ("upstream", format!("upstream/{branch}"))
    } else {
        ("origin", format!("origin/{branch}"))
    }
}

/// Result of an update check (rendered by `format_check_report`).
#[derive(Debug, Clone, PartialEq)]
pub enum CheckOutcome {
    UpToDate,
    Behind { count: i64, compare_ref: String },
    BehindShallow { compare_ref: String },
}

/// Implement `ulnclaw update --check` (hermes `_cmd_update_check`).
pub fn check_update(
    root: &Path,
    opts: &UpdateOptions,
) -> Result<(CheckOutcome, Vec<String>), String> {
    let mut log_lines: Vec<String> = Vec::new();
    let branch = resolve_update_branch(root, opts);
    let default_branch = current_branch(root).unwrap_or_else(|| "master".to_string());
    let shallow = is_shallow(root);

    let (remote, compare_ref) = pick_compare_remote(root, &branch, &default_branch);
    log_lines.push(format!("→ Fetching from {remote}..."));
    let fetch_result = fetch(root, remote, &branch, shallow);
    if !fetch_result.ok {
        // Fall back to origin when an upstream fetch fails (hermes parity).
        if remote == "upstream" {
            log_lines.push("→ Fetching from origin...".to_string());
            let fallback = fetch(root, "origin", &branch, shallow);
            if !fallback.ok {
                return Err(classify_fetch_error(&fallback.stderr));
            }
        } else {
            return Err(classify_fetch_error(&fetch_result.stderr));
        }
    }
    let compare_ref = if remote == "upstream"
        && git(root, &["rev-parse", "--verify", "--quiet", &compare_ref]).ok
    {
        compare_ref.clone()
    } else {
        format!("origin/{branch}")
    };

    if !git(root, &["rev-parse", "--verify", "--quiet", &compare_ref]).ok {
        let remote_name = compare_ref.split('/').next().unwrap_or("remote");
        return Err(format!("✗ Branch '{branch}' not found on {remote_name}."));
    }

    if shallow {
        let head_sha = capture_head_sha(root).unwrap_or_default();
        let target_sha = git(root, &["rev-parse", &compare_ref]).stdout;
        if !head_sha.is_empty() && head_sha == target_sha {
            return Ok((CheckOutcome::UpToDate, log_lines));
        }
        return Ok((CheckOutcome::BehindShallow { compare_ref }, log_lines));
    }

    let behind = count_commits_between(root, "HEAD", &compare_ref);
    if behind <= 0 {
        Ok((CheckOutcome::UpToDate, log_lines))
    } else {
        Ok((
            CheckOutcome::Behind {
                count: behind,
                compare_ref,
            },
            log_lines,
        ))
    }
}

/// Render a check outcome the way hermes prints it.
pub fn format_check_report(outcome: &CheckOutcome) -> String {
    match outcome {
        CheckOutcome::UpToDate => "✓ Already up to date.\n".to_string(),
        CheckOutcome::Behind { count, compare_ref } => {
            let word = if *count == 1 { "commit" } else { "commits" };
            format!(
                "⚕ Update available: {count} {word} behind {compare_ref}.\n  Run 'ulnclaw update' to install.\n"
            )
        }
        CheckOutcome::BehindShallow { compare_ref } => {
            format!(
                "⚕ Update available (behind {compare_ref}).\n  Run 'ulnclaw update' to install.\n"
            )
        }
    }
}

/// Stash local changes before mutating the tree (hermes
/// `_stash_local_changes_if_needed`). Returns the stash name when a stash
/// entry was actually created.
fn stash_local_changes_if_needed(
    root: &Path,
    log_lines: &mut Vec<String>,
) -> Result<Option<String>, String> {
    let status = git(root, &["status", "--porcelain"]);
    if !status.ok {
        return Err(format!("git status failed: {}", status.stderr));
    }
    if status.stdout.trim().is_empty() {
        return Ok(None);
    }

    let unmerged = git(root, &["ls-files", "--unmerged"]);
    if !unmerged.stdout.trim().is_empty() {
        log_lines.push("→ Clearing unmerged index entries from a previous conflict...".to_string());
        git(root, &["reset"]);
    }

    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let stash_name = format!("ulnclaw-update-autostash-{stamp}");
    log_lines.push("→ Local changes detected — stashing before update...".to_string());
    let prev_stash = git(root, &["rev-parse", "--verify", "refs/stash"]).stdout;
    let push = git(
        root,
        &["stash", "push", "--include-untracked", "-m", &stash_name],
    );
    if !push.stdout.trim().is_empty() {
        log_lines.push(push.stdout.trim().to_string());
    }
    let probe = git(root, &["rev-parse", "--verify", "refs/stash"]);
    if probe.ok && !probe.stdout.is_empty() && probe.stdout != prev_stash {
        Ok(Some(stash_name))
    } else {
        Ok(None)
    }
}

/// Pop the autostash entry after the update (hermes `_restore_stashed_changes`,
/// simplified to the entry we created).
fn restore_stashed_changes(root: &Path, log_lines: &mut Vec<String>) {
    log_lines.push("→ Restoring stashed local changes...".to_string());
    let pop = git(root, &["stash", "pop"]);
    if pop.ok {
        log_lines.push("✓ Stashed changes restored.".to_string());
    } else {
        log_lines.push(format!(
            "⚠ Could not restore stash automatically ({}). Resolve conflicts, then run 'git stash drop'.",
            if pop.stderr.is_empty() { "conflict".to_string() } else { pop.stderr.lines().next().unwrap_or("conflict").to_string() }
        ));
    }
}

/// Result of an applied update.
#[derive(Debug, Clone, Default)]
pub struct UpdateReport {
    pub log_lines: Vec<String>,
    pub old_sha: Option<String>,
    pub new_sha: Option<String>,
    pub new_commits: i64,
    pub rebuilt: bool,
    pub rebuild_output: Option<String>,
}

/// Apply the update (hermes `_cmd_update_impl`, git core): stash → fetch →
/// fast-forward → restore stash → `cargo build --release`.
pub fn apply_update(root: &Path, opts: &UpdateOptions) -> Result<UpdateReport, String> {
    let mut report = UpdateReport::default();
    let branch = resolve_update_branch(root, opts);
    let default_branch = current_branch(root).unwrap_or_else(|| "master".to_string());
    let shallow = is_shallow(root);

    report.log_lines.push("⚕ Updating ulnclaw...".to_string());
    report.log_lines.push(String::new());

    report.old_sha = capture_head_sha(root);

    // Pre-mutation stash (hermes runs the pre-update backup here; ulnclaw
    // state lives in <home> and is not touched by a source update).
    let stash_name = stash_local_changes_if_needed(root, &mut report.log_lines)?;

    // If this is a fork without an upstream remote, add it so future checks
    // can compare against the canonical repo (hermes `_add_upstream_remote`;
    // ulnclaw does it without prompting). Local-path origins (worktrees,
    // test repos) are never treated as forks.
    let origin_url = get_origin_url(root);
    let is_network_origin = origin_url
        .as_deref()
        .map(|url| url.contains("://") || url.starts_with("git@"))
        .unwrap_or(false);
    if is_network_origin && is_fork(origin_url.as_deref()) && !has_upstream_remote(root) {
        if git(root, &["remote", "add", "upstream", OFFICIAL_REPO_URL]).ok {
            report.log_lines.push(format!(
                "→ Added official repo as 'upstream' remote ({OFFICIAL_REPO_URL})."
            ));
        }
    }

    let (remote, _compare) = {
        let preferred = if branch == default_branch && has_upstream_remote(root) {
            "upstream"
        } else {
            "origin"
        };
        (preferred, format!("{preferred}/{branch}"))
    };
    report
        .log_lines
        .push(format!("→ Fetching from {remote}..."));
    let fetch_result = fetch(root, remote, &branch, shallow);
    if !fetch_result.ok {
        if stash_name.is_some() {
            restore_stashed_changes(root, &mut report.log_lines);
        }
        return Err(classify_fetch_error(&fetch_result.stderr));
    }

    let target_ref = format!("{remote}/{branch}");
    if !git(root, &["rev-parse", "--verify", "--quiet", &target_ref]).ok {
        if stash_name.is_some() {
            restore_stashed_changes(root, &mut report.log_lines);
        }
        return Err(format!("✗ Branch '{branch}' not found on {remote}."));
    }

    // Fast-forward only — a diverged local history needs manual resolution.
    let merge = git(root, &["merge", "--ff-only", &target_ref]);
    if !merge.ok {
        if stash_name.is_some() {
            restore_stashed_changes(root, &mut report.log_lines);
        }
        let hint = if merge.stderr.contains("Not possible to fast-forward")
            || merge.stderr.contains("not possible")
        {
            "\n  Local history has diverged. Rebase or merge manually, then re-run 'ulnclaw update'."
        } else {
            ""
        };
        return Err(format!(
            "✗ Fast-forward failed: {}{hint}",
            merge.stderr.lines().next().unwrap_or("unknown error")
        ));
    }
    if !merge.stdout.trim().is_empty() {
        report.log_lines.push(merge.stdout.trim().to_string());
    }

    report.new_sha = capture_head_sha(root);
    if let (Some(old), Some(new)) = (&report.old_sha, &report.new_sha) {
        report.new_commits = count_commits_between(root, old, new).max(0);
        if report.new_commits > 0 {
            let oneline = git(root, &["log", "--oneline", &format!("{old}..{new}")]);
            if oneline.ok {
                for line in oneline.stdout.lines().take(20) {
                    report.log_lines.push(format!("  {line}"));
                }
            }
        }
    }

    if let Some(stash) = &stash_name {
        let _ = stash;
        restore_stashed_changes(root, &mut report.log_lines);
    }

    // Rebuild the binary (the Rust equivalent of hermes' dependency refresh).
    if root.join("Cargo.toml").exists()
        && Command::new("cargo")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        report
            .log_lines
            .push("→ Rebuilding (cargo build --release)...".to_string());
        let build = Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(root)
            .output();
        match build {
            Ok(output) if output.status.success() => {
                report.rebuilt = true;
                report.log_lines.push("✓ Build succeeded.".to_string());
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                report.rebuild_output =
                    Some(stderr.lines().take(10).collect::<Vec<_>>().join("\n"));
                report
                    .log_lines
                    .push("✗ Build failed — the previous binary is still in place.".to_string());
            }
            Err(e) => {
                report.log_lines.push(format!("✗ Could not run cargo: {e}"));
            }
        }
    } else {
        report
            .log_lines
            .push("⚠ No cargo toolchain found — rebuild manually to run the new code.".to_string());
    }

    Ok(report)
}

/// Render the final update summary.
pub fn format_update_report(report: &UpdateReport) -> String {
    let mut out = String::new();
    for line in &report.log_lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    if report.new_commits == 0 {
        out.push_str("✓ Already up to date.\n");
    } else {
        let word = if report.new_commits == 1 {
            "commit"
        } else {
            "commits"
        };
        out.push_str(&format!("✓ Updated: {} new {word}.\n", report.new_commits));
    }
    if report.rebuilt {
        out.push_str("✓ Release binary rebuilt — restart any running gateway/REPL to use it.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as Cmd;

    fn run(dir: &Path, args: &[&str]) {
        let out = Cmd::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = Cmd::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo_with_commits(dir: &Path, commits: usize) {
        run(dir, &["git", "init", "-b", "master"]);
        run(dir, &["git", "config", "user.email", "test@example.com"]);
        run(dir, &["git", "config", "user.name", "Test"]);
        for i in 0..commits {
            std::fs::write(dir.join("file.txt"), format!("content {i}\n")).unwrap();
            run(dir, &["git", "add", "file.txt"]);
            run(dir, &["git", "commit", "-m", &format!("commit {i}")]);
        }
    }

    /// Clone `origin_dir` into `work_dir` and return the working clone path.
    fn clone_work(origin_dir: &Path, work_dir: &Path) -> PathBuf {
        let work = work_dir.join("work");
        run(
            work_dir,
            &[
                "git",
                "clone",
                origin_dir.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        run(&work, &["git", "config", "user.email", "test@example.com"]);
        run(&work, &["git", "config", "user.name", "Test"]);
        work
    }

    #[test]
    fn is_fork_detects_official_vs_fork() {
        assert!(!is_fork(Some(OFFICIAL_REPO_URL)));
        assert!(!is_fork(Some("https://gitee.com/ushaw/ulnclaw")));
        assert!(is_fork(Some("https://gitee.com/someone/ulnclaw.git")));
        assert!(!is_fork(None));
    }

    #[test]
    fn check_reports_up_to_date_and_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let origin_dir = tmp.path().join("origin");
        std::fs::create_dir_all(&origin_dir).unwrap();
        init_repo_with_commits(&origin_dir, 2);
        let work = clone_work(&origin_dir, tmp.path());

        let opts = UpdateOptions::default();
        let (outcome, _) = check_update(&work, &opts).unwrap();
        assert_eq!(outcome, CheckOutcome::UpToDate);

        // Add two commits to origin → clone is 2 behind.
        for i in 10..12 {
            std::fs::write(origin_dir.join("file.txt"), format!("content {i}\n")).unwrap();
            run(&origin_dir, &["git", "add", "file.txt"]);
            run(
                &origin_dir,
                &["git", "commit", "-m", &format!("commit {i}")],
            );
        }
        let (outcome, _) = check_update(&work, &opts).unwrap();
        assert_eq!(
            outcome,
            CheckOutcome::Behind {
                count: 2,
                compare_ref: "origin/master".to_string()
            }
        );
        let text = format_check_report(&outcome);
        assert!(text.contains("2 commits behind"));
    }

    #[test]
    fn apply_update_fast_forwards_and_stashes() {
        let tmp = tempfile::tempdir().unwrap();
        let origin_dir = tmp.path().join("origin");
        std::fs::create_dir_all(&origin_dir).unwrap();
        init_repo_with_commits(&origin_dir, 2);
        let work = clone_work(&origin_dir, tmp.path());

        // New upstream commit + dirty working tree.
        std::fs::write(origin_dir.join("new.txt"), "new\n").unwrap();
        run(&origin_dir, &["git", "add", "new.txt"]);
        run(&origin_dir, &["git", "commit", "-m", "add new.txt"]);
        std::fs::write(work.join("local.txt"), "local change\n").unwrap();

        let report = apply_update(&work, &UpdateOptions::default()).unwrap();
        assert_eq!(report.new_commits, 1);
        assert!(work.join("new.txt").exists(), "pulled the new file");
        assert!(
            work.join("local.txt").exists(),
            "stash restored the local file"
        );
        let status = git_out(&work, &["status", "--porcelain"]);
        assert!(
            status.contains("local.txt"),
            "local change back in working tree"
        );
    }

    #[test]
    fn apply_update_noop_when_current() {
        let tmp = tempfile::tempdir().unwrap();
        let origin_dir = tmp.path().join("origin");
        std::fs::create_dir_all(&origin_dir).unwrap();
        init_repo_with_commits(&origin_dir, 1);
        let work = clone_work(&origin_dir, tmp.path());
        let report = apply_update(&work, &UpdateOptions::default()).unwrap();
        assert_eq!(report.new_commits, 0);
        let text = format_update_report(&report);
        assert!(text.contains("Already up to date"));
    }

    #[test]
    fn check_unknown_branch_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let origin_dir = tmp.path().join("origin");
        std::fs::create_dir_all(&origin_dir).unwrap();
        init_repo_with_commits(&origin_dir, 1);
        let work = clone_work(&origin_dir, tmp.path());
        let opts = UpdateOptions {
            branch: Some("does-not-exist".into()),
            ..Default::default()
        };
        // Hermes parity: a missing remote ref fails at the fetch stage
        // ("couldn't find remote ref ..."), not at ref verification.
        let err = check_update(&work, &opts).unwrap_err();
        assert!(err.contains("Failed to fetch"), "got: {err}");
        assert!(err.contains("does-not-exist"), "got: {err}");
    }
}

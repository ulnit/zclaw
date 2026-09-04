//! Filesystem git-repo discovery feeding the `projects.db`
//! `discovered_repos` cache (P161).
//!
//! Hermes ships the cache schema (`projects_db.discovered_repos`) but no
//! scanner — its Electron desktop walks the disk in TypeScript. This module
//! supplies the missing scanner as CLI: `ulnclaw project scan [--root PATH
//! ...] [--max-depth N]` records every git checkout it finds (replace
//! semantics + discovery policy key) and `ulnclaw project repos [--clear]`
//! reads the cache back.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

/// Default scan depth below each root (levels of directories descended).
pub const DEFAULT_MAX_DEPTH: usize = 6;

/// Discovery policy stamp written to `project_meta` by CLI scans, so a
/// future policy change can invalidate the cache (hermes
/// `reconcile_discovered_repos_policy` contract).
pub const CLI_SCAN_POLICY_KEY: &str = "cli-scan:v1";

/// Directory names never descended into (build / cache / dependency noise).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".git",
    ".hg",
    ".svn",
    ".worktrees",
    ".cargo",
    ".rustup",
    ".npm",
    ".cache",
    ".gradle",
    ".m2",
    "Library",
    ".Trash",
];

/// One discovered git checkout: normalized root + display label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepo {
    pub root: String,
    pub label: String,
}

/// Scan `roots` up to `max_depth` levels below each root for git
/// checkouts — any directory containing a `.git` entry (a directory, or a
/// file for linked worktrees / submodules). Hidden (dot-prefixed)
/// directories and [`SKIP_DIRS`] entries are pruned; symlinks are never
/// followed. Nested checkouts ARE found (scanning continues below a repo).
/// Results are deduplicated and sorted by root path.
pub fn scan_for_repos(roots: &[PathBuf], max_depth: usize) -> Vec<DiscoveredRepo> {
    let mut found: Vec<DiscoveredRepo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for root in roots {
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
        queue.push_back((root.clone(), 0));
        while let Some((dir, depth)) = queue.pop_front() {
            if dir.join(".git").exists() {
                let norm = crate::projects_db::normalize_path(&dir.to_string_lossy());
                if seen.insert(norm.clone()) {
                    let label = dir
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| norm.clone());
                    found.push(DiscoveredRepo { root: norm, label });
                }
            }
            if depth >= max_depth {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                // Symlinks are never followed (loop + escape hatch safety).
                if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                    continue;
                }
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                queue.push_back((path, depth + 1));
            }
        }
    }
    found.sort_by(|a, b| a.root.cmp(&b.root));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn mkdirs(root: &Path, rel: &str) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn touch_git(dir: &Path, worktree_style: bool) {
        let git = dir.join(".git");
        if worktree_style {
            std::fs::write(git, "gitdir: /dev/null\n").unwrap();
        } else {
            std::fs::create_dir_all(&git).unwrap();
        }
    }

    #[test]
    fn scan_finds_repos_skips_noise_and_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repo_a = mkdirs(root, "code/repo-a");
        touch_git(&repo_a, false);
        // Linked-worktree style `.git` file counts too.
        let repo_b = mkdirs(root, "code/nested/repo-b");
        touch_git(&repo_b, true);
        // Nested checkout inside a repo is found as well.
        let sub = repo_a.join("sub-checkout");
        std::fs::create_dir_all(&sub).unwrap();
        touch_git(&sub, false);
        // Noise: dependency dir + hidden dir + skip-listed dir.
        let junk = mkdirs(root, "code/node_modules/junk");
        touch_git(&junk, false);
        let hidden = mkdirs(root, "code/.hidden-repos/secret");
        touch_git(&hidden, false);
        let built = mkdirs(root, "code/target/checkout");
        touch_git(&built, false);

        let found = scan_for_repos(&[root.to_path_buf()], DEFAULT_MAX_DEPTH);
        let roots: Vec<&str> = found.iter().map(|r| r.root.as_str()).collect();
        let norm = |p: &Path| crate::projects_db::normalize_path(&p.to_string_lossy());
        assert!(
            roots.contains(&norm(&repo_a).as_str()),
            "repo-a missing: {roots:?}"
        );
        assert!(
            roots.contains(&norm(&repo_b).as_str()),
            "repo-b missing: {roots:?}"
        );
        assert!(
            roots.contains(&norm(&sub).as_str()),
            "sub-checkout missing: {roots:?}"
        );
        assert_eq!(found.len(), 3, "noise leaked: {roots:?}");
        // Label falls back to the basename.
        let b = found.iter().find(|r| r.root == norm(&repo_b)).unwrap();
        assert_eq!(b.label, "repo-b");
    }

    #[test]
    fn scan_respects_max_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let shallow = mkdirs(root, "shallow");
        touch_git(&shallow, false);
        let deep = mkdirs(root, "a/b/c/d");
        touch_git(&deep, false);

        let found = scan_for_repos(&[root.to_path_buf()], 1);
        assert_eq!(found.len(), 1);
        assert!(found[0].root.ends_with("shallow"));

        let found = scan_for_repos(&[root.to_path_buf()], 4);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn scan_dedupes_overlapping_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repo = mkdirs(root, "code/repo");
        touch_git(&repo, false);
        // Scanning both the root and a subpath must not duplicate.
        let found = scan_for_repos(&[root.to_path_buf(), root.join("code")], DEFAULT_MAX_DEPTH);
        assert_eq!(found.len(), 1);
    }
}

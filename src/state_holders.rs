//! Who else holds `state.db`? — the scanning primitives behind the auto-VACUUM
//! admission gate (hermes `hermes_state_holders.py`).
//!
//! Structural maintenance (VACUUM plus its TRUNCATE checkpoint) retires the WAL
//! generation that any other open handle still reads. Upstream therefore refuses
//! to VACUUM unless it can PROVE the store is quiet, and — the load-bearing
//! property — **an incomplete scan is reported as a holder, never as an
//! all-clear**. This module is the first slice: the pure, path/argv-level
//! primitives. The `/proc` walk, the in-process generation arm and the Windows
//! Restart Manager arm build on top of them in later slices.
//!
//! # How this differs from upstream, and why
//!
//! Upstream decides "is this process one of ours?" by parsing PYTHON argv:
//! `_looks_like_python_executable`, `_python_execution_target`,
//! `_HERMES_PYTHON_MODULES = {acp_adapter, hermes_cli.main}` and
//! `_HERMES_PYTHON_SCRIPTS = {hermes_cli/main.py, run_agent.py}`, plus the
//! `-Q`/`-W`/`-X`/`--check-hash-based-pycs`/`--jit` operand-skip tables.
//!
//! ulnclaw is ONE statically linked Rust binary (`Cargo.toml` has a single
//! `[[bin]] name = "ulnclaw"`); there is no interpreter in the argv of any of
//! our processes, so `argv[0]`'s basename IS the identity. That machinery is
//! structurally inapplicable here rather than merely unported — replicating it
//! would add ~120 lines that can never match anything.
//!
//! 🔴 **Direction of error matters: this set gates fail-CLOSED suspicion.** When
//! a `/proc/<pid>/fd` directory is unreadable we can only fall back to argv, and
//! we count the process as a holder precisely when it looks like ours. A name
//! missing from [`ULNCLAW_EXECUTABLES`] makes that process invisible to the scan,
//! which reads as "quiet" and lets maintenance run under a live writer. **Any new
//! `[[bin]]` target must be added here.**

use std::path::{Component, Path, PathBuf};

/// Basenames (lowercased, `.exe` stripped) that identify one of our processes.
///
/// Upstream's equivalent is `_HERMES_EXECUTABLES = {hermes, hermes-agent,
/// hermes-acp}` — three names because hermes ships a CLI, an agent entry point
/// and an ACP adapter. ulnclaw ships all three as one binary, so the set has one
/// member. See the module header for why a missing entry fails open.
pub const ULNCLAW_EXECUTABLES: &[&str] = &["ulnclaw"];

/// CLI flags that select the home directory.
///
/// 🔴 Deliberately EMPTY. Upstream reads `--hermes-home`; ulnclaw's `Cli` struct
/// (`main.rs`) declares only `--config`, `--profile`, `--verbose`, `--resume`,
/// `--continue`, … — there is no home flag, so home selection reaches a process
/// only through the `ULNCLAW_HOME` environment, which [`argv_env_home`] catches
/// in the `env ULNCLAW_HOME=… ulnclaw …` form. Upstream cannot see a pure-env
/// home either, so this is parity, not a narrowing.
pub const HOME_FLAGS: &[&str] = &[];

/// CLI flags that select a named profile (hermes `_PROFILE_FLAGS = ("--profile", "-p")`).
///
/// 🔴 `-p` is deliberately absent: ulnclaw's `--profile` is `#[arg(long)]` with
/// no short alias, and `-p` is not claimed by any flag in `Cli`. Keeping it would
/// read an unrelated short option as a profile name and could dismiss a real
/// holder of ours — the fail-open direction.
pub const PROFILE_FLAGS: &[&str] = &["--profile"];

/// The SQLite family a store owns (hermes `_STATE_DB_NAMES`).
pub const STATE_DB_NAMES: &[&str] = &["state.db", "state.db-wal", "state.db-shm"];

/// Suffix Linux appends to a `/proc/<pid>/fd` link whose target was unlinked
/// while still open — exactly the retired-WAL generation we must not VACUUM over.
const DELETED_SUFFIX: &str = " (deleted)";

/// `(<base>, <base>-wal, <base>-shm)` — hermes `_sqlite_family`.
///
/// A holder of the WAL or SHM sidecar pins the generation just as surely as a
/// holder of the main database file.
pub fn sqlite_family(base: &str) -> [String; 3] {
    [
        base.to_string(),
        format!("{base}-wal"),
        format!("{base}-shm"),
    ]
}

/// Strip Linux's `" (deleted)"` marker, leaving the path it was attached to.
fn strip_deleted_suffix(path: &str) -> &str {
    path.strip_suffix(DELETED_SUFFIX).unwrap_or(path)
}

/// Collapse `.` and `..` lexically WITHOUT touching the filesystem.
///
/// Mirrors `os.path.normpath`: absolute stays absolute, relative stays relative,
/// and no symlink is resolved (that is `realpath`'s job, used only on the store
/// path itself — see the later scan slice).
fn normpath_lexical(path: &str) -> PathBuf {
    let mut out = PathBuf::new();

    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Only collapse when the tail is a normal segment; a leading `..`
                // (or one above the root) must survive, as normpath keeps it.
                match out.components().next_back() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    _ => out.push(component.as_os_str()),
                }
            }
            other => out.push(other.as_os_str()),
        }
    }

    if out.as_os_str().is_empty() {
        out.push(".");
    }

    out
}

/// Absolutize without resolving symlinks — the `os.path.abspath` half of
/// `canonical_sqlite_path`. `std::fs::canonicalize` is NOT usable here: it
/// resolves symlinks, and upstream deliberately keeps this step lexical.
fn abspath_lexical(path: &str) -> PathBuf {
    let parsed = Path::new(path);

    if parsed.is_absolute() {
        return normpath_lexical(path);
    }

    match std::env::current_dir() {
        Ok(cwd) => normpath_lexical(&cwd.join(parsed).to_string_lossy()),
        // No cwd to anchor on: stay lexical rather than inventing a root.
        Err(_) => normpath_lexical(path),
    }
}

/// `os.path.normcase`: case- and separator-insensitive comparison on Windows,
/// identity elsewhere. Path equality in this module always goes through it.
#[cfg(windows)]
fn normalize_case(path: &str) -> String {
    path.to_lowercase().replace('/', "\\")
}

#[cfg(not(windows))]
fn normalize_case(path: &str) -> String {
    path.to_string()
}

/// `os.path.normcase(os.path.normpath(value))` — hermes `_norm_path`.
pub fn norm_path(value: &str) -> String {
    normalize_case(&normpath_lexical(value).to_string_lossy())
}

/// Normalize a `/proc/<pid>/fd` target for comparison against the watched set —
/// hermes `canonical_sqlite_path`:
/// `normcase(abspath(strip(" (deleted)")))`.
///
/// The deleted-suffix strip is what makes a retired-but-still-open WAL generation
/// match the store we are about to VACUUM.
pub fn canonical_sqlite_path(path: &str) -> String {
    normalize_case(&abspath_lexical(strip_deleted_suffix(path)).to_string_lossy())
}

/// Last `--flag X` / `--flag=X` value, token-exact — hermes `_argv_flag_value`.
///
/// "Token-exact" is load-bearing: `--profile timothy` must not answer a query for
/// `tim`, because a prefix match would read a DIFFERENT profile as ours and let
/// maintenance run against the store that profile is writing.
pub fn argv_flag_value(argv: &[String], flags: &[&str]) -> Option<String> {
    let mut value: Option<String> = None;
    let mut index = 0;

    while index < argv.len() {
        let token = argv[index].as_str();

        if flags.contains(&token) {
            if let Some(next) = argv.get(index + 1) {
                value = Some(next.clone());
                index += 2;
                continue;
            }
        }

        for flag in flags {
            let prefix = format!("{flag}=");

            if let Some(rest) = token.strip_prefix(prefix.as_str()) {
                value = Some(rest.to_string());
                break;
            }
        }

        index += 1;
    }

    value
}

/// `ULNCLAW_HOME=<path>` env-style assignment on the argv — hermes `_argv_env_home`,
/// for the `env ULNCLAW_HOME=… ulnclaw …` launch form. Scanned backwards, so the
/// last assignment wins, matching upstream's `reversed(argv)`.
pub fn argv_env_home(argv: &[String]) -> Option<String> {
    const PREFIX: &str = "ULNCLAW_HOME=";

    argv.iter()
        .rev()
        .find_map(|token| token.strip_prefix(PREFIX).map(str::to_string))
}

/// `(argv index, normalized absolute path)` for every path-bearing token —
/// hermes `_argv_path_tokens`.
///
/// Only tokens that are themselves absolute, or `--opt=/abs/path` whose value is
/// absolute, count. Relative tokens are skipped on purpose: anchoring one on our
/// cwd would manufacture evidence about a process whose cwd we cannot see.
pub fn argv_path_tokens(argv: &[String]) -> Vec<(usize, String)> {
    let mut tokens = Vec::new();

    for (index, token) in argv.iter().enumerate() {
        let candidate = if is_absolute_token(token) {
            Some(token.as_str())
        } else if token.starts_with('-') {
            // `--db=/abs/path`-style options carry a path value; anchor on the
            // text after '=' so the option name is not folded into the path.
            token
                .split_once('=')
                .map(|(_, value)| value)
                .filter(|value| is_absolute_token(value))
        } else {
            None
        };

        if let Some(path_token) = candidate {
            tokens.push((index, norm_path(path_token)));
        }
    }

    tokens
}

/// Absolute-path test that also accepts Windows drive/UNC forms, so the scan
/// behaves the same on the platform that has no `/proc` (the Windows arm reports
/// holders through Restart Manager instead, but shares these primitives).
fn is_absolute_token(token: &str) -> bool {
    Path::new(token).is_absolute()
}

/// Does `argv` identify one of our processes? — hermes `_looks_like_hermes`, with
/// the Python-target arm collapsed away (see the module header).
///
/// Only `argv[0]`'s basename matters: lowercased, `.exe` stripped, then matched
/// against [`ULNCLAW_EXECUTABLES`].
pub fn looks_like_ulnclaw(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };

    let name = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name = name.strip_suffix(".exe").unwrap_or(&name).to_string();

    ULNCLAW_EXECUTABLES.contains(&name.as_str())
}

// ===========================================================================
// slice2a — argv home/profile attribution (hermes `hermes_state_holders.py`
// L185-317). These decide whether a process whose `/proc/<pid>/fd` we cannot
// read is PROVABLY a different instance. Without them, every unreadable process
// on a multi-user host counts as a holder and VACUUM would never fire.
// ===========================================================================

/// Files marking a real ulnclaw home; an arbitrary directory that merely has a
/// `profiles` segment lacks them. Upstream's tuple is
/// `("config.yaml", ".env", "state.db")`; this fork's config is TOML
/// (`config/mod.rs:1838` reads `ulnclaw_home().join("config.toml")`), so
/// `config.toml` takes the `config.yaml` slot. Verified against a live home
/// (`.env` and `state.db` both present).
pub const HOME_MARKERS: &[&str] = &["config.toml", ".env", "state.db"];

/// Only `profile delete` creates this tombstone under `profiles/`, so its
/// presence proves the parent is a real home (upstream `_DELETED_PROFILES_DIR`).
pub const DELETED_PROFILES_DIR: &str = ".deleted";

/// Default home directory name (`~/.ulnclaw`); upstream's is `.hermes`.
pub const HOME_DIR_NAME: &str = ".ulnclaw";

/// The default home root: `ULNCLAW_HOME` when set, else `~/.ulnclaw`.
/// Delegates to the crate's own resolver rather than re-deriving it, so this
/// scan and every other consumer agree by construction (upstream
/// `get_default_hermes_root`).
fn default_home_root() -> PathBuf {
    crate::config::ulnclaw_home()
}

/// `canonicalize` when possible, else the lexical absolute path — upstream
/// `resolve(strict=False)`. A root that does not exist yet must still compare
/// equal to itself, so the fallback matters.
fn resolve_lenient(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| abspath_lexical(&path.to_string_lossy()))
}

/// True when `profiles_dir` is provably `<ulnclaw-home>/profiles`
/// (upstream `_is_hermes_profiles_root`).
fn is_ulnclaw_profiles_root(profiles_dir: &Path) -> bool {
    let Some(root) = profiles_dir.parent() else {
        return false;
    };
    if root
        .file_name()
        .map(|n| n == HOME_DIR_NAME)
        .unwrap_or(false)
    {
        return true;
    }
    if profiles_dir.join(DELETED_PROFILES_DIR).is_dir()
        || HOME_MARKERS.iter().any(|marker| root.join(marker).exists())
    {
        return true;
    }
    resolve_lenient(root) == resolve_lenient(&default_home_root())
}

/// `<root>/profiles/<name>` when `path` is that home or under it — upstream
/// `named_profile_home`. A default home whose path merely CONTAINS a `profiles`
/// segment is not a named profile, hence the `HOME_DIR_NAME` early return.
pub fn named_profile_home(path: &Path) -> Option<PathBuf> {
    let mut candidate: Option<&Path> = Some(path);

    while let Some(current) = candidate {
        let name = current
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let parent = current.parent();
        let parent_is_profiles = parent
            .and_then(|p| p.file_name())
            .map(|n| n == "profiles")
            .unwrap_or(false);

        if parent_is_profiles && !name.starts_with('.') {
            if let Some(profiles_dir) = parent {
                if is_ulnclaw_profiles_root(profiles_dir) {
                    return Some(current.to_path_buf());
                }
            }
        }
        if name == HOME_DIR_NAME {
            return None;
        }
        candidate = parent;
    }

    None
}

/// `(<install root>, <our profile name>)` for the home holding the store, else
/// `(None, None)` — upstream `_store_install_layout`.
///
/// Derived with the canonical `named_profile_home` predicate, NEVER a
/// `basename == "profiles"` string test: an arbitrary `<X>/profiles/<n>/` tree is
/// not an install, and promoting `<X>` to "ours" swallows an unrelated instance
/// living under it — the literal two-instance shape of upstream issue #92401.
/// The root store (`~/.ulnclaw/state.db`) is its own root with no profile name,
/// so ANY named-profile selection contradicts it.
pub fn store_install_layout(this_home: &str) -> (Option<String>, Option<String>) {
    let home = Path::new(this_home);

    if let Some(profile_home) = named_profile_home(home) {
        if let Some(root) = profile_home.parent().and_then(|p| p.parent()) {
            let name = profile_home
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let root_str = abspath_lexical(&root.to_string_lossy())
                .to_string_lossy()
                .to_string();
            return (Some(root_str), Some(name));
        }
    }

    if home
        .file_name()
        .map(|n| n == HOME_DIR_NAME)
        .unwrap_or(false)
    {
        let root_str = abspath_lexical(this_home).to_string_lossy().to_string();
        return (Some(root_str), None);
    }

    (None, None)
}

/// True when `normalized` sits under `<install root>/profiles/<name>` for a name
/// that is not ours — upstream `_names_other_profile`.
pub fn names_other_profile(
    normalized: &str,
    install_root: Option<&str>,
    our_profile: Option<&str>,
) -> bool {
    let Some(root) = install_root else {
        return false;
    };
    let prefix = format!(
        "{}{}",
        norm_path(&Path::new(root).join("profiles").to_string_lossy()),
        std::path::MAIN_SEPARATOR
    );
    let Some(rest) = normalized.strip_prefix(prefix.as_str()) else {
        return false;
    };
    let name = rest.split(std::path::MAIN_SEPARATOR).next().unwrap_or("");
    if name.is_empty() {
        return false;
    }
    match our_profile {
        Some(ours) => name != normalize_case(ours),
        // No profile of our own: any named profile is somebody else's.
        None => true,
    }
}

/// Which home the process's OWN flags select — upstream `_argv_home_selection`.
///
/// Under one process per host the shared binary path proves nothing about which
/// home a process serves; its `--profile` / `ULNCLAW_HOME=` selection does.
#[derive(Debug, PartialEq, Eq)]
pub enum HomeSelection {
    Ours,
    Other,
}

pub fn argv_home_selection(
    argv: &[String],
    this_home: &str,
    install_root: Option<&str>,
    our_profile: Option<&str>,
) -> Option<HomeSelection> {
    let home_value = argv_flag_value(argv, HOME_FLAGS).or_else(|| argv_env_home(argv));
    if let Some(value) = home_value {
        return Some(if norm_path(&value) == norm_path(this_home) {
            HomeSelection::Ours
        } else {
            HomeSelection::Other
        });
    }

    if let Some(value) = argv_flag_value(argv, PROFILE_FLAGS) {
        if let Some(ours) = our_profile {
            return Some(if value == ours {
                HomeSelection::Ours
            } else {
                HomeSelection::Other
            });
        }
        // Root/custom home: any explicit named profile selects a different home.
        return Some(if install_root.is_some() && value == "default" {
            HomeSelection::Ours
        } else {
            HomeSelection::Other
        });
    }

    None
}

/// Does argv PROVE the process belongs to a DIFFERENT instance?
/// (upstream `_argv_scoped_to_other_home`)
///
/// `state.db` lives at the home root, so an absolute-path token carrying a
/// `/.ulnclaw` segment — or naming a `state.db`/WAL/SHM under some other parent —
/// identifies that token's own home. When at least one such token exists AND no
/// token references this instance's store, sidecars or home, the process provably
/// works on a different generation and must not be counted as an uninspectable
/// holder of ours (upstream #92401: a second gateway under `/home/demo/.hermes`
/// deferred this instance's stale-FTS rebuild forever despite `lsof` proving zero
/// open handles). Ambiguous argv with no absolute-path tokens returns false and
/// keeps the fail-closed suspicion.
///
/// Evidence is RANKED because one host now runs ONE process for every profile:
/// 1. a token naming our store or a sidecar exactly — definitive, ours;
/// 2. the process's own home/profile selection — what decides which home a
///    multiplexer serves;
/// 3. path tokens: one under `<install root>/profiles/<other>` is another
///    profile's store even though it sits under our root; a token naming only the
///    SHARED install root is NEUTRAL (the same binary serves every profile, so it
///    can neither prove nor disprove a hold); `argv[0]` locates the INSTALL, not
///    the home, so it is not other-home evidence for a store whose home is not
///    part of an install layout.
pub fn argv_scoped_to_other_home(argv: &[String], db_path: &Path) -> bool {
    let db_path_str = abspath_lexical(&db_path.to_string_lossy())
        .to_string_lossy()
        .to_string();
    let this_home = Path::new(db_path_str.as_str())
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let (install_root, our_profile) = store_install_layout(this_home.as_str());

    let sidecars: std::collections::HashSet<String> = sqlite_family(db_path_str.as_str())
        .iter()
        .map(|candidate| normalize_case(candidate))
        .collect();
    let this_home_norm = normalize_case(this_home.as_str());
    let root_norm = install_root.as_deref().map(normalize_case);
    let path_tokens = argv_path_tokens(argv);

    // (1) Definitive: a token naming our store or one of its sidecars.
    if path_tokens.iter().any(|(_, n)| sidecars.contains(n)) {
        return false;
    }

    // (2) The process's own selection.
    let selection = argv_home_selection(
        argv,
        this_home.as_str(),
        install_root.as_deref(),
        our_profile.as_deref(),
    );
    if selection == Some(HomeSelection::Ours) {
        return false;
    }

    let argv0_locates_home = install_root.is_some();
    let mut other_home_seen = selection == Some(HomeSelection::Other);

    for (index, normalized) in path_tokens.iter() {
        if names_other_profile(normalized, install_root.as_deref(), our_profile.as_deref()) {
            other_home_seen = true;
            continue;
        }
        let under_this_home = format!("{}{}", this_home_norm, std::path::MAIN_SEPARATOR);
        if normalized == &this_home_norm || normalized.starts_with(under_this_home.as_str()) {
            return false;
        }
        if let Some(root) = root_norm.as_ref() {
            let under_root = format!("{}{}", root, std::path::MAIN_SEPARATOR);
            if normalized == root || normalized.starts_with(under_root.as_str()) {
                // Shared install root: neutral, every served profile lives under it.
                continue;
            }
        }
        if *index == 0 && !argv0_locates_home {
            continue;
        }
        // Upstream tests the literal POSIX form `"/.hermes" in normalized`. On
        // Windows `normalize_case` rewrites `/` to `\`, so this arm cannot fire
        // there. Replicated verbatim (with our dir name) rather than "fixed":
        // failing to set the flag keeps the process a suspected holder, which is
        // the fail-closed side, and inventing a Windows arm would be a deviation
        // upstream has no test for.
        let home_segment = format!("/{}", HOME_DIR_NAME);
        let file_name = Path::new(normalized.as_str())
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
        let is_state_name = file_name
            .as_deref()
            .map(|n| STATE_DB_NAMES.contains(&n))
            .unwrap_or(false);
        if normalized.contains(home_segment.as_str()) || is_state_name {
            other_home_seen = true;
        }
    }

    other_home_seen
}

#[cfg(test)]
mod slice2a_tests {
    use super::*;
    use std::fs;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// A real home root carrying a marker file, so `is_ulnclaw_profiles_root`
    /// can prove it (the marker arm, not the default-root arm).
    fn fake_home(root: &Path) {
        fs::create_dir_all(root.join("profiles").join("work")).unwrap();
        fs::create_dir_all(root.join("profiles").join("other")).unwrap();
        fs::write(root.join(".env"), "").unwrap();
    }

    #[test]
    fn store_install_layout_classifies_the_root_store_as_its_own_root() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(HOME_DIR_NAME);
        fake_home(&home);

        let (root, profile) = store_install_layout(home.to_str().unwrap());
        assert_eq!(profile, None, "the root store has no profile name");
        assert_eq!(
            root.as_deref().map(|r| norm_path(r)),
            Some(norm_path(home.to_str().unwrap()))
        );
    }

    #[test]
    fn store_install_layout_names_the_profile_and_its_install_root() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(HOME_DIR_NAME);
        fake_home(&home);
        let profile_home = home.join("profiles").join("work");

        let (root, profile) = store_install_layout(profile_home.to_str().unwrap());
        assert_eq!(profile.as_deref(), Some("work"));
        assert_eq!(
            root.as_deref().map(|r| norm_path(r)),
            Some(norm_path(home.to_str().unwrap()))
        );
    }

    #[test]
    fn store_install_layout_refuses_an_arbitrary_profiles_tree() {
        // An <X>/profiles/<n> tree with no home marker is NOT an install. Treating
        // it as ours is what swallowed an unrelated instance in upstream #92401.
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("some-app").join("profiles").join("work");
        fs::create_dir_all(&bogus).unwrap();

        let (root, profile) = store_install_layout(bogus.to_str().unwrap());
        assert_eq!(root, None);
        assert_eq!(profile, None);
    }

    #[test]
    fn names_other_profile_is_false_without_an_install_root() {
        assert!(!names_other_profile("/x/profiles/y", None, None));
    }

    #[test]
    fn names_other_profile_distinguishes_ours_from_theirs() {
        let root = norm_path("/home/u/.ulnclaw");
        let sep = std::path::MAIN_SEPARATOR;
        let ours = format!("{}{}profiles{}work", root, sep, sep);
        let theirs = format!("{}{}profiles{}other", root, sep, sep);

        assert!(!names_other_profile(
            ours.as_str(),
            Some(root.as_str()),
            Some("work")
        ));
        assert!(names_other_profile(
            theirs.as_str(),
            Some(root.as_str()),
            Some("work")
        ));
        // No profile of our own: any named profile is somebody else's.
        assert!(names_other_profile(
            theirs.as_str(),
            Some(root.as_str()),
            None
        ));
    }

    #[test]
    fn argv_home_selection_prefers_the_env_home_then_the_profile_flag() {
        let home = norm_path("/home/u/.ulnclaw");
        // No selection at all.
        assert_eq!(
            argv_home_selection(
                &argv(&["ulnclaw", "gateway"]),
                "/home/u/.ulnclaw",
                None,
                None
            ),
            None
        );
        // Our own home via the env form.
        assert_eq!(
            argv_home_selection(
                &argv(&["env", "ULNCLAW_HOME=/home/u/.ulnclaw", "ulnclaw"]),
                "/home/u/.ulnclaw",
                None,
                None
            ),
            Some(HomeSelection::Ours)
        );
        // A different home via the env form.
        assert_eq!(
            argv_home_selection(
                &argv(&["env", "ULNCLAW_HOME=/home/demo/.ulnclaw", "ulnclaw"]),
                "/home/u/.ulnclaw",
                None,
                None
            ),
            Some(HomeSelection::Other)
        );
        // Our profile.
        assert_eq!(
            argv_home_selection(
                &argv(&["ulnclaw", "--profile", "work"]),
                "/home/u/.ulnclaw",
                None,
                Some("work")
            ),
            Some(HomeSelection::Ours)
        );
        // Another profile.
        assert_eq!(
            argv_home_selection(
                &argv(&["ulnclaw", "--profile", "other"]),
                "/home/u/.ulnclaw",
                None,
                Some("work")
            ),
            Some(HomeSelection::Other)
        );
        let _ = home;
    }

    #[test]
    fn scoped_to_other_home_is_false_for_ambiguous_argv() {
        // No absolute-path tokens: keep the fail-closed suspicion.
        let db = Path::new("/home/u/.ulnclaw/state.db");
        assert!(!argv_scoped_to_other_home(
            &argv(&["ulnclaw", "gateway"]),
            db
        ));
    }

    #[test]
    fn scoped_to_other_home_is_false_when_argv_names_our_store() {
        let db = Path::new("/home/u/.ulnclaw/state.db");
        assert!(!argv_scoped_to_other_home(
            &argv(&["ulnclaw", "--db=/home/u/.ulnclaw/state.db"]),
            db
        ));
        // A WAL sidecar of ours is equally definitive.
        assert!(!argv_scoped_to_other_home(
            &argv(&["ulnclaw", "/home/u/.ulnclaw/state.db-wal"]),
            db
        ));
    }

    #[test]
    fn scoped_to_other_home_is_false_when_argv_names_our_home_dir() {
        let db = Path::new("/home/u/.ulnclaw/state.db");
        assert!(!argv_scoped_to_other_home(
            &argv(&["ulnclaw", "/home/u/.ulnclaw/memory/n.md"]),
            db
        ));
    }

    #[test]
    fn scoped_to_other_home_is_true_for_a_second_instance_under_another_home() {
        // The upstream #92401 shape: a sibling gateway under /home/demo/.ulnclaw.
        let db = Path::new("/home/u/.ulnclaw/state.db");
        assert!(argv_scoped_to_other_home(
            &argv(&["/home/demo/.ulnclaw/bin/ulnclaw", "gateway"]),
            db
        ));
        assert!(argv_scoped_to_other_home(
            &argv(&["ulnclaw", "--db=/home/demo/.ulnclaw/state.db"]),
            db
        ));
    }

    #[test]
    fn scoped_to_other_home_is_true_when_the_profile_flag_selects_another_profile() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(HOME_DIR_NAME);
        fake_home(&home);
        let db = home.join("profiles").join("work").join("state.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();

        // Same install, different named profile: its store is another generation.
        assert!(argv_scoped_to_other_home(
            &argv(&["ulnclaw", "--profile", "other"]),
            &db
        ));
        // Our own profile: ours.
        assert!(!argv_scoped_to_other_home(
            &argv(&["ulnclaw", "--profile", "work"]),
            &db
        ));
    }
}

// ===========================================================================
// slice2b — the cross-process scan: who else has state.db (or a WAL sidecar)
// open right now? (hermes `foreign_state_db_holders`, L425-566)
//
// 🔴 The load-bearing property, upstream's own words: "A scan failure is
// represented as an unknown holder… must not assume quiescence when an old,
// unlinked SQLite generation may still be open by another process." Every
// failure path below therefore PRODUCES a holder, never an empty list.
// ===========================================================================

use std::collections::HashSet;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// A holder of the store: `(pid, human-readable target)`. `pid < 0` marks an
/// UNKNOWN holder — a scan we could not complete — which must block maintenance
/// exactly like a real one.
pub type Holder = (i32, String);

/// Read `/proc/<pid>/cmdline` without losing argv boundaries (upstream
/// `_read_proc_argv`): NUL-separated, trailing NUL dropped, `None` when empty or
/// unreadable.
#[cfg(target_os = "linux")]
pub fn read_proc_argv(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    // Lossy UTF-8, split on NUL — argv boundaries must survive.
    let text = String::from_utf8_lossy(&raw);
    let mut argv: Vec<String> = text.split('\0').map(str::to_string).collect();
    if argv.last().map(|s| s.is_empty()).unwrap_or(false) {
        argv.pop();
    }
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// `PID 123 (ulnclaw gateway run)` for operator-facing holder lists
/// (upstream `describe_holder_pid`). Falls back to a fixed string when the
/// command line cannot be read.
#[cfg(target_os = "linux")]
pub fn describe_holder_pid(pid: i32) -> String {
    let who = match read_proc_argv(pid) {
        Some(argv) if !argv.is_empty() => {
            let base = Path::new(argv[0].as_str())
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let joined = std::iter::once(base)
                .chain(argv.iter().skip(1).cloned())
                .collect::<Vec<_>>()
                .join(" ");
            let collapsed = joined.split_whitespace().collect::<Vec<_>>().join(" ");
            collapsed.chars().take(80).collect::<String>()
        }
        _ => "command line unavailable".to_string(),
    };
    format!("PID {pid} ({who})")
}

/// ENOENT / ESRCH: the process or fd vanished mid-scan. Upstream treats these as
/// "skip this one, keep scanning" — a dead process holds nothing. Every OTHER
/// errno is a genuine inability to prove quiescence, i.e. fail closed.
#[cfg(unix)]
fn is_gone_error(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::ENOENT) | Some(libc::ESRCH))
}

/// Foreign holders of `db_path` or one of its WAL sidecars, cross-PROCESS.
///
/// Mirrors upstream `foreign_state_db_holders`. 🔴 `realpath` (not `abspath`):
/// `/proc` fd targets are kernel-resolved, so a symlinked `ULNCLAW_HOME` would
/// otherwise make every holder invisible and let maintenance proceed. Matching is
/// by `(st_dev, st_ino)`, not by name, so a hardlink or a bind-mount still
/// matches; the `" (deleted)"` suffix arm catches a retired-but-still-open WAL.
#[cfg(target_os = "linux")]
pub fn foreign_state_db_holders(db_path: &Path) -> Vec<Holder> {
    let mut holders: Vec<Holder> = Vec::new();

    // realpath the store first; every watched path derives from the real one.
    let real_db = match std::fs::canonicalize(db_path) {
        Ok(p) => p,
        // A store we cannot even resolve: fail closed (unknown holder).
        Err(e) => return vec![(-1, format!("state.db realpath failed: {e}"))],
    };
    let db_str = real_db.to_string_lossy().to_string();

    // watched = canonical paths of (db, -wal, -shm); watched_ids = their
    // (dev, ino) pairs; db_dev = the store's device (for the deleted-WAL arm).
    let mut watched: HashSet<String> = HashSet::new();
    let mut watched_ids: HashSet<(u64, u64)> = HashSet::new();
    let mut db_dev: Option<u64> = None;

    for candidate in sqlite_family(db_str.as_str()) {
        watched.insert(canonical_sqlite_path(candidate.as_str()));
        match std::fs::metadata(candidate.as_str()) {
            Ok(meta) => {
                watched_ids.insert((meta.dev(), meta.ino()));
                if candidate == db_str {
                    db_dev = Some(meta.dev());
                }
            }
            // A stat failure other than ENOENT/ESRCH is itself an unknown holder:
            // we cannot prove the sidecar is closed.
            Err(e) => {
                if !is_gone_error(&e) {
                    holders.push((-1, format!("watched-file stat failed: {candidate}: {e}")));
                }
            }
        }
    }

    let own_pid = std::process::id() as i32;

    // Walk /proc/*/fd. Any failure to even LIST /proc is fatal to the proof.
    let proc_entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(e) => {
            holders.push((-1, format!("open-file scan failed: {e}")));
            return holders;
        }
    };

    for entry in proc_entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(pid) = name.parse::<i32>() else {
            continue; // not a numeric pid dir
        };
        if pid == own_pid {
            continue; // foreign only; the in-process arm answers that question
        }

        let fd_dir = format!("/proc/{pid}/fd");
        let fds = match std::fs::read_dir(fd_dir.as_str()) {
            Ok(d) => d,
            // 🔴 Unreadable fd dir: fall back to argv. Count it ONLY when the
            // process is plausibly ours AND not provably another instance —
            // otherwise every other user's process on a shared host would pin
            // VACUUM forever (the exact failure slice2a exists to prevent).
            Err(_) => {
                if let Some(argv) = read_proc_argv(pid) {
                    if looks_like_ulnclaw(&argv) && !argv_scoped_to_other_home(&argv, &real_db) {
                        let cmdline = argv.join(" ");
                        let head: String = cmdline.chars().take(80).collect();
                        holders.push((pid, format!("uninspectable holder: {head}")));
                    }
                }
                continue;
            }
        };

        for fd in fds.flatten() {
            let fd_path = fd.path();
            let target = match std::fs::read_link(fd_path.as_path()) {
                Ok(t) => t.to_string_lossy().to_string(),
                Err(e) => {
                    if is_gone_error(&e) {
                        continue;
                    }
                    if let Some(argv) = read_proc_argv(pid) {
                        if looks_like_ulnclaw(&argv) && !argv_scoped_to_other_home(&argv, &real_db)
                        {
                            holders.push((
                                pid,
                                format!("uninspectable descriptor: {}: {e}", fd_path.display()),
                            ));
                        }
                    }
                    continue;
                }
            };

            let target_is_watched =
                watched.contains(canonical_sqlite_path(target.as_str()).as_str());

            // stat the fd to get (dev, ino). A failure here is fatal to THIS fd.
            let fd_stat = match std::fs::metadata(fd_path.as_path()) {
                Ok(m) => m,
                Err(e) => {
                    if is_gone_error(&e) {
                        continue;
                    }
                    if target_is_watched {
                        holders.push((pid, format!("uninspectable descriptor: {target}: {e}")));
                    } else if let Some(argv) = read_proc_argv(pid) {
                        if looks_like_ulnclaw(&argv) && !argv_scoped_to_other_home(&argv, &real_db)
                        {
                            holders.push((pid, format!("uninspectable descriptor: {target}: {e}")));
                        }
                    }
                    continue;
                }
            };

            let hit = watched_ids.contains(&(fd_stat.dev(), fd_stat.ino()))
                || (target_is_watched
                    && target.ends_with(DELETED_SUFFIX)
                    && db_dev.map(|d| fd_stat.dev() == d).unwrap_or(false));
            if hit {
                holders.push((pid, target));
            }
        }
    }

    holders
}

// ---------------------------------------------------------------------------
// Non-Linux / non-unix / Windows: fail CLOSED. Upstream proves quiescence on
// those platforms with psutil (macOS/BSD) and the Restart Manager (Windows);
// neither is ported yet (slice3/slice4), so an unprovable scan reports an
// unknown holder rather than an all-clear.
// ---------------------------------------------------------------------------

/// POSIX-but-not-Linux (macOS/BSD): upstream uses `psutil.process_iter`.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn foreign_state_db_holders(_db_path: &Path) -> Vec<Holder> {
    vec![(-1, "open-file scan unavailable".to_string())]
}

/// Windows: upstream uses the Restart Manager (slice4). Until then, fail closed.
#[cfg(windows)]
pub fn foreign_state_db_holders(_db_path: &Path) -> Vec<Holder> {
    vec![(
        -1,
        "Windows Restart Manager scan not yet ported".to_string(),
    )]
}

#[cfg(all(test, target_os = "linux"))]
mod slice2b_tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    /// Open a file and leak the fd for the test's lifetime, so this process is a
    /// real holder of it. Returns the File (kept alive by the caller).
    fn open_holder(path: &Path) -> fs::File {
        fs::OpenOptions::new().read(true).open(path).unwrap()
    }

    #[test]
    fn read_proc_argv_returns_our_own_command_line() {
        let pid = std::process::id() as i32;
        let argv = read_proc_argv(pid).expect("our own /proc/<pid>/cmdline is readable");
        assert!(!argv.is_empty());
        // The test harness binary; just assert argv[0] is non-empty and has no NUL.
        assert!(!argv[0].is_empty());
        assert!(!argv.iter().any(|a| a.contains('\0')));
    }

    #[test]
    fn read_proc_argv_is_none_for_a_dead_pid() {
        // pid 0 is never a real process dir with a readable cmdline.
        assert!(
            read_proc_argv(0).is_none() || read_proc_argv(0).map(|a| a.is_empty()).unwrap_or(true)
        );
    }

    #[test]
    fn describe_holder_pid_formats_pid_and_name() {
        let pid = std::process::id() as i32;
        let s = describe_holder_pid(pid);
        assert!(s.starts_with(&format!("PID {pid} (")), "got: {s}");
        assert!(s.ends_with(')'));
    }

    #[test]
    fn foreign_scan_finds_a_real_holder_of_our_own_store() {
        // This test process holds the file open, but foreign_state_db_holders
        // SKIPS our own pid by design (that is the in-process arm's job). So we
        // assert the weaker, still-load-bearing property: a store under a tempdir
        // with NO other holder yields no holders, and the scan does not error out.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let mut f = fs::File::create(&db).unwrap();
        f.write_all(b"x").unwrap();
        drop(f);

        let holders = foreign_state_db_holders(&db);
        // No foreign process opened it; our own fd is closed and our pid is
        // skipped anyway. Must be empty (no false positives) and no error holder.
        assert!(
            holders.is_empty(),
            "expected no holders for a quiescent store, got {holders:?}"
        );
    }

    #[test]
    fn foreign_scan_fails_closed_on_an_unresolvable_store() {
        // A path that cannot be realpath'd: the proof is impossible, so it must
        // report an UNKNOWN holder (pid < 0), never an empty all-clear.
        let holders = foreign_state_db_holders(Path::new("/definitely/not/here/state.db"));
        assert_eq!(holders.len(), 1);
        assert!(
            holders[0].0 < 0,
            "unknown holder must carry a negative pid: {holders:?}"
        );
        assert!(holders[0].1.contains("realpath"), "{:?}", holders[0].1);
    }

    #[test]
    fn wal_and_shm_sidecars_are_watched() {
        // A holder of the -wal sidecar pins the generation exactly like the main
        // db. Prove the watched set includes all three by canonicalizing each.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        fs::write(&db, b"x").unwrap();
        fs::write(dir.path().join("state.db-wal"), b"y").unwrap();
        fs::write(dir.path().join("state.db-shm"), b"z").unwrap();

        let fam = sqlite_family(db.to_str().unwrap());
        assert_eq!(fam.len(), 3);
        assert!(fam[1].ends_with("-wal"));
        assert!(fam[2].ends_with("-shm"));
        // All three exist, so the scan resolves them without an unknown holder.
        let holders = foreign_state_db_holders(&db);
        assert!(
            holders.is_empty(),
            "quiescent store with sidecars: {holders:?}"
        );
    }
}

// ===========================================================================
// slice3a — the in-process live-store registry (substrate for the in_process
// arm of the VACUUM gate). NOT YET WIRED into SqliteSessionStore; slice3b adds
// the `instance_id` field, registers in `open()`, deregisters in `Drop`, and
// feeds `in_process_state_db_holders` into `maybe_auto_prune_and_vacuum`.
//
// Why this exists: `foreign_state_db_holders` (slice2b) skips our own pid, so it
// answers a cross-PROCESS question only. Upstream pairs it with an in-process arm
// because "a VACUUM plus its TRUNCATE checkpoint retires the generation a sibling
// SessionDB in this very process still holds" (hermes `in_process_state_db_holders`
// → `hermes_state_registry.other_generations_for_path`). Upstream's registry is a
// 478-line refcount+teardown system because every store goes through
// `acquire()`/`release()`. This port opens each store independently, so the
// faithful minimum is a path-keyed set of live instance ids: register on open,
// deregister on drop, and answer "who else in THIS process holds this path?".
//
// 🔴 Without a registry the in-process arm cannot be implemented at all, and the
// VACUUM gate must stay fail-closed forever (it can never prove no in-process
// sibling). This registry is the load-bearing substrate that lets the gate open.
// It is now WIRED: `SqliteSessionStore::open` registers, `Drop` deregisters, and
// `maybe_auto_prune_and_vacuum` sums this arm with the foreign scan before it
// admits a VACUUM.
// ===========================================================================

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Live stores keyed by resolved db path → set of instance ids. Process-global:
/// the question "does THIS process hold another generation of this path?" is
/// inherently process-scoped. `Mutex` (not `RwLock`) because every op mutates or
/// is trivially short; contention is nil (open/close are rare).
static LIVE_STORES: OnceLock<Mutex<HashMap<String, HashSet<u64>>>> = OnceLock::new();
/// Monotonic instance id source. Ids are never reused within a process, so a
/// stale deregister cannot remove a live store's entry.
static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

fn live_stores() -> &'static Mutex<HashMap<String, HashSet<u64>>> {
    LIVE_STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The registry key for a store path: resolved (symlink-collapsed) + normalized,
/// so a symlinked `ULNCLAW_HOME` and its target share one key — the same
/// realpath-not-abspath discipline the foreign scan uses. A store that cannot be
/// resolved yet (rare at open time) falls back to the lexical absolute path.
fn store_registry_key(db_path: &Path) -> String {
    norm_path(&resolve_lenient(db_path).to_string_lossy())
}

/// Register a freshly-opened store; returns its process-unique instance id.
/// The caller (slice3b: `SqliteSessionStore::open`) stores the id and passes it
/// to [`unregister_store`] from `Drop`.
pub fn register_store(db_path: &Path) -> u64 {
    let id = NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed);
    let key = store_registry_key(db_path);
    // A poisoned lock means a prior holder panicked mid-update; recover the guard
    // rather than propagate — a holder scan must never take the process down.
    let mut map = match live_stores().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    map.entry(key).or_default().insert(id);
    id
}

/// Deregister a store on drop. Idempotent: a double-drop or an id that was never
/// registered is a no-op, never a panic.
pub fn unregister_store(db_path: &Path, instance_id: u64) {
    let key = store_registry_key(db_path);
    let mut map = match live_stores().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(set) = map.get_mut(key.as_str()) {
        set.remove(&instance_id);
        if set.is_empty() {
            map.remove(key.as_str());
        }
    }
}

/// Holders of `db_path` inside THIS process, other than `exclude` — the
/// in-process analogue of [`foreign_state_db_holders`]. Each entry carries the
/// current pid (upstream returns `(os.getpid(), description)`), so a consumer can
/// tell an in-process sibling from a foreign pid by the description text.
///
/// 🔴 Returns every OTHER live registration for the resolved path. An empty
/// result is the "no in-process sibling" proof the VACUUM gate needs, and it is
/// trustworthy because `SqliteSessionStore::open` registers the store and its
/// `Drop` deregisters it. This arm IS consumed: `maybe_auto_prune_and_vacuum`
/// sums it with `foreign_state_db_holders` (excluding `self.instance_id`) and
/// skips the VACUUM whenever the sum is non-empty.
pub fn in_process_state_db_holders(db_path: &Path, exclude: Option<u64>) -> Vec<Holder> {
    let key = store_registry_key(db_path);
    let map = match live_stores().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(set) = map.get(key.as_str()) else {
        return Vec::new();
    };
    let pid = std::process::id() as i32;
    let mut out: Vec<Holder> = set
        .iter()
        .filter(|id| Some(**id) != exclude)
        .map(|id| {
            (
                pid,
                format!("in-process live SqliteSessionStore generation (id {id})"),
            )
        })
        .collect();
    // Deterministic order for stable logs/tests (HashSet iteration is unordered).
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// Snapshot of the registry size, for tests and diagnostics (upstream `stats()`).
pub fn live_store_count() -> usize {
    let map = match live_stores().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    map.values().map(|set| set.len()).sum()
}

#[cfg(test)]
mod slice3a_tests {
    use super::*;
    use std::fs;

    // 🔴 The registry is process-global mutable state shared across tests. Each
    // test uses its OWN tempdir path as the key, so path-keying isolates them —
    // a test querying its own path never sees another test's registrations.
    // Instance ids are globally unique, so `exclude` is unambiguous too.

    #[test]
    fn register_then_query_returns_the_live_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        fs::write(&db, b"x").unwrap();

        let id = register_store(&db);
        let holders = in_process_state_db_holders(&db, None);
        assert_eq!(holders.len(), 1, "one live store => one holder");
        assert_eq!(
            holders[0].0,
            std::process::id() as i32,
            "in-process arm carries our pid"
        );
        assert!(holders[0].1.contains("in-process"), "{:?}", holders[0].1);

        unregister_store(&db, id);
        assert!(in_process_state_db_holders(&db, None).is_empty());
    }

    #[test]
    fn exclude_removes_only_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        fs::write(&db, b"x").unwrap();

        let a = register_store(&db);
        let b = register_store(&db);
        assert_ne!(a, b, "instance ids are unique per open");

        // The store doing the VACUUM excludes itself; the sibling remains.
        let others = in_process_state_db_holders(&db, Some(a));
        assert_eq!(others.len(), 1);
        assert!(
            others[0].1.contains(&format!("id {b}")),
            "{:?}",
            others[0].1
        );

        // Excluding the other one leaves the first.
        let others = in_process_state_db_holders(&db, Some(b));
        assert_eq!(others.len(), 1);
        assert!(others[0].1.contains(&format!("id {a}")));

        // With no exclusion, both are holders.
        assert_eq!(in_process_state_db_holders(&db, None).len(), 2);

        unregister_store(&db, a);
        unregister_store(&db, b);
    }

    #[test]
    fn distinct_paths_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let db1 = dir.path().join("one").join("state.db");
        let db2 = dir.path().join("two").join("state.db");
        fs::create_dir_all(db1.parent().unwrap()).unwrap();
        fs::create_dir_all(db2.parent().unwrap()).unwrap();
        fs::write(&db1, b"x").unwrap();
        fs::write(&db2, b"y").unwrap();

        let id1 = register_store(&db1);
        assert_eq!(in_process_state_db_holders(&db1, None).len(), 1);
        // A different path has no holders from db1's registration.
        assert!(in_process_state_db_holders(&db2, None).is_empty());

        unregister_store(&db1, id1);
    }

    #[test]
    fn unregister_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        fs::write(&db, b"x").unwrap();

        let id = register_store(&db);
        unregister_store(&db, id);
        // Double-drop and never-registered ids must be silent no-ops, not panics.
        unregister_store(&db, id);
        unregister_store(&db, 999_999_999);
        assert!(in_process_state_db_holders(&db, None).is_empty());
    }

    #[test]
    fn empty_dir_entry_is_removed_after_last_unregister() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        fs::write(&db, b"x").unwrap();

        let before = live_store_count();
        let id = register_store(&db);
        assert_eq!(live_store_count(), before + 1);
        unregister_store(&db, id);
        assert_eq!(
            live_store_count(),
            before,
            "empty path key is pruned, no leak"
        );
    }

    #[test]
    fn symlinked_path_shares_one_key_with_its_target() {
        // realpath-not-abspath discipline: a symlink and its target must collide
        // on one key, or an in-process sibling reached via the other name is
        // invisible — the same failure the foreign scan guards against.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-state.db");
        fs::write(&real, b"x").unwrap();
        let link = dir.path().join("link-state.db");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        let _ = (&real.clone(), &link.clone());

        #[cfg(unix)]
        {
            let id = register_store(&real);
            // Querying via the symlink resolves to the same key and finds it.
            let via_link = in_process_state_db_holders(&link, None);
            assert_eq!(
                via_link.len(),
                1,
                "symlink resolves to the same registry key"
            );
            unregister_store(&link, id);
            assert!(in_process_state_db_holders(&real, None).is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sqlite_family_covers_the_wal_sidecars() {
        assert_eq!(
            sqlite_family("/home/u/.ulnclaw/state.db"),
            [
                "/home/u/.ulnclaw/state.db".to_string(),
                "/home/u/.ulnclaw/state.db-wal".to_string(),
                "/home/u/.ulnclaw/state.db-shm".to_string(),
            ]
        );
    }

    #[test]
    fn canonical_path_strips_the_deleted_marker() {
        // A retired-but-still-open WAL generation must compare equal to the live
        // path, or the scan misses exactly the case VACUUM would corrupt.
        let live = canonical_sqlite_path("/home/u/.ulnclaw/state.db-wal");
        let deleted = canonical_sqlite_path("/home/u/.ulnclaw/state.db-wal (deleted)");

        assert_eq!(live, deleted);
    }

    #[test]
    fn canonical_path_collapses_segments_lexically() {
        let direct = canonical_sqlite_path("/home/u/.ulnclaw/state.db");
        let indirect = canonical_sqlite_path("/home/u/.ulnclaw/profiles/../state.db");

        assert_eq!(direct, indirect);
    }

    #[test]
    fn norm_path_keeps_relative_paths_relative() {
        // os.path.normpath does NOT absolutize; matching must not invent a cwd.
        assert_eq!(norm_path("a/./b/../c"), norm_path("a/c"));
        assert!(!norm_path("a/c").starts_with('/'));
    }

    #[test]
    fn argv_flag_value_is_token_exact_and_takes_the_last() {
        let args = argv(&["ulnclaw", "--profile", "timothy", "gateway"]);

        assert_eq!(
            argv_flag_value(&args, PROFILE_FLAGS).as_deref(),
            Some("timothy")
        );
        // A prefix of a profile name is a different profile: never a match.
        assert_eq!(argv_flag_value(&args, &["tim"]), None);
        // Token-exact in the other direction too: a flag that is a PREFIX of a
        // token on the command line must not match it, or an unrelated option
        // (`--profiletim`) would be read as our profile selection — and a wrong
        // "other profile" reading is what dismisses a real holder of ours.
        assert_eq!(
            argv_flag_value(&argv(&["ulnclaw", "--profiletim", "x"]), PROFILE_FLAGS),
            None
        );
        assert_eq!(argv_flag_value(&args, &["--prof"]), None);
    }

    #[test]
    fn argv_flag_value_accepts_the_equals_form_and_last_wins() {
        let args = argv(&["ulnclaw", "--profile=a", "--profile=b"]);

        assert_eq!(argv_flag_value(&args, PROFILE_FLAGS).as_deref(), Some("b"));
    }

    #[test]
    fn argv_flag_value_ignores_a_flag_with_no_operand() {
        // `--profile` as the final token has no value; scanning past the end must
        // not wrap around and answer with something unrelated.
        let args = argv(&["ulnclaw", "--profile"]);

        assert_eq!(argv_flag_value(&args, PROFILE_FLAGS), None);
    }

    #[test]
    fn home_flags_are_empty_so_no_flag_can_select_a_home() {
        // Documents the verified fact that ulnclaw has no `--ulnclaw-home` CLI
        // flag: home rides the ULNCLAW_HOME env instead. If a home flag is ever
        // added to `Cli`, this test fails and HOME_FLAGS must be filled in —
        // otherwise the other-home reasoning silently loses an input.
        assert!(HOME_FLAGS.is_empty());
        assert_eq!(
            argv_flag_value(&argv(&["--hermes-home", "/x"]), HOME_FLAGS),
            None
        );
    }

    #[test]
    fn argv_env_home_reads_the_env_launch_form_backwards() {
        let args = argv(&[
            "env",
            "ULNCLAW_HOME=/home/a/.ulnclaw",
            "ULNCLAW_HOME=/home/b/.ulnclaw",
            "ulnclaw",
            "gateway",
        ]);

        assert_eq!(argv_env_home(&args).as_deref(), Some("/home/b/.ulnclaw"));
        assert_eq!(argv_env_home(&argv(&["ulnclaw", "gateway"])), None);
    }

    #[test]
    fn argv_path_tokens_keeps_only_absolute_paths_and_remembers_their_index() {
        let args = argv(&[
            "/usr/local/bin/ulnclaw",
            "gateway",
            "--db=/home/a/.ulnclaw/state.db",
            "relative/state.db",
            "--config=config.toml",
        ]);
        let tokens = argv_path_tokens(&args);

        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].0, 0);
        assert_eq!(tokens[0].1, norm_path("/usr/local/bin/ulnclaw"));
        // The option VALUE is anchored, not the whole `--db=…` token.
        assert_eq!(tokens[1].0, 2);
        assert_eq!(tokens[1].1, norm_path("/home/a/.ulnclaw/state.db"));
    }

    #[test]
    fn argv_path_tokens_skips_a_relative_option_value() {
        // Anchoring a relative value on OUR cwd would fabricate evidence about a
        // process whose cwd we cannot observe.
        let tokens = argv_path_tokens(&argv(&["ulnclaw", "--db=state.db"]));

        assert!(tokens.is_empty());
    }

    #[test]
    fn looks_like_ulnclaw_matches_the_binary_by_basename() {
        assert!(looks_like_ulnclaw(&argv(&[
            "/usr/local/bin/ulnclaw",
            "gateway"
        ])));
        assert!(looks_like_ulnclaw(&argv(&["ulnclaw"])));
        assert!(looks_like_ulnclaw(&argv(&["./ULNCLAW"])));
    }

    /// The Windows argv form. `\` is only a path separator on Windows — on Linux
    /// `C:\bin\ulnclaw.exe` is a single filename component — and the scan only
    /// ever reads argv from its own OS, so this case belongs to that platform.
    #[cfg(windows)]
    #[test]
    fn looks_like_ulnclaw_matches_the_windows_binary_form() {
        assert!(looks_like_ulnclaw(&argv(&[
            "C:\\bin\\ulnclaw.exe",
            "gateway"
        ])));
    }

    #[test]
    fn looks_like_ulnclaw_rejects_other_processes_and_an_empty_argv() {
        assert!(!looks_like_ulnclaw(&argv(&[
            "/usr/bin/python3",
            "-m",
            "ulnclaw_cli.main"
        ])));
        assert!(!looks_like_ulnclaw(&argv(&["/usr/bin/ulnclaw-helper"])));
        assert!(!looks_like_ulnclaw(&[]));
    }

    #[test]
    fn executable_set_names_the_single_bin_target() {
        // Guards the fail-open direction described in the module header: this set
        // must cover every [[bin]] the crate ships. Cargo.toml declares exactly
        // one, `ulnclaw`.
        assert_eq!(ULNCLAW_EXECUTABLES, &["ulnclaw"]);

        let manifest = include_str!("../Cargo.toml");
        let bin_names: Vec<&str> = manifest
            .split("[[bin]]")
            .skip(1)
            .filter_map(|section| {
                section
                    .lines()
                    .find_map(|line| line.strip_prefix("name = "))
                    .map(|raw| raw.trim().trim_matches('"'))
            })
            .collect();

        for name in &bin_names {
            assert!(
                ULNCLAW_EXECUTABLES.contains(name),
                "[[bin]] `{name}` is missing from ULNCLAW_EXECUTABLES — the holder scan would \
                 fail OPEN for it (an unreadable /proc fd would not be suspected)"
            );
        }
        assert!(
            !bin_names.is_empty(),
            "expected at least one [[bin]] target"
        );
    }
}

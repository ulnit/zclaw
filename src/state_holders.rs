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

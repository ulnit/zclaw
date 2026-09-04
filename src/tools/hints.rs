//! Terminal failure intelligence — port of hermes' `tools/terminal_hints.py`
//! plus the `_interpret_exit_code` semantics table from `terminal_tool.py`.
//!
//! Two complementary layers keep the model from burning turns on
//! re-diagnosis after a failed command:
//!
//! - [`interpret_exit_code`] maps non-zero exit codes that are *not* real
//!   errors (grep=1 means "no matches", diff=1 means "files differ", …)
//!   to a short note surfaced as `exit_code_meaning`.
//! - [`annotate_failure`] scans the first [`SCAN_CHARS`] characters of a
//!   failed command's output for well-known failure shapes and returns at
//!   most one actionable recovery hint, surfaced as `hint`. Exit-code-only
//!   hints (124/126/137) cover codes the semantics table does not explain.
//!
//! Design rules carried over from hermes: hints only fire on non-zero exit
//! codes, at most one hint per result (first match wins, patterns ordered
//! by production frequency), the scan window is bounded, and hints state
//! the next action rather than a diagnosis essay. Pure functions, no I/O.

use regex::Regex;
use std::sync::OnceLock;

/// Bounded scan window: error headers appear early; deep output is noise.
pub const SCAN_CHARS: usize = 4000;

/// Return a human-readable note when a non-zero exit code is non-erroneous.
///
/// Returns `None` when the exit code is 0 or genuinely signals an error.
/// The note is appended to the tool result so the model doesn't waste
/// turns investigating expected exit codes.
pub fn interpret_exit_code(command: &str, exit_code: i32) -> Option<&'static str> {
    if exit_code == 0 {
        return None;
    }

    // Extract the last command in a pipeline/chain — that determines the
    // exit code. Handles `cmd1 && cmd2`, `cmd1 | cmd2`, `cmd1; cmd2`.
    let splitter = Regex::new(r"\s*(?:\|\||&&|[|;])\s*").expect("static regex");
    let segments: Vec<&str> = splitter.split(command).collect();
    let last_segment = segments.last().copied().unwrap_or(command).trim();

    // Get base command name (first word), stripping env var assignments
    // like `VAR=val cmd ...`.
    let mut base_cmd = String::new();
    for word in last_segment.split_whitespace() {
        if word.contains('=') && !word.starts_with('-') {
            continue; // skip VAR=val
        }
        // handle /usr/bin/grep -> grep
        base_cmd = word.rsplit('/').next().unwrap_or(word).to_string();
        break;
    }
    if base_cmd.is_empty() {
        return None;
    }

    let note = match base_cmd.as_str() {
        // grep/rg/ag/ack: 1=no matches found (normal), 2+=real error
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" if exit_code == 1 => {
            "No matches found (not an error)"
        }
        // diff: 1=files differ (expected), 2+=real error
        "diff" | "colordiff" if exit_code == 1 => "Files differ (expected, not an error)",
        // find: 1=some dirs inaccessible but results may still be valid
        "find" if exit_code == 1 => {
            "Some directories were inaccessible (partial results may still be valid)"
        }
        // test/[: 1=condition is false (expected)
        "test" | "[" if exit_code == 1 => "Condition evaluated to false (expected, not an error)",
        // curl: common non-error codes
        "curl" => match exit_code {
            6 => "Could not resolve host",
            7 => "Failed to connect to host",
            22 => "HTTP response code indicated error (e.g. 404, 500)",
            28 => "Operation timed out",
            _ => return None,
        },
        // git: 1 is context-dependent but often normal
        "git" if exit_code == 1 => {
            "Non-zero exit (often normal — e.g. 'git diff' returns 1 when files differ)"
        }
        _ => return None,
    };
    Some(note)
}

fn hint_gh_unknown_json_field(output: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"Unknown JSON field: "?(\w+)"#).expect("static regex"));
    let m = re.captures(output)?;
    Some(format!(
        "The installed gh does not support the JSON field '{}'. \
         The valid field list is printed in the output above — retry using \
         only fields from that list.",
        &m[1]
    ))
}

fn hint_merge_conflict(output: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?m)^CONFLICT |Automatic merge failed|needs merge").expect("static regex")
    });
    if !re.is_match(output) {
        return None;
    }
    Some(
        "Git merge conflict. Do not retry this command. Resolve the \
         conflicted files listed above (edit, then `git add`), then continue \
         (`git rebase --continue` / commit the merge) — or abort with \
         `--abort`."
            .to_string(),
    )
}

fn hint_command_not_found(output: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:bash: line \d+: |bash: |sh: \d*:? ?)?([\w.+-]+): command not found")
            .expect("static regex")
    });
    let m = re.captures(output)?;
    let missing = &m[1];
    if missing == "python" {
        return Some(
            "This system has no bare `python` — use `python3`, or the \
             project venv's interpreter (e.g. .venv/bin/python)."
                .to_string(),
        );
    }
    if missing == "pip" {
        return Some(
            "This system has no bare `pip` — use `pip3`, `python3 -m pip`, \
             or the project venv's pip (e.g. .venv/bin/pip)."
                .to_string(),
        );
    }
    Some(format!(
        "`{missing}` is not installed or not on PATH. Verify with \
         `which {missing}`; install it or use an absolute path instead of \
         retrying the same command."
    ))
}

fn hint_module_not_found(output: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:ModuleNotFoundError|ImportError): No module named '?([\w.]+)")
            .expect("static regex")
    });
    let m = re.captures(output)?;
    Some(format!(
        "Python cannot import '{}'. Most often the wrong \
         interpreter is running: activate the project venv (e.g. `source \
         .venv/bin/activate`) or invoke its python directly. Only pip \
         install if the package is genuinely absent from that venv.",
        &m[1]
    ))
}

fn hint_already_exists(output: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:fatal|error):.*?'([^']+)' already exists").expect("static regex")
    });
    let m = re.captures(output)?;
    Some(format!(
        "'{}' already exists — retrying unchanged will keep \
         failing. Reuse it, choose another name, or delete it first if it is \
         genuinely stale.",
        &m[1]
    ))
}

fn hint_gh_rate_limit(output: &str) -> Option<String> {
    if !output.contains("API rate limit") && !output.contains("was submitted too quickly") {
        return None;
    }
    Some(
        "GitHub API rate limit hit — immediate retries will keep failing. \
         Continue with other work and retry this operation later."
            .to_string(),
    )
}

fn hint_permission_denied(output: &str) -> Option<String> {
    if !output.contains("Permission denied") && !output.contains("EACCES") {
        return None;
    }
    Some(
        "Permission denied. Check ownership/mode of the target path \
         (`ls -la`); prefer a user-writable location. Only escalate to sudo \
         if the task genuinely requires it."
            .to_string(),
    )
}

/// Ordered by production frequency — first match wins.
fn output_hint(output: &str) -> Option<String> {
    if let Some(h) = hint_gh_unknown_json_field(output) {
        return Some(h);
    }
    if let Some(h) = hint_merge_conflict(output) {
        return Some(h);
    }
    if let Some(h) = hint_command_not_found(output) {
        return Some(h);
    }
    if let Some(h) = hint_module_not_found(output) {
        return Some(h);
    }
    if let Some(h) = hint_already_exists(output) {
        return Some(h);
    }
    if let Some(h) = hint_gh_rate_limit(output) {
        return Some(h);
    }
    hint_permission_denied(output)
}

/// Exit-code-only hints for codes the semantics table does not cover
/// per-command. Checked after output patterns.
fn exit_code_hint(exit_code: i32) -> Option<&'static str> {
    match exit_code {
        126 => Some(
            "Exit 126: the file was found but is not executable — `chmod +x` it \
             or invoke it via its interpreter (e.g. `bash script.sh`).",
        ),
        137 => Some(
            "Exit 137: the process was SIGKILLed — usually out-of-memory or an \
             external kill. Reduce memory use or check `dmesg | tail` before retrying.",
        ),
        124 => Some(
            "Exit 124: the command hit its timeout. Raise timeout= (foreground \
             max 600s) or run it with background=true and notify_on_complete=true.",
        ),
        _ => None,
    }
}

/// Return one short recovery hint for a failed command, or `None`.
///
/// Only the first [`SCAN_CHARS`] characters of `output` are examined and
/// at most one hint is returned. Returns `None` for `exit_code == 0`.
pub fn annotate_failure(_command: &str, exit_code: i32, output: &str) -> Option<String> {
    if exit_code == 0 {
        return None;
    }
    let window: String = output.chars().take(SCAN_CHARS).collect();
    if !window.is_empty() {
        if let Some(h) = output_hint(&window) {
            return Some(h);
        }
    }
    exit_code_hint(exit_code).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_grep_no_matches() {
        assert_eq!(
            interpret_exit_code("grep foo bar.txt", 1),
            Some("No matches found (not an error)")
        );
        assert_eq!(interpret_exit_code("grep foo bar.txt", 2), None);
    }

    #[test]
    fn interpret_pipeline_last_segment() {
        assert_eq!(
            interpret_exit_code("cat x | rg pattern", 1),
            Some("No matches found (not an error)")
        );
        assert_eq!(
            interpret_exit_code("cd /tmp && diff a b", 1),
            Some("Files differ (expected, not an error)")
        );
        assert_eq!(
            interpret_exit_code("FOO=bar /usr/bin/git status", 1),
            Some("Non-zero exit (often normal — e.g. 'git diff' returns 1 when files differ)")
        );
    }

    #[test]
    fn interpret_curl_codes() {
        assert_eq!(
            interpret_exit_code("curl https://x", 6),
            Some("Could not resolve host")
        );
        assert_eq!(
            interpret_exit_code("curl https://x", 28),
            Some("Operation timed out")
        );
        assert_eq!(interpret_exit_code("curl https://x", 0), None);
        assert_eq!(interpret_exit_code("curl https://x", 5), None);
    }

    #[test]
    fn interpret_test_and_find() {
        assert_eq!(
            interpret_exit_code("test -f missing", 1),
            Some("Condition evaluated to false (expected, not an error)")
        );
        assert_eq!(
            interpret_exit_code("[ -d /nope ]", 1),
            Some("Condition evaluated to false (expected, not an error)")
        );
        assert_eq!(
            interpret_exit_code("find / -name x", 1),
            Some("Some directories were inaccessible (partial results may still be valid)")
        );
    }

    #[test]
    fn annotate_success_is_none() {
        assert_eq!(annotate_failure("echo hi", 0, "hi"), None);
    }

    #[test]
    fn annotate_command_not_found_python() {
        let out = "bash: line 1: python: command not found";
        let hint = annotate_failure("python x.py", 127, out).unwrap();
        assert!(hint.contains("python3"), "got: {hint}");
    }

    #[test]
    fn annotate_command_not_found_generic() {
        let out = "sh: 1: jq: command not found";
        let hint = annotate_failure("jq .", 127, out).unwrap();
        assert!(hint.contains("`jq`"), "got: {hint}");
        assert!(hint.contains("which jq"), "got: {hint}");
    }

    #[test]
    fn annotate_module_not_found() {
        let out =
            "Traceback (most recent call last):\nModuleNotFoundError: No module named 'requests'";
        let hint = annotate_failure("python3 a.py", 1, out).unwrap();
        assert!(hint.contains("requests"), "got: {hint}");
        assert!(hint.contains("venv"), "got: {hint}");
    }

    #[test]
    fn annotate_merge_conflict() {
        let out = "Auto-merging src/main.rs\nCONFLICT (content): Merge conflict in src/main.rs\nAutomatic merge failed; fix conflicts and then commit the result.";
        let hint = annotate_failure("git merge feature", 1, out).unwrap();
        assert!(hint.contains("Do not retry"), "got: {hint}");
    }

    #[test]
    fn annotate_already_exists() {
        let out = "fatal: 'feature/x' already exists";
        let hint = annotate_failure("git branch feature/x", 128, out).unwrap();
        assert!(hint.contains("feature/x"), "got: {hint}");
    }

    #[test]
    fn annotate_gh_field_and_rate_limit() {
        let out = "Unknown JSON field: \"fooBar\"";
        let hint = annotate_failure("gh pr list --json fooBar", 1, out).unwrap();
        assert!(hint.contains("fooBar"), "got: {hint}");

        let out = "GraphQL: API rate limit exceeded";
        let hint = annotate_failure("gh api x", 1, out).unwrap();
        assert!(hint.contains("rate limit"), "got: {hint}");
    }

    #[test]
    fn annotate_permission_denied() {
        let hint =
            annotate_failure("cat /etc/shadow", 1, "cat: /etc/shadow: Permission denied").unwrap();
        assert!(hint.contains("Permission denied"), "got: {hint}");
    }

    #[test]
    fn annotate_exit_code_fallbacks() {
        let hint = annotate_failure("sleep 999", 124, "").unwrap();
        assert!(hint.contains("timeout"), "got: {hint}");
        let hint = annotate_failure("./big", 137, "").unwrap();
        assert!(hint.contains("SIGKILL"), "got: {hint}");
        let hint = annotate_failure("./script.sh", 126, "").unwrap();
        assert!(hint.contains("chmod +x"), "got: {hint}");
    }

    #[test]
    fn annotate_first_match_wins_and_bounded() {
        // conflict pattern outranks command-not-found when both present
        let out = "CONFLICT (content): x\nbash: foo: command not found";
        let hint = annotate_failure("git merge y", 1, out).unwrap();
        assert!(hint.contains("merge conflict"), "got: {hint}");

        // pattern buried past the scan window must not fire
        let mut big = "x".repeat(SCAN_CHARS + 10);
        big.push_str("Permission denied");
        assert!(annotate_failure("cmd", 1, &big).is_none());
    }
}

//! Local-environment toolchain probe — port of hermes' `tools/env_probe.py`.
//!
//! When the terminal backend is local (the agent's tools run on the same
//! machine as ulnclaw itself), a single deterministic line about the Python
//! toolchain state is surfaced in the system prompt so models don't have to
//! discover it by hitting walls: mismatched `pip`/`python3` versions, a
//! missing pip module, PEP-668 externally-managed environments, a missing
//! bare `python` alias.
//!
//! The probe is cheap (a handful of subprocess calls), cached for the
//! lifetime of the process, and emits **at most one short line** when
//! something non-default is detected. When the environment looks normal it
//! emits nothing — no token cost. Remote terminal backends (docker, ssh)
//! are skipped: the host's Python state is irrelevant when tools run in a
//! sandbox.
//!
//! Concurrency model (hermes #67964): the probe runs in exactly one
//! background worker thread; callers never execute the probe themselves and
//! never wait unboundedly — they block at most [`PROBE_WAIT_TIMEOUT`] and
//! then fail open with `""`. A stuck probe subprocess can therefore degrade
//! at most the probe line itself, never system-prompt construction.

use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Upper bound a prompt build will wait for the probe. Generous vs the
/// healthy ~0.5s runtime, but finite: prompt construction must proceed.
pub const PROBE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-subprocess timeout inside the probe.
const SUBPROC_TIMEOUT: Duration = Duration::from_secs(3);

/// Facts about the local Python toolchain gathered by the probe.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolchainFacts {
    /// `python3 --version` equivalent (`"3.12.4"`), if present.
    pub py3_ver: Option<String>,
    /// Bare `python` version, if present (alias on some systems).
    pub py_ver: Option<String>,
    /// `python3 -m pip --version` succeeds.
    pub py3_has_pip: bool,
    /// Python version the `pip` executable on PATH is bound to (`"3.12"`).
    pub pip_bound_to: Option<String>,
    /// python3's install is PEP-668 externally managed.
    pub py3_pep668: bool,
    /// `uv` is on PATH.
    pub has_uv: bool,
}

/// Locate a binary on PATH (std-only `shutil.which` stand-in).
pub fn which(binary: &str) -> Option<PathBuf> {
    if binary.contains('/') {
        let path = PathBuf::from(binary);
        return path.is_file().then_some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Run a short subprocess. Returns `(exit_code, stdout, stderr)`; failures
/// (binary missing, timeout, spawn error) return `(-1, "", reason)`.
fn run(cmd: &[&str]) -> (i32, String, String) {
    let mut command = std::process::Command::new(cmd[0]);
    command
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            let reason = if e.kind() == std::io::ErrorKind::NotFound {
                "not found".to_string()
            } else {
                format!("oserror: {e}")
            };
            return (-1, String::new(), reason);
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Outputs are tiny version strings; reading after exit is safe.
                fn read_stream<S: std::io::Read>(mut stream: Option<S>) -> String {
                    let mut buf = String::new();
                    if let Some(ref mut inner) = stream {
                        inner.read_to_string(&mut buf).ok();
                    }
                    buf.trim().to_string()
                }
                let stdout = read_stream(child.stdout.take());
                let stderr = read_stream(child.stderr.take());
                return (status.code().unwrap_or(-1), stdout, stderr);
            }
            Ok(None) => {
                if started.elapsed() > SUBPROC_TIMEOUT {
                    child.kill().ok();
                    child.wait().ok();
                    return (-1, String::new(), "timeout".to_string());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return (-1, String::new(), format!("oserror: {e}")),
        }
    }
}

/// Short version string like `3.12.4` for `binary`, or None.
fn python_version_of(binary: &str) -> Option<String> {
    if which(binary).is_none() {
        return None;
    }
    let (rc, out, _err) = run(&[
        binary,
        "-c",
        "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}')",
    ]);
    (rc == 0 && !out.is_empty()).then_some(out)
}

/// True if `<binary> -m pip --version` succeeds.
fn has_pip_module(binary: &str) -> bool {
    if which(binary).is_none() {
        return false;
    }
    let (rc, _out, _err) = run(&[binary, "-m", "pip", "--version"]);
    rc == 0
}

/// True when `binary`'s install location is PEP-668 externally managed
/// (the `EXTERNALLY-MANAGED` marker Debian/Ubuntu drop next to the stdlib).
fn detect_pep668(binary: &str) -> bool {
    if which(binary).is_none() {
        return false;
    }
    let code = "import sys, os;\
stdlib = os.path.dirname(os.__file__);\
marker = os.path.join(stdlib, 'EXTERNALLY-MANAGED');\
print('yes' if os.path.exists(marker) else 'no')";
    let (rc, out, _err) = run(&[binary, "-c", code]);
    rc == 0 && out.trim() == "yes"
}

/// If `pip` is on PATH, the Python version it is bound to (parses the
/// trailing `(python X.Y)` of `pip --version`).
fn pip_python_version() -> Option<String> {
    if which("pip").is_none() {
        return None;
    }
    let (rc, out, _err) = run(&["pip", "--version"]);
    if rc != 0 || out.is_empty() {
        return None;
    }
    if out.contains("(python ") && out.ends_with(')') {
        let tail = out.rsplit("(python ").next()?;
        return Some(tail[..tail.len() - 1].trim().to_string());
    }
    None
}

/// Probe the local toolchain (subprocess calls; ~0.5s healthy).
pub fn probe_facts() -> ToolchainFacts {
    let py3_ver = python_version_of("python3");
    let py_ver = python_version_of("python");
    let py3_has_pip = py3_ver.is_some() && has_pip_module("python3");
    let pip_bound_to = pip_python_version();
    let py3_pep668 = py3_ver.is_some() && detect_pep668("python3");
    let has_uv = which("uv").is_some();
    ToolchainFacts {
        py3_ver,
        py_ver,
        py3_has_pip,
        pip_bound_to,
        py3_pep668,
        has_uv,
    }
}

/// Assemble the one-liner from facts. Returns `""` when nothing notable is
/// detected — the goal is to save the model from hitting an avoidable wall,
/// not to narrate a healthy environment.
pub fn assemble_line(facts: &ToolchainFacts) -> String {
    let mismatch = facts
        .pip_bound_to
        .as_ref()
        .zip(facts.py3_ver.as_ref())
        .map(|(bound, py3)| !py3.starts_with(bound.as_str()))
        .unwrap_or(false);

    // python3 exists, has pip, no version mismatch, and either no PEP 668
    // or uv available → clean enough to stay silent.
    let silent = facts.py3_ver.is_some()
        && facts.py3_has_pip
        && !mismatch
        && (!facts.py3_pep668 || facts.has_uv);
    if silent {
        return String::new();
    }

    let mut bits: Vec<String> = Vec::new();
    match &facts.py3_ver {
        Some(py3) => {
            let mut bit = format!("python3={py3}");
            if !facts.py3_has_pip {
                bit.push_str(" (no pip module)");
            }
            bits.push(bit);
        }
        None => bits.push("python3=missing".to_string()),
    }

    match (&facts.py_ver, &facts.py3_ver) {
        (Some(py), Some(py3)) if py != py3 => bits.push(format!("python={py}")),
        (None, Some(_)) => {
            // Common on Debian/Ubuntu — call it out so the model doesn't
            // type `python` and hit "command not found".
            bits.push("python=missing (use python3)".to_string());
        }
        _ => {}
    }

    match &facts.pip_bound_to {
        Some(bound) if mismatch => bits.push(format!("pip→python{bound} (mismatch)")),
        Some(bound) if !facts.py3_has_pip => {
            // pip exists but `python3 -m pip` doesn't — the script works
            // but the module path doesn't.
            bits.push(format!("pip→python{bound}"));
        }
        Some(_) => {}
        // `pip` not on PATH and no module path either.
        None if !facts.py3_has_pip => bits.push("pip=missing".to_string()),
        None => {}
    }

    if facts.py3_pep668 {
        bits.push("PEP 668=yes (use venv or uv)".to_string());
    }
    if facts.has_uv {
        bits.push("uv=installed".to_string());
    }
    if bits.is_empty() {
        return String::new();
    }
    format!("Python toolchain: {}.", bits.join(", "))
}

struct ProbeState {
    line: Option<String>, // None = not probed yet
    done: bool,
    running: bool,
    gen: u64,
    wait_timed_out: bool,
    backend: String,
}

fn state() -> &'static Mutex<ProbeState> {
    static STATE: OnceLock<Mutex<ProbeState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(ProbeState {
            line: None,
            done: false,
            running: false,
            gen: 0,
            wait_timed_out: false,
            backend: "local".to_string(),
        })
    })
}

fn condvar() -> &'static Condvar {
    static CV: OnceLock<Condvar> = OnceLock::new();
    CV.get_or_init(Condvar::new)
}

/// Build the probe line for `backend` (remote backends skip the probe).
pub fn build_probe_line(backend: &str) -> String {
    let name = backend.trim().to_ascii_lowercase();
    if name != "local" && !name.is_empty() {
        // Remote terminal backend: the host's Python state isn't where the
        // agent's tools run.
        return String::new();
    }
    assemble_line(&probe_facts())
}

/// Return the cached probe line (building it on first call).
///
/// Returns `""` when the environment is clean — the prompt assembler drops
/// the line in that case. The probe always runs in a single background
/// worker thread; this function waits at most [`PROBE_WAIT_TIMEOUT`] and
/// then fails open. Once one caller has burned the full wait, later callers
/// stop paying it (50ms peek) — if the stuck worker ever finishes, the line
/// resumes appearing in new prompts.
pub fn get_environment_probe_line(backend: &str) -> String {
    get_line(backend, false)
}

/// Test-only: reset the cache and (optionally) force a fresh probe.
pub fn reset_for_tests() {
    let mut guard = state().lock().unwrap();
    guard.line = None;
    guard.done = false;
    guard.running = false;
    guard.gen += 1;
    guard.wait_timed_out = false;
    drop(guard);
    condvar().notify_all();
}

fn get_line(backend: &str, _force_refresh: bool) -> String {
    let cv = condvar();
    let mut guard = state().lock().unwrap();
    if guard.done {
        return guard.line.clone().unwrap_or_default();
    }
    if !guard.running {
        guard.running = true;
        guard.backend = backend.trim().to_ascii_lowercase();
        let gen = guard.gen;
        let probe_backend = guard.backend.clone();
        std::thread::Builder::new()
            .name("env-probe".into())
            .spawn(move || {
                let line = std::panic::catch_unwind(|| build_probe_line(&probe_backend))
                    .unwrap_or_default();
                let mut guard = state().lock().unwrap();
                if gen == guard.gen {
                    guard.line = Some(line);
                    guard.done = true;
                    guard.running = false;
                }
                drop(guard);
                condvar().notify_all();
            })
            .ok();
    }
    let wait = if guard.wait_timed_out {
        Duration::from_millis(50)
    } else {
        PROBE_WAIT_TIMEOUT
    };
    let deadline = Instant::now() + wait;
    while !guard.done {
        let now = Instant::now();
        if now >= deadline {
            if !guard.wait_timed_out {
                guard.wait_timed_out = true;
                tracing::warn!(
                    "env_probe did not finish within {}s; building the system prompt without the Python toolchain line",
                    PROBE_WAIT_TIMEOUT.as_secs()
                );
            }
            return String::new();
        }
        let (next, _timeout) = cv.wait_timeout(guard, deadline - now).unwrap();
        guard = next;
    }
    guard.line.clone().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> ToolchainFacts {
        ToolchainFacts {
            py3_ver: Some("3.12.4".into()),
            py_ver: None,
            py3_has_pip: true,
            pip_bound_to: Some("3.12".into()),
            py3_pep668: false,
            has_uv: false,
        }
    }

    #[test]
    fn clean_environment_is_silent() {
        assert_eq!(assemble_line(&facts()), "");
    }

    #[test]
    fn missing_python_is_reported() {
        let mut f = facts();
        f.py3_ver = None;
        f.py3_has_pip = false;
        f.pip_bound_to = None;
        let line = assemble_line(&f);
        assert!(line.contains("python3=missing"), "got: {line}");
        assert!(line.contains("pip=missing"), "got: {line}");
    }

    #[test]
    fn bare_python_missing_is_called_out() {
        let mut f = facts();
        f.py_ver = None;
        let line = assemble_line(&f);
        // silent overall only when pip etc. fine; here pip matches so silent
        assert_eq!(line, "");
        // force non-silent via pep668 without uv
        f.py3_pep668 = true;
        let line = assemble_line(&f);
        assert!(line.contains("python=missing (use python3)"), "got: {line}");
        assert!(line.contains("PEP 668=yes"), "got: {line}");
    }

    #[test]
    fn pep668_with_uv_is_silent() {
        // PEP 668 alone would warrant a warning, but uv neutralizes it.
        let mut f = facts();
        f.py3_pep668 = true;
        f.has_uv = true;
        assert_eq!(assemble_line(&f), "");
    }

    #[test]
    fn uv_bit_appears_when_not_silent() {
        let mut f = facts();
        f.has_uv = true;
        f.pip_bound_to = Some("3.11".into()); // mismatch breaks silence
        let line = assemble_line(&f);
        assert!(line.contains("uv=installed"), "got: {line}");
        assert!(line.contains("mismatch"), "got: {line}");
    }

    #[test]
    fn pip_version_mismatch_reported() {
        let mut f = facts();
        f.pip_bound_to = Some("3.11".into());
        let line = assemble_line(&f);
        assert!(line.contains("pip→python3.11 (mismatch)"), "got: {line}");
    }

    #[test]
    fn no_pip_module_reported() {
        let mut f = facts();
        f.py3_has_pip = false;
        f.pip_bound_to = None;
        let line = assemble_line(&f);
        assert!(line.contains("(no pip module)"), "got: {line}");
        assert!(line.contains("pip=missing"), "got: {line}");
    }

    #[test]
    fn pip_script_without_module_reported() {
        let mut f = facts();
        f.py3_has_pip = false;
        f.pip_bound_to = Some("3.12".into());
        let line = assemble_line(&f);
        assert!(line.contains("pip→python3.12"), "got: {line}");
        assert!(!line.contains("mismatch"), "got: {line}");
    }

    #[test]
    fn remote_backend_skips_probe() {
        assert_eq!(build_probe_line("docker"), "");
        assert_eq!(build_probe_line("ssh"), "");
        assert_eq!(build_probe_line("  Docker "), "");
    }

    #[test]
    fn local_probe_runs_and_caches() {
        reset_for_tests();
        let first = get_environment_probe_line("local");
        let second = get_environment_probe_line("local");
        assert_eq!(first, second);
        // Line is either empty (clean env) or starts with the marker.
        assert!(
            first.is_empty() || first.starts_with("Python toolchain: "),
            "got: {first}"
        );
    }

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("ulnclaw-no-such-binary-xyz").is_none());
    }

    #[test]
    fn probe_facts_on_this_machine() {
        let f = probe_facts();
        // CI/dev machines in this project have python3; keep the assertion
        // soft so the test also passes on minimal images.
        if f.py3_ver.is_some() {
            assert!(f.py3_ver.as_deref().unwrap().contains('.'));
        }
    }
}

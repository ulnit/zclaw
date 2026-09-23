//! `ulnclaw gui` — desktop GUI launcher (lean aligned with hermes `cmd_gui` /
//! `hermes desktop`).
//!
//! The desktop is the hermes-style Electron shell in `desktop-electron/`
//! ("ulnclaw desktop"), backed by the ulnclaw gateway. This command resolves
//! a packaged executable (`release/<platform>-unpacked/…`) and spawns it
//! detached; `--dev` runs the unpackaged app via `npm start` instead.
//! Resolution order: explicit `--binary`, `ULNCLAW_DESKTOP_BINARY`, then the
//! compile-time repo root's electron-builder output.

use std::path::{Path, PathBuf};

/// Compile-time repository root (`CARGO_MANIFEST_DIR` of this crate).
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Candidate packaged desktop executable paths in priority order.
pub fn binary_candidates(root: &Path) -> Vec<PathBuf> {
    let release = root.join("desktop-electron").join("release");
    vec![
        release.join("linux-unpacked").join("ulnclaw-desktop"),
        release.join("win-unpacked").join("ulnclaw-desktop.exe"),
        release
            .join("mac-unpacked")
            .join("ulnclaw desktop.app")
            .join("Contents")
            .join("MacOS")
            .join("ulnclaw desktop"),
        release
            .join("mac")
            .join("ulnclaw desktop.app")
            .join("Contents")
            .join("MacOS")
            .join("ulnclaw desktop"),
    ]
}

/// Resolve the desktop executable: explicit override → env var → candidates.
pub fn resolve_binary(explicit: Option<&Path>, root: &Path) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        return Err(format!("desktop binary not found: {}", path.display()));
    }
    if let Some(env_path) = std::env::var_os("ULNCLAW_DESKTOP_BINARY") {
        let path = PathBuf::from(env_path);
        if path.exists() {
            return Ok(path);
        }
        return Err(format!(
            "ULNCLAW_DESKTOP_BINARY points to a missing file: {}",
            path.display()
        ));
    }
    for candidate in binary_candidates(root) {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "No built desktop GUI found. Build it first:\n{}\n\
         or point --binary / ULNCLAW_DESKTOP_BINARY at an existing binary.",
        build_instructions()
    ))
}

/// Build instructions shown when no desktop executable exists.
pub fn build_instructions() -> String {
    "  cd desktop-electron\n  npm install\n  npm run dist".to_string()
}

/// Spawn the desktop GUI detached; returns the child PID.
pub fn launch(binary: &Path) -> Result<u32, String> {
    let child = std::process::Command::new(binary)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to launch {}: {e}", binary.display()))?;
    Ok(child.id())
}

/// Spawn the unpackaged Electron app (`npm start`) in desktop-electron.
pub fn launch_dev(root: &Path) -> Result<u32, String> {
    let desktop = root.join("desktop-electron");
    if !desktop.join("package.json").exists() {
        return Err(format!(
            "desktop-electron/ source not found at {}",
            desktop.display()
        ));
    }
    let child = std::process::Command::new("npm")
        .arg("start")
        .current_dir(&desktop)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start desktop dev server: {e}"))?;
    Ok(child.id())
}

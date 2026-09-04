//! Clipboard image extraction + text write — port of hermes
//! `hermes_cli/clipboard.py`.
//!
//! `save_clipboard_image(dest)` checks the system clipboard for image
//! data, saves it to `dest` as PNG, and returns true on success. No
//! external Rust dependencies — only OS-level CLI tools that ship with
//! the platform (or are commonly installed):
//!
//! - macOS   — pngpaste (if installed), osascript (always available)
//! - Windows — PowerShell WinForms, Get-Clipboard, file-drop fallback
//! - WSL2    — powershell.exe (same scripts as Windows)
//! - Linux   — wl-paste (Wayland), xclip (X11)
//!
//! Text write (`write_clipboard_text`) mirrors the TUI fallback order:
//! pbcopy → PowerShell Set-Clipboard → wl-copy → xclip → xsel. Callers
//! should fall back to OSC 52 when it returns false; over SSH sessions
//! (`is_remote_shell_session`) OSC 52 is almost always the right answer.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Image extensions accepted by the Windows FileDropList fallback.
const FILEDROP_IMAGE_EXTS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".tif",
];

// ---------------------------------------------------------------------------
// Subprocess helper with a hard timeout (hermes subprocess.run(timeout=...))
// ---------------------------------------------------------------------------

struct ProcOutput {
    status_ok: bool,
    stdout: Vec<u8>,
}

/// Run `cmd` with optional stdin bytes, killing it after `timeout`.
/// Returns `None` when the binary is missing or the run fails to start.
fn run_with_timeout(
    program: &str,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
    timeout: Duration,
) -> Option<ProcOutput> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::null());
    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut child = cmd.spawn().ok()?;
    if let Some(data) = stdin_bytes {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(data);
        }
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_end(&mut stdout);
                }
                return Some(ProcOutput {
                    status_ok: status.success(),
                    stdout,
                });
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

fn stdout_string(output: &ProcOutput) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// True when running inside a WSL session (hermes_constants.is_wsl).
pub fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let lower = v.to_ascii_lowercase();
            lower.contains("microsoft") || lower.contains("wsl")
        })
        .unwrap_or(false)
}

/// True when running inside an SSH session (hermes is_remote_shell_session).
/// Over SSH, native clipboard tools write the REMOTE machine's clipboard;
/// OSC 52 reaches the LOCAL terminal emulator instead.
pub fn is_remote_shell_session() -> bool {
    ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"]
        .iter()
        .any(|name| {
            crate::config::get_env_value(name)
                .map(|v| !v.is_empty())
                .unwrap_or(false)
        })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract an image from the system clipboard and save it as PNG.
/// Returns true if an image was found and saved.
pub fn save_clipboard_image(dest: &Path) -> bool {
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if cfg!(target_os = "macos") {
        return macos_save(dest);
    }
    if cfg!(target_os = "windows") {
        return windows_save(dest);
    }
    linux_save(dest)
}

/// Quick check: does the clipboard currently contain an image? Lighter
/// than `save_clipboard_image` — doesn't extract or write anything.
pub fn has_clipboard_image() -> bool {
    if cfg!(target_os = "macos") {
        return macos_has_image();
    }
    if cfg!(target_os = "windows") {
        return windows_has_image();
    }
    // Match linux_save fallthrough order: WSL → Wayland → X11.
    if is_wsl() && wsl_has_image() {
        return true;
    }
    if wayland_display().is_some() && wayland_has_image() {
        return true;
    }
    xclip_has_image()
}

/// Write `text` to the system clipboard via native platform tools.
/// Fallback order matches the hermes TUI: macOS pbcopy → Windows/WSL
/// PowerShell Set-Clipboard → wl-copy → xclip → xsel. Returns true if
/// any backend succeeded; callers should fall back to OSC 52 on false.
pub fn write_clipboard_text(text: &str) -> bool {
    for (argv, use_stdin) in write_clipboard_commands() {
        let result = if use_stdin {
            run_with_timeout(
                &argv[0],
                &argv[1..].iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                Some(text.as_bytes()),
                Duration::from_secs(10),
            )
        } else {
            let b64 = base64_encode(text.as_bytes());
            let script = powershell_write_script(&b64);
            let mut full: Vec<String> = argv.clone();
            full.push("-Command".to_string());
            full.push(script);
            run_with_timeout(
                &full[0],
                &full[1..].iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                None,
                Duration::from_secs(10),
            )
        };
        if result.map(|r| r.status_ok).unwrap_or(false) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Text-write backends
// ---------------------------------------------------------------------------

fn write_clipboard_commands() -> Vec<(Vec<String>, bool)> {
    if cfg!(target_os = "macos") {
        return vec![(vec!["pbcopy".into()], true)];
    }
    if cfg!(target_os = "windows") {
        return vec![(
            vec![
                "powershell".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
            ],
            false,
        )];
    }
    let mut attempts: Vec<(Vec<String>, bool)> = Vec::new();
    if is_wsl() {
        attempts.push((
            vec![
                "powershell.exe".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
            ],
            false,
        ));
    }
    if wayland_display().is_some() {
        attempts.push((
            vec!["wl-copy".into(), "--type".into(), "text/plain".into()],
            true,
        ));
    }
    attempts.push((
        vec![
            "xclip".into(),
            "-selection".into(),
            "clipboard".into(),
            "-in".into(),
        ],
        true,
    ));
    attempts.push((
        vec!["xsel".into(), "--clipboard".into(), "--input".into()],
        true,
    ));
    attempts
}

/// PowerShell decodes piped stdin with the system ANSI code page (e.g.
/// CP936), not UTF-8, so stdin-based writes mangle CJK/emoji. Base64 the
/// UTF-8 bytes and decode inside PowerShell instead (hermes TUI approach).
fn powershell_write_script(b64: &str) -> String {
    format!(
        "Set-Clipboard -Value ([System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{b64}')))"
    )
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn base64_decode(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()
}

fn wayland_display() -> Option<String> {
    crate::config::get_env_value("WAYLAND_DISPLAY").filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

/// pngpaste first (fast, handles more formats), osascript fallback.
fn macos_save(dest: &Path) -> bool {
    macos_pngpaste(dest) || macos_osascript(dest)
}

fn macos_has_image() -> bool {
    let output = run_with_timeout(
        "osascript",
        &["-e", "clipboard info"],
        None,
        Duration::from_secs(3),
    );
    match output {
        Some(out) if out.status_ok => {
            let text = stdout_string(&out);
            text.contains("«class PNGf»") || text.contains("«class TIFF»")
        }
        _ => false,
    }
}

fn macos_pngpaste(dest: &Path) -> bool {
    let Some(dest_str) = dest.to_str() else {
        return false;
    };
    let output = run_with_timeout("pngpaste", &[dest_str], None, Duration::from_secs(3));
    if let Some(out) = output {
        if out.status_ok && file_nonempty(dest) {
            return true;
        }
    }
    false
}

fn macos_osascript(dest: &Path) -> bool {
    if !macos_has_image() {
        return false;
    }
    let Some(dest_str) = dest.to_str() else {
        return false;
    };
    let script = format!(
        "try\n  set imgData to the clipboard as «class PNGf»\n  set f to open for access POSIX file \"{dest_str}\" with write permission\n  write imgData to f\n  close access f\non error\n  return \"fail\"\nend try\n"
    );
    let output = run_with_timeout("osascript", &["-e", &script], None, Duration::from_secs(5));
    if let Some(out) = output {
        if out.status_ok && !stdout_string(&out).contains("fail") && file_nonempty(dest) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Shared PowerShell scripts (native Windows + WSL2)
// ---------------------------------------------------------------------------

/// .NET System.Windows.Forms.Clipboard — used by both native Windows
/// (powershell) and WSL2 (powershell.exe) paths.
const PS_CHECK_IMAGE: &str =
    "Add-Type -AssemblyName System.Windows.Forms;[System.Windows.Forms.Clipboard]::ContainsImage()";

const PS_EXTRACT_IMAGE: &str = "Add-Type -AssemblyName System.Windows.Forms;Add-Type -AssemblyName System.Drawing;$img = [System.Windows.Forms.Clipboard]::GetImage();if ($null -eq $img) { exit 1 }$ms = New-Object System.IO.MemoryStream;$img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png);[System.Convert]::ToBase64String($ms.ToArray())";

const PS_CHECK_IMAGE_GET_CLIPBOARD: &str = "try { $img = Get-Clipboard -Format Image -ErrorAction Stop;if ($null -ne $img) { 'True' } else { 'False' }} catch { 'False' }";

const PS_EXTRACT_IMAGE_GET_CLIPBOARD: &str = "try { Add-Type -AssemblyName System.Drawing;Add-Type -AssemblyName PresentationCore;Add-Type -AssemblyName WindowsBase;$img = Get-Clipboard -Format Image -ErrorAction Stop;if ($null -eq $img) { exit 1 }$ms = New-Object System.IO.MemoryStream;if ($img -is [System.Drawing.Image]) {$img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)} elseif ($img -is [System.Windows.Media.Imaging.BitmapSource]) {$enc = New-Object System.Windows.Media.Imaging.PngBitmapEncoder;$enc.Frames.Add([System.Windows.Media.Imaging.BitmapFrame]::Create($img));$enc.Save($ms)} else { exit 2 }[System.Convert]::ToBase64String($ms.ToArray())} catch { exit 1 }";

fn ps_check_filedrop_image() -> String {
    let exts = FILEDROP_IMAGE_EXTS
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "try {{ $files = Get-Clipboard -Format FileDropList -ErrorAction Stop;$exts = @({exts});$hit = $files | Where-Object {{ $exts -contains ([System.IO.Path]::GetExtension($_).ToLowerInvariant()) }} | Select-Object -First 1;if ($null -ne $hit) {{ 'True' }} else {{ 'False' }} }} catch {{ 'False' }}"
    )
}

fn ps_extract_filedrop_image() -> String {
    let exts = FILEDROP_IMAGE_EXTS
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "try {{ $files = Get-Clipboard -Format FileDropList -ErrorAction Stop;$exts = @({exts});$hit = $files | Where-Object {{ $exts -contains ([System.IO.Path]::GetExtension($_).ToLowerInvariant()) }} | Select-Object -First 1;if ($null -eq $hit) {{ exit 1 }}[System.Convert]::ToBase64String([System.IO.File]::ReadAllBytes($hit)) }} catch {{ exit 1 }}"
    )
}

fn powershell_has_image_scripts() -> Vec<String> {
    vec![
        PS_CHECK_IMAGE.to_string(),
        PS_CHECK_IMAGE_GET_CLIPBOARD.to_string(),
        ps_check_filedrop_image(),
    ]
}

fn powershell_extract_image_scripts() -> Vec<String> {
    vec![
        PS_EXTRACT_IMAGE.to_string(),
        PS_EXTRACT_IMAGE_GET_CLIPBOARD.to_string(),
        ps_extract_filedrop_image(),
    ]
}

fn run_powershell(exe: &str, script: &str, timeout: Duration) -> Option<ProcOutput> {
    run_with_timeout(
        exe,
        &["-NoProfile", "-NonInteractive", "-Command", script],
        None,
        timeout,
    )
}

fn write_base64_image(dest: &Path, b64_data: &str) -> bool {
    let Some(bytes) = base64_decode(b64_data) else {
        return false;
    };
    if bytes.is_empty() {
        return false;
    }
    std::fs::write(dest, bytes).is_ok() && file_nonempty(dest)
}

fn powershell_has_image(exe: &str, timeout: Duration) -> bool {
    for script in powershell_has_image_scripts() {
        match run_powershell(exe, &script, timeout) {
            Some(out) => {
                if out.status_ok && stdout_string(&out).contains("True") {
                    return true;
                }
            }
            // Executable missing — clipboard unavailable via this exe.
            None => return false,
        }
    }
    false
}

fn powershell_save_image(exe: &str, dest: &Path, timeout: Duration) -> bool {
    for script in powershell_extract_image_scripts() {
        match run_powershell(exe, &script, timeout) {
            Some(out) => {
                if !out.status_ok {
                    continue;
                }
                let b64_data = stdout_string(&out);
                let b64_data = b64_data.trim();
                if b64_data.is_empty() {
                    continue;
                }
                if write_base64_image(dest, b64_data) {
                    return true;
                }
            }
            None => return false,
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Native Windows
// ---------------------------------------------------------------------------

use std::sync::OnceLock;
static PS_EXE: OnceLock<Option<String>> = OnceLock::new();

/// First available PowerShell executable (cached per process).
fn get_ps_exe() -> Option<&'static str> {
    PS_EXE
        .get_or_init(|| {
            for name in ["powershell", "pwsh"] {
                let probe = run_with_timeout(
                    name,
                    &["-NoProfile", "-NonInteractive", "-Command", "echo ok"],
                    None,
                    Duration::from_secs(5),
                );
                if let Some(out) = probe {
                    if out.status_ok && stdout_string(&out).contains("ok") {
                        return Some(name.to_string());
                    }
                }
            }
            None
        })
        .as_deref()
}

fn windows_has_image() -> bool {
    let Some(ps) = get_ps_exe() else {
        return false;
    };
    powershell_has_image(ps, Duration::from_secs(5))
}

fn windows_save(dest: &Path) -> bool {
    let Some(ps) = get_ps_exe() else {
        return false;
    };
    powershell_save_image(ps, dest, Duration::from_secs(15))
}

// ---------------------------------------------------------------------------
// WSL2 (powershell.exe)
// ---------------------------------------------------------------------------

fn wsl_has_image() -> bool {
    powershell_has_image("powershell.exe", Duration::from_secs(8))
}

fn wsl_save(dest: &Path) -> bool {
    powershell_save_image("powershell.exe", dest, Duration::from_secs(15))
}

// ---------------------------------------------------------------------------
// Linux dispatch: WSL → Wayland → X11
// ---------------------------------------------------------------------------

fn linux_save(dest: &Path) -> bool {
    if is_wsl() && wsl_save(dest) {
        return true;
    }
    // Fall through — WSLg might have wl-paste or xclip working.
    if wayland_display().is_some() && wayland_save(dest) {
        return true;
    }
    xclip_save(dest)
}

// ---------------------------------------------------------------------------
// Wayland (wl-paste)
// ---------------------------------------------------------------------------

fn wl_paste_types() -> Option<Vec<String>> {
    let output = run_with_timeout("wl-paste", &["--list-types"], None, Duration::from_secs(3))?;
    if !output.status_ok {
        return None;
    }
    Some(
        stdout_string(&output)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

fn wayland_has_image() -> bool {
    wl_paste_types()
        .map(|types| types.iter().any(|t| t.starts_with("image/")))
        .unwrap_or(false)
}

fn wayland_save(dest: &Path) -> bool {
    let Some(types) = wl_paste_types() else {
        return false;
    };
    // Prefer PNG, fall back to other image formats (hermes order).
    let mime: Option<&str> = [
        "image/png",
        "image/jpeg",
        "image/bmp",
        "image/gif",
        "image/webp",
    ]
    .into_iter()
    .find(|preferred| types.iter().any(|t| t == *preferred));
    let Some(mime) = mime else {
        return false;
    };
    let output = run_with_timeout("wl-paste", &["--type", mime], None, Duration::from_secs(5));
    let saved = match output {
        Some(out) if out.status_ok && !out.stdout.is_empty() => {
            std::fs::write(dest, &out.stdout).is_ok() && file_nonempty(dest)
        }
        _ => false,
    };
    if !saved {
        let _ = std::fs::remove_file(dest);
        return false;
    }
    // save_clipboard_image() promises a PNG output path. Wayland can offer
    // JPEG/GIF/WebP/BMP payloads, so normalize non-PNG results.
    if mime != "image/png" && !is_png_file(dest) {
        if !convert_to_png(dest) || !is_png_file(dest) {
            // Conversion unavailable — the file is still usable as-is for
            // most APIs (hermes _convert_to_png final fallback).
            return file_nonempty(dest);
        }
    }
    true
}

/// Convert an image file to PNG in-place via ImageMagick `convert`
/// (hermes tries Pillow first; the Rust port has no in-process decoder).
fn convert_to_png(path: &Path) -> bool {
    let tmp = path.with_extension("imgconvert.tmp");
    if std::fs::rename(path, &tmp).is_err() {
        return file_nonempty(path);
    }
    let tmp_str = tmp.to_string_lossy().to_string();
    let dest_str = format!("png:{}", path.to_string_lossy());
    let output = run_with_timeout(
        "convert",
        &[&tmp_str, &dest_str],
        None,
        Duration::from_secs(5),
    );
    if let Some(out) = output {
        if out.status_ok && file_nonempty(path) {
            let _ = std::fs::remove_file(&tmp);
            return true;
        }
    }
    // Convert failed — restore the original file.
    let _ = std::fs::rename(&tmp, path);
    file_nonempty(path)
}

fn is_png_file(path: &Path) -> bool {
    std::fs::read(path)
        .map(|data| data.starts_with(PNG_SIGNATURE))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// X11 (xclip)
// ---------------------------------------------------------------------------

fn xclip_targets() -> Option<String> {
    let output = run_with_timeout(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        None,
        Duration::from_secs(3),
    )?;
    Some(stdout_string(&output))
}

fn xclip_has_image() -> bool {
    xclip_targets()
        .map(|targets| targets.contains("image/png"))
        .unwrap_or(false)
}

fn xclip_save(dest: &Path) -> bool {
    let Some(targets) = xclip_targets() else {
        return false;
    };
    if !targets.contains("image/png") {
        return false;
    }
    let output = run_with_timeout(
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
        None,
        Duration::from_secs(5),
    );
    match output {
        Some(out) if out.status_ok && !out.stdout.is_empty() => {
            if std::fs::write(dest, &out.stdout).is_ok() && file_nonempty(dest) {
                return true;
            }
            let _ = std::fs::remove_file(dest);
            false
        }
        _ => false,
    }
}

fn file_nonempty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// Default clipboard-image directory under the ulnclaw home
/// (hermes keeps pasted images in the session scratch area).
pub fn clipboard_dir() -> PathBuf {
    crate::config::ulnclaw_home().join("clipboard")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_write_script_embeds_base64() {
        let script = powershell_write_script("aGVsbG8=");
        assert!(script.contains("Set-Clipboard"));
        assert!(script.contains("aGVsbG8="));
        assert!(script.contains("FromBase64String"));
    }

    #[test]
    fn base64_roundtrip() {
        let encoded = base64_encode("hello 世界".as_bytes());
        assert_eq!(base64_decode(&encoded).unwrap(), "hello 世界".as_bytes());
        assert!(base64_decode("!!!not-base64!!!").is_none());
    }

    #[test]
    fn png_signature_detection() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-clip-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let png = dir.join("x.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        assert!(is_png_file(&png));
        std::fs::write(&png, b"JPEG-ish").unwrap();
        assert!(!is_png_file(&png));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_commands_follow_platform_order() {
        let commands = write_clipboard_commands();
        assert!(!commands.is_empty());
        if cfg!(target_os = "macos") {
            assert_eq!(commands[0].0[0], "pbcopy");
        } else if cfg!(target_os = "windows") {
            assert_eq!(commands[0].0[0], "powershell");
        }
    }

    #[test]
    fn filedrop_scripts_list_image_extensions() {
        let check = ps_check_filedrop_image();
        let extract = ps_extract_filedrop_image();
        for script in [&check, &extract] {
            assert!(script.contains("'.png'"));
            assert!(script.contains("'.webp'"));
            assert!(script.contains("FileDropList"));
        }
    }

    #[test]
    fn has_image_reports_false_on_headless() {
        // No clipboard tools in CI/headless — every backend must degrade
        // to false instead of panicking.
        assert!(!has_clipboard_image());
    }

    #[test]
    fn save_clipboard_image_degrades_to_false_headless() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-clip-save-{}", std::process::id()));
        let dest = dir.join("clip.png");
        assert!(!save_clipboard_image(&dest));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

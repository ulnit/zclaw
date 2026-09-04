//! Backup & restore — port of `hermes_cli/backup.py`.
//!
//! Subsystems:
//! - Full zip backup of `<home>` with hermes exclusion rules and WAL-safe
//!   SQLite snapshots (`create_backup` / `format_backup_summary`)
//! - Import/restore from a backup zip (`validate_backup_zip`,
//!   `import_backup`) with runtime-state skip list + secret-file chmod
//! - Quick state snapshots (`create_quick_snapshot`, `list_quick_snapshots`,
//!   `restore_quick_snapshot`, `prune_quick_snapshots`)
//! - Cron safety net (`restore_cron_jobs_if_emptied`)
//!
//! Hermes machinery with no ulnclaw counterpart (external memory-provider
//! paths under `_external/`) is out of scope.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::DatabaseName;

/// Directory names skipped entirely during backup walks (hermes
/// `_EXCLUDED_DIRS`, adapted: ulnclaw has no codebase dir inside home, but
/// adds `target`/`sandboxes`/`state-snapshots`).
pub const EXCLUDED_DIRS: &[&str] = &[
    "__pycache__",
    ".git",
    "node_modules",
    "backups",
    "checkpoints",
    "state-snapshots",
    "target",
    "sandboxes",
    ".venv",
    "venv",
    "site-packages",
    ".cache",
    ".tox",
    ".nox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];

/// File suffixes skipped (hermes `_EXCLUDED_SUFFIXES`): bytecode plus SQLite
/// sidecars — the backup ships a consistent `sqlite backup()` snapshot of
/// each `.db`, and pairing it with a live WAL/journal would tear restores.
pub const EXCLUDED_SUFFIXES: &[&str] = &[".pyc", ".pyo", ".db-wal", ".db-shm", ".db-journal"];

/// Runtime-state file names skipped at backup time (hermes `_EXCLUDED_NAMES`).
pub const EXCLUDED_NAMES: &[&str] = &["gateway.pid", "cron.pid"];

/// Files `import_backup` must never overwrite (hermes `_IMPORT_SKIP_NAMES`):
/// volatile, machine-namespaced runtime state.
pub const IMPORT_SKIP_NAMES: &[&str] = &[
    "gateway_state.json",
    "gateway.pid",
    "cron.pid",
    "gateway.lock",
    "processes.json",
];

/// Files tightened to 0600 after import (hermes `_SECRET_FILE_NAMES`).
pub const SECRET_FILE_NAMES: &[&str] = &[".env", "auth.json", "state.db"];

/// Critical state files captured by quick snapshots (hermes
/// `_QUICK_STATE_FILES`, adapted to the ulnclaw layout).
pub const QUICK_STATE_FILES: &[&str] = &[
    "state.db",
    "projects.db",
    "config.toml",
    ".env",
    "auth.json",
    "cron/suggestions.json",
    "skills/.usage.json",
    "memory",
];

pub const QUICK_SNAPSHOTS_DIR: &str = "state-snapshots";
pub const QUICK_DEFAULT_KEEP: usize = 20;

const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

/// Human-readable file size (hermes `_format_size`).
pub fn format_size(mut bytes: f64) -> String {
    for unit in ["B", "KB", "MB", "GB"] {
        if bytes < 1024.0 {
            return if unit == "B" {
                format!("{} B", bytes as u64)
            } else {
                format!("{bytes:.1} {unit}")
            };
        }
        bytes /= 1024.0;
    }
    format!("{bytes:.1} TB")
}

/// True when the file looks like the hermes #68474 zeroed-state.db
/// signature: size > 0, leading bytes all NUL (no SQLite header).
pub fn is_zeroed_sqlite_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if meta.len() == 0 {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut head = vec![0u8; 100.min(meta.len() as usize)];
    if file.read_exact(&mut head).is_err() {
        return false;
    }
    if head.starts_with(SQLITE_HEADER) {
        return false;
    }
    head.iter().all(|b| *b == 0)
}

/// Verify a SQLite file: header magic, then `PRAGMA integrity_check`
/// (hermes `verify_sqlite_integrity`; the 2 GiB skip ceiling is not applied —
/// ulnclaw callers only verify fresh backup copies).
pub fn verify_sqlite_integrity(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0u8; 16];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    if header != SQLITE_HEADER {
        return false;
    }
    drop(file);
    match rusqlite::Connection::open(path) {
        Ok(conn) => conn
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .map(|result| result == "ok")
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Copy a SQLite database safely via the backup() API (hermes
/// `_safe_copy_db`): consistent even while the DB is being written (WAL),
/// fail-closed when no consistent snapshot is possible.
pub fn safe_copy_db(src: &Path, dst: &Path) -> bool {
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let result = (|| -> rusqlite::Result<()> {
        let conn =
            rusqlite::Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.backup(DatabaseName::Main, dst, None)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(dst);
        return false;
    }
    true
}

/// Safe copy + integrity verification (hermes `copy_db_and_verify`).
pub fn copy_db_and_verify(src: &Path, dst: &Path) -> bool {
    if !safe_copy_db(src, dst) {
        return false;
    }
    verify_sqlite_integrity(dst)
}

fn should_exclude_dir(name: &str) -> bool {
    EXCLUDED_DIRS.contains(&name)
}

fn should_skip_backup_file(abs_path: &Path, out_path: &Path) -> bool {
    // Never include the output archive in itself.
    if let (Ok(abs_canon), Ok(out_canon)) = (abs_path.canonicalize(), out_path.canonicalize()) {
        if abs_canon == out_canon {
            return true;
        }
    }
    let name = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if EXCLUDED_NAMES.contains(&name.as_str()) {
        return true;
    }
    if EXCLUDED_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return true;
    }
    false
}

/// Result of a full backup run.
#[derive(Debug, Clone, Default)]
pub struct BackupSummary {
    pub out_path: PathBuf,
    pub file_count: usize,
    pub total_bytes: u64,
    pub zip_bytes: u64,
    pub elapsed_secs: f64,
    pub skipped_dirs: Vec<String>,
    pub errors: Vec<String>,
}

/// Create a zip backup of the ulnclaw home directory (hermes `run_backup`).
/// `output` may name a file or a directory (the zip is placed inside it);
/// default is `<user home>/ulnclaw-backup-<timestamp>.zip`.
pub fn create_backup(home: &Path, output: Option<&Path>) -> Result<BackupSummary, String> {
    if !home.is_dir() {
        return Err(format!(
            "Error: ulnclaw home directory not found at {}",
            home.display()
        ));
    }

    let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S");
    let mut out_path: PathBuf = match output {
        Some(p) => {
            let expanded = p.to_path_buf();
            if expanded.is_dir() {
                expanded.join(format!("ulnclaw-backup-{stamp}.zip"))
            } else {
                expanded
            }
        }
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(format!("ulnclaw-backup-{stamp}.zip")),
    };
    if out_path.extension().map(|e| e != "zip").unwrap_or(true) {
        let mut name = out_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        name.push_str(".zip");
        out_path.set_file_name(name);
    }
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    // Collect files (iterative walk with in-place dir pruning, hermes parity).
    let mut files_to_add: Vec<(PathBuf, PathBuf)> = Vec::new(); // (abs, rel)
    let mut skipped_dirs: Vec<String> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rel_dir = dir
            .strip_prefix(home)
            .unwrap_or(Path::new(""))
            .to_path_buf();
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut entries = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                return Err(format!("cannot read {}: {e}", dir.display()));
            }
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries.by_ref().flatten() {
            let path = entry.path();
            let file_type = entry.file_type();
            let Ok(file_type) = file_type else { continue };
            if file_type.is_dir() {
                subdirs.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
        }
        subdirs.sort();
        for sub in subdirs {
            let name = sub
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if should_exclude_dir(&name) {
                let rel = if rel_dir.as_os_str().is_empty() {
                    name.clone()
                } else {
                    rel_dir.join(&name).to_string_lossy().into_owned()
                };
                skipped_dirs.push(rel);
                continue;
            }
            stack.push(sub);
        }
        for fpath in files {
            let rel = fpath
                .strip_prefix(home)
                .unwrap_or(Path::new(""))
                .to_path_buf();
            if should_skip_backup_file(&fpath, &out_path) {
                continue;
            }
            files_to_add.push((fpath, rel));
        }
    }

    if files_to_add.is_empty() {
        return Err("No files to back up.".to_string());
    }

    let file_count = files_to_add.len();
    let mut total_bytes: u64 = 0;
    let mut errors: Vec<String> = Vec::new();
    let started = std::time::Instant::now();

    let out_file = File::create(&out_path)
        .map_err(|e| format!("cannot create {}: {e}", out_path.display()))?;
    let mut zip = zip::ZipWriter::new(out_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6));

    for (abs_path, rel_path) in &files_to_add {
        let arcname = rel_path.to_string_lossy().replace('\\', "/");
        let result = if abs_path.extension().map(|e| e == "db").unwrap_or(false) {
            // Stage the WAL-safe snapshot next to the output zip (same
            // filesystem, hermes parity — /tmp may be a small tmpfs).
            let tmp_db = out_path.with_extension("tmp.db");
            if safe_copy_db(abs_path, &tmp_db) {
                let size = fs::metadata(&tmp_db).map(|m| m.len()).unwrap_or(0);
                let write_result = (|| -> std::io::Result<()> {
                    zip.start_file(arcname, options)?;
                    let mut f = File::open(&tmp_db)?;
                    std::io::copy(&mut f, &mut zip)?;
                    Ok(())
                })();
                let _ = fs::remove_file(&tmp_db);
                write_result.map(|_| size)
            } else {
                let _ = fs::remove_file(&tmp_db);
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "SQLite safe copy failed",
                ))
            }
        } else {
            let size = fs::metadata(abs_path).map(|m| m.len()).unwrap_or(0);
            (|| -> std::io::Result<u64> {
                zip.start_file(arcname, options)?;
                let mut f = File::open(abs_path)?;
                std::io::copy(&mut f, &mut zip)?;
                Ok(size)
            })()
        };
        match result {
            Ok(size) => total_bytes += size,
            Err(e) => errors.push(format!("  {}: {e}", rel_path.display())),
        }
    }

    zip.finish()
        .map_err(|e| format!("failed to finalize zip: {e}"))?;
    let zip_bytes = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);

    skipped_dirs.sort();
    skipped_dirs.dedup();
    Ok(BackupSummary {
        out_path,
        file_count,
        total_bytes,
        zip_bytes,
        elapsed_secs: started.elapsed().as_secs_f64(),
        skipped_dirs,
        errors,
    })
}

/// Render the backup summary (hermes run_backup tail).
pub fn format_backup_summary(summary: &BackupSummary) -> String {
    let mut out = String::new();
    out.push('\n');
    if summary.errors.is_empty() {
        out.push_str(&format!(
            "Backup complete: {}\n",
            summary.out_path.display()
        ));
    } else {
        out.push_str(&format!(
            "Backup incomplete: {}\n",
            summary.out_path.display()
        ));
    }
    out.push_str(&format!("  Files:       {}\n", summary.file_count));
    out.push_str(&format!(
        "  Original:    {}\n",
        format_size(summary.total_bytes as f64)
    ));
    out.push_str(&format!(
        "  Compressed:  {}\n",
        format_size(summary.zip_bytes as f64)
    ));
    out.push_str(&format!("  Time:        {:.1}s\n", summary.elapsed_secs));
    if !summary.skipped_dirs.is_empty() {
        out.push_str("\n  Excluded directories:\n");
        for dir in &summary.skipped_dirs {
            out.push_str(&format!("    {dir}/\n"));
        }
    }
    if !summary.errors.is_empty() {
        out.push_str(&format!(
            "\n  Warnings ({} files skipped):\n",
            summary.errors.len()
        ));
        for err in summary.errors.iter().take(10) {
            out.push_str(err);
            out.push('\n');
        }
        if summary.errors.len() > 10 {
            out.push_str(&format!("  ... and {} more\n", summary.errors.len() - 10));
        }
    }
    if summary.errors.is_empty() {
        let name = summary
            .out_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push_str(&format!("\nRestore with: ulnclaw import {name}\n"));
    }
    out
}

// =============================================================================
// Import / restore
// =============================================================================

/// Check that a zip looks like an ulnclaw/hermes backup (hermes
/// `_validate_backup_zip`). Returns `(ok, reason)`.
pub fn validate_backup_zip(zip_path: &Path) -> (bool, String) {
    let file = match File::open(zip_path) {
        Ok(f) => f,
        Err(e) => return (false, format!("cannot open zip: {e}")),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(e) => return (false, format!("invalid zip archive: {e}")),
    };
    if archive.len() == 0 {
        return (false, "zip archive is empty".to_string());
    }
    let markers = ["config.yaml", "config.toml", ".env", "state.db"];
    let mut found = false;
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else {
            continue;
        };
        let basename = Path::new(entry.name())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if markers.contains(&basename.as_str()) {
            found = true;
            break;
        }
    }
    if !found {
        return (
            false,
            "zip does not appear to be an ulnclaw backup (no config.toml, .env, or state databases found)"
                .to_string(),
        );
    }
    (true, String::new())
}

/// Detect a common top-level directory wrapping all entries (hermes
/// `_detect_prefix`), extended with ulnclaw dir names.
pub fn detect_prefix(names: &[String]) -> String {
    let file_names: Vec<&String> = names.iter().filter(|n| !n.ends_with('/')).collect();
    if file_names.is_empty() {
        return String::new();
    }
    let first_parts: Vec<&str> = file_names
        .iter()
        .filter_map(|n| {
            let mut parts = n.split('/');
            let first = parts.next()?;
            if parts.next().is_some() {
                Some(first)
            } else {
                None
            }
        })
        .collect();
    if first_parts.is_empty() {
        return String::new();
    }
    let first = first_parts[0];
    if first_parts.iter().all(|p| *p == first)
        && matches!(first, ".hermes" | "hermes" | ".ulnclaw" | "ulnclaw")
    {
        return format!("{first}/");
    }
    String::new()
}

/// Result of an import run.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub restored: usize,
    pub skipped_runtime: usize,
    pub secrets_tightened: usize,
    pub errors: Vec<String>,
}

/// Restore from a backup zip, overlaying onto `home` (hermes `run_import`).
/// Entries are extracted to a staging dir first; runtime-state files are
/// never overwritten; secret files end up 0600.
pub fn import_backup(home: &Path, zip_path: &Path) -> Result<ImportReport, String> {
    let (ok, reason) = validate_backup_zip(zip_path);
    if !ok {
        return Err(reason);
    }

    let file = File::open(zip_path).map_err(|e| format!("cannot open zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("invalid zip: {e}"))?;

    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index_raw(i).ok().map(|e| e.name().to_string()))
        .collect();
    let prefix = detect_prefix(&names);

    let staging = tempfile::tempdir().map_err(|e| format!("cannot stage import: {e}"))?;
    let staging_root = staging.path();

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("zip entry error: {e}"))?;
        let raw_name = entry.name().to_string();
        let stripped = raw_name.strip_prefix(prefix.as_str()).unwrap_or(&raw_name);
        if stripped.is_empty() || stripped.ends_with('/') {
            continue;
        }
        // zip-slip guard: every component must stay inside the staging dir.
        let target = staging_root.join(stripped);
        let canon_root = staging_root
            .canonicalize()
            .unwrap_or_else(|_| staging_root.to_path_buf());
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let canon_target_parent = target
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_default();
        if !canon_target_parent.starts_with(&canon_root) {
            continue;
        }
        if entry.is_dir() {
            let _ = fs::create_dir_all(&target);
            continue;
        }
        let mut out =
            File::create(&target).map_err(|e| format!("cannot stage {}: {e}", target.display()))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("cannot extract {}: {e}", stripped))?;
    }

    // Overlay the staged tree onto home.
    let mut report = ImportReport::default();
    let mut stack: Vec<PathBuf> = vec![staging_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path.strip_prefix(staging_root).unwrap_or(Path::new(""));
            let basename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if IMPORT_SKIP_NAMES.contains(&basename.as_str()) {
                report.skipped_runtime += 1;
                continue;
            }
            let dst = home.join(rel);
            if let Some(parent) = dst.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Err(e) = fs::copy(&path, &dst) {
                report.errors.push(format!("  {}: {e}", rel.display()));
                continue;
            }
            if SECRET_FILE_NAMES.contains(&basename.as_str()) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o600));
                }
                report.secrets_tightened += 1;
            }
            report.restored += 1;
        }
    }
    Ok(report)
}

/// Render the import summary.
pub fn format_import_report(report: &ImportReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("✓ Restored {} file(s).\n", report.restored));
    if report.skipped_runtime > 0 {
        out.push_str(&format!(
            "  Skipped {} volatile runtime-state file(s) (machine-specific).\n",
            report.skipped_runtime
        ));
    }
    if report.secrets_tightened > 0 {
        out.push_str(&format!(
            "  Tightened permissions on {} secret file(s) to 0600.\n",
            report.secrets_tightened
        ));
    }
    if !report.errors.is_empty() {
        out.push_str(&format!(
            "  Warnings ({} files failed):\n",
            report.errors.len()
        ));
        for err in report.errors.iter().take(10) {
            out.push_str(err);
            out.push('\n');
        }
    }
    out
}

// =============================================================================
// Quick snapshots
// =============================================================================

fn quick_snapshot_root(home: &Path) -> PathBuf {
    home.join(QUICK_SNAPSHOTS_DIR)
}

/// Zip a quick snapshot directory into memory for download (hermes
/// `/api/ops/backup/download` parity). The id must be a plain snapshot
/// directory name; anything with path components is rejected.
pub fn export_quick_snapshot_zip(home: &Path, id: &str) -> Result<Vec<u8>, String> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id == "." || id == ".." {
        return Err("invalid snapshot id".to_string());
    }
    let root = quick_snapshot_root(home).join(id);
    if !root.is_dir() {
        return Err(format!("snapshot '{id}' not found"));
    }
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(Path::new(""))
                    .to_path_buf();
                files.push((path, rel));
            }
        }
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));
    if files.is_empty() {
        return Err(format!("snapshot '{id}' is empty"));
    }
    let mut buffer = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(6));
        for (abs, rel) in &files {
            let arcname = rel.to_string_lossy().replace('\\', "/");
            zip.start_file(arcname, options)
                .map_err(|e| format!("zip start_file: {e}"))?;
            let mut file = File::open(abs).map_err(|e| format!("open {}: {e}", abs.display()))?;
            std::io::copy(&mut file, &mut zip).map_err(|e| format!("zip write: {e}"))?;
        }
        zip.finish().map_err(|e| format!("zip finish: {e}"))?;
    }
    Ok(buffer)
}

/// One quick-snapshot listing entry.
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub id: String,
    pub files: usize,
    pub bytes: u64,
}

/// Create a quick state snapshot of critical files (hermes
/// `create_quick_snapshot`). Returns the snapshot id, or None when nothing
/// was found. `max_file_size` skips oversized files (used by the pre-update
/// safety snapshot so a multi-GB state.db cannot stall an update).
pub fn create_quick_snapshot(
    home: &Path,
    label: Option<&str>,
    keep: Option<usize>,
    max_file_size: Option<u64>,
) -> Result<Option<String>, String> {
    let root = quick_snapshot_root(home);
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let snap_id = match label {
        Some(l) if !l.trim().is_empty() => format!("{ts}-{l}"),
        _ => ts.to_string(),
    };
    let snap_dir = root.join(&snap_id);
    fs::create_dir_all(&snap_dir).map_err(|e| format!("cannot create snapshot dir: {e}"))?;

    let mut manifest: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();

    let too_large = |path: &Path, rel_name: &str, warnings: &mut Vec<String>| -> bool {
        let Some(cap) = max_file_size else {
            return false;
        };
        let Ok(size) = fs::metadata(path).map(|m| m.len()) else {
            return false;
        };
        if size <= cap {
            return false;
        }
        warnings.push(format!(
            "  ⚠ Snapshot: skipping {rel_name} ({} exceeds {} limit)",
            format_size(size as f64),
            format_size(cap as f64)
        ));
        true
    };

    for rel in QUICK_STATE_FILES {
        let src = home.join(rel);
        if !src.exists() {
            continue;
        }
        if src.is_dir() {
            let walker = walkdir::WalkDir::new(&src).follow_links(false);
            for entry in walker.into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let sub = entry.path();
                let sub_rel = sub.strip_prefix(home).unwrap_or(Path::new(""));
                let sub_rel_str = sub_rel.to_string_lossy().replace('\\', "/");
                if too_large(sub, &sub_rel_str, &mut warnings) {
                    continue;
                }
                let dst = snap_dir.join(&sub_rel_str);
                if let Some(parent) = dst.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let copied = if sub.extension().map(|e| e == "db").unwrap_or(false) {
                    if !safe_copy_db(sub, &dst) {
                        warnings.push(format!(
                            "  ⚠ Snapshot: SQLite safe copy FAILED for {sub_rel_str} — file may be locked or corrupted"
                        ));
                        if is_zeroed_sqlite_file(sub) {
                            let size = fs::metadata(sub).map(|m| m.len()).unwrap_or(0);
                            warnings.push(format!(
                                "  ⚠ Snapshot: {sub_rel_str} looks ZEROED (no SQLite header; {size} bytes of NULs?)"
                            ));
                        }
                        false
                    } else {
                        true
                    }
                } else {
                    fs::copy(sub, &dst).is_ok()
                };
                if copied {
                    let size = fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
                    manifest.insert(sub_rel_str, size);
                }
            }
            continue;
        }
        let rel_str = rel.to_string();
        if too_large(&src, &rel_str, &mut warnings) {
            continue;
        }
        let dst = snap_dir.join(&rel_str);
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let copied = if src.extension().map(|e| e == "db").unwrap_or(false) {
            if !safe_copy_db(&src, &dst) {
                warnings.push(format!(
                    "  ⚠ Snapshot: SQLite safe copy FAILED for {rel_str} — file may be locked or corrupted"
                ));
                false
            } else {
                true
            }
        } else {
            fs::copy(&src, &dst).is_ok()
        };
        if copied {
            let size = fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
            manifest.insert(rel_str, size);
        }
    }

    if manifest.is_empty() {
        let _ = fs::remove_dir_all(&snap_dir);
        return Ok(None);
    }

    let meta = serde_json::json!({
        "created": chrono::Utc::now().to_rfc3339(),
        "label": label,
        "files": manifest,
    });
    fs::write(
        snap_dir.join("manifest.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| format!("cannot write manifest: {e}"))?;

    let keep = keep.unwrap_or(QUICK_DEFAULT_KEEP);
    prune_quick_snapshots(home, keep);

    for warning in &warnings {
        eprintln!("{warning}");
    }
    Ok(Some(snap_id))
}

/// List quick snapshots, newest first (hermes `list_quick_snapshots`).
pub fn list_quick_snapshots(home: &Path) -> Vec<SnapshotInfo> {
    let root = quick_snapshot_root(home);
    let mut snapshots: Vec<SnapshotInfo> = Vec::new();
    let Ok(entries) = fs::read_dir(&root) else {
        return snapshots;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let manifest_path = path.join("manifest.json");
        let (files, bytes) = match fs::read_to_string(&manifest_path) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) => {
                    let files_map = value.get("files").and_then(|f| f.as_object());
                    match files_map {
                        Some(map) => {
                            let mut total: u64 = 0;
                            for size in map.values() {
                                total += size.as_u64().unwrap_or(0);
                            }
                            (map.len(), total)
                        }
                        None => (0, 0),
                    }
                }
                Err(_) => (0, 0),
            },
            Err(_) => (0, 0),
        };
        snapshots.push(SnapshotInfo { id, files, bytes });
    }
    snapshots.sort_by(|a, b| b.id.cmp(&a.id));
    snapshots
}

/// Restore state from a quick snapshot (hermes `restore_quick_snapshot`).
/// Returns Ok(true) when at least one file was restored; rejects path
/// traversal in the snapshot id and manifest entries.
pub fn restore_quick_snapshot(home: &Path, snapshot_id: &str) -> Result<bool, String> {
    if snapshot_id.is_empty()
        || snapshot_id.contains('/')
        || snapshot_id.contains('\\')
        || snapshot_id == "."
        || snapshot_id == ".."
    {
        return Err(format!("Invalid snapshot id: {snapshot_id}"));
    }
    let root = quick_snapshot_root(home);
    let snap_dir = root.join(snapshot_id);
    if !snap_dir.is_dir() {
        return Ok(false);
    }
    let manifest_path = snap_dir.join("manifest.json");
    let Ok(text) = fs::read_to_string(&manifest_path) else {
        return Ok(false);
    };
    let Ok(meta) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(false);
    };
    let Some(files) = meta.get("files").and_then(|f| f.as_object()) else {
        return Ok(false);
    };

    let mut restored = 0usize;
    for rel in files.keys() {
        let src = snap_dir.join(rel);
        let dst = home.join(rel);
        // Traversal guards on both ends.
        let src_ok = src
            .canonicalize()
            .ok()
            .map(|p| {
                snap_dir
                    .canonicalize()
                    .ok()
                    .map(|r| p.starts_with(r))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !src_ok {
            continue;
        }
        if !src.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let copied = if dst.extension().map(|e| e == "db").unwrap_or(false) {
            // Atomic-ish replace for databases (hermes parity).
            let tmp = dst.parent().unwrap_or(Path::new(".")).join(format!(
                ".{}.snap_restore",
                dst.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
            let result = fs::copy(&src, &tmp).is_ok()
                && fs::remove_file(&dst).map(|_| true).unwrap_or(true)
                && fs::rename(&tmp, &dst).is_ok();
            if !result {
                let _ = fs::remove_file(&tmp);
            }
            result
        } else {
            fs::copy(&src, &dst).is_ok()
        };
        if copied {
            restored += 1;
        }
    }
    Ok(restored > 0)
}

/// Prune snapshots beyond `keep` (newest preserved; hermes
/// `_prune_quick_snapshots`). Returns the number removed.
pub fn prune_quick_snapshots(home: &Path, keep: usize) -> usize {
    let snapshots = list_quick_snapshots(home);
    if snapshots.len() <= keep {
        return 0;
    }
    let mut removed = 0usize;
    for snapshot in snapshots.iter().skip(keep) {
        let dir = quick_snapshot_root(home).join(&snapshot.id);
        if fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

// =============================================================================
// Cron safety net
// =============================================================================

/// Count cron jobs in a state.db (ulnclaw keeps them in the `cron_jobs`
/// table; hermes reads cron/jobs.json).
pub fn count_cron_jobs(state_db: &Path) -> Option<usize> {
    let conn =
        rusqlite::Connection::open_with_flags(state_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='cron_jobs'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !table_exists {
        return Some(0);
    }
    conn.query_row("SELECT COUNT(*) FROM cron_jobs", [], |row| {
        row.get::<_, usize>(0)
    })
    .ok()
}

/// After an import/update, if the live state.db has zero cron jobs but a
/// snapshot's state.db still has some, restore that snapshot (hermes
/// `restore_cron_jobs_if_emptied`). Returns a message when a restore happened.
pub fn restore_cron_jobs_if_emptied(home: &Path) -> Option<String> {
    let live_db = home.join("state.db");
    if !live_db.exists() {
        return None;
    }
    if count_cron_jobs(&live_db).unwrap_or(0) > 0 {
        return None;
    }
    for snapshot in list_quick_snapshots(home) {
        let snap_db = quick_snapshot_root(home)
            .join(&snapshot.id)
            .join("state.db");
        if !snap_db.exists() {
            continue;
        }
        let snap_jobs = count_cron_jobs(&snap_db).unwrap_or(0);
        if snap_jobs == 0 {
            continue;
        }
        match restore_quick_snapshot(home, &snapshot.id) {
            Ok(true) => {
                return Some(format!(
                    "⚠ Cron jobs were missing after the update — restored {snap_jobs} job(s) from snapshot {}.",
                    snapshot.id
                ))
            }
            _ => continue,
        }
    }
    None
}

/// Pre-update max file size — a multi-GB `state.db` must never stall an
/// update (hermes `_run_pre_update_backup` quick-path semantics).
pub const PRE_UPDATE_MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Pre-update safety snapshot (hermes `_run_pre_update_backup` quick path):
/// captures the small critical state files so an update can never lose
/// config/cron/auth; oversized databases are skipped with a warning.
/// Returns the snapshot id when one was created.
pub fn create_pre_update_backup(home: &Path) -> Option<String> {
    create_quick_snapshot(
        home,
        Some("pre-update"),
        Some(QUICK_DEFAULT_KEEP),
        Some(PRE_UPDATE_MAX_FILE_SIZE),
    )
    .ok()
    .flatten()
}

/// Shared `/snapshot` dispatch for REPL and gateway (P668 — hermes
/// `/snapshot` parity over the quick-snapshot system): create (default),
/// list, restore, prune. One formatted string back, no process spawn.
pub fn snapshot_slash(home: &Path, rest: &str) -> String {
    let mut parts = rest.split_whitespace();
    let sub = parts.next().unwrap_or("create");
    match sub {
        "create" | "new" | "save" => {
            let label = parts.collect::<Vec<_>>().join(" ");
            let label = if label.is_empty() {
                None
            } else {
                Some(label.as_str())
            };
            match create_quick_snapshot(home, label, None, None) {
                Ok(Some(id)) => format!("📸 snapshot created: {id}\n"),
                Ok(None) => "(o_o) nothing to snapshot yet (no state files).\n".to_string(),
                Err(e) => format!("(._.) snapshot failed: {e}\n"),
            }
        }
        "list" | "ls" => {
            let snapshots = list_quick_snapshots(home);
            if snapshots.is_empty() {
                return "(o_o) no snapshots yet. Run /snapshot to create one.\n".to_string();
            }
            let mut out = String::new();
            out.push_str(&format!("{} snapshot(s):\n", snapshots.len()));
            for snapshot in &snapshots {
                out.push_str(&format!(
                    "  {:<32} {:>4} file(s)  {:>10}\n",
                    snapshot.id,
                    snapshot.files,
                    format_size(snapshot.bytes as f64)
                ));
            }
            out
        }
        "restore" => {
            let Some(id) = parts.next() else {
                return "(o_o) usage: /snapshot restore <snapshot-id>\n".to_string();
            };
            match restore_quick_snapshot(home, id) {
                Ok(true) => format!("✓ restored state from snapshot {id}\n"),
                Ok(false) => format!("(o_o) snapshot '{id}' not found or empty\n"),
                Err(e) => format!("(._.) restore failed: {e}\n"),
            }
        }
        "prune" => {
            let keep = parts
                .next()
                .and_then(|raw| raw.parse::<usize>().ok())
                .unwrap_or(QUICK_DEFAULT_KEEP);
            let removed = prune_quick_snapshots(home, keep);
            format!("pruned {removed} snapshot(s) (keeping {keep})\n")
        }
        _ => "(o_o) usage: /snapshot [create [label]|list|restore <id>|prune [keep]]\n".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_sqlite_db(path: &Path, table: &str, rows: usize) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE {table} (id INTEGER PRIMARY KEY, v TEXT);"
        ))
        .unwrap();
        for i in 0..rows {
            conn.execute(
                &format!("INSERT INTO {table} (v) VALUES (?1)"),
                rusqlite::params![format!("row {i}")],
            )
            .unwrap();
        }
    }

    fn count_rows(path: &Path, table: &str) -> usize {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, usize>(0)
        })
        .unwrap()
    }

    #[test]
    fn format_size_buckets() {
        assert_eq!(format_size(512.0), "512 B");
        assert_eq!(format_size(2048.0), "2.0 KB");
        assert_eq!(format_size(5.0 * 1024.0 * 1024.0), "5.0 MB");
        assert_eq!(format_size(3.0 * 1024.0 * 1024.0 * 1024.0), "3.0 GB");
        assert_eq!(format_size(2.0f64.powi(4)), "16 B");
    }

    #[test]
    fn zeroed_sqlite_detection() {
        let dir = tempfile::tempdir().unwrap();
        let zeroed = dir.path().join("zeroed.db");
        fs::write(&zeroed, vec![0u8; 512]).unwrap();
        assert!(is_zeroed_sqlite_file(&zeroed));

        let real = dir.path().join("real.db");
        make_sqlite_db(&real, "t", 1);
        assert!(!is_zeroed_sqlite_file(&real));
        assert!(verify_sqlite_integrity(&real));
        assert!(!verify_sqlite_integrity(&zeroed));
    }

    #[test]
    fn safe_copy_db_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("state.db");
        make_sqlite_db(&src, "cron_jobs", 3);
        let dst = dir.path().join("copy.db");
        assert!(safe_copy_db(&src, &dst));
        assert!(verify_sqlite_integrity(&dst));
        assert_eq!(count_rows(&dst, "cron_jobs"), 3);
        assert!(copy_db_and_verify(&src, &dir.path().join("copy2.db")));
        assert!(!safe_copy_db(
            &dir.path().join("missing.db"),
            &dir.path().join("x.db")
        ));
    }

    fn populate_home(home: &Path) {
        fs::create_dir_all(home.join("skills")).unwrap();
        fs::create_dir_all(home.join("memory")).unwrap();
        fs::create_dir_all(home.join("cron")).unwrap();
        fs::create_dir_all(home.join("node_modules").join("junk")).unwrap();
        fs::create_dir_all(home.join("checkpoints")).unwrap();
        fs::write(home.join("config.toml"), "[model]\nprovider = \"ollama\"\n").unwrap();
        fs::write(home.join(".env"), "SECRET=1\n").unwrap();
        fs::write(home.join("memory").join("notes.md"), "memory").unwrap();
        fs::write(home.join("cron").join("suggestions.json"), "{}").unwrap();
        fs::write(home.join("node_modules").join("junk").join("big.bin"), "x").unwrap();
        fs::write(home.join("checkpoints").join("c1"), "x").unwrap();
        fs::write(home.join("state.db-wal"), "wal").unwrap();
        make_sqlite_db(&home.join("state.db"), "cron_jobs", 2);
    }

    #[test]
    fn create_backup_zips_home_with_exclusions() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        populate_home(&home);

        let summary = create_backup(&home, Some(&dir.path().join("out.zip"))).unwrap();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert_eq!(summary.out_path, dir.path().join("out.zip"));
        assert!(summary.skipped_dirs.iter().any(|d| d == "node_modules"));
        assert!(summary.skipped_dirs.iter().any(|d| d == "checkpoints"));

        let mut archive = zip::ZipArchive::new(File::open(&summary.out_path).unwrap()).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index_raw(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"config.toml".to_string()));
        assert!(names.contains(&".env".to_string()));
        assert!(names.contains(&"state.db".to_string()));
        assert!(names.contains(&"memory/notes.md".to_string()));
        assert!(names.contains(&"cron/suggestions.json".to_string()));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
        assert!(!names.iter().any(|n| n.contains("checkpoints")));
        assert!(!names.iter().any(|n| n.ends_with(".db-wal")));

        // The archived state.db is a consistent snapshot.
        let mut archive = zip::ZipArchive::new(File::open(&summary.out_path).unwrap()).unwrap();
        let extract_dir = dir.path().join("extract");
        fs::create_dir_all(&extract_dir).unwrap();
        let mut entry = archive.by_name("state.db").unwrap();
        let mut out = File::create(extract_dir.join("state.db")).unwrap();
        std::io::copy(&mut entry, &mut out).unwrap();
        drop(entry);
        assert_eq!(count_rows(&extract_dir.join("state.db"), "cron_jobs"), 2);
    }

    #[test]
    fn import_backup_restores_and_skips_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        let source_home = dir.path().join("source");
        fs::create_dir_all(&source_home).unwrap();
        populate_home(&source_home);
        fs::write(source_home.join("gateway.pid"), "1234").unwrap();

        let zip_path = dir.path().join("backup.zip");
        let summary = create_backup(&source_home, Some(&zip_path)).unwrap();
        assert!(summary.errors.is_empty());

        // Craft a zip that also carries volatile runtime state.
        let target_home = dir.path().join("target");
        fs::create_dir_all(&target_home).unwrap();
        let (ok, reason) = validate_backup_zip(&zip_path);
        assert!(ok, "{reason}");

        let report = import_backup(&target_home, &zip_path).unwrap();
        assert!(report.restored > 0);
        assert!(target_home.join("config.toml").exists());
        assert!(target_home.join("state.db").exists());
        assert!(target_home.join("memory").join("notes.md").exists());

        // Secrets tightened to 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(target_home.join(".env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Runtime-state files inside an archive are never restored.
        let evil_zip = dir.path().join("evil.zip");
        {
            let f = File::create(&evil_zip).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("config.toml", opts).unwrap();
            zip.write_all(b"[model]\n").unwrap();
            zip.start_file("gateway_state.json", opts).unwrap();
            zip.write_all(b"{\"state\": \"running\"}").unwrap();
            zip.start_file("gateway.pid", opts).unwrap();
            zip.write_all(b"999").unwrap();
            zip.finish().unwrap();
        }
        let report = import_backup(&target_home, &evil_zip).unwrap();
        assert_eq!(report.skipped_runtime, 2);
        assert!(!target_home.join("gateway_state.json").exists());
        assert!(!target_home.join("gateway.pid").exists());

        // Non-backup zips are rejected.
        let bogus_zip = dir.path().join("bogus.zip");
        {
            let f = File::create(&bogus_zip).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"hi").unwrap();
            zip.finish().unwrap();
        }
        let (ok, reason) = validate_backup_zip(&bogus_zip);
        assert!(!ok);
        assert!(reason.contains("does not appear"));
    }

    #[test]
    fn detect_prefix_strips_wrapper_dirs() {
        let names: Vec<String> = vec![".hermes/config.yaml".into(), ".hermes/state.db".into()];
        assert_eq!(detect_prefix(&names), ".hermes/");
        let names: Vec<String> = vec!["config.toml".into(), "state.db".into()];
        assert_eq!(detect_prefix(&names), "");
        let names: Vec<String> = vec![".ulnclaw/config.toml".into(), ".ulnclaw/memory/n.md".into()];
        assert_eq!(detect_prefix(&names), ".ulnclaw/");
        let names: Vec<String> = vec!["other/config.toml".into()];
        assert_eq!(detect_prefix(&names), "");
    }

    #[test]
    fn quick_snapshot_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        populate_home(&home);

        let snap_id = create_quick_snapshot(&home, Some("test"), None, None)
            .unwrap()
            .expect("snapshot created");
        assert!(snap_id.ends_with("-test"));

        let listing = list_quick_snapshots(&home);
        assert_eq!(listing.len(), 1);
        assert!(listing[0].files >= 4, "{:?}", listing);

        // Wipe live state, then restore.
        fs::remove_file(home.join("state.db")).unwrap();
        fs::remove_file(home.join("config.toml")).unwrap();
        assert!(restore_quick_snapshot(&home, &snap_id).unwrap());
        assert!(home.join("config.toml").exists());
        assert!(home.join("state.db").exists());
        assert_eq!(count_rows(&home.join("state.db"), "cron_jobs"), 2);

        // Traversal attempts are rejected.
        assert!(restore_quick_snapshot(&home, "../escape").is_err());
        assert!(restore_quick_snapshot(&home, "..").is_err());
        assert!(!restore_quick_snapshot(&home, "no-such-snapshot").unwrap());
    }

    #[test]
    fn quick_snapshot_prune_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config.toml"), "x").unwrap();

        let root = quick_snapshot_root(&home);
        for i in 0..5 {
            let snap = root.join(format!("2026010{i}-000000"));
            fs::create_dir_all(&snap).unwrap();
            fs::copy(home.join("config.toml"), snap.join("config.toml")).unwrap();
            let meta = serde_json::json!({"files": {"config.toml": 1}});
            fs::write(snap.join("manifest.json"), meta.to_string()).unwrap();
        }
        assert_eq!(list_quick_snapshots(&home).len(), 5);
        let removed = prune_quick_snapshots(&home, 2);
        assert_eq!(removed, 3);
        let listing = list_quick_snapshots(&home);
        assert_eq!(listing.len(), 2);
        assert_eq!(listing[0].id, "20260104-000000");
        assert_eq!(listing[1].id, "20260103-000000");
    }

    #[test]
    fn cron_jobs_safety_net_restores_emptied_db() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        make_sqlite_db(&home.join("state.db"), "cron_jobs", 3);

        let snap_id = create_quick_snapshot(&home, None, None, None)
            .unwrap()
            .unwrap();

        // Simulate a bad update: state.db recreated without jobs.
        fs::remove_file(home.join("state.db")).unwrap();
        make_sqlite_db(&home.join("state.db"), "sessions", 0);
        assert_eq!(count_cron_jobs(&home.join("state.db")), Some(0));

        let message = restore_cron_jobs_if_emptied(&home);
        assert!(message.is_some(), "safety net should fire");
        assert!(message.unwrap().contains(&snap_id));
        assert_eq!(count_cron_jobs(&home.join("state.db")), Some(3));
    }

    #[test]
    fn snapshot_slash_create_list_restore_prune() {
        // P668: /snapshot slash over the quick-snapshot system.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        populate_home(&home);

        // Create (with a label).
        let out = snapshot_slash(&home, "create before-upgrade");
        assert!(out.contains("snapshot created:"), "{out}");
        let id = out
            .trim()
            .trim_start_matches("\u{1f4f8} snapshot created:")
            .trim()
            .to_string();
        assert!(id.ends_with("before-upgrade"), "{id}");

        // List shows it with file counts.
        let out = snapshot_slash(&home, "list");
        assert!(out.contains(&id), "{out}");
        assert!(out.contains("file(s)"), "{out}");

        // Restore round-trip + unknown id.
        let out = snapshot_slash(&home, &format!("restore {id}"));
        assert!(out.contains("restored state from snapshot"), "{out}");
        let out = snapshot_slash(&home, "restore nope");
        assert!(out.contains("not found"), "{out}");

        // Prune with a generous keep removes nothing.
        let out = snapshot_slash(&home, "prune 5");
        assert!(out.contains("pruned 0 snapshot(s)"), "{out}");

        // Usage fallback.
        let out = snapshot_slash(&home, "bogus");
        assert!(out.contains("usage:"), "{out}");
    }
}

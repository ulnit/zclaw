//! Per-profile first-class Project store — port of hermes
//! `hermes_cli/projects_db.py` (v2026.8.3).
//!
//! A **Project** is a human-named, multi-folder workspace. Unlike inferred
//! workspaces (derived from a session cwd) and kanban's self-generated
//! worktrees, a Project is an explicit, persisted entity the user creates
//! and names. It anchors:
//!
//! - **Desktop session grouping** — a session belongs to a project when its
//!   cwd lives under one of the project's folders (longest-prefix match).
//! - **Kanban task worktrees** — a task linked to a project creates its
//!   worktree under the project's primary repo with a deterministic branch
//!   name, instead of the random `wt/<task-id>` fallback.
//!
//! Scope: **per-profile**, stored at `<home>/projects.db`, mirroring
//! sessions / config / cron. A Project may *bind* a kanban board
//! (`board_slug`) so the two systems agree on the repo + branch convention
//! without merging their stores.
//!
//! The schema is intentionally small and additive: column additions go
//! through an idempotent migration so opening an old DB is always safe.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::error::{AgentError, Result};

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// The per-profile projects DB path (`<home>/projects.db`).
pub fn projects_db_path() -> PathBuf {
    crate::config::ulnclaw_home().join("projects.db")
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id            TEXT PRIMARY KEY,
    slug          TEXT NOT NULL UNIQUE,
    name          TEXT NOT NULL,
    description   TEXT,
    icon          TEXT,
    color         TEXT,
    board_slug    TEXT,
    primary_path  TEXT,
    created_at    INTEGER NOT NULL,
    archived      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS project_folders (
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path        TEXT NOT NULL,
    label       TEXT,
    is_primary  INTEGER NOT NULL DEFAULT 0,
    added_at    INTEGER NOT NULL,
    PRIMARY KEY (project_id, path)
);

CREATE INDEX IF NOT EXISTS idx_project_folders_path
    ON project_folders(path);

CREATE TABLE IF NOT EXISTS project_meta (
    key    TEXT PRIMARY KEY,
    value  TEXT
);

-- Git repos found by scanning the filesystem (desktop repo-first discovery).
-- Cached here so the overview is instant after the first scan instead of
-- re-walking the disk every time the Projects view opens.
CREATE TABLE IF NOT EXISTS discovered_repos (
    root          TEXT PRIMARY KEY,
    label         TEXT,
    last_seen     INTEGER NOT NULL
);
";

// ---------------------------------------------------------------------------
// Slug + id helpers
// ---------------------------------------------------------------------------

fn slug_char_ok(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
}

/// Derive a slug candidate from a human name (best-effort) — hermes
/// `_slugify`.
pub fn slugify(name: &str) -> String {
    let s = name.trim().to_ascii_lowercase();
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    // Collapse nothing: python's re.sub("[^a-z0-9]+", "-") collapses runs.
    let mut collapsed = String::new();
    let mut prev_dash = false;
    for c in out.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push(c);
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    let trimmed = collapsed.trim_matches(|c| c == '-' || c == '_');
    let cut: String = trimmed.chars().take(64).collect();
    let cut = cut.trim_matches(|c| c == '-' || c == '_');
    if cut.is_empty() {
        "project".to_string()
    } else {
        cut.to_string()
    }
}

/// Lowercase + strip a slug; validate; `Ok(None)` for empty — hermes
/// `normalize_slug`. Invalid slugs are an error: 1-64 chars, lowercase
/// alphanumerics / hyphens / underscores, not starting with '-' or '_'.
pub fn normalize_slug(slug: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = slug else { return Ok(None) };
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Ok(None);
    }
    let valid =
        s.len() <= 64 && s.chars().all(slug_char_ok) && !s.starts_with('-') && !s.starts_with('_');
    if !valid {
        return Err(AgentError::session(format!(
            "invalid project slug '{}': must be 1-64 chars, lowercase alphanumerics / hyphens / underscores, not starting with '-' or '_'",
            raw
        )));
    }
    Ok(Some(s))
}

fn new_project_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // hermes uses secrets.token_hex(4); mix time + counter for uniqueness.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id() as u64;
    format!(
        "p_{:08x}",
        (nanos ^ (n.wrapping_mul(0x9E3779B9)) ^ pid) & 0xFFFF_FFFF
    )
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Absolute, user-expanded, separator-normalized path (no trailing sep) —
/// hermes `_normalize_path` (lexical; symlinks are not resolved).
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let expanded = expand_tilde(trimmed);
    let joined = if Path::new(&expanded).is_absolute() {
        PathBuf::from(expanded)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    };
    let mut parts: Vec<String> = Vec::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if parts.len() > 1 || (parts.len() == 1 && !parts[0].is_empty()) {
                    parts.pop();
                }
            }
            std::path::Component::RootDir => parts.push(String::new()),
            other => parts.push(other.as_os_str().to_string_lossy().to_string()),
        }
    }
    let out = if parts.first().map(|s| s.is_empty()).unwrap_or(false) {
        format!("/{}", parts[1..].join("/"))
    } else {
        parts.join("/")
    };
    if out.is_empty() {
        "/".to_string()
    } else {
        out
    }
}

fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let home = home.to_string_lossy().to_string();
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

// ---------------------------------------------------------------------------
// Connection management
// ---------------------------------------------------------------------------

fn initialized_paths() -> &'static Mutex<HashSet<String>> {
    static PATHS: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    PATHS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Open (and initialize if needed) the per-profile projects DB — hermes
/// `connect`. WAL with DELETE fallback; schema init is idempotent and
/// cached per-path per-process.
pub fn connect(db_path: Option<&Path>) -> Result<Connection> {
    let path = match db_path {
        Some(p) => p.to_path_buf(),
        None => projects_db_path(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(&path)
        .map_err(|e| AgentError::session(format!("open projects db {}: {}", path.display(), e)))?;
    apply_wal_with_fallback(&conn);
    conn.execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|e| AgentError::session(format!("projects db pragmas: {}", e)))?;
    let resolved = std::fs::canonicalize(&path)
        .unwrap_or_else(|_| path.clone())
        .to_string_lossy()
        .to_string();
    let already = initialized_paths()
        .lock()
        .map(|g| g.contains(&resolved))
        .unwrap_or(false);
    if !already {
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| AgentError::session(format!("projects db schema: {}", e)))?;
        migrate_add_optional_columns(&conn)?;
        if let Ok(mut g) = initialized_paths().lock() {
            g.insert(resolved);
        }
    }
    Ok(conn)
}

/// Try WAL; fall back to the default journal on network filesystems —
/// hermes `apply_wal_with_fallback`.
fn apply_wal_with_fallback(conn: &Connection) {
    let ok = conn
        .query_row("PRAGMA journal_mode=WAL;", [], |row| {
            row.get::<_, String>(0)
        })
        .map(|mode| mode.eq_ignore_ascii_case("wal"))
        .unwrap_or(false);
    if !ok {
        conn.execute_batch("PRAGMA journal_mode=DELETE;").ok();
    }
}

/// TEXT columns added to `projects` after v1; re-applied idempotently on
/// every open so a legacy DB upgrades in place.
const OPTIONAL_PROJECT_COLUMNS: &[&str] = &["board_slug", "primary_path", "icon", "color"];

fn migrate_add_optional_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(projects)")
        .map_err(|e| AgentError::session(format!("table_info(projects): {}", e)))?;
    let cols: HashSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AgentError::session(format!("table_info(projects): {}", e)))?
        .filter_map(|r| r.ok())
        .collect();
    for col in OPTIONAL_PROJECT_COLUMNS {
        if !cols.contains(*col) {
            conn.execute_batch(&format!("ALTER TABLE projects ADD COLUMN {} TEXT;", col))
                .map_err(|e| AgentError::session(format!("add column {}: {}", col, e)))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectFolder {
    pub path: String,
    pub label: Option<String>,
    pub is_primary: bool,
    pub added_at: i64,
}

impl ProjectFolder {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "label": self.label,
            "is_primary": self.is_primary,
            "added_at": self.added_at,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub board_slug: Option<String>,
    pub primary_path: Option<String>,
    pub created_at: i64,
    pub archived: bool,
    pub folders: Vec<ProjectFolder>,
}

impl Project {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "slug": self.slug,
            "name": self.name,
            "description": self.description,
            "icon": self.icon,
            "color": self.color,
            "board_slug": self.board_slug,
            "primary_path": self.primary_path,
            "archived": self.archived,
            "created_at": self.created_at,
            "folders": self.folders.iter().map(|f| f.to_json()).collect::<Vec<_>>(),
        })
    }
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get("id")?,
        slug: row.get("slug")?,
        name: row.get("name")?,
        created_at: row.get("created_at")?,
        description: row.get("description").ok(),
        icon: row.get("icon").ok(),
        color: row.get("color").ok(),
        board_slug: row.get("board_slug").ok(),
        primary_path: row.get("primary_path").ok(),
        archived: row.get::<_, i64>("archived").unwrap_or(0) != 0,
        folders: Vec::new(),
    })
}

fn load_folders(conn: &Connection, project_id: &str) -> Result<Vec<ProjectFolder>> {
    let mut stmt = conn
        .prepare(
            "SELECT path, label, is_primary, added_at FROM project_folders \
             WHERE project_id = ?1 ORDER BY is_primary DESC, added_at ASC",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![project_id], |row| {
            Ok(ProjectFolder {
                path: row.get(0)?,
                label: row.get(1)?,
                is_primary: row.get::<_, i64>(2)? != 0,
                added_at: row.get(3)?,
            })
        })
        .map_err(db_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
}

fn attach_folders(conn: &Connection, mut project: Project) -> Result<Project> {
    project.folders = load_folders(conn, &project.id)?;
    Ok(project)
}

fn db_err(e: rusqlite::Error) -> AgentError {
    AgentError::session(format!("projects db: {}", e))
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

fn unique_slug(conn: &Connection, candidate: &str) -> Result<String> {
    let mut slug = candidate.to_string();
    let mut n: usize = 1;
    while conn
        .query_row(
            "SELECT 1 FROM projects WHERE slug = ?1",
            params![slug],
            |_| Ok(()),
        )
        .optional()
        .map_err(db_err)?
        .is_some()
    {
        n += 1;
        let suffix = format!("-{}", n);
        let keep = 64usize.saturating_sub(suffix.len());
        let base: String = candidate.chars().take(keep).collect();
        slug = format!("{}{}", base.trim_matches(|c| c == '-' || c == '_'), suffix);
    }
    Ok(slug)
}

/// `create_project` arguments (hermes keyword params).
#[derive(Default)]
pub struct CreateProject<'a> {
    pub name: &'a str,
    pub slug: Option<&'a str>,
    pub folders: &'a [&'a str],
    pub primary_path: Option<&'a str>,
    pub description: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub color: Option<&'a str>,
    pub board_slug: Option<&'a str>,
}

/// Create a project and return its id — hermes `create_project`.
///
/// Folders are normalized to absolute paths. If `primary_path` is given it
/// is added to the folder set (if not already present) and marked primary;
/// otherwise the first folder becomes primary.
pub fn create_project(conn: &Connection, args: &CreateProject<'_>) -> Result<String> {
    let name = args.name.trim();
    if name.is_empty() {
        return Err(AgentError::session("project name must not be empty"));
    }
    let slug_candidate = match args.slug {
        Some(s) => normalize_slug(Some(s))?.unwrap_or_else(|| slugify(name)),
        None => slugify(name),
    };
    let pid = new_project_id();
    let now = now_secs();

    let mut folder_paths: Vec<String> = Vec::new();
    for f in args.folders {
        let norm = normalize_path(f);
        if !norm.is_empty() && !folder_paths.contains(&norm) {
            folder_paths.push(norm);
        }
    }
    let mut primary: Option<String> = args
        .primary_path
        .map(|p| normalize_path(p))
        .filter(|p| !p.is_empty());
    if let Some(ref p) = primary {
        if !folder_paths.contains(p) {
            folder_paths.insert(0, p.clone());
        }
    } else if let Some(first) = folder_paths.first().cloned() {
        primary = Some(first);
    }

    let board = match args.board_slug {
        Some(b) if !b.trim().is_empty() => normalize_slug(Some(b))?,
        _ => None,
    };

    let tx = conn.unchecked_transaction().map_err(db_err)?;
    let unique = unique_slug(&tx, &slug_candidate)?;
    tx.execute(
        "INSERT INTO projects \
         (id, slug, name, description, icon, color, board_slug, primary_path, created_at, archived) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)",
        params![
            pid,
            unique,
            name,
            args.description,
            args.icon,
            args.color,
            board,
            primary,
            now,
        ],
    )
    .map_err(db_err)?;
    for path in &folder_paths {
        tx.execute(
            "INSERT INTO project_folders (project_id, path, label, is_primary, added_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                pid,
                path,
                Option::<String>::None,
                Some(path) == primary.as_ref(),
                now
            ],
        )
        .map_err(db_err)?;
    }
    tx.commit().map_err(db_err)?;
    Ok(pid)
}

/// List projects ordered by creation (hermes `list_projects`).
pub fn list_projects(conn: &Connection, include_archived: bool) -> Result<Vec<Project>> {
    let sql = if include_archived {
        "SELECT * FROM projects ORDER BY created_at ASC"
    } else {
        "SELECT * FROM projects WHERE archived = 0 ORDER BY created_at ASC"
    };
    let mut stmt = conn.prepare(sql).map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| project_from_row(row))
        .map_err(db_err)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(attach_folders(conn, row.map_err(db_err)?)?);
    }
    Ok(out)
}

/// Look up a project by id first, then by slug (hermes `get_project`).
pub fn get_project(conn: &Connection, id_or_slug: &str) -> Result<Option<Project>> {
    let row = conn
        .query_row(
            "SELECT * FROM projects WHERE id = ?1",
            params![id_or_slug],
            |r| project_from_row(r),
        )
        .optional()
        .map_err(db_err)?
        .or_else(|| {
            conn.query_row(
                "SELECT * FROM projects WHERE slug = ?1",
                params![id_or_slug.to_ascii_lowercase()],
                |r| project_from_row(r),
            )
            .optional()
            .map_err(db_err)
            .unwrap_or(None)
        });
    match row {
        Some(p) => Ok(Some(attach_folders(conn, p)?)),
        None => Ok(None),
    }
}

/// `update_project` arguments — `Some("")` clears icon/color/board_slug,
/// `None` leaves the field untouched (hermes semantics).
#[derive(Default)]
pub struct UpdateProject<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub color: Option<&'a str>,
    pub board_slug: Option<&'a str>,
}

/// Patch top-level project fields; only provided fields change (hermes
/// `update_project`). Returns whether a row changed.
pub fn update_project(
    conn: &Connection,
    project_id: &str,
    args: &UpdateProject<'_>,
) -> Result<bool> {
    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(name) = args.name {
        let n = name.trim();
        if n.is_empty() {
            return Err(AgentError::session("project name must not be empty"));
        }
        sets.push("name = ?".to_string());
        values.push(Box::new(n.to_string()));
    }
    if let Some(d) = args.description {
        sets.push("description = ?".to_string());
        values.push(Box::new(d.to_string()));
    }
    if let Some(icon) = args.icon {
        sets.push("icon = ?".to_string());
        values.push(Box::new(if icon.is_empty() {
            None::<String>
        } else {
            Some(icon.to_string())
        }));
    }
    if let Some(color) = args.color {
        sets.push("color = ?".to_string());
        values.push(Box::new(if color.is_empty() {
            None::<String>
        } else {
            Some(color.to_string())
        }));
    }
    if let Some(board) = args.board_slug {
        let normalized = if board.trim().is_empty() {
            None
        } else {
            normalize_slug(Some(board))?
        };
        sets.push("board_slug = ?".to_string());
        values.push(Box::new(normalized));
    }
    if sets.is_empty() {
        return Ok(false);
    }
    values.push(Box::new(project_id.to_string()));
    let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    let sql = format!("UPDATE projects SET {} WHERE id = ?", sets.join(", "));
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    let changed = tx
        .execute(&sql, rusqlite::params_from_iter(refs))
        .map_err(db_err)?
        > 0;
    tx.commit().map_err(db_err)?;
    Ok(changed)
}

/// Add a folder to a project; returns the normalized path (hermes
/// `add_folder`). When `is_primary` is set, the folder becomes the
/// project's primary repo (the previous primary is demoted, and
/// `projects.primary_path` updates). The first folder of an empty project
/// becomes primary implicitly.
pub fn add_folder(
    conn: &Connection,
    project_id: &str,
    path: &str,
    label: Option<&str>,
    is_primary: bool,
) -> Result<String> {
    let norm = normalize_path(path);
    if norm.is_empty() {
        return Err(AgentError::session("folder path must not be empty"));
    }
    if get_project(conn, project_id)?.is_none() {
        return Err(AgentError::session(format!(
            "no such project: {}",
            project_id
        )));
    }
    let now = now_secs();
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    tx.execute(
        "INSERT OR IGNORE INTO project_folders (project_id, path, label, is_primary, added_at) \
         VALUES (?1, ?2, ?3, 0, ?4)",
        params![project_id, norm, label, now],
    )
    .map_err(db_err)?;
    if let Some(l) = label {
        tx.execute(
            "UPDATE project_folders SET label = ?1 WHERE project_id = ?2 AND path = ?3",
            params![l, project_id, norm],
        )
        .map_err(db_err)?;
    }
    if is_primary {
        set_primary_locked(&tx, project_id, &norm)?;
    } else {
        let existing: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM project_folders WHERE project_id = ?1 AND is_primary = 1",
                params![project_id],
                |_| Ok(1),
            )
            .optional()
            .map_err(db_err)?;
        if existing.is_none() {
            set_primary_locked(&tx, project_id, &norm)?;
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(norm)
}

/// Remove a folder; repoints primary if it was primary (hermes
/// `remove_folder`).
pub fn remove_folder(conn: &Connection, project_id: &str, path: &str) -> Result<bool> {
    let norm = normalize_path(path);
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    let was_primary: Option<bool> = tx
        .query_row(
            "SELECT is_primary FROM project_folders WHERE project_id = ?1 AND path = ?2",
            params![project_id, norm],
            |row| Ok(row.get::<_, i64>(0)? != 0),
        )
        .optional()
        .map_err(db_err)?;
    let removed = tx
        .execute(
            "DELETE FROM project_folders WHERE project_id = ?1 AND path = ?2",
            params![project_id, norm],
        )
        .map_err(db_err)?
        > 0;
    if was_primary == Some(true) {
        let next: Option<String> = tx
            .query_row(
                "SELECT path FROM project_folders WHERE project_id = ?1 ORDER BY added_at ASC LIMIT 1",
                params![project_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
        match next {
            Some(p) => set_primary_locked(&tx, project_id, &p)?,
            None => {
                tx.execute(
                    "UPDATE projects SET primary_path = NULL WHERE id = ?1",
                    params![project_id],
                )
                .map_err(db_err)?;
            }
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(removed)
}

/// Set the primary folder (caller already holds a write txn).
fn set_primary_locked(tx: &Connection, project_id: &str, path: &str) -> Result<()> {
    tx.execute(
        "UPDATE project_folders SET is_primary = 0 WHERE project_id = ?1",
        params![project_id],
    )
    .map_err(db_err)?;
    tx.execute(
        "UPDATE project_folders SET is_primary = 1 WHERE project_id = ?1 AND path = ?2",
        params![project_id, path],
    )
    .map_err(db_err)?;
    tx.execute(
        "UPDATE projects SET primary_path = ?1 WHERE id = ?2",
        params![path, project_id],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Promote an existing folder to primary (hermes `set_primary`).
pub fn set_primary(conn: &Connection, project_id: &str, path: &str) -> Result<bool> {
    let norm = normalize_path(path);
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM project_folders WHERE project_id = ?1 AND path = ?2",
            params![project_id, norm],
            |_| Ok(1),
        )
        .optional()
        .map_err(db_err)?;
    if exists.is_none() {
        return Ok(false);
    }
    set_primary_locked(&tx, project_id, &norm)?;
    tx.commit().map_err(db_err)?;
    Ok(true)
}

pub fn archive_project(conn: &Connection, project_id: &str) -> Result<bool> {
    Ok(conn
        .execute(
            "UPDATE projects SET archived = 1 WHERE id = ?1",
            params![project_id],
        )
        .map_err(db_err)?
        > 0)
}

pub fn restore_project(conn: &Connection, project_id: &str) -> Result<bool> {
    Ok(conn
        .execute(
            "UPDATE projects SET archived = 0 WHERE id = ?1",
            params![project_id],
        )
        .map_err(db_err)?
        > 0)
}

/// Hard-delete a project and its folders (cascade) — hermes
/// `delete_project`.
pub fn delete_project(conn: &Connection, project_id: &str) -> Result<bool> {
    Ok(conn
        .execute("DELETE FROM projects WHERE id = ?1", params![project_id])
        .map_err(db_err)?
        > 0)
}

// ---------------------------------------------------------------------------
// Active-project pointer (project_meta KV)
// ---------------------------------------------------------------------------

const ACTIVE_META_KEY: &str = "active_id";
const DISCOVERY_POLICY_META_KEY: &str = "repo_discovery_policy";

/// Set (or clear, when `None`) the active project pointer.
pub fn set_active(conn: &Connection, project_id: Option<&str>) -> Result<()> {
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    match project_id {
        None => {
            tx.execute(
                "DELETE FROM project_meta WHERE key = ?1",
                params![ACTIVE_META_KEY],
            )
            .map_err(db_err)?;
        }
        Some(id) => {
            tx.execute(
                "INSERT INTO project_meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![ACTIVE_META_KEY, id],
            )
            .map_err(db_err)?;
        }
    }
    tx.commit().map_err(db_err)?;
    Ok(())
}

pub fn get_active_id(conn: &Connection) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM project_meta WHERE key = ?1",
        params![ACTIVE_META_KEY],
        |row| row.get(0),
    )
    .optional()
    .map_err(db_err)
}

pub fn get_discovery_policy_key(conn: &Connection) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM project_meta WHERE key = ?1",
        params![DISCOVERY_POLICY_META_KEY],
        |row| row.get(0),
    )
    .optional()
    .map_err(db_err)
}

/// Clear cached scan rows when their discovery policy changes (hermes
/// `reconcile_discovered_repos_policy`). Existing pre-policy rows are
/// retained only for the backward-compatible default policy. Returns
/// whether rows were cleared.
pub fn reconcile_discovered_repos_policy(
    conn: &Connection,
    policy_key: &str,
    preserve_unversioned: bool,
) -> Result<bool> {
    let current = get_discovery_policy_key(conn)?;
    if current.as_deref() == Some(policy_key) {
        return Ok(false);
    }
    let cleared = current.is_some() || !preserve_unversioned;
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    if cleared {
        tx.execute("DELETE FROM discovered_repos", [])
            .map_err(db_err)?;
    }
    tx.execute(
        "INSERT INTO project_meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![DISCOVERY_POLICY_META_KEY, policy_key],
    )
    .map_err(db_err)?;
    tx.commit().map_err(db_err)?;
    Ok(cleared)
}

pub fn clear_discovered_repos(conn: &Connection, policy_key: Option<&str>) -> Result<()> {
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    tx.execute("DELETE FROM discovered_repos", [])
        .map_err(db_err)?;
    if let Some(key) = policy_key {
        tx.execute(
            "INSERT INTO project_meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![DISCOVERY_POLICY_META_KEY, key],
        )
        .map_err(db_err)?;
    }
    tx.commit().map_err(db_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovered repos (filesystem scan cache)
// ---------------------------------------------------------------------------

/// Persist scanned git repo roots into the cache (hermes
/// `record_discovered_repos`). Returns the number of rows written.
pub fn record_discovered_repos(
    conn: &Connection,
    repos: &[(String, Option<String>)],
    replace: bool,
    policy_key: Option<&str>,
) -> Result<usize> {
    let now = now_secs();
    let mut rows: Vec<(String, String, i64)> = Vec::new();
    for (root, label) in repos {
        let norm = normalize_path(root);
        if norm.is_empty() {
            continue;
        }
        let fallback = Path::new(&norm)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| norm.clone());
        let label = label.clone().filter(|l| !l.is_empty()).unwrap_or(fallback);
        rows.push((norm, label, now));
    }
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    if replace {
        tx.execute("DELETE FROM discovered_repos", [])
            .map_err(db_err)?;
    }
    for (root, label, seen) in &rows {
        tx.execute(
            "INSERT INTO discovered_repos (root, label, last_seen) VALUES (?1, ?2, ?3) \
             ON CONFLICT(root) DO UPDATE SET label = excluded.label, last_seen = excluded.last_seen",
            params![root, label, seen],
        )
        .map_err(db_err)?;
    }
    if let Some(key) = policy_key {
        tx.execute(
            "INSERT INTO project_meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![DISCOVERY_POLICY_META_KEY, key],
        )
        .map_err(db_err)?;
    }
    tx.commit().map_err(db_err)?;
    Ok(rows.len())
}

/// All cached discovered repo roots, most-recently-seen first.
pub fn list_discovered_repos(conn: &Connection) -> Result<Vec<Value>> {
    let mut stmt = conn
        .prepare("SELECT root, label, last_seen FROM discovered_repos ORDER BY last_seen DESC")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(json!({
                "root": row.get::<_, String>(0)?,
                "label": row.get::<_, Option<String>>(1)?,
                "last_seen": row.get::<_, i64>(2)?,
            }))
        })
        .map_err(db_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
}

// ---------------------------------------------------------------------------
// Resolution + naming
// ---------------------------------------------------------------------------

/// Return the project owning `path` (longest-prefix folder match) — hermes
/// `project_for_path`. Nested projects resolve to the innermost one.
pub fn project_for_path(
    conn: &Connection,
    path: &str,
    include_archived: bool,
) -> Result<Option<Project>> {
    if path.trim().is_empty() {
        return Ok(None);
    }
    let target = normalize_path(path);
    let sql = if include_archived {
        "SELECT pf.project_id AS pid, pf.path AS folder \
         FROM project_folders pf JOIN projects p ON p.id = pf.project_id"
    } else {
        "SELECT pf.project_id AS pid, pf.path AS folder \
         FROM project_folders pf JOIN projects p ON p.id = pf.project_id \
         WHERE p.archived = 0"
    };
    let mut stmt = conn.prepare(sql).map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(db_err)?;
    let mut best_pid: Option<String> = None;
    let mut best_len: usize = 0;
    for row in rows {
        let (pid, folder) = row.map_err(db_err)?;
        let base = folder.trim_end_matches('/');
        let owned = target == folder || target.starts_with(&format!("{}/", base));
        if owned && folder.len() >= best_len {
            best_len = folder.len();
            best_pid = Some(pid);
        }
    }
    match best_pid {
        Some(pid) => get_project(conn, &pid),
        None => Ok(None),
    }
}

/// True when `target` equals `folder` or is nested below it (separator-
/// aware prefix match — a sibling like `/repo2` never matches `/repo`).
fn path_under(folder: &str, target: &str) -> bool {
    if target == folder {
        return true;
    }
    let trimmed = folder.trim_end_matches(['/', '\\']);
    let bytes = target.as_bytes();
    if bytes.len() > trimmed.len() && target.starts_with(trimmed) {
        let next = bytes[trimmed.len()];
        return next == b'/' || next == b'\\';
    }
    false
}

/// Batched [`project_for_path`]: resolve the owning project for many
/// paths with a single folder scan (hermes desktop session grouping by
/// project). Longest-prefix folder wins; archived projects are excluded.
/// Returns one entry per input path that matched, keyed by the original
/// (un-normalized) input.
pub fn projects_for_paths(
    conn: &Connection,
    paths: &[String],
) -> Result<std::collections::HashMap<String, Project>> {
    let mut out: std::collections::HashMap<String, Project> = std::collections::HashMap::new();
    if paths.is_empty() {
        return Ok(out);
    }
    let folders: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT pf.path, pf.project_id FROM project_folders pf \
                 JOIN projects p ON p.id = pf.project_id WHERE p.archived = 0",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)?
    };
    if folders.is_empty() {
        return Ok(out);
    }
    let mut cache: std::collections::HashMap<String, Option<Project>> =
        std::collections::HashMap::new();
    for path in paths {
        let target = normalize_path(path);
        if target.is_empty() {
            continue;
        }
        let mut best: Option<(String, usize)> = None;
        for (folder, pid) in &folders {
            if path_under(folder, &target)
                && best
                    .as_ref()
                    .map(|(_, len)| folder.len() > *len)
                    .unwrap_or(true)
            {
                best = Some((pid.clone(), folder.len()));
            }
        }
        if let Some((pid, _)) = best {
            let entry = cache
                .entry(pid.clone())
                .or_insert_with(|| get_project(conn, &pid).ok().flatten());
            if let Some(project) = entry {
                out.insert(path.clone(), project.clone());
            }
        }
    }
    Ok(out)
}

/// Deterministic branch name for a project-linked kanban task (hermes
/// `branch_name_for`). Shape: `<project-slug>/<task-id>` (optionally
/// `-<title-slug>`).
pub fn branch_name_for(project: &Project, task_id: &str, title: &str) -> String {
    let slug = if project.slug.is_empty() {
        slugify(&project.name)
    } else {
        project.slug.clone()
    };
    let mut base = format!("{}/{}", slug, task_id);
    if !title.trim().is_empty() {
        let lower = title.trim().to_ascii_lowercase();
        let mut tslug = String::new();
        for c in lower.chars() {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-' {
                tslug.push(c);
            } else {
                tslug.push('-');
            }
        }
        let tslug = tslug.trim_matches('-');
        let tslug: String = tslug.chars().take(40).collect();
        let tslug = tslug.trim_matches('-');
        if !tslug.is_empty() {
            base = format!("{}-{}", base, tslug);
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> (Connection, PathBuf) {
        let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("ulnclaw-projects-db-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("projects.db");
        let conn = connect(Some(&path)).unwrap();
        (conn, dir)
    }

    #[test]
    fn slugify_and_normalize() {
        assert_eq!(slugify("Aurora Demo"), "aurora-demo");
        assert_eq!(slugify("  Weird!!  Name__  "), "weird-name");
        assert_eq!(slugify("!!!"), "project");
        assert_eq!(
            normalize_slug(Some("  My-Slug ")).unwrap(),
            Some("my-slug".to_string())
        );
        assert_eq!(normalize_slug(Some("")).unwrap(), None);
        assert_eq!(normalize_slug(None).unwrap(), None);
        assert!(normalize_slug(Some("-bad")).is_err());
        assert!(normalize_slug(Some("_bad")).is_err());
        assert!(normalize_slug(Some("has space")).is_err());
        assert!(normalize_slug(Some(&"x".repeat(65))).is_err());
    }

    #[test]
    fn path_normalization() {
        let abs = normalize_path("/tmp/foo//bar/");
        assert_eq!(abs, "/tmp/foo/bar");
        let up = normalize_path("/tmp/foo/../baz");
        assert_eq!(up, "/tmp/baz");
        assert_eq!(normalize_path("   "), "");
    }

    #[test]
    fn create_list_get_and_slug_collision() {
        let (conn, dir) = temp_db();
        let pid = create_project(
            &conn,
            &CreateProject {
                name: "Aurora Demo",
                folders: &["/srv/aurora"],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(pid.starts_with("p_"));

        // Same name again -> unique slug with -2 suffix.
        let pid2 = create_project(
            &conn,
            &CreateProject {
                name: "Aurora Demo",
                ..Default::default()
            },
        )
        .unwrap();
        let p2 = get_project(&conn, &pid2).unwrap().unwrap();
        assert_eq!(p2.slug, "aurora-demo-2");

        let listed = list_projects(&conn, false).unwrap();
        assert_eq!(listed.len(), 2);

        // Lookup by id, slug, and slug case-insensitively.
        assert!(get_project(&conn, &pid).unwrap().is_some());
        assert!(get_project(&conn, "aurora-demo").unwrap().is_some());
        assert!(get_project(&conn, "AURORA-DEMO").unwrap().is_some());
        assert!(get_project(&conn, "nope").unwrap().is_none());

        // First folder became primary implicitly.
        let p1 = get_project(&conn, &pid).unwrap().unwrap();
        assert_eq!(p1.primary_path.as_deref(), Some("/srv/aurora"));
        assert!(p1.folders[0].is_primary);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_requires_name() {
        let (conn, dir) = temp_db();
        let err = create_project(
            &conn,
            &CreateProject {
                name: "  ",
                ..Default::default()
            },
        );
        assert!(err.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folder_lifecycle_and_primary_repoint() {
        let (conn, dir) = temp_db();
        let pid = create_project(
            &conn,
            &CreateProject {
                name: "Multi",
                ..Default::default()
            },
        )
        .unwrap();
        let a = add_folder(&conn, &pid, "/srv/a", None, false).unwrap();
        assert_eq!(a, "/srv/a");
        // First folder of an empty project becomes primary implicitly.
        let proj = get_project(&conn, &pid).unwrap().unwrap();
        assert_eq!(proj.primary_path.as_deref(), Some("/srv/a"));

        add_folder(&conn, &pid, "/srv/b", Some("backend"), true).unwrap();
        let proj = get_project(&conn, &pid).unwrap().unwrap();
        assert_eq!(proj.primary_path.as_deref(), Some("/srv/b"));
        let b = proj.folders.iter().find(|f| f.path == "/srv/b").unwrap();
        assert!(b.is_primary);
        assert_eq!(b.label.as_deref(), Some("backend"));

        // Removing the primary repoints to the remaining folder.
        assert!(remove_folder(&conn, &pid, "/srv/b").unwrap());
        let proj = get_project(&conn, &pid).unwrap().unwrap();
        assert_eq!(proj.primary_path.as_deref(), Some("/srv/a"));

        // Removing the last folder clears primary.
        assert!(remove_folder(&conn, &pid, "/srv/a").unwrap());
        let proj = get_project(&conn, &pid).unwrap().unwrap();
        assert!(proj.primary_path.is_none());

        // set_primary on a missing folder is a no-op.
        assert!(!set_primary(&conn, &pid, "/srv/ghost").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn update_patch_semantics() {
        let (conn, dir) = temp_db();
        let pid = create_project(
            &conn,
            &CreateProject {
                name: "Patchy",
                icon: Some("rocket"),
                color: Some("#ff0000"),
                ..Default::default()
            },
        )
        .unwrap();

        // No-op patch returns false.
        assert!(!update_project(&conn, &pid, &UpdateProject::default()).unwrap());

        update_project(
            &conn,
            &pid,
            &UpdateProject {
                name: Some("Patchy 2"),
                icon: Some(""), // clears
                ..Default::default()
            },
        )
        .unwrap();
        let proj = get_project(&conn, &pid).unwrap().unwrap();
        assert_eq!(proj.name, "Patchy 2");
        assert!(proj.icon.is_none());
        assert_eq!(proj.color.as_deref(), Some("#ff0000"));

        // Empty name is rejected.
        assert!(update_project(
            &conn,
            &pid,
            &UpdateProject {
                name: Some(" "),
                ..Default::default()
            }
        )
        .is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn archive_restore_delete() {
        let (conn, dir) = temp_db();
        let pid = create_project(
            &conn,
            &CreateProject {
                name: "Ephemeral",
                ..Default::default()
            },
        )
        .unwrap();
        assert!(archive_project(&conn, &pid).unwrap());
        assert_eq!(list_projects(&conn, false).unwrap().len(), 0);
        assert_eq!(list_projects(&conn, true).unwrap().len(), 1);
        assert!(restore_project(&conn, &pid).unwrap());
        assert_eq!(list_projects(&conn, false).unwrap().len(), 1);
        assert!(delete_project(&conn, &pid).unwrap());
        assert!(get_project(&conn, &pid).unwrap().is_none());
        // Cascade removed folders too.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_folders", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn active_pointer() {
        let (conn, dir) = temp_db();
        assert!(get_active_id(&conn).unwrap().is_none());
        let pid = create_project(
            &conn,
            &CreateProject {
                name: "Active",
                ..Default::default()
            },
        )
        .unwrap();
        set_active(&conn, Some(&pid)).unwrap();
        assert_eq!(get_active_id(&conn).unwrap(), Some(pid.clone()));
        set_active(&conn, None).unwrap();
        assert!(get_active_id(&conn).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn project_for_path_longest_prefix() {
        let (conn, dir) = temp_db();
        let outer = create_project(
            &conn,
            &CreateProject {
                name: "Outer",
                folders: &["/work"],
                ..Default::default()
            },
        )
        .unwrap();
        let inner = create_project(
            &conn,
            &CreateProject {
                name: "Inner",
                folders: &["/work/inner"],
                ..Default::default()
            },
        )
        .unwrap();

        let hit = project_for_path(&conn, "/work/inner/src/main.rs", false)
            .unwrap()
            .unwrap();
        assert_eq!(hit.id, inner);
        let hit = project_for_path(&conn, "/work/other/file.txt", false)
            .unwrap()
            .unwrap();
        assert_eq!(hit.id, outer);
        assert!(project_for_path(&conn, "/elsewhere", false)
            .unwrap()
            .is_none());
        assert!(project_for_path(&conn, "  ", false).unwrap().is_none());
        // Exact folder match counts as owned.
        let hit = project_for_path(&conn, "/work", false).unwrap().unwrap();
        assert_eq!(hit.id, outer);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discovered_repos_and_policy() {
        let (conn, dir) = temp_db();
        let n = record_discovered_repos(
            &conn,
            &[
                ("/srv/one/".to_string(), None),
                ("/srv/two".to_string(), Some("Two".to_string())),
            ],
            false,
            None,
        )
        .unwrap();
        assert_eq!(n, 2);
        let rows = list_discovered_repos(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["label"], "one"); // basename fallback

        // replace=true wipes stale rows first.
        let n = record_discovered_repos(
            &conn,
            &[("/srv/three".to_string(), None)],
            true,
            Some("git-only"),
        )
        .unwrap();
        assert_eq!(n, 1);
        let rows = list_discovered_repos(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["root"], "/srv/three");
        assert_eq!(
            get_discovery_policy_key(&conn).unwrap(),
            Some("git-only".to_string())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discovered_repos_policy_matrix() {
        // Preserve-unversioned path: no prior key + preserve -> keep rows.
        let (conn, dir) = temp_db();
        record_discovered_repos(&conn, &[("/srv/one".to_string(), None)], false, None).unwrap();
        let cleared = reconcile_discovered_repos_policy(&conn, "git-only", true).unwrap();
        assert!(!cleared);
        assert_eq!(list_discovered_repos(&conn).unwrap().len(), 1);
        // Same key again -> no-op.
        assert!(!reconcile_discovered_repos_policy(&conn, "git-only", true).unwrap());
        // Key change -> clear.
        record_discovered_repos(&conn, &[("/srv/two".to_string(), None)], false, None).unwrap();
        assert!(reconcile_discovered_repos_policy(&conn, "all-dirs", false).unwrap());
        assert_eq!(list_discovered_repos(&conn).unwrap().len(), 0);
        assert_eq!(
            get_discovery_policy_key(&conn).unwrap(),
            Some("all-dirs".to_string())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn projects_for_paths_longest_prefix_and_archived() {
        let (conn, _tmp) = temp_db();
        let outer = create_project(
            &conn,
            &CreateProject {
                name: "Outer",
                slug: Some("outer"),
                folders: &["/work/code"],
                primary_path: None,
                description: None,
                icon: None,
                color: None,
                board_slug: None,
            },
        )
        .unwrap();
        let inner = create_project(
            &conn,
            &CreateProject {
                name: "Inner",
                slug: Some("inner"),
                folders: &["/work/code/nested"],
                primary_path: None,
                description: None,
                icon: None,
                color: None,
                board_slug: None,
            },
        )
        .unwrap();
        let archived = create_project(
            &conn,
            &CreateProject {
                name: "Gone",
                slug: Some("gone"),
                folders: &["/work/old"],
                primary_path: None,
                description: None,
                icon: None,
                color: None,
                board_slug: None,
            },
        )
        .unwrap();
        archive_project(&conn, &archived).unwrap();

        let paths: Vec<String> = [
            "/work/code",          // exact outer folder
            "/work/code/src/main", // nested under outer
            "/work/code/nested/x", // inner wins (longest prefix)
            "/work/code2",         // sibling — no match
            "/work/old/stuff",     // archived — no match
            "",                    // empty — skipped
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let resolved = projects_for_paths(&conn, &paths).unwrap();
        assert_eq!(
            resolved.get("/work/code").map(|p| p.id.as_str()),
            Some(outer.as_str())
        );
        assert_eq!(
            resolved.get("/work/code/src/main").map(|p| p.id.as_str()),
            Some(outer.as_str())
        );
        assert_eq!(
            resolved.get("/work/code/nested/x").map(|p| p.id.as_str()),
            Some(inner.as_str())
        );
        assert!(!resolved.contains_key("/work/code2"));
        assert!(!resolved.contains_key("/work/old/stuff"));
        assert_eq!(resolved.len(), 3);
    }

    #[test]
    fn branch_name_shape() {
        let project = Project {
            id: "p_x".into(),
            slug: "aurora-demo".into(),
            name: "Aurora Demo".into(),
            description: None,
            icon: None,
            color: None,
            board_slug: None,
            primary_path: None,
            created_at: 0,
            archived: false,
            folders: vec![],
        };
        assert_eq!(branch_name_for(&project, "t-123", ""), "aurora-demo/t-123");
        assert_eq!(
            branch_name_for(&project, "t-123", "Fix The Login!"),
            "aurora-demo/t-123-fix-the-login"
        );
    }
}

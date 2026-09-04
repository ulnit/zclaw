//! Cron — port of hermes' cron/ package (job store + schedule parsing +
//! scheduler loop).
//!
//! Jobs live in the state DB (`cron_jobs` table). Schedules support:
//!   - interval shorthands: "30m", "every 2h", "1d"
//!   - 5-field cron expressions: "0 9 * * *"
//!   - ISO timestamps for one-shot runs: "2026-06-01T09:00:00"

pub mod blueprints;
pub mod chronos;
pub mod delivery;
pub mod suggestions;

use crate::error::{AgentError, Result};
use chrono::{DateTime, Duration, Local, NaiveDateTime};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// Where a job was created (platform chat), used by `deliver = "origin"`
/// (hermes job `origin` dict).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobOrigin {
    pub platform: String,
    pub chat_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub schedule: String,
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<String>,
    pub enabled: bool,
    /// Remaining runs (None = forever).
    pub repeat: Option<i64>,
    pub next_run: Option<f64>,
    pub created_at: f64,
    pub last_run: Option<f64>,
    pub last_status: Option<String>,
    /// Where the final response is auto-delivered: `"local"` (default),
    /// `"origin"`, a platform name, `"platform:chat[:thread]"`, or a
    /// comma-separated mix incl. the `all` routing token (hermes
    /// `deliver`).
    #[serde(default)]
    pub deliver: Option<String>,
    /// The chat the job was created in (drives `deliver = "origin"`).
    #[serde(default)]
    pub origin: Option<JobOrigin>,
    /// Last delivery failure, tracked separately from the agent error —
    /// a job can succeed but fail delivery (hermes
    /// `last_delivery_error`).
    #[serde(default)]
    pub last_delivery_error: Option<String>,
    /// Per-job delivery-mirror gate override (hermes job
    /// `attach_to_session`): when Some, wins over the global
    /// `cron.mirror_delivery` setting; when None the global applies.
    /// Mirroring appends each successful origin-chat delivery to that
    /// chat's session transcript so replies see the cron output in
    /// context.
    #[serde(default)]
    pub attach_to_session: Option<bool>,
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Schedule parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    /// Repeat every N seconds.
    Interval(i64),
    /// One-shot at a fixed unix time.
    OneShot(f64),
    /// 5-field cron expression (minute hour day month weekday).
    Cron(CronExpr),
}

/// Minimal 5-field cron expression (supports *, lists, ranges, steps).
#[derive(Debug, Clone, PartialEq)]
pub struct CronExpr {
    pub minutes: Vec<u32>,
    pub hours: Vec<u32>,
    pub days: Vec<u32>,
    pub months: Vec<u32>,
    pub weekdays: Vec<u32>,
}

fn parse_field(field: &str, min: u32, max: u32) -> std::result::Result<Vec<u32>, String> {
    let mut values = Vec::new();
    for part in field.split(',') {
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| format!("bad step: {}", s))?),
            None => (part, 1),
        };
        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            (
                a.parse::<u32>()
                    .map_err(|_| format!("bad range: {}", part))?,
                b.parse::<u32>()
                    .map_err(|_| format!("bad range: {}", part))?,
            )
        } else {
            let v = range_part
                .parse::<u32>()
                .map_err(|_| format!("bad value: {}", part))?;
            (v, v)
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("value out of range: {}", part));
        }
        let mut v = lo;
        while v <= hi {
            values.push(v);
            v += step.max(1);
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

impl CronExpr {
    pub fn parse(expr: &str) -> std::result::Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron expression needs 5 fields, got {}",
                fields.len()
            ));
        }
        Ok(Self {
            minutes: parse_field(fields[0], 0, 59)?,
            hours: parse_field(fields[1], 0, 23)?,
            days: parse_field(fields[2], 1, 31)?,
            months: parse_field(fields[3], 1, 12)?,
            weekdays: parse_field(fields[4], 0, 6)?,
        })
    }

    /// Next run strictly after `from` (local time).
    pub fn next_after(&self, from: DateTime<Local>) -> Option<DateTime<Local>> {
        use chrono::Timelike;
        let mut candidate = from.checked_add_signed(Duration::minutes(1))?;
        // Zero out seconds.
        candidate = candidate
            .date_naive()
            .and_hms_opt(candidate.hour(), candidate.minute(), 0)?
            .and_local_timezone(Local)
            .single()?;
        // Walk forward up to 2 years.
        for _ in 0..(366 * 2 * 24 * 60) {
            if self.matches(candidate) {
                return Some(candidate);
            }
            candidate = candidate.checked_add_signed(Duration::minutes(1))?;
        }
        None
    }

    fn matches(&self, time: DateTime<Local>) -> bool {
        use chrono::Datelike;
        use chrono::Timelike;
        let weekday = time.weekday().num_days_from_sunday();
        self.minutes.contains(&time.minute())
            && self.hours.contains(&time.hour())
            && self.days.contains(&time.day())
            && self.months.contains(&time.month())
            && self.weekdays.contains(&weekday)
    }
}

/// Parse hermes-style schedule strings.
pub fn parse_schedule(raw: &str) -> Result<Schedule> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AgentError::config("empty schedule"));
    }

    // Hermes-style @-aliases: `@every 30m`, `@at <unix-ts>`, and the
    // classic cron macros (`@hourly`, `@daily`, `@weekly`, `@monthly`,
    // `@yearly`/`@annually`).
    if let Some(rest) = raw.strip_prefix('@') {
        let rest = rest.trim();
        if let Some(interval) = rest.strip_prefix("every") {
            return parse_schedule(interval.trim());
        }
        if let Some(at) = rest.strip_prefix("at") {
            let at = at.trim();
            let ts: f64 = at
                .parse()
                .map_err(|_| AgentError::config(format!("bad @at timestamp '{}'", at)))?;
            return Ok(Schedule::OneShot(ts));
        }
        let cron = match rest {
            "hourly" => "0 * * * *",
            "daily" | "midnight" => "0 0 * * *",
            "weekly" => "0 0 * * 0",
            "monthly" => "0 0 1 * *",
            "yearly" | "annually" => "0 0 1 1 *",
            other => {
                return Err(AgentError::config(format!(
                    "unknown @schedule '{}' (use @every 30m, @at <unix-ts>, or a cron expression)",
                    other
                )))
            }
        };
        return CronExpr::parse(cron)
            .map(Schedule::Cron)
            .map_err(|e| AgentError::config(format!("schedule '{}': {}", raw, e)));
    }

    // ISO timestamp one-shot (starts with YYYY-...).
    if raw.len() >= 10
        && raw.as_bytes()[4] == b'-'
        && raw.chars().take(4).all(|c| c.is_ascii_digit())
    {
        let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S"))
            .or_else(|_| {
                NaiveDateTime::parse_from_str(&format!("{} 00:00:00", raw), "%Y-%m-%d %H:%M:%S")
            })
            .map_err(|e| AgentError::config(format!("bad ISO timestamp '{}': {}", raw, e)))?;
        let local = naive
            .and_local_timezone(Local)
            .single()
            .ok_or_else(|| AgentError::config(format!("ambiguous local time: {}", raw)))?;
        return Ok(Schedule::OneShot(local.timestamp() as f64));
    }

    // Interval shorthand: optional "every " prefix + number + unit, no spaces.
    let body = raw.strip_prefix("every ").unwrap_or(raw);
    if !body.contains(char::is_whitespace) {
        let split = body
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(body.len());
        let (number, unit_raw) = body.split_at(split);
        let unit = unit_raw.trim().to_lowercase();
        if !number.is_empty() {
            let n: i64 = number
                .parse()
                .map_err(|_| AgentError::config(format!("bad interval: {}", raw)))?;
            let seconds = match unit.as_str() {
                "s" | "sec" | "secs" | "second" | "seconds" => n,
                "m" | "min" | "mins" | "minute" | "minutes" => n * 60,
                "h" | "hr" | "hrs" | "hour" | "hours" => n * 3600,
                "d" | "day" | "days" => n * 86400,
                "w" | "week" | "weeks" => n * 604800,
                "" => {
                    return Err(AgentError::config(format!(
                        "schedule '{}' needs a unit (s/m/h/d/w) or a cron expression",
                        raw
                    )));
                }
                other => {
                    return Err(AgentError::config(format!(
                        "unknown interval unit '{}' (use s/m/h/d/w or a cron expression)",
                        other
                    )));
                }
            };
            if seconds < 60 {
                return Err(AgentError::config("minimum interval is 60 seconds"));
            }
            return Ok(Schedule::Interval(seconds));
        }
    }

    // 5-field cron expression.
    CronExpr::parse(raw)
        .map(Schedule::Cron)
        .map_err(|e| AgentError::config(format!("schedule '{}': {}", raw, e)))
}

/// Compute the next run time (unix seconds) for a schedule, strictly after now.
pub fn next_run(schedule: &Schedule) -> Option<f64> {
    match schedule {
        Schedule::Interval(seconds) => Some(now() + *seconds as f64),
        Schedule::OneShot(at) => Some(*at),
        Schedule::Cron(expr) => expr.next_after(Local::now()).map(|t| t.timestamp() as f64),
    }
}

// ---------------------------------------------------------------------------
// Job store (SQLite)
// ---------------------------------------------------------------------------

const CRON_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cron_jobs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    schedule TEXT NOT NULL,
    prompt TEXT NOT NULL,
    skills TEXT NOT NULL DEFAULT '[]',
    enabled INTEGER NOT NULL DEFAULT 1,
    repeat INTEGER,
    next_run REAL,
    created_at REAL NOT NULL,
    last_run REAL,
    last_status TEXT,
    deliver TEXT,
    origin TEXT,
    last_delivery_error TEXT,
    attach_to_session INTEGER
);
"#;

/// Columns added after the initial schema (stores created before P219
/// lack them); migrated in place on open.
const CRON_MIGRATION_COLUMNS: &[&str] = &[
    "deliver TEXT",
    "origin TEXT",
    "last_delivery_error TEXT",
    "attach_to_session INTEGER",
];

fn migrate_cron_schema(conn: &Connection) -> Result<()> {
    let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut stmt = conn
            .prepare("PRAGMA table_info(cron_jobs)")
            .map_err(|e| AgentError::session(format!("cron schema: {}", e)))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| AgentError::session(format!("cron schema: {}", e)))?;
        for row in rows {
            if let Ok(name) = row {
                existing.insert(name);
            }
        }
    }
    for column in CRON_MIGRATION_COLUMNS {
        let name = column.split_whitespace().next().unwrap_or_default();
        if !existing.contains(name) {
            conn.execute(&format!("ALTER TABLE cron_jobs ADD COLUMN {}", column), [])
                .map_err(|e| AgentError::session(format!("cron migrate: {}", e)))?;
        }
    }
    Ok(())
}

pub struct CronStore {
    conn: Mutex<Connection>,
}

impl CronStore {
    /// Open the cron store inside an existing state DB file.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| AgentError::session(format!("open cron db: {}", e)))?;
        conn.execute_batch(CRON_SCHEMA)
            .map_err(|e| AgentError::session(format!("cron schema: {}", e)))?;
        migrate_cron_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_default() -> Result<Self> {
        let home = crate::config::ensure_home()?;
        Self::open(&home.join("state.db"))
    }

    pub fn add(&self, job: &CronJob) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT INTO cron_jobs (id, name, schedule, prompt, skills, enabled, repeat, next_run, created_at, deliver, origin, attach_to_session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                job.id,
                job.name,
                job.schedule,
                job.prompt,
                serde_json::to_string(&job.skills).unwrap_or_else(|_| "[]".into()),
                job.enabled as i32,
                job.repeat,
                job.next_run,
                job.created_at,
                job.deliver,
                job.origin.as_ref().and_then(|origin| serde_json::to_string(origin).ok()),
                job.attach_to_session.map(|flag| flag as i32),
            ],
        )
        .map_err(|e| AgentError::session(format!("add job: {}", e)))?;
        Ok(())
    }

    pub fn update(&self, job: &CronJob) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE cron_jobs SET name=?2, schedule=?3, prompt=?4, skills=?5, enabled=?6,
                repeat=?7, next_run=?8, last_run=?9, last_status=?10, deliver=?11, origin=?12,
                last_delivery_error=?13, attach_to_session=?14
             WHERE id=?1",
            params![
                job.id,
                job.name,
                job.schedule,
                job.prompt,
                serde_json::to_string(&job.skills).unwrap_or_else(|_| "[]".into()),
                job.enabled as i32,
                job.repeat,
                job.next_run,
                job.last_run,
                job.last_status,
                job.deliver,
                job.origin
                    .as_ref()
                    .and_then(|origin| serde_json::to_string(origin).ok()),
                job.last_delivery_error,
                job.attach_to_session.map(|flag| flag as i32),
            ],
        )
        .map_err(|e| AgentError::session(format!("update job: {}", e)))?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<CronJob>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let row = conn
            .query_row(
                "SELECT id, name, schedule, prompt, skills, enabled, repeat, next_run, created_at, last_run, last_status,
                    deliver, origin, last_delivery_error, attach_to_session
                 FROM cron_jobs WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i32>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<f64>>(7)?,
                        row.get::<_, f64>(8)?,
                        row.get::<_, Option<f64>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<String>>(13)?,
                        row.get::<_, Option<i32>>(14)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(row.map(
            |(
                id,
                name,
                schedule,
                prompt,
                skills,
                enabled,
                repeat,
                next_run,
                created_at,
                last_run,
                last_status,
                deliver,
                origin,
                last_delivery_error,
                attach_to_session,
            )| {
                CronJob {
                    id,
                    name,
                    schedule,
                    prompt,
                    skills: serde_json::from_str(&skills).unwrap_or_default(),
                    enabled: enabled != 0,
                    repeat,
                    next_run,
                    created_at,
                    last_run,
                    last_status,
                    deliver,
                    origin: origin.and_then(|raw| serde_json::from_str(&raw).ok()),
                    last_delivery_error,
                    attach_to_session: attach_to_session.map(|flag| flag != 0),
                }
            },
        ))
    }

    pub fn list(&self) -> Result<Vec<CronJob>> {
        let mut ids: Vec<String> = Vec::new();
        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| AgentError::session(e.to_string()))?;
            let mut stmt = conn
                .prepare("SELECT id FROM cron_jobs ORDER BY created_at")
                .map_err(|e| AgentError::session(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| AgentError::session(e.to_string()))?;
            for row in rows {
                if let Ok(id) = row {
                    ids.push(id);
                }
            }
        }
        let mut jobs = Vec::new();
        for id in ids {
            if let Some(job) = self.get(&id)? {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let n = conn
            .execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])
            .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(n > 0)
    }

    /// Jobs due to run (enabled + next_run <= now).
    pub fn due_jobs(&self) -> Result<Vec<CronJob>> {
        let now = now();
        Ok(self
            .list()?
            .into_iter()
            .filter(|job| job.enabled && job.next_run.map(|t| t <= now).unwrap_or(false))
            .collect())
    }
}

/// The scheduler loop — call from a tokio task. Checks for due jobs every
/// `poll_secs` and executes them via the provided runner.
pub async fn run_scheduler<F, Fut>(store: std::sync::Arc<CronStore>, poll_secs: u64, mut runner: F)
where
    F: FnMut(CronJob) -> Fut + Send,
    Fut: std::future::Future<Output = Result<String>> + Send,
{
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs.max(5)));
    loop {
        interval.tick().await;
        let due = match store.due_jobs() {
            Ok(due) => due,
            Err(_) => continue,
        };
        for mut job in due {
            let result = runner(job.clone()).await;
            job.last_run = Some(now());
            job.last_status = Some(match &result {
                // The runner reports its own status text (e.g. "running
                // (run <id>)" when dispatched as a tracked gateway run);
                // the final outcome is recorded by whoever owns the run.
                Ok(status) => status.clone(),
                Err(e) => format!("error: {}", e),
            });
            // Reschedule or disable.
            match parse_schedule(&job.schedule) {
                Ok(schedule) => {
                    if let Schedule::OneShot(_) = schedule {
                        job.enabled = false;
                        job.next_run = None;
                    } else if let Some(repeat) = job.repeat {
                        if repeat <= 1 {
                            job.enabled = false;
                            job.next_run = None;
                        } else {
                            job.repeat = Some(repeat - 1);
                            job.next_run = next_run(&schedule);
                        }
                    } else {
                        job.next_run = next_run(&schedule);
                    }
                }
                Err(e) => {
                    job.enabled = false;
                    job.last_status = Some(format!("reschedule failed: {}", e));
                }
            }
            store.update(&job).ok();
        }
    }
}

// ---------------------------------------------------------------------------
// `/cron` slash command (P663 — hermes `/cron` parity)
// ---------------------------------------------------------------------------

/// Outcome of one `/cron` slash invocation (P663): the formatted reply
/// plus an optional agent-turn seed — `/cron run <id>` hands the job's
/// prompt back so the REPL/gateway can execute it as the next turn.
#[derive(Debug, Default)]
pub struct CronSlashResult {
    pub text: String,
    /// Present when the command resolved to "run this job now".
    pub run_prompt: Option<String>,
}

fn slash_ok(text: String) -> CronSlashResult {
    CronSlashResult {
        text,
        run_prompt: None,
    }
}

/// Humanize a duration in seconds (`5s`, `3m`, `2h`, `4d`).
fn human_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}

/// Humanize a unix timestamp relative to now (`in 5m` / `3m ago` / `due now`).
fn human_ts(ts: f64) -> String {
    let delta = ts - now();
    if delta.abs() < 30.0 {
        "due now".to_string()
    } else if delta > 0.0 {
        format!("in {}", human_duration(delta as i64))
    } else {
        format!("{} ago", human_duration((-delta) as i64))
    }
}

/// Resolve a `/cron` argument to a job: exact id, unique id prefix, or
/// name (case-insensitive).
fn resolve_job<'a>(jobs: &'a [CronJob], needle: &str) -> Option<&'a CronJob> {
    if let Some(job) = jobs.iter().find(|j| j.id == needle) {
        return Some(job);
    }
    let mut prefix_matches = jobs.iter().filter(|j| j.id.starts_with(needle));
    let first = prefix_matches.next();
    if let Some(job) = first {
        if prefix_matches.next().is_none() {
            return Some(job);
        }
    }
    let lowered = needle.to_lowercase();
    if let Some(job) = jobs.iter().find(|j| j.name.to_lowercase() == lowered) {
        return Some(job);
    }
    None
}

const CRON_SLASH_USAGE: &str =
    "(o_o) usage: /cron [list|show <id>|pause <id>|resume <id>|run <id>|remove <id>|status]\n";

/// Shared `/cron` dispatch for REPL and gateway (P663): one formatted
/// string back (plus an optional run seed), no process spawn. The store
/// lives at `<home>/state.db`, matching `ulnclaw cron` and the gateway
/// scheduler.
pub fn run_slash(home: &std::path::Path, rest: &str) -> CronSlashResult {
    let mut parts = rest.split_whitespace();
    let sub = parts.next().unwrap_or("list");
    let store = match CronStore::open(&home.join("state.db")) {
        Ok(s) => s,
        Err(e) => return slash_ok(format!("(._.) cron error: {e}\n")),
    };
    match sub {
        "list" | "ls" => {
            let jobs = match store.list() {
                Ok(jobs) => jobs,
                Err(e) => return slash_ok(format!("(._.) cron error: {e}\n")),
            };
            if jobs.is_empty() {
                return slash_ok(
                    "(o_o) no cron jobs. Create one with /blueprint or `ulnclaw cron create`.\n"
                        .to_string(),
                );
            }
            let mut out = String::new();
            for job in &jobs {
                let state = if job.enabled { "active" } else { "paused" };
                out.push_str(&format!(
                    "{}  [{:<6}]  {}  ({})\n",
                    job.id, state, job.name, job.schedule
                ));
                let mut detail = String::new();
                if job.enabled {
                    if let Some(next) = job.next_run {
                        detail.push_str(&format!("next {}", human_ts(next)));
                    }
                }
                if let Some(repeat) = job.repeat {
                    if !detail.is_empty() {
                        detail.push_str("  ");
                    }
                    detail.push_str(&format!("{repeat} run(s) left"));
                }
                if let Some(status) = &job.last_status {
                    if !detail.is_empty() {
                        detail.push_str("  ");
                    }
                    let when = job
                        .last_run
                        .map(|ts| format!(" ({})", human_ts(ts)))
                        .unwrap_or_default();
                    detail.push_str(&format!("last: {status}{when}"));
                }
                if !detail.is_empty() {
                    out.push_str(&format!("      {detail}\n"));
                }
            }
            slash_ok(out)
        }
        "show" => {
            let needle = parts.collect::<Vec<_>>().join(" ");
            if needle.is_empty() {
                return slash_ok("(o_o) usage: /cron show <id>\n".to_string());
            }
            let jobs = match store.list() {
                Ok(jobs) => jobs,
                Err(e) => return slash_ok(format!("(._.) cron error: {e}\n")),
            };
            let Some(job) = resolve_job(&jobs, &needle) else {
                return slash_ok(format!("(._.) cron job '{needle}' not found\n"));
            };
            let mut out = String::new();
            out.push_str(&format!("id:       {}\n", job.id));
            out.push_str(&format!("name:     {}\n", job.name));
            out.push_str(&format!("schedule: {}\n", job.schedule));
            out.push_str(&format!("enabled:  {}\n", job.enabled));
            if let Some(repeat) = job.repeat {
                out.push_str(&format!("repeat:   {repeat} run(s) remaining\n"));
            }
            if let Some(deliver) = &job.deliver {
                out.push_str(&format!("deliver:  {deliver}\n"));
            }
            if !job.skills.is_empty() {
                out.push_str(&format!("skills:   {}\n", job.skills.join(", ")));
            }
            if let Some(next) = job.next_run {
                out.push_str(&format!("next:     {}\n", human_ts(next)));
            }
            if let Some(last) = job.last_run {
                out.push_str(&format!("last:     {}\n", human_ts(last)));
            }
            if let Some(status) = &job.last_status {
                out.push_str(&format!("status:   {status}\n"));
            }
            if let Some(err) = &job.last_delivery_error {
                out.push_str(&format!("deliver-error: {err}\n"));
            }
            out.push_str("prompt:\n");
            out.push_str(&job.prompt);
            out.push('\n');
            slash_ok(out)
        }
        "pause" | "resume" => {
            let needle = parts.collect::<Vec<_>>().join(" ");
            if needle.is_empty() {
                return slash_ok(format!("(o_o) usage: /cron {sub} <id>\n"));
            }
            let jobs = match store.list() {
                Ok(jobs) => jobs,
                Err(e) => return slash_ok(format!("(._.) cron error: {e}\n")),
            };
            let Some(job) = resolve_job(&jobs, &needle) else {
                return slash_ok(format!("(._.) cron job '{needle}' not found\n"));
            };
            let mut updated = job.clone();
            if sub == "pause" {
                updated.enabled = false;
            } else {
                updated.enabled = true;
                if let Ok(schedule) = parse_schedule(&updated.schedule) {
                    updated.next_run = next_run(&schedule);
                }
            }
            match store.update(&updated) {
                Ok(()) => slash_ok(format!(
                    "{} {} ({})\n",
                    if sub == "pause" { "⏸" } else { "▶" },
                    updated.id,
                    if sub == "pause" { "paused" } else { "resumed" }
                )),
                Err(e) => slash_ok(format!("(._.) cron error: {e}\n")),
            }
        }
        "remove" | "rm" | "delete" => {
            let needle = parts.collect::<Vec<_>>().join(" ");
            if needle.is_empty() {
                return slash_ok("(o_o) usage: /cron remove <id>\n".to_string());
            }
            let jobs = match store.list() {
                Ok(jobs) => jobs,
                Err(e) => return slash_ok(format!("(._.) cron error: {e}\n")),
            };
            let Some(job) = resolve_job(&jobs, &needle) else {
                return slash_ok(format!("(._.) cron job '{needle}' not found\n"));
            };
            match store.remove(&job.id) {
                Ok(true) => slash_ok(format!("🗑 removed {}\n", job.id)),
                Ok(false) => slash_ok(format!("(._.) cron job '{}' not found\n", job.id)),
                Err(e) => slash_ok(format!("(._.) cron error: {e}\n")),
            }
        }
        "run" => {
            // hermes `cron run`: fire the job once, now. The REPL/gateway
            // runs the job's prompt as an agent turn (unattended semantics
            // belong to the scheduler; the slash runs it in-session).
            let needle = parts.collect::<Vec<_>>().join(" ");
            if needle.is_empty() {
                return slash_ok("(o_o) usage: /cron run <id>\n".to_string());
            }
            let jobs = match store.list() {
                Ok(jobs) => jobs,
                Err(e) => return slash_ok(format!("(._.) cron error: {e}\n")),
            };
            let Some(job) = resolve_job(&jobs, &needle) else {
                return slash_ok(format!("(._.) cron job '{needle}' not found\n"));
            };
            if job.prompt.trim().is_empty() {
                return slash_ok(format!(
                    "(._.) cron job '{}' has no prompt to run\n",
                    job.id
                ));
            }
            CronSlashResult {
                text: format!("▶ running cron job {} ({}) now…\n", job.id, job.name),
                run_prompt: Some(job.prompt.clone()),
            }
        }
        "status" => {
            let jobs = match store.list() {
                Ok(jobs) => jobs,
                Err(e) => return slash_ok(format!("(._.) cron error: {e}\n")),
            };
            let active = jobs.iter().filter(|j| j.enabled).count();
            let paused = jobs.len() - active;
            let mut out = String::new();
            out.push_str("cron status:\n");
            out.push_str(&format!(
                "  jobs:      {} active, {} paused\n",
                active, paused
            ));
            let next_fire = jobs
                .iter()
                .filter(|j| j.enabled)
                .filter_map(|j| j.next_run.map(|ts| (ts, j)))
                .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((ts, job)) = next_fire {
                out.push_str(&format!("  next fire: {} ({})\n", human_ts(ts), job.name));
            } else if active > 0 {
                out.push_str("  next fire: (none scheduled)\n");
            }
            let failures = jobs
                .iter()
                .filter(|j| {
                    j.last_status
                        .as_deref()
                        .map(|s| s.starts_with("error"))
                        .unwrap_or(false)
                })
                .count();
            if failures > 0 {
                out.push_str(&format!("  ⚠ {failures} job(s) with a last-run error\n"));
            }
            slash_ok(out)
        }
        _ => slash_ok(CRON_SLASH_USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_intervals() {
        assert_eq!(parse_schedule("30m").unwrap(), Schedule::Interval(1800));
        assert_eq!(
            parse_schedule("every 2h").unwrap(),
            Schedule::Interval(7200)
        );
        assert_eq!(parse_schedule("1d").unwrap(), Schedule::Interval(86400));
        assert!(parse_schedule("5s").is_err()); // below minimum
    }

    #[test]
    fn test_parse_at_aliases() {
        // P661: hermes-style @every / @at / cron macros.
        assert_eq!(
            parse_schedule("@every 30m").unwrap(),
            Schedule::Interval(1800)
        );
        assert_eq!(
            parse_schedule("@every 2h").unwrap(),
            Schedule::Interval(7200)
        );
        assert_eq!(
            parse_schedule("@at 1893456000").unwrap(),
            Schedule::OneShot(1893456000.0)
        );
        match parse_schedule("@daily").unwrap() {
            Schedule::Cron(e) => {
                assert_eq!(e.minutes, vec![0]);
                assert_eq!(e.hours, vec![0]);
            }
            other => panic!("expected cron, got {:?}", other),
        }
        assert!(matches!(
            parse_schedule("@hourly").unwrap(),
            Schedule::Cron(_)
        ));
        assert!(parse_schedule("@fortnightly").is_err());
        assert!(parse_schedule("@at not-a-number").is_err());
    }

    #[test]
    fn test_parse_cron_expr() {
        use chrono::Timelike;
        let expr = match parse_schedule("0 9 * * *").unwrap() {
            Schedule::Cron(e) => e,
            other => panic!("expected cron, got {:?}", other),
        };
        assert_eq!(expr.minutes, vec![0]);
        assert_eq!(expr.hours, vec![9]);
        let next = expr.next_after(Local::now()).unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn test_parse_oneshot() {
        match parse_schedule("2030-06-01T09:00:00").unwrap() {
            Schedule::OneShot(ts) => assert!(ts > now()),
            other => panic!("expected one-shot, got {:?}", other),
        }
    }

    #[test]
    fn test_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CronStore::open(&dir.path().join("state.db")).unwrap();
        let job = CronJob {
            id: "job-1".into(),
            name: "daily".into(),
            schedule: "0 9 * * *".into(),
            prompt: "Say good morning".into(),
            skills: vec![],
            enabled: true,
            repeat: None,
            next_run: Some(now() + 3600.0),
            created_at: now(),
            last_run: None,
            last_status: None,
            deliver: Some("origin".into()),
            origin: Some(JobOrigin {
                platform: "telegram".into(),
                chat_id: "-100123".into(),
                thread_id: Some("7".into()),
            }),
            last_delivery_error: None,
            attach_to_session: Some(true),
        };
        store.add(&job).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        let loaded = store.get("job-1").unwrap().unwrap();
        assert_eq!(loaded.deliver.as_deref(), Some("origin"));
        assert_eq!(loaded.origin.as_ref().unwrap().chat_id, "-100123");
        assert_eq!(loaded.attach_to_session, Some(true));
        // Update flips the override back to unset (NULL round-trip).
        let mut updated = loaded.clone();
        updated.attach_to_session = None;
        store.update(&updated).unwrap();
        assert_eq!(store.get("job-1").unwrap().unwrap().attach_to_session, None);
        assert!(store.remove("job-1").unwrap());
        assert_eq!(store.list().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_scheduler_dispatches_due_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(CronStore::open(&dir.path().join("state.db")).unwrap());
        let job = CronJob {
            id: "job-due".into(),
            name: "due".into(),
            schedule: "60s".into(),
            prompt: "tick".into(),
            skills: vec![],
            enabled: true,
            repeat: None,
            next_run: Some(now() - 5.0), // already due
            created_at: now(),
            last_run: None,
            last_status: None,
            deliver: None,
            origin: None,
            last_delivery_error: None,
            attach_to_session: None,
        };
        store.add(&job).unwrap();

        let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
        let seen_in = seen.clone();
        let handle = tokio::spawn(run_scheduler(store.clone(), 5, move |job| {
            let seen = seen_in.clone();
            async move {
                seen.lock().await.push(job.id.clone());
                Ok(format!("dispatched {}", job.id))
            }
        }));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        handle.abort();

        assert_eq!(*seen.lock().await, vec!["job-due".to_string()]);
        let updated = store.get("job-due").unwrap().unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("dispatched job-due"));
        assert!(updated.last_run.is_some());
        // Rescheduled into the future, still enabled.
        assert!(updated.next_run.unwrap() > now());
        assert!(updated.enabled);
    }

    // P663: /cron slash coverage.
    fn slash_job(id: &str, name: &str, schedule: &str) -> CronJob {
        CronJob {
            id: id.to_string(),
            name: name.to_string(),
            schedule: schedule.to_string(),
            prompt: format!("prompt for {name}"),
            skills: vec![],
            enabled: true,
            repeat: None,
            next_run: Some(now() + 300.0),
            created_at: now(),
            last_run: None,
            last_status: None,
            deliver: None,
            origin: None,
            last_delivery_error: None,
            attach_to_session: None,
        }
    }

    fn slash_home_with_jobs() -> (tempfile::TempDir, CronStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = CronStore::open(&dir.path().join("state.db")).unwrap();
        store
            .add(&slash_job("aabbccddeeff", "Morning briefing", "@daily"))
            .unwrap();
        store
            .add(&slash_job("ffeeddccbbaa", "Inbox sweep", "@every 30m"))
            .unwrap();
        (dir, store)
    }

    #[test]
    fn cron_slash_list_and_status() {
        let (dir, _store) = slash_home_with_jobs();
        let out = run_slash(dir.path(), "").text;
        assert!(out.contains("Morning briefing"), "{out}");
        assert!(out.contains("Inbox sweep"), "{out}");
        assert!(out.contains("[active"), "{out}");
        assert!(out.contains("next in"), "{out}");

        let out = run_slash(dir.path(), "status").text;
        assert!(out.contains("2 active, 0 paused"), "{out}");
        assert!(out.contains("next fire"), "{out}");
    }

    #[test]
    fn cron_slash_show_pause_resume_remove() {
        let (dir, store) = slash_home_with_jobs();

        // Name resolution works for show.
        let out = run_slash(dir.path(), "show morning briefing").text;
        assert!(out.contains("id:       aabbccddeeff"), "{out}");
        assert!(out.contains("prompt for Morning briefing"), "{out}");

        // Prefix resolution works for pause.
        let out = run_slash(dir.path(), "pause aabb").text;
        assert!(out.contains("paused"), "{out}");
        assert!(!store.get("aabbccddeeff").unwrap().unwrap().enabled);

        let out = run_slash(dir.path(), "resume aabbccddeeff").text;
        assert!(out.contains("resumed"), "{out}");
        assert!(store.get("aabbccddeeff").unwrap().unwrap().enabled);

        let out = run_slash(dir.path(), "remove Inbox sweep").text;
        assert!(out.contains("removed"), "{out}");
        assert!(store.get("ffeeddccbbaa").unwrap().is_none());

        let out = run_slash(dir.path(), "show zzz").text;
        assert!(out.contains("not found"), "{out}");
    }

    #[test]
    fn cron_slash_run_seeds_agent_turn() {
        let (dir, _store) = slash_home_with_jobs();
        let result = run_slash(dir.path(), "run morning briefing");
        assert_eq!(
            result.run_prompt.as_deref(),
            Some("prompt for Morning briefing")
        );
        assert!(result.text.contains("running cron job"), "{}", result.text);

        let empty = run_slash(dir.path(), "run missing");
        assert!(empty.text.contains("not found"), "{}", empty.text);
        assert!(empty.run_prompt.is_none());
    }

    #[test]
    fn cron_slash_usage_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_slash(dir.path(), "bogus").text;
        assert!(out.contains("usage:"), "{out}");

        let out = run_slash(dir.path(), "list").text;
        assert!(out.contains("no cron jobs"), "{out}");
    }
}

//! Log file management — port of `hermes_cli/logs.py` (viewer) and the
//! rotating file handlers of `hermes_logging.py` (writer).
//!
//! Files live under `<home>/logs/`:
//! - `agent.log`   — INFO+ activity log (5 MB x 3 backups)
//! - `errors.log`  — WARNING+ triage log (2 MB x 2 backups)
//! - `gateway.log` — INFO+ gateway-component lines
//!
//! Line format matches hermes `_LOG_FORMAT`:
//! `2026-08-05 22:35:00,123 INFO [sess_x] target: message`
//! so the viewer regexes (timestamp / level / logger name) work on both
//! hermes- and ulnclaw-produced logs.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDateTime};
use regex::Regex;

/// Known log files (name -> filename). Hermes parity minus the
/// GUI/desktop/MCP-stderr channels that have no ulnclaw producer.
pub const LOG_FILES: &[(&str, &str)] = &[
    ("agent", "agent.log"),
    ("errors", "errors.log"),
    ("gateway", "gateway.log"),
];

/// Component name -> logger-target prefixes (`hermes_logging.COMPONENT_PREFIXES`,
/// adapted to ulnclaw module paths).
pub const COMPONENT_PREFIXES: &[(&str, &[&str])] = &[
    (
        "gateway",
        &["ulnclaw::gateway", "gateway", "managed_gateway"],
    ),
    ("agent", &["ulnclaw::agent", "agent"]),
    ("tools", &["ulnclaw::tools", "tools"]),
    ("cli", &["ulnclaw::main", "main", "cli"]),
    ("cron", &["ulnclaw::cron", "cron"]),
    ("browser", &["ulnclaw::browser", "browser"]),
];

const LEVEL_ORDER: &[(&str, u8)] = &[
    ("TRACE", 0),
    ("DEBUG", 0),
    ("INFO", 1),
    ("WARN", 2),
    ("WARNING", 2),
    ("ERROR", 3),
    ("CRITICAL", 4),
];

fn level_rank(level: &str) -> Option<u8> {
    LEVEL_ORDER
        .iter()
        .find(|(name, _)| *name == level)
        .map(|(_, rank)| *rank)
}

/// Directory holding all log files.
pub fn logs_dir() -> PathBuf {
    crate::config::ulnclaw_home().join("logs")
}

/// Display form of the home dir (hermes `display_hermes_home`).
pub fn display_home() -> String {
    let home = crate::config::ulnclaw_home();
    if let Some(home_str) = home.to_str() {
        if let Some(user_home) = dirs::home_dir() {
            if let Some(user_str) = user_home.to_str() {
                if home_str == user_str {
                    return "~".to_string();
                }
                if let Some(rest) = home_str.strip_prefix(user_str) {
                    if let Some(tail) = rest.strip_prefix('/') {
                        return format!("~/{tail}");
                    }
                }
            }
        }
    }
    home.display().to_string()
}

/// Parse a relative time string like `1h`, `30m`, `2d` into a cutoff.
pub fn parse_since(since: &str) -> Option<DateTime<Local>> {
    let re = Regex::new(r"^(\d+)\s*([smhd])$").ok()?;
    let lowered = since.trim().to_lowercase();
    let caps = re.captures(&lowered)?;
    let value: i64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str();
    let delta = match unit {
        "s" => chrono::Duration::seconds(value),
        "m" => chrono::Duration::minutes(value),
        "h" => chrono::Duration::hours(value),
        "d" => chrono::Duration::days(value),
        _ => return None,
    };
    Some(Local::now() - delta)
}

fn ts_regex() -> Regex {
    Regex::new(r"^(\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2})").expect("static regex")
}

fn level_regex() -> Regex {
    Regex::new(r"\s(TRACE|DEBUG|INFO|WARN|WARNING|ERROR|CRITICAL)\s").expect("static regex")
}

fn logger_name_regex() -> Regex {
    // Rust targets contain `::` (hermes uses dots), so capture lazily up to
    // the ": " that separates the logger name from the message.
    Regex::new(r"\s(?:TRACE|DEBUG|INFO|WARN|WARNING|ERROR|CRITICAL)(?:\s+\[[^\]]*\])?\s+(\S+?):\s")
        .expect("static regex")
}

/// Extract the leading timestamp of a log line.
pub fn parse_line_timestamp(line: &str) -> Option<DateTime<Local>> {
    let caps = ts_regex().captures(line)?;
    let naive = NaiveDateTime::parse_from_str(caps.get(1)?.as_str(), "%Y-%m-%d %H:%M:%S").ok()?;
    naive.and_local_timezone(Local).single()
}

/// Extract the level token of a log line.
pub fn extract_level(line: &str) -> Option<String> {
    level_regex()
        .captures(line)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// Extract the logger/target name of a log line.
pub fn extract_logger_name(line: &str) -> Option<String> {
    logger_name_regex()
        .captures(line)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// All active filters for a tail/follow pass.
#[derive(Default, Clone)]
pub struct LogFilters {
    pub min_level: Option<String>,
    pub session: Option<String>,
    pub since: Option<DateTime<Local>>,
    pub component_prefixes: Option<Vec<String>>,
}

impl LogFilters {
    pub fn is_empty(&self) -> bool {
        self.min_level.is_none()
            && self.session.is_none()
            && self.since.is_none()
            && self.component_prefixes.is_none()
    }
}

/// Check whether one line passes all active filters (hermes `_matches_filters`).
pub fn matches_filters(line: &str, filters: &LogFilters) -> bool {
    if let Some(since) = filters.since {
        if let Some(ts) = parse_line_timestamp(line) {
            if ts < since {
                return false;
            }
        }
    }
    if let Some(min_level) = &filters.min_level {
        if let Some(level) = extract_level(line) {
            let want = level_rank(min_level).unwrap_or(0);
            if level_rank(&level).unwrap_or(0) < want {
                return false;
            }
        }
    }
    if let Some(session) = &filters.session {
        if !line.contains(session.as_str()) {
            return false;
        }
    }
    if let Some(prefixes) = &filters.component_prefixes {
        match extract_logger_name(line) {
            Some(name) => {
                if !prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix.as_str()))
                {
                    return false;
                }
            }
            None => return false,
        }
    }
    true
}

/// Efficiently read the last N lines from a file (hermes `_read_last_n_lines`).
/// Files <= 1 MiB are read whole; larger files are scanned in growing chunks
/// from the end.
pub fn read_last_n_lines(path: &Path, n: usize) -> io::Result<Vec<String>> {
    let size = std::fs::metadata(path)?.len() as usize;
    if size == 0 || n == 0 {
        return Ok(Vec::new());
    }

    if size <= 1_048_576 {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let all: Vec<String> = reader
            .split(b'\n')
            .filter_map(|chunk| chunk.ok())
            .map(|raw| String::from_utf8_lossy(&raw).into_owned())
            .collect();
        let start = all.len().saturating_sub(n);
        return Ok(all[start..].to_vec());
    }

    // Large file: walk backwards in growing chunks until we have enough lines.
    let mut file = File::open(path)?;
    let chunk_start = 8192usize;
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut pos = size;
    let mut chunk_size = chunk_start;

    while pos > 0 && lines.len() <= n + 1 {
        use std::io::{Read, Seek, SeekFrom};
        let read_size = chunk_size.min(pos);
        pos -= read_size;
        file.seek(SeekFrom::Start(pos as u64))?;
        let mut buf = vec![0u8; read_size];
        file.read_exact(&mut buf)?;
        let mut chunk_lines: Vec<Vec<u8>> =
            buf.split(|b| *b == b'\n').map(|s| s.to_vec()).collect();
        if !lines.is_empty() {
            // Merge the trailing partial line of this chunk with the leading
            // partial line already collected.
            let tail = chunk_lines.pop().unwrap_or_default();
            let mut merged = tail;
            merged.extend_from_slice(&lines[0]);
            lines[0] = merged;
            lines.splice(0..0, chunk_lines);
        } else {
            lines = chunk_lines;
        }
        chunk_size = (chunk_size * 2).min(65536);
    }

    let decoded: Vec<String> = lines
        .into_iter()
        .filter(|raw| !raw.iter().all(|b| b.is_ascii_whitespace()))
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
        .collect();
    let start = decoded.len().saturating_sub(n);
    Ok(decoded[start..].to_vec())
}

/// Read the last `num_lines` lines that pass `filters` (hermes `_read_tail`):
/// with filters active we scan a larger window so the tail still fills up.
pub fn read_tail(path: &Path, num_lines: usize, filters: &LogFilters) -> io::Result<Vec<String>> {
    if filters.is_empty() {
        return read_last_n_lines(path, num_lines);
    }
    let raw = read_last_n_lines(path, (num_lines * 20).max(2000))?;
    let filtered: Vec<String> = raw
        .into_iter()
        .filter(|line| matches_filters(line, filters))
        .collect();
    let start = filtered.len().saturating_sub(num_lines);
    Ok(filtered[start..].to_vec())
}

/// List available log files with sizes and ages (hermes `list_logs`).
pub fn list_logs() -> String {
    let dir = logs_dir();
    let mut out = String::new();
    if !dir.exists() {
        out.push_str(&format!("No logs directory at {}/logs/\n", display_home()));
        return out;
    }
    out.push_str(&format!("Log files in {}/logs/:\n\n", display_home()));
    let mut found = false;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().map(|ext| ext == "log").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    for entry in entries {
        let (size, mtime) = match std::fs::metadata(&entry) {
            Ok(meta) => (
                meta.len(),
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
            ),
            Err(_) => continue,
        };
        let size_str = if size < 1024 {
            format!("{size}B")
        } else if size < 1024 * 1024 {
            format!("{:.1}KB", size as f64 / 1024.0)
        } else {
            format!("{:.1}MB", size as f64 / (1024.0 * 1024.0))
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let age = now - mtime;
        let age_str = if mtime == 0.0 {
            "?".to_string()
        } else if age < 60.0 {
            "just now".to_string()
        } else if age < 3600.0 {
            format!("{}m ago", (age / 60.0) as u64)
        } else if age < 86400.0 {
            format!("{}h ago", (age / 3600.0) as u64)
        } else {
            crate::status::relative_time(mtime)
        };
        let name = entry
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push_str(&format!("  {:<25} {:>8}   {}\n", name, size_str, age_str));
        found = true;
    }
    if !found {
        out.push_str("  (no log files yet — run 'ulnclaw chat' to generate logs)\n");
    }
    out
}

/// Options for `tail_log` (hermes `cmd_logs` arguments).
#[derive(Debug, Clone, Default)]
pub struct TailOptions {
    pub num_lines: usize,
    pub follow: bool,
    pub level: Option<String>,
    pub session: Option<String>,
    pub since: Option<String>,
    pub component: Option<String>,
}

/// View (and optionally follow) a log file — the `ulnclaw logs` entry point.
/// Returns the rendered text; errors mirror hermes exit-1 messages.
pub fn tail_log(log_name: &str, opts: &TailOptions) -> Result<String, String> {
    let filename = LOG_FILES
        .iter()
        .find(|(name, _)| *name == log_name)
        .map(|(_, file)| *file)
        .ok_or_else(|| {
            let available: Vec<&str> = LOG_FILES.iter().map(|(name, _)| *name).collect();
            format!(
                "Unknown log: '{log_name}'. Available: {}",
                available.join(", ")
            )
        })?;

    let log_path = logs_dir().join(filename);
    if !log_path.exists() {
        return Err(format!(
            "Log file not found: {}\n(Logs are created when ulnclaw runs — try 'ulnclaw chat' first)",
            log_path.display()
        ));
    }

    let mut filters = LogFilters::default();
    if let Some(since) = &opts.since {
        match parse_since(since) {
            Some(cutoff) => filters.since = Some(cutoff),
            None => {
                return Err(format!(
                    "Invalid --since value: '{since}'. Use format like '1h', '30m', '2d'."
                ))
            }
        }
    }
    if let Some(level) = &opts.level {
        let upper = level.to_uppercase();
        if !matches!(
            upper.as_str(),
            "DEBUG" | "INFO" | "WARNING" | "WARN" | "ERROR" | "CRITICAL"
        ) {
            return Err(format!(
                "Invalid --level: '{level}'. Use DEBUG, INFO, WARNING, ERROR, or CRITICAL."
            ));
        }
        filters.min_level = Some(if upper == "WARN" {
            "WARNING".to_string()
        } else {
            upper
        });
    }
    if let Some(session) = &opts.session {
        filters.session = Some(session.clone());
    }
    if let Some(component) = &opts.component {
        let lower = component.to_lowercase();
        let entry = COMPONENT_PREFIXES.iter().find(|(name, _)| *name == lower);
        match entry {
            Some((_, prefixes)) => {
                filters.component_prefixes = Some(prefixes.iter().map(|p| p.to_string()).collect());
            }
            None => {
                let available: Vec<&str> =
                    COMPONENT_PREFIXES.iter().map(|(name, _)| *name).collect();
                return Err(format!(
                    "Unknown component: '{component}'. Available: {}",
                    available.join(", ")
                ));
            }
        }
    }

    let lines = read_tail(&log_path, opts.num_lines.max(1), &filters).map_err(|e| {
        format!(
            "Permission denied or read error: {} ({e})",
            log_path.display()
        )
    })?;

    let mut filter_parts: Vec<String> = Vec::new();
    if let Some(level) = &filters.min_level {
        filter_parts.push(format!("level>={level}"));
    }
    if let Some(session) = &opts.session {
        filter_parts.push(format!("session={session}"));
    }
    if let Some(component) = &opts.component {
        filter_parts.push(format!("component={component}"));
    }
    if let Some(since) = &opts.since {
        filter_parts.push(format!("since={since}"));
    }
    let filter_desc = if filter_parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", filter_parts.join(", "))
    };

    let mut out = String::new();
    if opts.follow {
        out.push_str(&format!(
            "--- {}/logs/{filename}{filter_desc} (Ctrl+C to stop) ---\n",
            display_home()
        ));
    } else {
        out.push_str(&format!(
            "--- {}/logs/{filename}{filter_desc} (last {}) ---\n",
            display_home(),
            opts.num_lines
        ));
    }
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Follow a log file in real time (hermes `_follow_log`) — blocks forever
/// until interrupted. Prints matching lines to stdout as they arrive.
pub fn follow_log(log_name: &str, opts: &TailOptions) -> Result<(), String> {
    let filename = LOG_FILES
        .iter()
        .find(|(name, _)| *name == log_name)
        .map(|(_, file)| *file)
        .ok_or_else(|| format!("Unknown log: '{log_name}'"))?;
    let log_path = logs_dir().join(filename);

    let mut filters = LogFilters::default();
    if let Some(since) = &opts.since {
        filters.since = parse_since(since);
    }
    if let Some(level) = &opts.level {
        filters.min_level = Some(level.to_uppercase());
    }
    if let Some(session) = &opts.session {
        filters.session = Some(session.clone());
    }
    if let Some(component) = &opts.component {
        let lower = component.to_lowercase();
        if let Some((_, prefixes)) = COMPONENT_PREFIXES.iter().find(|(name, _)| *name == lower) {
            filters.component_prefixes = Some(prefixes.iter().map(|p| p.to_string()).collect());
        }
    }

    let file =
        File::open(&log_path).map_err(|e| format!("Cannot open {}: {e}", log_path.display()))?;
    let mut reader = BufReader::new(file);
    use std::io::Seek;
    let _ = reader.seek(io::SeekFrom::End(0));
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => std::thread::sleep(Duration::from_millis(300)),
            Ok(_) => {
                if matches_filters(line.trim_end_matches('\n'), &filters) {
                    print!("{line}");
                    let _ = io::stdout().flush();
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(300)),
        }
    }
}

// =============================================================================
// Writer side — rotating file handlers (hermes_logging.py `_add_rotating_handler`)
// =============================================================================

/// Append-only file with size-based rotation: when `max_bytes` would be
/// exceeded, `path.N` -> `path.N+1` shifts and the current file becomes
/// `path.1` (hermes `RotatingFileHandler` semantics).
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    backup_count: u32,
    file: File,
    written: u64,
}

impl RotatingFile {
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64, backup_count: u32) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            max_bytes,
            backup_count,
            file,
            written,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        // Drop the oldest backup, shift the rest.
        if self.backup_count > 0 {
            let oldest = backup_path(&self.path, self.backup_count);
            let _ = std::fs::remove_file(&oldest);
            for i in (1..self.backup_count).rev() {
                let from = backup_path(&self.path, i);
                let to = backup_path(&self.path, i + 1);
                if from.exists() {
                    let _ = std::fs::rename(&from, &to);
                }
            }
            let first = backup_path(&self.path, 1);
            let _ = std::fs::rename(&self.path, &first);
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

fn backup_path(path: &Path, index: u32) -> PathBuf {
    let name = format!(
        "{}.{index}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    path.with_file_name(name)
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.max_bytes && !buf.is_empty() {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Cloneable handle around a `RotatingFile` for use as a tracing writer.
#[derive(Clone)]
pub struct RotatingFileHandle(Arc<Mutex<RotatingFile>>);

impl RotatingFileHandle {
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64, backup_count: u32) -> io::Result<Self> {
        Ok(Self(Arc::new(Mutex::new(RotatingFile::open(
            path,
            max_bytes,
            backup_count,
        )?))))
    }
}

impl Write for RotatingFileHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.lock() {
            Ok(mut inner) => inner.write(buf),
            Err(_) => Err(io::Error::new(io::ErrorKind::Other, "log lock poisoned")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.lock() {
            Ok(mut inner) => inner.flush(),
            Err(_) => Err(io::Error::new(io::ErrorKind::Other, "log lock poisoned")),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingFileHandle {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// hermes `_LOG_FORMAT` renderer for tracing events:
/// `YYYY-MM-DD HH:MM:SS,mmm LEVEL [session] target: message`
pub struct HermesLogFormat;

struct FieldCollector {
    message: String,
    session: Option<String>,
}

impl tracing::field::Visit for FieldCollector {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "session_id" | "session" => self.session = Some(value.to_string()),
            _ => {}
        }
    }
}

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for HermesLogFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut collector = FieldCollector {
            message: String::new(),
            session: None,
        };
        event.record(&mut collector);
        let now = Local::now();
        let level = match *event.metadata().level() {
            tracing::Level::TRACE => "DEBUG",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::INFO => "INFO",
            tracing::Level::WARN => "WARNING",
            tracing::Level::ERROR => "ERROR",
        };
        let session_tag = match &collector.session {
            Some(session) => format!(" [{session}]"),
            None => String::new(),
        };
        writeln!(
            writer,
            "{} {}{} {}: {}",
            now.format("%Y-%m-%d %H:%M:%S,%3f"),
            level,
            session_tag,
            event.metadata().target(),
            collector.message
        )
    }
}

/// Build the file log layers (agent.log INFO+, errors.log WARNING+,
/// gateway.log INFO+ for gateway targets). Returns boxed layers ready to
/// attach to a `tracing_subscriber::registry()`, or an empty vec when the
/// logs directory cannot be created.
pub fn file_layers<S>() -> Vec<Box<dyn tracing_subscriber::Layer<S> + Send + Sync>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::filter::FilterExt;
    use tracing_subscriber::Layer;

    let dir = logs_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }

    let mut layers: Vec<Box<dyn tracing_subscriber::Layer<S> + Send + Sync>> = Vec::new();

    if let Ok(handle) = RotatingFileHandle::open(dir.join("agent.log"), 5 * 1024 * 1024, 3) {
        let layer = tracing_subscriber::fmt::layer()
            .event_format(HermesLogFormat)
            .with_ansi(false)
            .with_writer(handle)
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
        layers.push(layer.boxed());
    }

    if let Ok(handle) = RotatingFileHandle::open(dir.join("errors.log"), 2 * 1024 * 1024, 2) {
        let layer = tracing_subscriber::fmt::layer()
            .event_format(HermesLogFormat)
            .with_ansi(false)
            .with_writer(handle)
            .with_filter(tracing_subscriber::filter::LevelFilter::WARN);
        layers.push(layer.boxed());
    }

    if let Ok(handle) = RotatingFileHandle::open(dir.join("gateway.log"), 5 * 1024 * 1024, 3) {
        let gateway_filter = tracing_subscriber::filter::FilterFn::new(|meta| {
            let target = meta.target();
            target.starts_with("ulnclaw::gateway")
                || target.starts_with("gateway")
                || target.starts_with("managed_gateway")
        });
        let layer = tracing_subscriber::fmt::layer()
            .event_format(HermesLogFormat)
            .with_ansi(false)
            .with_writer(handle)
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO.and(gateway_filter));
        layers.push(layer.boxed());
    }

    layers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut file = File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    #[test]
    fn parse_since_buckets() {
        let now = Local::now();
        let hour = parse_since("1h").unwrap();
        assert!((now - hour).num_minutes() >= 59 && (now - hour).num_minutes() <= 61);
        let half = parse_since("30m").unwrap();
        assert!((now - half).num_seconds() >= 1790 && (now - half).num_seconds() <= 1810);
        let days = parse_since("2d").unwrap();
        assert!((now - days).num_hours() >= 47 && (now - days).num_hours() <= 49);
        assert!(parse_since("bogus").is_none());
        assert!(parse_since("10x").is_none());
    }

    #[test]
    fn timestamp_and_level_extraction() {
        let line = "2026-08-05 22:35:00,123 INFO ulnclaw::agent: hello";
        assert!(parse_line_timestamp(line).is_some());
        assert_eq!(extract_level(line).as_deref(), Some("INFO"));
        assert_eq!(extract_logger_name(line).as_deref(), Some("ulnclaw::agent"));
        let tagged = "2026-08-05 22:35:00 WARNING [sess_1] ulnclaw::tools::shell: boom";
        assert_eq!(extract_level(tagged).as_deref(), Some("WARNING"));
        assert_eq!(
            extract_logger_name(tagged).as_deref(),
            Some("ulnclaw::tools::shell")
        );
        assert!(parse_line_timestamp("no timestamp here").is_none());
    }

    #[test]
    fn logger_name_from_real_gateway_line() {
        let line = "2026-08-05 12:46:12,715 INFO ulnclaw::gateway: gateway profile multiplexing: off (prefix ignored) (0 profile(s) configured)";
        let name = extract_logger_name(line);
        assert_eq!(name.as_deref(), Some("ulnclaw::gateway"));
        let component = LogFilters {
            min_level: None,
            session: None,
            since: None,
            component_prefixes: Some(vec!["ulnclaw::gateway".to_string()]),
        };
        assert!(matches_filters(line, &component));
    }

    #[test]
    fn filters_level_session_component() {
        let filters = LogFilters {
            min_level: Some("WARNING".to_string()),
            session: Some("abc".to_string()),
            since: None,
            component_prefixes: None,
        };
        assert!(matches_filters(
            "2026-08-05 10:00:00 ERROR [abc] x: fail",
            &filters
        ));
        assert!(!matches_filters(
            "2026-08-05 10:00:00 INFO [abc] x: fine",
            &filters
        ));
        assert!(!matches_filters(
            "2026-08-05 10:00:00 ERROR [xyz] x: other",
            &filters
        ));

        let component = LogFilters {
            min_level: None,
            session: None,
            since: None,
            component_prefixes: Some(vec!["ulnclaw::tools".to_string()]),
        };
        assert!(matches_filters(
            "2026-08-05 10:00:00 INFO ulnclaw::tools::shell: ran",
            &component
        ));
        assert!(!matches_filters(
            "2026-08-05 10:00:00 INFO ulnclaw::agent: ran",
            &component
        ));
    }

    #[test]
    fn read_last_n_small_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        write_lines(
            &path,
            &[
                "2026-08-05 10:00:00 INFO a: one",
                "2026-08-05 10:00:01 ERROR b: two",
                "2026-08-05 10:00:02 INFO c: three",
                "2026-08-05 10:00:03 WARNING d: four",
            ],
        );
        let tail = read_last_n_lines(&path, 2).unwrap();
        assert_eq!(tail.len(), 2);
        assert!(tail[1].contains("four"));

        let filters = LogFilters {
            min_level: Some("WARNING".to_string()),
            ..Default::default()
        };
        let filtered = read_tail(&path, 10, &filters).unwrap();
        assert_eq!(filtered.len(), 2);
        assert!(filtered[0].contains("two"));
        assert!(filtered[1].contains("four"));
    }

    #[test]
    fn read_last_n_large_file_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let mut file = File::create(&path).unwrap();
        let padding = "x".repeat(200);
        for i in 0..8000 {
            writeln!(file, "2026-08-05 10:00:00 INFO t: line {i} {padding}").unwrap();
        }
        drop(file);
        assert!(std::fs::metadata(&path).unwrap().len() > 1_048_576);
        let tail = read_last_n_lines(&path, 5).unwrap();
        assert_eq!(tail.len(), 5);
        assert!(tail[4].contains("line 7999"));
        assert!(tail[0].contains("line 7995"));
    }

    #[test]
    fn tail_log_renders_header_and_errors() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        let err = tail_log(
            "nope",
            &TailOptions {
                num_lines: 10,
                ..Default::default()
            },
        );
        assert!(err.unwrap_err().contains("Unknown log"));

        let err = tail_log(
            "agent",
            &TailOptions {
                num_lines: 10,
                ..Default::default()
            },
        );
        assert!(err.unwrap_err().contains("not found"));

        std::fs::create_dir_all(logs_dir()).unwrap();
        write_lines(
            &logs_dir().join("agent.log"),
            &["2026-08-05 10:00:00 INFO t: hello"],
        );
        let out = tail_log(
            "agent",
            &TailOptions {
                num_lines: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(out.contains("(last 10)"));
        assert!(out.contains("hello"));

        let err = tail_log(
            "agent",
            &TailOptions {
                num_lines: 10,
                since: Some("bogus".into()),
                ..Default::default()
            },
        );
        assert!(err.unwrap_err().contains("Invalid --since"));

        let err = tail_log(
            "agent",
            &TailOptions {
                num_lines: 10,
                level: Some("LOUD".into()),
                ..Default::default()
            },
        );
        assert!(err.unwrap_err().contains("Invalid --level"));

        let err = tail_log(
            "agent",
            &TailOptions {
                num_lines: 10,
                component: Some("warp".into()),
                ..Default::default()
            },
        );
        assert!(err.unwrap_err().contains("Unknown component"));

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn list_logs_reports_files() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        let out = list_logs();
        assert!(out.contains("No logs directory"));

        std::fs::create_dir_all(logs_dir()).unwrap();
        write_lines(&logs_dir().join("agent.log"), &["line"]);
        let out = list_logs();
        assert!(out.contains("agent.log"));
        assert!(out.contains("just now"));

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn rotating_file_shifts_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        let mut rotating = RotatingFile::open(&path, 64, 2).unwrap();
        for i in 0..8 {
            writeln!(rotating, "payload line {i} — padding padding").unwrap();
        }
        rotating.flush().unwrap();
        assert!(path.exists(), "current file recreated after rotation");
        assert!(backup_path(&path, 1).exists(), "backup .1 exists");
        assert!(backup_path(&path, 2).exists(), "backup .2 exists");
        assert!(!backup_path(&path, 3).exists(), "backup_count honored");
    }
}

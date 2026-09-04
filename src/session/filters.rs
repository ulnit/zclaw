//! Session prune/archive filter parsing — port of hermes
//! `hermes_cli/session_filters.py` (v2026.8.3), scoped to ulnclaw's
//! sessions schema.
//!
//! Two value shapes are accepted anywhere a point in time is expected:
//!
//! * Durations (relative to now): `5h`, `30m`, `2d`, `1w` — and, for
//!   backward compatibility with the original `--older-than N` flag, a
//!   bare number which means **days**.
//! * Absolute timestamps: `2026-07-05`, `2026-07-05 14:30`,
//!   `2026-07-05T14:30:00` (naive values are interpreted in local time).

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};

// ---------------------------------------------------------------------------
// Duration / point-in-time parsing
// ---------------------------------------------------------------------------

/// Parse `5h` / `30m` / `2d` / `1w` / `90` (bare = days) into seconds.
/// Returns `None` when the value doesn't look like a duration (hermes
/// `parse_duration_seconds`).
pub fn parse_duration_seconds(value: &str) -> Option<f64> {
    let s = value.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    // Bare number = days (backward compatible with --older-than 90).
    if s.chars().all(|c| c.is_ascii_digit() || c == '.')
        && s.chars().filter(|c| *c == '.').count() <= 1
    {
        return s.parse::<f64>().ok().map(|days| days * 86400.0);
    }
    // Split numeric prefix from unit suffix.
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (number_part, unit_part) = s.split_at(split);
    let number: f64 = number_part.trim().parse().ok()?;
    let unit = unit_part.trim();
    let unit_seconds = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        "w" | "wk" | "wks" | "week" | "weeks" => 604800.0,
        _ => return None,
    };
    Some(number * unit_seconds)
}

/// Parse a CLI time value into an epoch timestamp (hermes
/// `parse_point_in_time`). Durations mean "that long ago"; absolute ISO
/// timestamps are returned as-is (naive = local time).
pub fn parse_point_in_time(value: &str, flag: &str) -> Result<f64, String> {
    let s = value.trim();
    if let Some(seconds) = parse_duration_seconds(s) {
        return Ok(now_epoch() - seconds);
    }
    let error = || {
        format!(
            "Invalid value for {}: '{}'. Use a duration like '5h', '30m', '2d', '1w', \
             a bare number of days, or an ISO timestamp like '2026-07-05' or '2026-07-05 14:30'.",
            flag, value
        )
    };
    // RFC3339 / ISO with offset first.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp() as f64);
    }
    // Naive local forms.
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
        .or_else(|_| {
            NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|date| date.and_hms_opt(0, 0, 0).unwrap())
        })
        .map_err(|_| error())?;
    let local = Local
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(error)?;
    Ok(local.timestamp() as f64)
}

/// Render an epoch timestamp as a short local-time string (hermes
/// `format_epoch`).
pub fn format_epoch(ts: Option<f64>) -> String {
    let Some(ts) = ts else {
        return "-".to_string();
    };
    let Some(dt) = chrono::DateTime::from_timestamp(ts as i64, 0) else {
        return "-".to_string();
    };
    dt.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Prune filters
// ---------------------------------------------------------------------------

/// Filters for session prune/archive selection (hermes
/// `build_prune_filters` output, scoped to ulnclaw's schema columns).
///
/// String matching conventions (hermes `_prune_filter_where`):
/// `model_like` / `title_like` are case-insensitive substring matches;
/// `source` / `end_reason` are exact. Token bounds apply to
/// `input_tokens + output_tokens`. Only ENDED sessions are ever
/// candidates (`ended_at IS NOT NULL`) so a live session is never
/// selected. `archived` is tri-state: `None` = both, `Some(true)` = only
/// archived rows, `Some(false)` = only unarchived rows.
#[derive(Debug, Clone, Default)]
pub struct PruneFilters {
    /// Last activity (latest message timestamp, falling back to
    /// `started_at`) strictly before this epoch.
    pub last_active_before: Option<f64>,
    /// Last activity at/after this epoch.
    pub last_active_after: Option<f64>,
    /// Session start strictly before this epoch.
    pub started_before: Option<f64>,
    /// Session start at/after this epoch.
    pub started_after: Option<f64>,
    pub source: Option<String>,
    pub title_like: Option<String>,
    pub end_reason: Option<String>,
    pub cwd_prefix: Option<String>,
    pub min_messages: Option<i64>,
    pub max_messages: Option<i64>,
    pub model_like: Option<String>,
    pub min_tokens: Option<i64>,
    pub max_tokens: Option<i64>,
    pub min_tool_calls: Option<i64>,
    pub max_tool_calls: Option<i64>,
    /// Tri-state archived filter (see struct docs).
    pub archived: Option<bool>,
}

/// Typed bind parameter for the generated WHERE clause.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterParam {
    Real(f64),
    Int(i64),
    Text(String),
}

/// The last-active expression shared by all filter clauses (hermes
/// `COALESCE(MAX(messages.timestamp), started_at)`).
pub const LAST_ACTIVE_EXPR: &str = "COALESCE(
                       (SELECT MAX(m.timestamp) FROM messages m
                        WHERE m.session_id = s.id),
                       s.started_at
                   )";

impl PruneFilters {
    /// Build the WHERE clause + bind params (hermes `_prune_filter_where`).
    /// The clause references the `s` table alias.
    pub fn where_clause(&self) -> (String, Vec<FilterParam>) {
        let mut clauses: Vec<String> = vec!["s.ended_at IS NOT NULL".to_string()];
        let mut params: Vec<FilterParam> = Vec::new();
        if let Some(v) = self.last_active_before {
            clauses.push(format!("{} < ?", LAST_ACTIVE_EXPR));
            params.push(FilterParam::Real(v));
        }
        if let Some(v) = self.last_active_after {
            clauses.push(format!("{} >= ?", LAST_ACTIVE_EXPR));
            params.push(FilterParam::Real(v));
        }
        if let Some(v) = self.started_before {
            clauses.push("s.started_at < ?".to_string());
            params.push(FilterParam::Real(v));
        }
        if let Some(v) = self.started_after {
            clauses.push("s.started_at >= ?".to_string());
            params.push(FilterParam::Real(v));
        }
        if let Some(source) = self.source.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("s.source = ?".to_string());
            params.push(FilterParam::Text(source.to_string()));
        }
        if let Some(title) = self.title_like.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("LOWER(COALESCE(s.title, '')) LIKE ?".to_string());
            params.push(FilterParam::Text(format!(
                "%{}%",
                title.to_ascii_lowercase()
            )));
        }
        if let Some(reason) = self.end_reason.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("s.end_reason = ?".to_string());
            params.push(FilterParam::Text(reason.to_string()));
        }
        if let Some(cwd) = self.cwd_prefix.as_deref().filter(|s| !s.is_empty()) {
            // Prefix match on the stored cwd (hermes `_cwd_prefix_clause`
            // normalizes trailing slashes).
            let mut normalized = cwd.to_string();
            if !normalized.ends_with('/') {
                normalized.push('/');
            }
            clauses.push("(COALESCE(s.cwd, '') = ? OR COALESCE(s.cwd, '') LIKE ?)".to_string());
            params.push(FilterParam::Text(cwd.trim_end_matches('/').to_string()));
            params.push(FilterParam::Text(format!("{}%", normalized)));
        }
        if let Some(v) = self.min_messages {
            clauses.push("s.message_count >= ?".to_string());
            params.push(FilterParam::Int(v));
        }
        if let Some(v) = self.max_messages {
            clauses.push("s.message_count <= ?".to_string());
            params.push(FilterParam::Int(v));
        }
        if let Some(model) = self.model_like.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("LOWER(COALESCE(s.model, '')) LIKE ?".to_string());
            params.push(FilterParam::Text(format!(
                "%{}%",
                model.to_ascii_lowercase()
            )));
        }
        if let Some(v) = self.min_tokens {
            clauses.push("(s.input_tokens + s.output_tokens) >= ?".to_string());
            params.push(FilterParam::Int(v));
        }
        if let Some(v) = self.max_tokens {
            clauses.push("(s.input_tokens + s.output_tokens) <= ?".to_string());
            params.push(FilterParam::Int(v));
        }
        if let Some(v) = self.min_tool_calls {
            clauses.push("s.tool_call_count >= ?".to_string());
            params.push(FilterParam::Int(v));
        }
        if let Some(v) = self.max_tool_calls {
            clauses.push("s.tool_call_count <= ?".to_string());
            params.push(FilterParam::Int(v));
        }
        match self.archived {
            Some(true) => clauses.push("s.archived = 1".to_string()),
            Some(false) => clauses.push("s.archived = 0".to_string()),
            None => {}
        }
        (clauses.join(" AND "), params)
    }

    /// True when no filter at all is set (hermes bare-prune detection).
    pub fn is_empty(&self) -> bool {
        self.last_active_before.is_none()
            && self.last_active_after.is_none()
            && self.started_before.is_none()
            && self.started_after.is_none()
            && self.source.is_none()
            && self.title_like.is_none()
            && self.end_reason.is_none()
            && self.cwd_prefix.is_none()
            && self.min_messages.is_none()
            && self.max_messages.is_none()
            && self.model_like.is_none()
            && self.min_tokens.is_none()
            && self.max_tokens.is_none()
            && self.min_tool_calls.is_none()
            && self.max_tool_calls.is_none()
    }

    /// Human-readable summary for confirmation prompts (hermes
    /// `describe_filters`).
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(v) = self.last_active_before {
            parts.push(format!("last active before {}", format_epoch(Some(v))));
        }
        if let Some(v) = self.last_active_after {
            parts.push(format!("last active after {}", format_epoch(Some(v))));
        }
        if let Some(v) = self.started_before {
            parts.push(format!("started before {}", format_epoch(Some(v))));
        }
        if let Some(v) = self.started_after {
            parts.push(format!("started after {}", format_epoch(Some(v))));
        }
        if let Some(v) = &self.source {
            parts.push(format!("source '{}'", v));
        }
        if let Some(v) = &self.title_like {
            parts.push(format!("title contains '{}'", v));
        }
        if let Some(v) = &self.end_reason {
            parts.push(format!("end reason '{}'", v));
        }
        if let Some(v) = &self.cwd_prefix {
            parts.push(format!("cwd under '{}'", v));
        }
        if let Some(v) = self.min_messages {
            parts.push(format!(">= {} messages", v));
        }
        if let Some(v) = self.max_messages {
            parts.push(format!("<= {} messages", v));
        }
        if let Some(v) = &self.model_like {
            parts.push(format!("model contains '{}'", v));
        }
        if let Some(v) = self.min_tokens {
            parts.push(format!(">= {} tokens", v));
        }
        if let Some(v) = self.max_tokens {
            parts.push(format!("<= {} tokens", v));
        }
        if let Some(v) = self.min_tool_calls {
            parts.push(format!(">= {} tool calls", v));
        }
        if let Some(v) = self.max_tool_calls {
            parts.push(format!("<= {} tool calls", v));
        }
        if parts.is_empty() {
            "no filters (all ended sessions)".to_string()
        } else {
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration_seconds("90"), Some(90.0 * 86400.0));
        assert_eq!(parse_duration_seconds("5h"), Some(5.0 * 3600.0));
        assert_eq!(parse_duration_seconds("30m"), Some(30.0 * 60.0));
        assert_eq!(parse_duration_seconds("2d"), Some(2.0 * 86400.0));
        assert_eq!(parse_duration_seconds("1w"), Some(604800.0));
        assert_eq!(parse_duration_seconds("2 weeks"), Some(2.0 * 604800.0));
        assert_eq!(parse_duration_seconds("90seconds"), Some(90.0));
        assert_eq!(parse_duration_seconds("1.5h"), Some(5400.0));
        assert_eq!(parse_duration_seconds("nope"), None);
        assert_eq!(parse_duration_seconds(""), None);
        assert_eq!(parse_duration_seconds("5x"), None);
    }

    #[test]
    fn point_in_time_durations_are_relative_to_now() {
        let now = now_epoch();
        let ts = parse_point_in_time("5h", "--older-than").unwrap();
        assert!((now - 5.0 * 3600.0 - ts).abs() < 5.0);
        let ts = parse_point_in_time("30", "--older-than").unwrap();
        assert!((now - 30.0 * 86400.0 - ts).abs() < 5.0);
    }

    #[test]
    fn point_in_time_absolute_timestamps() {
        let ts = parse_point_in_time("2026-07-05", "--before").unwrap();
        let local = chrono::DateTime::from_timestamp(ts as i64, 0)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(local.year(), 2026);
        assert_eq!(local.month(), 7);
        assert_eq!(local.day(), 5);
        assert_eq!(local.hour(), 0);
        // With time component.
        let ts = parse_point_in_time("2026-07-05 14:30", "--before").unwrap();
        let local = chrono::DateTime::from_timestamp(ts as i64, 0)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(local.hour(), 14);
        assert_eq!(local.minute(), 30);
        // Unparseable.
        assert!(parse_point_in_time("someday", "--before").is_err());
    }

    #[test]
    fn format_epoch_shapes() {
        assert_eq!(format_epoch(None), "-");
        let rendered = format_epoch(Some(0.0));
        assert!(rendered.contains("-"), "renders a date: {}", rendered);
    }

    #[test]
    fn where_clause_base_and_filters() {
        let filters = PruneFilters::default();
        let (clause, params) = filters.where_clause();
        assert_eq!(clause, "s.ended_at IS NOT NULL");
        assert!(params.is_empty());
        assert!(filters.is_empty());

        let mut filters = PruneFilters {
            source: Some("cli".into()),
            title_like: Some("Fix".into()),
            max_messages: Some(10),
            archived: Some(false),
            ..Default::default()
        };
        filters.last_active_before = Some(1000.0);
        let (clause, params) = filters.where_clause();
        assert!(clause.contains("s.ended_at IS NOT NULL"));
        assert!(clause.contains("s.source = ?"));
        assert!(clause.contains("LOWER(COALESCE(s.title, '')) LIKE ?"));
        assert!(clause.contains("s.message_count <= ?"));
        assert!(clause.contains("s.archived = 0"));
        assert!(params.contains(&FilterParam::Text("cli".into())));
        assert!(params.contains(&FilterParam::Text("%fix%".into())));
        assert!(params.contains(&FilterParam::Int(10)));
        assert!(!filters.is_empty());
    }

    #[test]
    fn describe_filters_readable() {
        let mut filters = PruneFilters {
            source: Some("cron".into()),
            ..Default::default()
        };
        filters.last_active_before = Some(now_epoch() - 86400.0);
        let description = filters.describe();
        assert!(description.contains("last active before"));
        assert!(description.contains("source 'cron'"));
        assert_eq!(
            PruneFilters::default().describe(),
            "no filters (all ended sessions)"
        );
    }
}

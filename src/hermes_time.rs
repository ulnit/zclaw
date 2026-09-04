//! Timezone-aware clock — port of hermes `hermes_time.py` (v2026.8.3).
//!
//! Provides timezone-aware "now" helpers for prompt timestamps and
//! compression anchoring. Resolution order for the IANA timezone:
//!
//!   1. `ULNCLAW_TIMEZONE` env var (`HERMES_TIMEZONE` honored for migration)
//!   2. `timezone` key in `config.toml`
//!   3. falls back to the server's local time
//!
//! Invalid timezone values log a warning and fall back safely — ulnclaw
//! never crashes due to a bad timezone string. The resolution is cached
//! process-wide; call `reset_cache()` after config/env changes.

use std::sync::Mutex;

use chrono::{Datelike, Local, NaiveDateTime, Utc};
use chrono_tz::Tz;

/// Cached resolution state (hermes `_cached_tz` / `_cache_resolved`).
struct TzCache {
    resolved: bool,
    tz: Option<Tz>,
}

static CACHE: Mutex<Option<TzCache>> = Mutex::new(None);

/// Read the configured IANA timezone string (or `None`).
///
/// Env vars take priority over the config key (hermes
/// `_resolve_timezone_name`).
pub fn resolve_timezone_name(config_timezone: Option<&str>) -> Option<String> {
    // 1. Environment variable (highest priority — set by supervisors etc.).
    for var in ["ULNCLAW_TIMEZONE", "HERMES_TIMEZONE"] {
        if let Ok(value) = std::env::var(var) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    // 2. config.toml `timezone` key.
    config_timezone
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Validate an IANA name; warn + `None` on failure (hermes
/// `_get_zoneinfo`).
fn parse_zoneinfo(name: &str) -> Option<Tz> {
    match name.parse::<Tz>() {
        Ok(tz) => Some(tz),
        Err(e) => {
            tracing::warn!(
                "Invalid timezone '{}': {}. Falling back to server local time.",
                name,
                e
            );
            None
        }
    }
}

/// The user's configured zone, or `None` meaning server-local. Resolved
/// once and cached (hermes `get_timezone`).
pub fn get_timezone(config_timezone: Option<&str>) -> Option<Tz> {
    let mut cache = CACHE.lock().expect("hermes_time cache poisoned");
    if let Some(entry) = cache.as_ref() {
        if entry.resolved {
            return entry.tz;
        }
    }
    let name = resolve_timezone_name(config_timezone);
    let tz = name.as_deref().and_then(parse_zoneinfo);
    *cache = Some(TzCache { resolved: true, tz });
    tz
}

/// Clear the cached timezone so the next call re-resolves (hermes
/// `reset_cache`) — call after config or env changes.
pub fn reset_cache() {
    let mut cache = CACHE.lock().expect("hermes_time cache poisoned");
    *cache = None;
}

/// Wall-clock "now" in the configured zone: the naive local wall time plus
/// a human-readable zone label (IANA name, or the local offset when
/// server-local).
pub fn now_wall(config_timezone: Option<&str>) -> (NaiveDateTime, String) {
    match get_timezone(config_timezone) {
        Some(tz) => {
            let zoned = Utc::now().with_timezone(&tz);
            (zoned.naive_local(), tz.to_string())
        }
        None => {
            let local = Local::now();
            let label = local.format("%z").to_string();
            (local.naive_local(), label)
        }
    }
}

/// System-prompt timestamp line, date-only (hermes `system_prompt.py`
/// volatile block): byte-stable for the whole day so prefix-cache KV is
/// not invalidated on every rebuild — the model can query the exact
/// wall-clock time via tools when it needs it (hermes PR #20451).
pub fn conversation_started_line(config_timezone: Option<&str>) -> String {
    let (wall, _label) = now_wall(config_timezone);
    format!(
        "Conversation started: {}, {} {}",
        weekday_name(wall.weekday()),
        month_name(wall.month()),
        wall.format("%d, %Y")
    )
}

/// `YYYY-MM-DD` in the configured zone (hermes compressor temporal
/// anchoring).
pub fn today_iso(config_timezone: Option<&str>) -> String {
    let (wall, _label) = now_wall(config_timezone);
    wall.format("%Y-%m-%d").to_string()
}

fn weekday_name(weekday: chrono::Weekday) -> &'static str {
    match weekday {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    }
}

fn month_name(month: u32) -> &'static str {
    match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        _ => "December",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-wide lock for tests that mutate timezone env vars (mirrors
    /// `models_dev::test_env_lock`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the env lock, recovering from a poisoned lock so one
    /// panicking test does not cascade PoisonError into every sibling.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_env() {
        std::env::remove_var("ULNCLAW_TIMEZONE");
        std::env::remove_var("HERMES_TIMEZONE");
    }

    #[test]
    fn valid_iana_zone_parses() {
        let _guard = env_lock();
        clear_env();
        reset_cache();
        let tz = get_timezone(Some("Asia/Shanghai"));
        assert_eq!(tz, Some(Tz::Asia__Shanghai));
        reset_cache();
    }

    #[test]
    fn invalid_zone_falls_back_to_local() {
        let _guard = env_lock();
        clear_env();
        reset_cache();
        assert_eq!(get_timezone(Some("Not/AZone"),), None);
        let (wall, label) = now_wall(Some("Not/AZone"));
        assert!(!label.is_empty());
        // Wall clock should be close to UTC now (same instant, any zone).
        let drift = (wall.and_utc() - Utc::now()).num_minutes().abs();
        assert!(drift <= 14 * 60, "wall clock drift too large: {}min", drift);
        reset_cache();
    }

    #[test]
    fn env_overrides_config() {
        let _guard = env_lock();
        clear_env();
        reset_cache();
        std::env::set_var("ULNCLAW_TIMEZONE", "America/New_York");
        let name = resolve_timezone_name(Some("Asia/Shanghai"));
        assert_eq!(name.as_deref(), Some("America/New_York"));
        std::env::remove_var("ULNCLAW_TIMEZONE");

        std::env::set_var("HERMES_TIMEZONE", "Europe/Berlin");
        let name = resolve_timezone_name(Some("Asia/Shanghai"));
        assert_eq!(name.as_deref(), Some("Europe/Berlin"));
        clear_env();
        reset_cache();
    }

    #[test]
    fn config_used_when_env_absent() {
        let _guard = env_lock();
        clear_env();
        reset_cache();
        let name = resolve_timezone_name(Some("  Asia/Kolkata  "));
        assert_eq!(name.as_deref(), Some("Asia/Kolkata"));
        assert_eq!(resolve_timezone_name(Some("   ")), None);
        assert_eq!(resolve_timezone_name(None), None);
        reset_cache();
    }

    #[test]
    fn conversation_started_line_shape() {
        let _guard = env_lock();
        clear_env();
        reset_cache();
        let line = conversation_started_line(Some("UTC"));
        assert!(line.starts_with("Conversation started: "), "{}", line);
        // "Conversation started: <Weekday>, <Month> DD, YYYY"
        let rest = line.strip_prefix("Conversation started: ").unwrap();
        let parts: Vec<&str> = rest.split(", ").collect();
        assert_eq!(parts.len(), 3, "{}", line);
        assert!([
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday"
        ]
        .contains(&parts[0]));
        reset_cache();
    }

    #[test]
    fn today_iso_shape() {
        let _guard = env_lock();
        clear_env();
        reset_cache();
        let today = today_iso(Some("UTC"));
        assert_eq!(today.len(), 10);
        assert_eq!(&today[4..5], "-");
        assert_eq!(&today[7..8], "-");
        assert_eq!(today, Utc::now().format("%Y-%m-%d").to_string());
        reset_cache();
    }

    #[test]
    fn wall_clock_matches_zone_offset() {
        let _guard = env_lock();
        clear_env();
        reset_cache();
        // UTC wall time must equal UTC now (to the minute).
        let (wall, label) = now_wall(Some("UTC"));
        assert_eq!(label, "UTC");
        let drift = (wall.and_utc() - Utc::now()).num_seconds().abs();
        assert!(drift < 5, "UTC wall drift {}s", drift);
        reset_cache();
    }
}

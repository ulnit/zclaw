//! Bounded, non-destructive readiness probes for the gateway health
//! surface (hermes `gateway/readiness.py` parity).
//!
//! Probes expose status and counts only — never config values,
//! credentials, paths, commands, queue payloads, or exception
//! messages. Every probe is read-only and bounded so a health poll
//! never competes with normal gateway work.

use serde_json::{json, Value};
use std::path::Path;

/// Disk usage at or above this percentage reads "degraded" (hermes
/// `_DISK_DEGRADED_PERCENT`).
const DISK_DEGRADED_PERCENT: f64 = 90.0;

fn check(status: &str, detail: Option<&str>, extras: Vec<(&str, Value)>) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("status".into(), json!(status));
    if let Some(detail) = detail {
        map.insert("detail".into(), json!(detail));
    }
    for (key, value) in extras {
        map.insert(key.into(), value);
    }
    Value::Object(map)
}

/// Read-only schema peek at the session store (hermes
/// `_probe_state_db`): catches unreadable/corrupt databases without
/// taking a write reservation on every health poll.
pub fn probe_state_db(home: &Path) -> Value {
    let path = home.join("state.db");
    if !path.exists() {
        return check("ok", Some("not initialized"), Vec::new());
    }
    let open =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY);
    match open {
        Ok(conn) => {
            let _ = conn.busy_timeout(std::time::Duration::from_secs(1));
            let _ = conn.pragma_update(None, "query_only", "ON");
            match conn.query_row("SELECT name FROM sqlite_master LIMIT 1", [], |row| {
                row.get::<_, String>(0)
            }) {
                // An empty database still answers the schema query with
                // no rows — that is healthy, not corrupt.
                Ok(_) | Err(rusqlite::Error::QueryReturnedNoRows) => check("ok", None, Vec::new()),
                Err(_) => check("degraded", Some("sqlite_error"), Vec::new()),
            }
        }
        Err(_) => check("degraded", Some("sqlite_error"), Vec::new()),
    }
}

/// Parse-check the config file without interpreting any values
/// (hermes `_probe_config` over ulnclaw's `config.toml`).
pub fn probe_config(home: &Path) -> Value {
    let path = home.join("config.toml");
    if !path.exists() {
        return check("ok", Some("using defaults"), Vec::new());
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => match toml::from_str::<toml::Value>(&content) {
            Ok(value) if value.is_table() => check("ok", None, Vec::new()),
            Ok(_) => check("degraded", Some("top level is not a table"), Vec::new()),
            Err(_) => check("degraded", Some("invalid config"), Vec::new()),
        },
        Err(_) => check("degraded", Some("unreadable config"), Vec::new()),
    }
}

/// A gateway with no configured model cannot run turns (hermes
/// `_probe_model` parity).
pub fn probe_model(configured_model: &str) -> Value {
    if configured_model.trim().is_empty() {
        check("degraded", None, Vec::new())
    } else {
        check("ok", None, Vec::new())
    }
}

/// Filesystem headroom under the home directory (hermes
/// `_probe_disk`, `shutil.disk_usage` semantics).
pub fn probe_disk(home: &Path) -> Value {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_path = match std::ffi::CString::new(home.as_os_str().as_bytes()) {
            Ok(path) => path,
            Err(_) => return check("degraded", Some("bad_path"), Vec::new()),
        };
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if rc != 0 {
            return check("degraded", Some("statvfs_failed"), Vec::new());
        }
        let block = stat.f_frsize as u64;
        let total = stat.f_blocks as u64 * block;
        let used = stat.f_blocks.saturating_sub(stat.f_bfree) as u64 * block;
        let free = stat.f_bavail as u64 * block;
        let used_percent = if total == 0 {
            0.0
        } else {
            ((used as f64 / total as f64) * 1000.0).round() / 10.0
        };
        let status = if used_percent >= DISK_DEGRADED_PERCENT {
            "degraded"
        } else {
            "ok"
        };
        check(
            status,
            None,
            vec![
                ("used_percent", json!(used_percent)),
                ("free_bytes", json!(free)),
            ],
        )
    }
    #[cfg(not(unix))]
    {
        let _ = home;
        check("degraded", Some("unsupported_platform"), Vec::new())
    }
}

/// Gateway liveness + platform connectivity rollup (hermes
/// `_probe_gateway`): ok while the gateway is running/draining, with
/// connected-vs-configured platform counts.
pub fn probe_gateway(gateway_state: &str, rows: &[(&str, &str, String)]) -> Value {
    let configured = rows
        .iter()
        .filter(|row| row.2 != "disabled" && row.2 != "not_configured")
        .count();
    let connected = rows.iter().filter(|row| row.2 == "connected").count();
    let status = if matches!(gateway_state, "running" | "draining") {
        "ok"
    } else {
        "degraded"
    };
    check(
        status,
        None,
        vec![
            ("state", json!(gateway_state)),
            ("connected_platforms", json!(connected)),
            ("platforms", json!(configured)),
        ],
    )
}

/// Assemble the full readiness report (hermes
/// `collect_runtime_readiness`): overall status is "ok" only when
/// every check is.
pub fn collect_runtime_readiness(
    home: &Path,
    configured_model: &str,
    gateway_state: &str,
    platform_rows: &[(&str, &str, String)],
    active_api_runs: usize,
    queued_prompt_depth: usize,
) -> Value {
    let mut checks = serde_json::Map::new();
    checks.insert("state_db".into(), probe_state_db(home));
    checks.insert("config".into(), probe_config(home));
    checks.insert("model".into(), probe_model(configured_model));
    checks.insert("disk".into(), probe_disk(home));
    checks.insert(
        "gateway".into(),
        probe_gateway(gateway_state, platform_rows),
    );
    checks.insert(
        "background_queues".into(),
        check(
            "ok",
            None,
            vec![
                ("active_api_runs", json!(active_api_runs)),
                ("queued_prompts", json!(queued_prompt_depth)),
            ],
        ),
    );
    let overall = if checks
        .values()
        .all(|item| item.get("status").and_then(Value::as_str) == Some("ok"))
    {
        "ok"
    } else {
        "degraded"
    };
    json!({ "status": overall, "checks": Value::Object(checks) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(value: &Value) -> &str {
        value.get("status").and_then(Value::as_str).unwrap()
    }

    #[test]
    fn state_db_missing_reads_not_initialized() {
        let dir = tempfile::tempdir().unwrap();
        let probe = probe_state_db(dir.path());
        assert_eq!(status_of(&probe), "ok");
        assert_eq!(probe["detail"], "not initialized");
    }

    #[test]
    fn state_db_healthy_store_reads_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::session::SqliteSessionStore::open(dir.path().join("state.db"))
            .expect("store opens");
        drop(store);
        let probe = probe_state_db(dir.path());
        assert_eq!(status_of(&probe), "ok", "{probe}");
    }

    #[test]
    fn state_db_corrupt_file_reads_degraded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.db"), b"this is not sqlite").unwrap();
        let probe = probe_state_db(dir.path());
        assert_eq!(status_of(&probe), "degraded", "{probe}");
    }

    #[test]
    fn config_missing_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let probe = probe_config(dir.path());
        assert_eq!(status_of(&probe), "ok");
        assert_eq!(probe["detail"], "using defaults");
    }

    #[test]
    fn config_valid_toml_reads_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[model]\nname = \"anything\"\n",
        )
        .unwrap();
        let probe = probe_config(dir.path());
        assert_eq!(status_of(&probe), "ok", "{probe}");
    }

    #[test]
    fn config_invalid_toml_reads_degraded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[model\nname = ").unwrap();
        let probe = probe_config(dir.path());
        assert_eq!(status_of(&probe), "degraded", "{probe}");
    }

    #[test]
    fn model_probe_gates_empty_model() {
        assert_eq!(status_of(&probe_model("gpt-x")), "ok");
        assert_eq!(status_of(&probe_model("   ")), "degraded");
    }

    #[test]
    fn disk_probe_reports_percent_and_free_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let probe = probe_disk(dir.path());
        // The CI disk may legitimately be above the degraded line;
        // assert structure, not a status.
        assert!(matches!(status_of(&probe), "ok" | "degraded"), "{probe}");
        #[cfg(unix)]
        {
            assert!(probe.get("used_percent").is_some(), "{probe}");
            assert!(probe.get("free_bytes").is_some(), "{probe}");
        }
    }

    #[test]
    fn gateway_probe_counts_connected_platforms() {
        let rows: Vec<(&str, &str, String)> = vec![
            ("telegram", "Telegram", "connected".to_string()),
            ("discord", "Discord", "disabled".to_string()),
            ("slack", "Slack", "not_configured".to_string()),
            ("matrix", "Matrix", "exited".to_string()),
        ];
        let probe = probe_gateway("running", &rows);
        assert_eq!(status_of(&probe), "ok");
        assert_eq!(probe["connected_platforms"], 1);
        assert_eq!(probe["platforms"], 2);
        let stopped = probe_gateway("stopped", &rows);
        assert_eq!(status_of(&stopped), "degraded");
    }

    #[test]
    fn collect_reports_overall_status() {
        let dir = tempfile::tempdir().unwrap();
        let rows: Vec<(&str, &str, String)> = Vec::new();
        let report = collect_runtime_readiness(dir.path(), "test-model", "running", &rows, 0, 0);
        assert!(report["checks"]["state_db"].is_object(), "{report}");
        assert!(
            report["checks"]["background_queues"].is_object(),
            "{report}"
        );
        assert_eq!(report["status"], "ok", "{report}");

        let degraded = collect_runtime_readiness(dir.path(), "", "running", &rows, 0, 0);
        assert_eq!(degraded["status"], "degraded", "{degraded}");
    }
}

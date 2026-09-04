//! Persistent MCP tool-schema cache for lazy server startup — port of
//! hermes' `tools/mcp_schema_cache.py` (+ the lazy-registration wiring in
//! `tools/mcp_tool.py`).
//!
//! Stores per-server tool manifests on disk so ulnclaw can register MCP
//! tools into the registry WITHOUT spawning the stdio child process at
//! startup. Cache entries are keyed by server name + a fingerprint of the
//! connection config (command/args/url/tools filters). The cache file is
//! trusted input on the lazy registration path, so it is written 0600 in a
//! 0700 `cache/` directory (hermes precedent: `tools/registry.py`
//! `_save_discovery_cache`).

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const CACHE_FILENAME: &str = "mcp_schema_cache.json";

/// Global lock around cache file I/O (hermes `_cache_lock`).
static CACHE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Default cache location under the ulnclaw home.
pub fn cache_path() -> PathBuf {
    cache_path_in(&crate::config::ulnclaw_home())
}

/// Cache location under an explicit home (profile homes, tests).
pub fn cache_path_in(home: &Path) -> PathBuf {
    home.join("cache").join(CACHE_FILENAME)
}

/// Stable hash of the connection-defining parts of an MCP server config.
///
/// Same payload shape as hermes `config_fingerprint` (sorted keys, compact
/// separators, sha256 hex[:16]). `url`/`transport` carry the real values
/// for remote servers (Streamable HTTP / SSE); the tools filters stay in
/// the payload for format compatibility with hermes cache files.
pub fn config_fingerprint(config: &super::McpServerConfig) -> String {
    let payload = json!({
        "command": config.command,
        "args": config.args,
        "url": config.url,
        "transport": config.transport.as_deref().unwrap_or(if config.url.is_some() { "streamable-http" } else { "stdio" }),
        "tools_include": [],
        "tools_exclude": [],
    });
    let raw = serde_json::to_string(&payload).expect("static payload serializes");
    let digest = Sha256::digest(raw.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    hex[..16].to_string()
}

fn load_all_in(home: &Path) -> Map<String, Value> {
    let path = cache_path_in(home);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Atomic write: stage next to the target, 0600, rename over (hermes
/// `utils.atomic_json_write`).
fn save_all_in(home: &Path, data: &Map<String, Value>) -> Result<(), String> {
    let path = cache_path_in(home);
    let dir = path
        .parent()
        .ok_or_else(|| "cache path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok();
    }
    let staged = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let body = serde_json::to_string(&Value::Object(data.clone()))
        .map_err(|e| format!("serialize cache: {}", e))?;
    std::fs::write(&staged, body).map_err(|e| format!("write {}: {}", staged.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).ok();
    }
    std::fs::rename(&staged, &path).map_err(|e| format!("rename {}: {}", path.display(), e))
}

/// Return the cached entry for `server_name` when its fingerprint matches,
/// else `None`.
pub fn get_cached_entry(server_name: &str, fingerprint: &str) -> Option<Value> {
    get_cached_entry_in(&crate::config::ulnclaw_home(), server_name, fingerprint)
}

/// `get_cached_entry` against an explicit home.
pub fn get_cached_entry_in(home: &Path, server_name: &str, fingerprint: &str) -> Option<Value> {
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let entry = load_all_in(home).get(server_name).cloned()?;
    let Value::Object(map) = &entry else {
        return None;
    };
    if map.get("fingerprint").and_then(|v| v.as_str()) != Some(fingerprint) {
        return None;
    }
    Some(entry)
}

pub fn has_cached_entry(server_name: &str, fingerprint: &str) -> bool {
    get_cached_entry(server_name, fingerprint).is_some()
}

/// Persist tool schemas after a successful live connect.
///
/// Write-through fires on every registration (reconnects, refreshes); skip
/// the load-all+rewrite churn when the entry is identical to what is
/// already on disk (hermes `write_cache_entry`).
pub fn write_cache_entry(
    server_name: &str,
    fingerprint: &str,
    tools: &[Value],
    utility_tools: &[Value],
) -> Result<(), String> {
    write_cache_entry_in(
        &crate::config::ulnclaw_home(),
        server_name,
        fingerprint,
        tools,
        utility_tools,
    )
}

/// `write_cache_entry` against an explicit home.
pub fn write_cache_entry_in(
    home: &Path,
    server_name: &str,
    fingerprint: &str,
    tools: &[Value],
    utility_tools: &[Value],
) -> Result<(), String> {
    let entry = json!({
        "fingerprint": fingerprint,
        "tools": tools,
        "utility_tools": utility_tools,
    });
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut data = load_all_in(home);
    if data.get(server_name) == Some(&entry) {
        return Ok(());
    }
    data.insert(server_name.to_string(), entry);
    save_all_in(home, &data)
}

/// Drop one server's entry; returns true if anything was removed.
pub fn clear_cache_entry(server_name: &str) -> bool {
    clear_cache_entry_in(&crate::config::ulnclaw_home(), server_name)
}

/// `clear_cache_entry` against an explicit home.
pub fn clear_cache_entry_in(home: &Path, server_name: &str) -> bool {
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut data = load_all_in(home);
    if data.remove(server_name).is_some() {
        let _ = save_all_in(home, &data);
        true
    } else {
        false
    }
}

/// Cached MCP tool dicts (name, description, inputSchema).
pub fn tools_from_cache_entry(entry: &Value) -> Vec<Value> {
    entry
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Cached utility-tool dicts (schema + handler_key); ulnclaw's stdio client
/// does not expose resources/prompts yet, so this stays empty in practice.
pub fn utility_tools_from_cache_entry(entry: &Value) -> Vec<Value> {
    entry
        .get("utility_tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::McpServerConfig;
    use std::collections::HashMap;

    fn cfg(command: &str, args: Vec<&str>) -> McpServerConfig {
        McpServerConfig {
            name: "test".into(),
            command: command.into(),
            args: args.into_iter().map(String::from).collect(),
            env: HashMap::new(),
            url: None,
            transport: None,
            headers: HashMap::new(),
            auth: None,
            oauth: crate::mcp::oauth::McpOAuthConfig::default(),
            lazy: false,
            enabled: true,
        }
    }

    #[test]
    fn fingerprint_is_stable_and_hermes_shaped() {
        let fp = config_fingerprint(&cfg("echo", vec![]));
        assert_eq!(fp.len(), 16);
        assert_eq!(fp, config_fingerprint(&cfg("echo", vec![])));
        // Pinned vector computed with hermes' python
        // json.dumps(payload, sort_keys=True) + sha256 hex[:16].
        assert_eq!(fp, "d0970df106c2dcd7");
    }

    #[test]
    fn fingerprint_covers_remote_url_and_transport() {
        let mut stdio_cfg = cfg("echo", vec![]);
        let mut http_cfg = cfg("", vec![]);
        http_cfg.url = Some("https://mcp.example.com/sse".into());
        let mut sse_cfg = http_cfg.clone();
        sse_cfg.transport = Some("sse".into());
        // url and transport both participate in the fingerprint.
        assert_ne!(
            config_fingerprint(&stdio_cfg),
            config_fingerprint(&http_cfg)
        );
        assert_ne!(config_fingerprint(&http_cfg), config_fingerprint(&sse_cfg));
        // Same url but different command stays distinct too.
        stdio_cfg.url = Some("https://mcp.example.com/sse".into());
        assert_ne!(
            config_fingerprint(&stdio_cfg),
            config_fingerprint(&http_cfg)
        );
    }

    #[test]
    fn fingerprint_changes_with_connection_config() {
        let base = config_fingerprint(&cfg("echo", vec![]));
        assert_ne!(base, config_fingerprint(&cfg("echo", vec!["hi"])));
        assert_ne!(base, config_fingerprint(&cfg("cat", vec![])));
        // env is NOT part of the fingerprint (hermes parity).
        let mut with_env = cfg("echo", vec![]);
        with_env.env.insert("API_KEY".into(), "secret".into());
        assert_eq!(base, config_fingerprint(&with_env));
    }

    fn tool(name: &str) -> Value {
        json!({
            "name": name,
            "description": format!("{name} tool"),
            "inputSchema": {"type": "object", "properties": {}},
        })
    }

    #[test]
    fn write_read_roundtrip_and_fingerprint_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let fp = "fp123";
        assert!(get_cached_entry_in(home, "srv", fp).is_none());

        write_cache_entry_in(home, "srv", fp, &[tool("a"), tool("b")], &[]).unwrap();
        let entry = get_cached_entry_in(home, "srv", fp).expect("entry present");
        assert_eq!(tools_from_cache_entry(&entry).len(), 2);
        assert!(utility_tools_from_cache_entry(&entry).is_empty());
        // Fingerprint mismatch is a miss.
        assert!(get_cached_entry_in(home, "srv", "other").is_none());
    }

    #[test]
    fn identical_write_skips_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_cache_entry_in(home, "srv", "fp", &[tool("a")], &[]).unwrap();
        let path = cache_path_in(home);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_cache_entry_in(home, "srv", "fp", &[tool("a")], &[]).unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "identical entry must not be rewritten");
        // A changed entry IS rewritten.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_cache_entry_in(home, "srv", "fp", &[tool("a"), tool("b")], &[]).unwrap();
        let after2 = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(after2 > before);
    }

    #[test]
    fn clear_entry_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_cache_entry_in(home, "srv", "fp", &[tool("a")], &[]).unwrap();
        assert!(clear_cache_entry_in(home, "srv"));
        assert!(get_cached_entry_in(home, "srv", "fp").is_none());
        assert!(!clear_cache_entry_in(home, "srv"));
    }

    #[test]
    fn corrupt_cache_file_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join("cache")).unwrap();
        std::fs::write(cache_path_in(home), "{ not json").unwrap();
        assert!(get_cached_entry_in(home, "srv", "fp").is_none());
        // And a subsequent write heals the file.
        write_cache_entry_in(home, "srv", "fp", &[tool("a")], &[]).unwrap();
        assert!(get_cached_entry_in(home, "srv", "fp").is_some());
    }

    #[test]
    fn non_object_entries_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join("cache")).unwrap();
        std::fs::write(
            cache_path_in(home),
            json!({"srv": {"fingerprint": "fp", "tools": "nope"}}).to_string(),
        )
        .unwrap();
        let entry = get_cached_entry_in(home, "srv", "fp").unwrap();
        assert!(tools_from_cache_entry(&entry).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cache_file_is_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_cache_entry_in(home, "srv", "fp", &[tool("a")], &[]).unwrap();
        let mode = std::fs::metadata(cache_path_in(home))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(home.join("cache"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }
}

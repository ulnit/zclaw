//! Tool registry — the 12 native tools of zclaw-mobile (reconstructed from
//! the shipped libzclaw.so symbols): file_read, file_write, file_edit,
//! glob_search, content_search, shell, http_request, web_fetch,
//! web_search_tool, memory_store, memory_recall, memory_forget, datetime.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ── v0.8.4 backports (zeroclaw-labs/zeroclaw) ──────────────────────────────
// #9824: cap per-result content AND total tool output to keep mobile context
//        windows from blowing up; realistic browser headers + throttle on DDG.
const TOOL_OUTPUT_CAP: usize = 8_000;      // any single tool result
const SEARCH_RESULT_CAP: usize = 1_200;    // per web_search result
const SEARCH_TOTAL_CAP: usize = 6_000;     // all web_search output
const SHELL_TIMEOUT_SECS: u64 = 60;        // #9105 pattern: bounded subprocess
const DDG_MIN_INTERVAL_MS: u64 = 1_500;    // #9824: throttle consecutive scrapes

static LAST_DDG_SCRAPE: Mutex<Option<Instant>> = Mutex::new(None);

fn cap_output(mut s: String) -> String {
    if s.len() > TOOL_OUTPUT_CAP {
        s.truncate(TOOL_OUTPUT_CAP);
        s.push_str("\n[output truncated]");
    }
    s
}

fn cap_result(s: &str) -> String {
    if s.len() > SEARCH_RESULT_CAP {
        // truncate at a UTF-8 char boundary (stable API)
        let mut end = SEARCH_RESULT_CAP;
        while end > 0 && !s.is_char_boundary(end) { end -= 1; }
        format!("{}…", &s[..end])
    } else {
        s.to_string()
    }
}

fn ddg_headers() -> [(&'static str, &'static str); 3] {
    // #9824: rotate a realistic browser UA instead of the bare reqwest agent
    [
        ("User-Agent", "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36"),
        ("Accept", "text/html,application/xhtml+xml"),
        ("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8"),
    ]
}

fn ddg_throttle() {
    let mut last = LAST_DDG_SCRAPE.lock().unwrap();
    if let Some(t) = *last {
        let elapsed = t.elapsed();
        if elapsed < Duration::from_millis(DDG_MIN_INTERVAL_MS) {
            std::thread::sleep(Duration::from_millis(DDG_MIN_INTERVAL_MS) - elapsed);
        }
    }
    *last = Some(Instant::now());
}

pub struct ToolCtx<'a> {
    pub workspace: PathBuf,
    pub memory: &'a crate::memory::MemoryStore,
}

/// JSON-schema tool definitions for the chat-completions `tools` parameter.
pub fn tool_schemas() -> Value {
    fn t(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {
                    "type": "object",
                    "properties": props,
                    "required": required,
                }
            }
        })
    }
    let mut list: Vec<Value> = Vec::new();
    list.push(t("file_read", "Read file contents with line numbers. Supports partial reading via offset and limit.",
          json!({
            "path": {"type":"string","description":"Path to the file. Relative paths resolve from workspace."},
            "offset": {"type":"integer","description":"Starting line number (1-based, default: 1)"},
            "limit": {"type":"integer","description":"Maximum number of lines to return (default: all)"}
          }), &["path"]));
    list.push(t("file_write", "Write contents to a file in the workspace.",
          json!({
            "path": {"type":"string","description":"Path to the file. Relative paths resolve from workspace."},
            "content": {"type":"string","description":"Content to write to the file"}
          }), &["path","content"]));
    list.push(t("file_edit", "Edit a file by replacing an exact string match with new content.",
          json!({
            "path": {"type":"string","description":"Path to the file."},
            "old_string": {"type":"string","description":"The exact text to find and replace (must appear exactly once in the file)"},
            "new_string": {"type":"string","description":"The replacement text (empty string to delete the matched text)"}
          }), &["path","old_string","new_string"]));
    list.push(t("glob_search", "Search for files matching a glob pattern within the workspace. Returns sorted matching paths.",
          json!({"pattern": {"type":"string","description":"Glob pattern to match files, e.g. '**/*.rs'"}}), &["pattern"]));
    list.push(t("content_search", "Search file contents by regex pattern. Returns matching lines with file paths and line numbers.",
          json!({
            "pattern": {"type":"string","description":"Regex pattern to search for"},
            "path": {"type":"string","description":"Directory to search in (optional, defaults to workspace)"}
          }), &["pattern"]));
    list.push(t("shell", "Execute a shell command in the workspace directory.",
          json!({"command": {"type":"string","description":"The shell command to execute"}}), &["command"]));
    list.push(t("http_request", "Make HTTP requests to external APIs. Supports GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS.",
          json!({
            "url": {"type":"string","description":"HTTP or HTTPS URL to request"},
            "method": {"type":"string","description":"HTTP method (GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS)"},
            "headers": {"type":"object","description":"Optional HTTP headers as key-value pairs"},
            "body": {"type":"string","description":"Optional request body (for POST, PUT, PATCH requests)"}
          }), &["url"]));
    list.push(t("web_fetch", "Fetch a web page and return its content as clean plain text. Only GET requests; follows redirects.",
          json!({"url": {"type":"string","description":"The HTTP or HTTPS URL to fetch"}}), &["url"]));
    list.push(t("web_search_tool", "Search the web for information. Returns relevant search results with titles, URLs, and descriptions.",
          json!({"query": {"type":"string","description":"The search query. Be specific for better results."}}), &["query"]));
    list.push(t("memory_store", "Store a fact, preference, or note in long-term memory.",
          json!({
            "key": {"type":"string","description":"Unique key for this memory"},
            "content": {"type":"string","description":"The information to remember"},
            "category": {"type":"string","description":"Memory category: 'core' (permanent), 'daily' (session), 'conversation' (chat), or a custom category name. Defaults to 'core'."}
          }), &["key","content"]));
    list.push(t("memory_recall", "Search long-term memory for relevant facts, preferences, or context. Returns scored results ranked by relevance.",
          json!({
            "query": {"type":"string","description":"Keywords or phrase to search for in memory"},
            "limit": {"type":"integer","description":"Max results to return (default: 5)"}
          }), &["query"]));
    list.push(t("memory_forget", "Remove a memory by key. Returns whether the memory was found and removed.",
          json!({"key": {"type":"string","description":"The key of the memory to forget"}}), &["key"]));
    list.push(t("datetime", "Get the current date and time.", json!({}), &[]));
    Value::Array(list)
}

/// Execute one tool; returns the result string (capped, #9824).
pub fn execute(ctx: &ToolCtx, name: &str, args: &Value) -> String {
    let raw = match name {
        "file_read" => file_read(ctx, args),
        "file_write" => file_write(ctx, args),
        "file_edit" => file_edit(ctx, args),
        "glob_search" => glob_search(ctx, args),
        "content_search" => content_search(ctx, args),
        "shell" => shell(ctx, args),
        "http_request" => http_request_blocking(args),
        "web_fetch" => web_fetch_blocking(args),
        "web_search_tool" => web_search_blocking(args),
        "memory_store" => memory_store(ctx, args),
        "memory_recall" => memory_recall(ctx, args),
        "memory_forget" => memory_forget(ctx, args),
        "datetime" => datetime(),
        other => format!("Unknown tool: {}", other),
    };
    cap_output(raw)
}

// ── helpers ──

fn resolve(ctx: &ToolCtx, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { ctx.workspace.join(path) }
}

fn file_read(ctx: &ToolCtx, args: &Value) -> String {
    let Some(path) = args["path"].as_str() else { return "Missing 'path' parameter".into() };
    let full = resolve(ctx, path);
    let content = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(e) => return format!("Error reading file: {}", e),
    };
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(2000) as usize;
    let lines: Vec<String> = content
        .lines()
        .skip(offset - 1)
        .take(limit)
        .enumerate()
        .map(|(i, l)| format!("{}|{}", offset + i, l))
        .collect();
    lines.join("\n")
}

fn file_write(ctx: &ToolCtx, args: &Value) -> String {
    let Some(path) = args["path"].as_str() else { return "Missing 'path' parameter".into() };
    let content = args["content"].as_str().unwrap_or_default();
    let full = resolve(ctx, path);
    if let Some(parent) = full.parent() { std::fs::create_dir_all(parent).ok(); }
    match std::fs::write(&full, content) {
        Ok(_) => format!("Wrote {} bytes to {}", content.len(), path),
        Err(e) => format!("Error writing file: {}", e),
    }
}

fn file_edit(ctx: &ToolCtx, args: &Value) -> String {
    let Some(path) = args["path"].as_str() else { return "Missing 'path' parameter".into() };
    let Some(old) = args["old_string"].as_str() else { return "Missing 'old_string' parameter".into() };
    if old.is_empty() { return "old_string must not be empty".into() }
    let new = args["new_string"].as_str().unwrap_or_default();
    let full = resolve(ctx, path);
    let content = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(e) => return format!("Error reading file: {}", e),
    };
    let count = content.matches(old).count();
    if count == 0 { return "old_string not found in file".into() }
    if count > 1 { return format!("old_string found {} times; must appear exactly once", count) }
    let updated = content.replacen(old, new, 1);
    match std::fs::write(&full, updated) {
        Ok(_) => format!("Edited {}", path),
        Err(e) => format!("Error writing file: {}", e),
    }
}

fn glob_search(ctx: &ToolCtx, args: &Value) -> String {
    let Some(pattern) = args["pattern"].as_str() else { return "Missing 'pattern' parameter".into() };
    let full_pattern = ctx.workspace.join(pattern).to_string_lossy().replace('\\', "/");
    match glob::glob(&full_pattern) {
        Ok(paths) => {
            let mut out: Vec<String> = paths
                .filter_map(|p| p.ok())
                .take(100)
                .filter_map(|p| p.strip_prefix(&ctx.workspace).ok().map(|r| r.to_string_lossy().replace('\\', "/")))
                .collect();
            out.sort();
            if out.is_empty() { "No files matched".into() } else { out.join("\n") }
        }
        Err(e) => format!("Invalid glob pattern: {}", e),
    }
}

fn content_search(ctx: &ToolCtx, args: &Value) -> String {
    let Some(pattern) = args["pattern"].as_str() else { return "Missing 'pattern' parameter".into() };
    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return format!("Invalid regex: {}", e),
    };
    let dir = args["path"].as_str().map(|p| resolve(ctx, p)).unwrap_or_else(|| ctx.workspace.clone());
    let mut results = Vec::new();
    walk(&dir, &mut |file: &Path| {
        if results.len() >= 100 { return; }
        if let Ok(content) = std::fs::read_to_string(file) {
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    let rel = file.strip_prefix(&ctx.workspace).unwrap_or(file).to_string_lossy();
                    results.push(format!("{}:{}:{}", rel, i + 1, line.trim()));
                    if results.len() >= 100 { break; }
                }
            }
        }
    });
    if results.is_empty() { "No matches".into() } else { results.join("\n") }
}

fn walk(dir: &Path, cb: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !matches!(name.as_str(), ".git" | "node_modules" | "target" | "build") {
                walk(&path, cb);
            }
        } else {
            cb(&path);
        }
    }
}

fn shell(ctx: &ToolCtx, args: &Value) -> String {
    let Some(command) = args["command"].as_str() else { return "Missing 'command' parameter".into() };
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&ctx.workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("Failed to execute: {}", e),
    };
    // Bounded wait (mobile agents must not hang a chat turn, cf. #9105)
    let deadline = Instant::now() + Duration::from_secs(SHELL_TIMEOUT_SECS);
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    child.kill().ok();
                    child.wait().ok();
                    return format!("[shell timed out after {}s and was killed]", SHELL_TIMEOUT_SECS);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return format!("Failed to wait for command: {}", e),
        }
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return format!("Failed to read output: {}", e),
    };
    let mut out = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() { out.push_str(&stdout); }
    if !stderr.is_empty() { out.push_str("\n[stderr]\n"); out.push_str(&stderr); }
    if out.is_empty() { out = format!("(exit code {})", output.status.code().unwrap_or(-1)); }
    out
}

fn http_request_blocking(args: &Value) -> String {
    let Some(url) = args["url"].as_str() else { return "Missing 'url' parameter".into() };
    let method = args["method"].as_str().unwrap_or("GET").to_uppercase();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let mut req = match method.as_str() {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        "HEAD" => client.head(url),
        _ => client.get(url),
    };
    if let Some(headers) = args["headers"].as_object() {
        for (k, v) in headers {
            if let Some(vs) = v.as_str() {
                req = req.header(k.as_str(), vs);
            }
        }
    }
    if let Some(body) = args["body"].as_str() {
        req = req.body(body.to_string());
    }
    match req.send() {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            format!("HTTP {}\n{}", status, &text[..text.len().min(8000)])
        }
        Err(e) => format!("Request failed: {}", e),
    }
}

fn web_fetch_blocking(args: &Value) -> String {
    let Some(url) = args["url"].as_str() else { return "Missing 'url' parameter".into() };
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("ZeroClaw/0.1 (web_fetch)")
        .build()
        .unwrap();
    match client.get(url).send() {
        Ok(resp) => {
            let text = resp.text().unwrap_or_default();
            // crude HTML-to-text: strip tags
            if text.trim_start().starts_with('<') {
                let no_script = regex::Regex::new(r"(?is)<(script|style)[^>]*>.*?</\1>").map(|re| re.replace_all(&text, "").into_owned()).unwrap_or(text.clone());
                let stripped = regex::Regex::new(r"(?s)<[^>]+>").map(|re| re.replace_all(&no_script, " ").into_owned()).unwrap_or(no_script);
                let compact = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
                compact[..compact.len().min(8000)].to_string()
            } else {
                text[..text.len().min(8000)].to_string()
            }
        }
        Err(e) => format!("Fetch failed: {}", e),
    }
}

fn web_search_blocking(args: &Value) -> String {
    let Some(query) = args["query"].as_str() else { return "Missing 'query' parameter".into() };
    // #9824: Brave Search API if configured, else DuckDuckGo HTML fallback.
    // Every path caps per-result content AND total output.
    if let Ok(brave_key) = std::env::var("BRAVE_API_KEY") {
        if !brave_key.is_empty() {
            let client = reqwest::blocking::Client::new();
            let url = format!("https://api.search.brave.com/res/v1/web/search?q={}", urlencoding::encode(query));
            if let Ok(resp) = client.get(&url).header("X-Subscription-Token", brave_key).header("Accept", "application/json").send() {
                if let Ok(val) = resp.json::<Value>() {
                    let results = val["web"]["results"].as_array().cloned().unwrap_or_default();
                    if results.is_empty() { return "Invalid Brave API response".into(); }
                    let joined = results.iter().take(5).map(|r| {
                        let line = format!("- {}\n  {}\n  {}",
                            cap_result(r["title"].as_str().unwrap_or("")),
                            r["url"].as_str().unwrap_or(""),
                            cap_result(r["description"].as_str().unwrap_or("")));
                        line
                    }).collect::<Vec<_>>().join("\n");
                    return cap_search_total(joined);
                }
            }
            return "Invalid Brave API response".into();
        }
    }
    // fallback: DuckDuckGo lite (#9824: realistic headers + throttle)
    ddg_throttle();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding::encode(query));
    let mut req = client.get(&url);
    for (k, v) in ddg_headers() { req = req.header(k, v); }
    match req.send() {
        Ok(resp) => {
            if !resp.status().is_success() {
                // #9824: give the model an accurate next step instead of silent junk
                return format!("Search failed: DuckDuckGo returned HTTP {} (possibly rate-limited). Try again later or rephrase the query.", resp.status());
            }
            let html = resp.text().unwrap_or_default();
            if let Ok(re) = regex::Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#) {
                let items: Vec<String> = re.captures_iter(&html).take(5).map(|c| {
                    let title = regex::Regex::new(r"(?s)<[^>]+>").map(|r2| r2.replace_all(&c[2], "").into_owned()).unwrap_or(c[2].to_string());
                    format!("- {}\n  {}", cap_result(title.trim()), &c[1])
                }).collect();
                if items.is_empty() { "No results".into() } else { cap_search_total(items.join("\n")) }
            } else {
                "Search failed".into()
            }
        }
        Err(e) => format!("Search failed: {}", e),
    }
}

fn cap_search_total(mut s: String) -> String {
    if s.len() > SEARCH_TOTAL_CAP {
        s.truncate(SEARCH_TOTAL_CAP);
        s.push_str("\n[search output truncated]");
    }
    s
}

fn memory_store(ctx: &ToolCtx, args: &Value) -> String {
    let Some(key) = args["key"].as_str() else { return "Missing 'key' parameter".into() };
    let content = args["content"].as_str().unwrap_or_default();
    let category = args["category"].as_str().unwrap_or("core");
    match ctx.memory.store(key, content, category) {
        Ok(_) => format!("Stored memory '{}'", key),
        Err(e) => format!("Failed to store memory: {}", e),
    }
}

fn memory_recall(ctx: &ToolCtx, args: &Value) -> String {
    let Some(query) = args["query"].as_str() else { return "Missing 'query' parameter".into() };
    let limit = args["limit"].as_u64().unwrap_or(5) as usize;
    let results = ctx.memory.recall(query, limit);
    if results.is_empty() { return "No matching memories".into(); }
    results.iter().map(|m| format!("[{}|{}] {} = {}", m.category, m.mem_type, m.key, m.content)).collect::<Vec<_>>().join("\n")
}

fn memory_forget(ctx: &ToolCtx, args: &Value) -> String {
    let Some(key) = args["key"].as_str() else { return "Missing 'key' parameter".into() };
    match ctx.memory.forget(key) {
        Ok(removed) => {
            if removed { format!("Forgot memory '{}'", key) } else { format!("Memory '{}' not found", key) }
        }
        Err(e) => format!("Failed to forget memory: {}", e),
    }
}

fn datetime() -> String {
    let now = chrono::Local::now();
    now.format("%Y-%m-%d %H:%M:%S %Z (%A)").to_string()
}

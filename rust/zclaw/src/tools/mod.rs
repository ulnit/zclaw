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

/// 🔴 按 UTF-8 字符边界安全截断到 `max_bytes` 以内。
///
/// 为什么必须自己写：Rust 的 `&s[..n]` 与 `String::truncate(n)` 在 `n`
/// 落在多字节字符中间时**直接 panic**。而本库以 `panic = "abort"` 编译
/// （Cargo.toml [profile.release]），且 FFI 层（ffi.rs）没有 catch_unwind
/// —— abort 时 catch_unwind 本就无效。所以任何一次 panic 都**直接杀死
/// 整个 App 进程**，不是返回错误字符串。
///
/// 实测触发：web_fetch 抓中文网页，`compact[..8000]` 落在 '气'（bytes
/// 7998..8001）内部 → panic → App abort。中文/emoji 内容下这是必然事件，
/// 不是边界情况。
///
/// 返回 (截断后的切片, 是否发生截断)，让调用方决定是否加省略提示。
#[inline]
pub(crate) fn truncate_utf8(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes { return (s, false); }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    (&s[..end], true)
}

fn cap_output(mut s: String) -> String {
    // 不能用 s.truncate(CAP)：CAP 落在中文字符中间会 panic → abort 整个 App
    if s.len() > TOOL_OUTPUT_CAP {
        let (head, _) = truncate_utf8(&s, TOOL_OUTPUT_CAP);
        s = format!("{}\n[output truncated]", head);
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
    // 🔴 工具描述必须如实说明「可能搜不到」，不能过度承诺。
    // 此前写的是 "Returns relevant search results with titles, URLs, and descriptions."
    // —— 暗示一定有相关结果，于是模型对冷门本地 POI（小区名/小商户）搜不到时，
    // 会认为「是我措辞不对」而无限换词重搜（后端实测 prompt_tokens 3.3k→18k、
    // 9 轮零正文）。改成明确告知：搜索引擎对冷门/本地查询常返回不相关页面，
    // 结果需要自己判断相关性，搜不到就用自身知识回答。与 system prompt 的策略呼应。
    list.push(t("web_search_tool", "Search the web. Returns titles, URLs and snippets, but results may be irrelevant: for obscure or highly local queries (a residential compound, a small shop, a street) search engines often return generic city/travel pages instead. Judge relevance yourself — if the results do not mention the specific thing you asked about, treat it as not found; try at most one reworded query, then answer from your own knowledge and state that it is unverified.",
          json!({"query": {"type":"string","description":"The search query. Prefer short keyword phrases over full questions."}}), &["query"]));
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
            format!("HTTP {}\n{}", status, truncate_utf8(&text, 8000).0)
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
                truncate_utf8(&compact, 8000).0.to_string()
            } else {
                truncate_utf8(&text, 8000).0.to_string()
            }
        }
        Err(e) => format!("Fetch failed: {}", e),
    }
}

/// 单个搜索源的失败记录（provider + 分类状态 + 细节），用于全源失败时汇总。
struct SearchFail {
    provider: &'static str,
    status: SearchStatus,
    detail: String,
}

/// 搜索失败分类（backport 自上游 zeroclaw v0.8.4 #8890）。
///
/// 为什么要分类：移动端 agent 拿到笼统的 "Search failed" 时无法决策——
/// 该重试？换词？还是放弃改走 web_fetch？把 HTTP 状态归类成稳定标签
/// 写进错误文本（`search_status=<tag>`），模型就能选对下一步。
/// 只列真正会产生 的三类（wire-or-remove，同上游）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchStatus {
    /// 403/429 等：被限流或反爬拦截，换词或稍后重试可能有效
    Blocked,
    /// 5xx / 连接失败 / DNS / TLS：源本身不可达，重试同一个源无意义
    Unavailable,
    /// 4xx（非限流）：请求本身有问题（参数/鉴权），重试不会变好
    ClientError,
}

impl SearchStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Unavailable => "unavailable",
            Self::ClientError => "client_error",
        }
    }
}

/// 把 HTTP 状态码映射为搜索失败类别。
fn classify_http_status(status: u16) -> SearchStatus {
    match status {
        403 | 429 => SearchStatus::Blocked,
        s if s >= 500 => SearchStatus::Unavailable,
        s if s >= 400 => SearchStatus::ClientError,
        _ => SearchStatus::Unavailable,
    }
}

fn web_search_blocking(args: &Value) -> String {
    let Some(query) = args["query"].as_str() else { return "Missing 'query' parameter".into() };
    // 搜索源优先级：
    //   1. Brave Search API —— 需 BRAVE_API_KEY 且 api.search.brave.com 在中国大陆不可达
    //   2. Bing（cn.bing.com）—— 中国大陆可达、无需密钥，故列为首选 HTML 抓取源
    //   3. DuckDuckGo HTML —— 境外兜底
    //
    // 🔴 历史 bug：此前只有 Brave + DuckDuckGo 两条路，而两者在中国大陆均不可达
    //    （实测 html.duckduckgo.com / api.search.brave.com 均 HTTP=000 超时，
    //    同时 cn.bing.com HTTP=200）。且三端均未设置 BRAVE_API_KEY
    //    （Android JNI / Harmony NAPI / iOS 都没有 setenv），所以必然落到 DDG，
    //    每次都要等满 20s 超时才返回 "Search failed" —— 工具在国内 100% 不可用。
    // 因此把 Bing 提到 DDG 之前，让国内请求直接走可达源。
    let mut failures: Vec<SearchFail> = Vec::new();

    if let Ok(brave_key) = std::env::var("BRAVE_API_KEY") {
        if !brave_key.is_empty() {
            match search_via_brave(query, &brave_key) {
                Ok(out) => return out,
                Err(f) => failures.push(f),
            }
        }
    }
    match search_via_bing(query) {
        Ok(out) => return out,
        Err(f) => failures.push(f),
    }
    match search_via_ddg(query) {
        Ok(out) => return out,
        Err(f) => failures.push(f),
    }

    // 全部源都失败：给出每个源的分类状态，让模型能判断「换词重试」
    // 还是「源不可达，别再试搜索」。
    let detail = failures.iter()
        .map(|f| format!("  - {}: search_status={} ({})", f.provider, f.status.as_str(), f.detail))
        .collect::<Vec<_>>()
        .join("\n");
    // 整体状态取最严重的一类：有 blocked 说明源活着但拦我们（换词/稍后重试有意义），
    // 全 unavailable 说明网络层不通（重试同一个源无意义）
    let overall = if failures.iter().any(|f| f.status == SearchStatus::Blocked) {
        SearchStatus::Blocked
    } else if !failures.is_empty() && failures.iter().all(|f| f.status == SearchStatus::ClientError) {
        SearchStatus::ClientError
    } else {
        SearchStatus::Unavailable
    };
    let hint = match overall {
        SearchStatus::Blocked => "所有搜索源都返回限流/拦截：稍后重试或换更短的查询词，不要连续重试。",
        SearchStatus::Unavailable => "所有搜索源均不可达（网络/DNS/TLS）：不要重试搜索，可改用 web_fetch 直接抓取已知 URL，或告知用户当前网络无法联网搜索。",
        SearchStatus::ClientError => "搜索请求被拒绝（参数或鉴权问题）：重试不会变好，检查 query 是否为空或过长。",
    };
    format!("Search failed: search_status={}\n{}\n{}", overall.as_str(), detail, hint)
}

/// Bing HTML 抓取。中国大陆可达（cn.bing.com HTTP=200），无需 API key。
/// Err 携带分类状态，供调用方汇总给模型。
fn search_via_bing(query: &str) -> Result<String, SearchFail> {
    const PROVIDER: &str = "bing";
    let fail = |detail: String, status: SearchStatus| -> SearchFail {
        SearchFail { provider: PROVIDER, status, detail }
    };
    ddg_throttle();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| fail(format!("client build: {}", e), SearchStatus::Unavailable))?;
    // cn.bing.com 对大陆网络返回 200；www.bing.com 会 302 跳转
    let url = format!("https://cn.bing.com/search?q={}&count=10", urlencoding::encode(query));
    let mut req = client.get(&url);
    for (k, v) in ddg_headers() { req = req.header(k, v); }
    let resp = req.send().map_err(|e| {
        // 连接失败/超时 = 源不可达（国内访问境外域名时的典型情况）
        fail(describe_transport_error(&e), SearchStatus::Unavailable)
    })?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        return Err(fail(format!("HTTP {}", code), classify_http_status(code)));
    }
    let html = resp.text().unwrap_or_default();
    if html.is_empty() {
        return Err(fail("empty response body".into(), SearchStatus::Unavailable));
    }

    // 结果结构（实测 cn.bing.com 2026-09）：
    //   <li class="b_algo" …>
    //     … <a class="tilk" aria-label="tianqi.com" href="https://www.tianqi.com/shanghai/today/"> …
    //     <h2 class="">标题文本（纯文本，h2 内**没有** <a>）</h2> … <cite>域名 › 路径</cite>
    // 🔴 关键：真实可点链接在 `class="tilk"` 的 a 上，**不在 h2 里**；
    //    `<cite>` 只是显示格式（"https://www.tianqi.com › shanghai › today"），
    //    把它当 URL 会让模型后续的 web_fetch 全部失败。
    // 按 b_algo 分块后在块内配对标题与真链接，正则只编译一次（循环内复用）。
    let re_tag = regex::Regex::new(r"(?s)<[^>]+>")
        .map_err(|e| fail(format!("regex: {}", e), SearchStatus::ClientError))?;
    let re_h2 = regex::Regex::new(r"(?s)<h2[^>]*>(.*?)</h2>")
        .map_err(|e| fail(format!("regex: {}", e), SearchStatus::ClientError))?;
    let re_tilk = regex::Regex::new(r#"(?s)<a[^>]*class="tilk"[^>]*href="([^"]+)""#)
        .map_err(|e| fail(format!("regex: {}", e), SearchStatus::ClientError))?;
    let re_href = regex::Regex::new(r#"(?s)<h2[^>]*>\s*<a[^>]*href="([^"]+)""#)
        .map_err(|e| fail(format!("regex: {}", e), SearchStatus::ClientError))?;
    let re_cite = regex::Regex::new(r"(?s)<cite[^>]*>(.*?)</cite>")
        .map_err(|e| fail(format!("regex: {}", e), SearchStatus::ClientError))?;
    // 摘要：Bing 的结果摘要在块内 <p>（对事实型问题「X 在 Y 对面吗」是主要信息载体，
    // 只给标题+URL 会迫使模型逐个 web_fetch，进而陷入反复重试）
    let re_p = regex::Regex::new(r"(?s)<p[^>]*>(.*?)</p>")
        .map_err(|e| fail(format!("regex: {}", e), SearchStatus::ClientError))?;
    let strip_tags = |s: &str| -> String { re_tag.replace_all(s, "").trim().to_string() };

    let mut items: Vec<String> = Vec::new();
    for block in html.split(r#"<li class="b_algo""#).skip(1).take(6) {
        let title = re_h2.captures(block)
            .map(|c| strip_tags(&c[1]))
            .unwrap_or_default();
        if title.is_empty() { continue; }
        // 真链接优先级：tilk(实测有效) > h2内嵌a(旧版式) > cite(显示格式，仅兜底)
        let url = re_tilk.captures(block).map(|c| c[1].to_string())
            .or_else(|| re_href.captures(block).map(|c| c[1].to_string()))
            .filter(|u| u.starts_with("http"))
            .or_else(|| re_cite.captures(block).map(|c| strip_tags(&c[1])))
            .unwrap_or_default();
        let snippet = re_p.captures(block)
            .map(|c| strip_tags(&c[1]))
            .unwrap_or_default();
        // 标题 / URL / 摘要三行（摘要封顶，避免撑爆 SEARCH_TOTAL_CAP）
        items.push(if snippet.is_empty() {
            format!("- {}\n  {}", cap_result(&title), url)
        } else {
            format!("- {}\n  {}\n  摘要: {}", cap_result(&title), url, cap_snippet(&snippet))
        });
        if items.len() >= 5 { break; }
    }
    if items.is_empty() {
        // 抓到了 HTML 但没解析出结果 —— 选择器变了或被反爬。
        // 🔴 别用 "b_no" 当反爬标记：实测降级页与正常页的 b_no 计数都是 1，
        //    它不是判别信号（此前误判）。只用 captcha/验证页特征。
        let blocked = html.contains("captcha") || html.contains("g_eeconfig")
            || html.contains("请验证");
        return Err(fail(
            format!("HTML {} bytes but 0 results parsed{}", html.len(),
                if blocked { " (anti-bot page suspected)" } else { "" }),
            if blocked { SearchStatus::Blocked } else { SearchStatus::Unavailable },
        ));
    }

    // 注：曾尝试加「相关性自检」（判断 Bing 查无匹配时静默降级返回泛化热门页），
    // 但原型实验证明指标不可分：降级查询「长沙 唯一新城 小区 二手房」的
    // 2-gram 命中率 10% / 3-gram 0%，而**相关**查询「杭州 二手房 价格走势」是
    // 12% / 0%、「长沙 橘子洲 门票 预约」是 25% / 0% —— 两者完全重叠。
    // 原因：结果语料只有 5 条标题+摘要，查询里跨词 bigram 本就不会逐字出现。
    // 照此上线会在相关结果上误报「无关，别重试」，比原问题更糟，故不采纳。
    // 降级问题的治理改在 agent loop 侧（见 dispatcher.rs 的迭代耗尽兜底）。
    Ok(cap_search_total(items.join("\n")))
}

/// 摘要封顶（比标题宽松，因为摘要承载事实信息）
fn cap_snippet(s: &str) -> String {
    let (head, truncated) = truncate_utf8(s, 420);
    if truncated { format!("{}…", head) } else { head.to_string() }
}

/// Brave Search API（需 BRAVE_API_KEY；该域名在中国大陆不可达，仅在配了 key 时尝试）
fn search_via_brave(query: &str, brave_key: &str) -> Result<String, SearchFail> {
    const PROVIDER: &str = "brave";
    let fail = |detail: String, status: SearchStatus| -> SearchFail {
        SearchFail { provider: PROVIDER, status, detail }
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| fail(format!("client build: {}", e), SearchStatus::Unavailable))?;
    let url = format!("https://api.search.brave.com/res/v1/web/search?q={}", urlencoding::encode(query));
    let resp = client.get(&url)
        .header("X-Subscription-Token", brave_key)
        .header("Accept", "application/json")
        .send()
        .map_err(|e| fail(describe_transport_error(&e), SearchStatus::Unavailable))?;
    let code = resp.status().as_u16();
    if !resp.status().is_success() {
        return Err(fail(format!("HTTP {}", code), classify_http_status(code)));
    }
    let val: Value = resp.json()
        .map_err(|e| fail(format!("json: {}", e), SearchStatus::Unavailable))?;
    let results = val["web"]["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        return Err(fail("0 results in response".into(), SearchStatus::Unavailable));
    }
    let joined = results.iter().take(5).map(|r| {
        format!("- {}\n  {}\n  {}",
            cap_result(r["title"].as_str().unwrap_or("")),
            r["url"].as_str().unwrap_or(""),
            cap_result(r["description"].as_str().unwrap_or("")))
    }).collect::<Vec<_>>().join("\n");
    Ok(cap_search_total(joined))
}

/// DuckDuckGo HTML 兜底（境外网络）。最后一个源，失败同样带分类状态。
fn search_via_ddg(query: &str) -> Result<String, SearchFail> {
    const PROVIDER: &str = "duckduckgo";
    let fail = |detail: String, status: SearchStatus| -> SearchFail {
        SearchFail { provider: PROVIDER, status, detail }
    };
    // #9824: realistic headers + throttle
    ddg_throttle();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| fail(format!("client build: {}", e), SearchStatus::Unavailable))?;
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding::encode(query));
    let mut req = client.get(&url);
    for (k, v) in ddg_headers() { req = req.header(k, v); }
    let resp = req.send().map_err(|e| {
        // 国内访问 DDG 必然走到这里（DNS 污染/超时）
        fail(describe_transport_error(&e), SearchStatus::Unavailable)
    })?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        return Err(fail(format!("HTTP {}", code), classify_http_status(code)));
    }
    let html = resp.text().unwrap_or_default();
    let re = regex::Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
        .map_err(|e| fail(format!("regex: {}", e), SearchStatus::ClientError))?;
    let items: Vec<String> = re.captures_iter(&html).take(5).map(|c| {
        let title = regex::Regex::new(r"(?s)<[^>]+>")
            .map(|r2| r2.replace_all(&c[2], "").into_owned())
            .unwrap_or_else(|_| c[2].to_string());
        format!("- {}\n  {}", cap_result(title.trim()), &c[1])
    }).collect();
    if items.is_empty() {
        return Err(fail("0 results parsed from HTML".into(), SearchStatus::Unavailable));
    }
    Ok(cap_search_total(items.join("\n")))
}

/// 把 reqwest 传输层错误压成一句人话（DNS/连接/超时/TLS），便于模型与日志判读。
fn describe_transport_error(e: &reqwest::Error) -> String {
    let s = e.to_string();
    let low = s.to_lowercase();
    if low.contains("dns") || low.contains("resolve") { format!("DNS 解析失败: {}", s) }
    else if low.contains("timed out") || low.contains("timeout") { format!("连接超时: {}", s) }
    else if low.contains("tls") || low.contains("ssl") || low.contains("certificate") { format!("TLS 失败: {}", s) }
    else if low.contains("connection refused") { format!("连接被拒: {}", s) }
    else { format!("传输失败: {}", s) }
}

fn cap_search_total(mut s: String) -> String {
    // 不能用 s.truncate(CAP)：中文搜索结果的 CAP 边界极易落在多字节字符中间
    // → panic → 本库 panic="abort" → 杀死整个 App（与 cap_output 同一缺陷）
    if s.len() > SEARCH_TOTAL_CAP {
        let (head, _) = truncate_utf8(&s, SEARCH_TOTAL_CAP);
        s = format!("{}\n[search output truncated]", head);
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

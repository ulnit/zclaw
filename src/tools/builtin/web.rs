//! Web tools — port of hermes' web_search/web_extract + web provider registry
//!
//! `web_search` dispatches to a pluggable backend:
//!   - tavily   (TAVILY_API_KEY)
//!   - brave    (BRAVE_API_KEY)
//!   - searxng  (SEARXNG_URL)
//!   - duckduckgo (built-in HTML scrape, no key needed)
//! `web_extract` fetches pages and strips HTML to readable text.

use crate::error::Result;
use crate::tools::{tool, ToolContext, ToolRegistry};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(web_search_tool());
    registry.register(web_extract_tool());
}

fn http_client() -> reqwest::Client {
    crate::url_safety::ssrf_guarded_client(Duration::from_secs(30))
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

fn web_search_tool() -> crate::tools::Tool {
    tool("web_search")
        .description(
            "Search the web. Returns a list of results with title, url, and description. \
             Use web_extract to read the content of promising results.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "max_results": {"type": "integer", "description": "Maximum results (default 5, max 10)", "default": 5}
            },
            "required": ["query"]
        }))
        .handler(|args, ctx| async move {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if query.is_empty() {
                return Ok(json!({"success": false, "error": "web_search: 'query' is required"}));
            }
            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(5)
                .min(10) as usize;
            web_search_impl(&ctx, &query, max_results).await
        })
        .toolset("web")
        .emoji("🔎")
        .build()
        .expect("web_search builds")
}

async fn web_search_impl(
    ctx: &Arc<ToolContext>,
    query: &str,
    max_results: usize,
) -> Result<serde_json::Value> {
    // Backend selection — hermes precedence: config > env availability.
    let configured = ctx
        .config
        .web
        .search_backend
        .clone()
        .unwrap_or_else(|| "auto".to_string());

    let tavily_key = crate::config::get_env_value("TAVILY_API_KEY");
    let brave_key = crate::config::get_env_value("BRAVE_API_KEY");
    let searxng_url = crate::config::get_env_value("SEARXNG_URL");

    let backend = match configured.as_str() {
        "tavily" => "tavily",
        "brave" => "brave",
        "searxng" => "searxng",
        "duckduckgo" | "ddgs" => "duckduckgo",
        _ => {
            // auto / legacy preference order (hermes): tavily → brave → searxng → ddgs
            if tavily_key.is_some() {
                "tavily"
            } else if brave_key.is_some() {
                "brave"
            } else if searxng_url.is_some() {
                "searxng"
            } else {
                "duckduckgo"
            }
        }
    };

    let result = match backend {
        "tavily" => search_tavily(query, max_results, tavily_key.unwrap_or_default()).await,
        "brave" => search_brave(query, max_results, brave_key.unwrap_or_default()).await,
        "searxng" => search_searxng(query, max_results, searxng_url.unwrap_or_default()).await,
        _ => search_duckduckgo(query, max_results).await,
    };

    match result {
        Ok(results) => Ok(json!({
            "success": true,
            "backend": backend,
            "data": {"web": results},
        })),
        Err(e) => {
            if backend != "duckduckgo" {
                // Fall through to the built-in backend on provider failure.
                if let Ok(results) = search_duckduckgo(query, max_results).await {
                    return Ok(json!({
                        "success": true,
                        "backend": "duckduckgo",
                        "note": format!("{} backend failed ({}); fell back", backend, e),
                        "data": {"web": results},
                    }));
                }
            }
            Ok(json!({"success": false, "error": format!("web_search failed: {}", e)}))
        }
    }
}

type SearchResults = Vec<serde_json::Value>;

async fn search_tavily(
    query: &str,
    max_results: usize,
    api_key: String,
) -> std::result::Result<SearchResults, String> {
    let client = http_client();
    let response: serde_json::Value = client
        .post("https://api.tavily.com/search")
        .json(&json!({"api_key": api_key, "query": query, "max_results": max_results}))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let results = response
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        return Err("no results".into());
    }
    Ok(results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            json!({
                "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "url": r.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                "description": r.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "position": i + 1,
            })
        })
        .collect())
}

async fn search_brave(
    query: &str,
    max_results: usize,
    api_key: String,
) -> std::result::Result<SearchResults, String> {
    let client = http_client();
    let response: serde_json::Value = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", &max_results.to_string())])
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let items = response
        .pointer("/web/results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return Err("no results".into());
    }
    Ok(items
        .iter()
        .enumerate()
        .map(|(i, r)| {
            json!({
                "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "url": r.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                "description": r.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "position": i + 1,
            })
        })
        .collect())
}

async fn search_searxng(
    query: &str,
    max_results: usize,
    base_url: String,
) -> std::result::Result<SearchResults, String> {
    let client = http_client();
    let url = format!("{}/search", base_url.trim_end_matches('/'));
    let response: serde_json::Value = client
        .get(&url)
        .query(&[("q", query), ("format", "json")])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let items = response
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return Err("no results".into());
    }
    Ok(items
        .iter()
        .take(max_results)
        .enumerate()
        .map(|(i, r)| {
            json!({
                "title": r.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "url": r.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                "description": r.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "position": i + 1,
            })
        })
        .collect())
}

/// Built-in DuckDuckGo HTML backend (no API key).
async fn search_duckduckgo(
    query: &str,
    max_results: usize,
) -> std::result::Result<SearchResults, String> {
    let client = http_client();
    let html = client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;

    let mut results = SearchResults::new();
    // Results live in <a class="result__a" href="...">Title</a> blocks with
    // <a class="result__snippet">description</a>.
    let blocks: Vec<&str> = html.split("result__a").collect();
    for block in blocks.iter().skip(1) {
        let Some(href_start) = block.find("href=\"") else {
            continue;
        };
        let href_rest = &block[href_start + 6..];
        let Some(href_end) = href_rest.find('"') else {
            continue;
        };
        let mut url = href_rest[..href_end].to_string();
        // DuckDuckGo wraps URLs in a redirect: //duckduckgo.com/l/?uddg=<encoded>
        if url.contains("uddg=") {
            if let Some(encoded) = url.split("uddg=").nth(1) {
                let encoded = encoded.split('&').next().unwrap_or(encoded);
                url = urlencoding_decode(encoded);
            }
        }
        let title =
            extract_between(&html[block_offset(&html, block)..], ">", "</a>").unwrap_or_default();
        let snippet = extract_between(block, "result__snippet", "</a>")
            .map(|s| strip_tags(&s))
            .unwrap_or_default();
        if !url.is_empty() {
            results.push(json!({
                "title": strip_tags(&title),
                "url": url,
                "description": snippet,
                "position": results.len() + 1,
            }));
        }
        if results.len() >= max_results {
            break;
        }
    }
    if results.is_empty() {
        return Err("no results parsed".into());
    }
    Ok(results)
}

fn block_offset<'a>(haystack: &'a str, needle: &'a str) -> usize {
    let hay_start = haystack.as_ptr() as usize;
    let needle_start = needle.as_ptr() as usize;
    needle_start.saturating_sub(hay_start)
}

fn extract_between(text: &str, start_marker: &str, end_marker: &str) -> Option<String> {
    let start = text.find(start_marker)? + start_marker.len();
    let end = text[start..].find(end_marker)? + start;
    Some(text[start..end].to_string())
}

fn urlencoding_decode(input: &str) -> String {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal HTML → text conversion (script/style stripped, tags removed,
/// entities decoded, whitespace collapsed).
pub fn strip_tags(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut in_tag = false;
    let mut in_script = false;
    let lower = html.to_lowercase();
    let _ = &lower;
    while let Some(ch) = chars.next() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
            }
            continue;
        }
        if ch == '<' {
            // crude script/style skipping
            let rest: String = chars.clone().take(8).collect::<String>().to_lowercase();
            if rest.starts_with("script") || rest.starts_with("style") {
                in_script = true;
            }
            if in_script && rest.starts_with("/script") || in_script && rest.starts_with("/style") {
                in_script = false;
            }
            in_tag = true;
            if !in_script {
                text.push(' ');
            }
            continue;
        }
        if in_script {
            continue;
        }
        text.push(ch);
    }
    // Decode common entities.
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    // Collapse whitespace.
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// web_extract
// ---------------------------------------------------------------------------

fn web_extract_tool() -> crate::tools::Tool {
    tool("web_extract")
        .description(
            "Fetch one or more URLs and extract readable text content (HTML stripped). \
             Returns title + content per URL. Use after web_search to read pages.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "urls": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "URLs to fetch and extract (max 5 per call)"
                },
                "max_chars": {"type": "integer", "description": "Max characters of content per URL (default 20000)", "default": 20000}
            },
            "required": ["urls"]
        }))
        .handler(|args, _ctx| async move {
            let urls: Vec<String> = args
                .get("urls")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|u| u.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if urls.is_empty() {
                return Ok(json!({"success": false, "error": "web_extract: 'urls' is required"}));
            }
            if urls.len() > 5 {
                return Ok(json!({"success": false, "error": "web_extract: at most 5 URLs per call"}));
            }
            // ── URL safety (hermes url_safety wiring) ──────────────────
            // Normalize IRIs / scheme-whitespace artifacts, then refuse the
            // whole call when a URL appears to embed credentials.
            let mut normalized_urls: Vec<String> = Vec::with_capacity(urls.len());
            for url in &urls {
                let normalized = crate::url_safety::normalize_url_for_request(url);
                let decoded_raw = urlencoding_decode(url);
                let decoded_normalized = urlencoding_decode(&normalized);
                if crate::redact::contains_token_prefix(url)
                    || crate::redact::contains_token_prefix(&decoded_raw)
                    || crate::redact::contains_token_prefix(&normalized)
                    || crate::redact::contains_token_prefix(&decoded_normalized)
                {
                    return Ok(json!({
                        "success": false,
                        "error": "Blocked: URL contains what appears to be an API key or token. Secrets must not be sent in URLs."
                    }));
                }
                if let Some(key) = crate::url_safety::sensitive_query_param_name(&normalized) {
                    return Ok(json!({
                        "success": false,
                        "error": format!(
                            "Blocked: URL contains a credential-like query parameter ({key}). Remove the sensitive query parameter before extracting."
                        )
                    }));
                }
                normalized_urls.push(normalized);
            }

            let max_chars = args.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(20000) as usize;
            let client = http_client();
            let mut data = Vec::new();
            for url in normalized_urls {
                // ── SSRF protection: block private/internal targets ─────
                if !crate::url_safety::is_safe_url(&url).await {
                    data.push(json!({
                        "url": url,
                        "error": "Blocked: URL targets a private or internal network address"
                    }));
                    continue;
                }
                match client.get(&url).send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        let content_type = response
                            .headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        match response.text().await {
                            Ok(body) => {
                                let (title, text) = if content_type.contains("html") {
                                    let title = extract_between(&body, "<title>", "</title>")
                                        .map(|t| strip_tags(&t))
                                        .unwrap_or_default();
                                    (title, strip_tags(&body))
                                } else {
                                    (String::new(), body)
                                };
                                let text: String = text.chars().take(max_chars).collect();
                                data.push(json!({
                                    "url": url,
                                    "status": status,
                                    "title": title,
                                    "content": text,
                                }));
                            }
                            Err(e) => data.push(json!({"url": url, "error": format!("body: {}", e)})),
                        }
                    }
                    Err(e) => data.push(json!({"url": url, "error": e.to_string()})),
                }
            }
            Ok(json!({"success": true, "data": data}))
        })
        .toolset("web")
        .emoji("📰")
        .build()
        .expect("web_extract builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_tags() {
        let html = "<html><head><title>T</title><style>x{}</style></head><body><p>Hello&nbsp;<b>world</b></p><script>evil()</script></body></html>";
        let text = strip_tags(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains("evil"));
    }

    #[test]
    fn test_url_decode() {
        assert_eq!(
            urlencoding_decode("https%3A%2F%2Fexample.com%2Fx"),
            "https://example.com/x"
        );
    }

    #[test]
    fn test_extract_between() {
        assert_eq!(
            extract_between("<title>Hello</title>", "<title>", "</title>").unwrap(),
            "Hello"
        );
    }
}

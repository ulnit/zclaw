//! 移动端工具集：只注册手机安全的子集 + 用 Bing 覆盖 web_search。
//!
//! 为什么不用 register_builtin_tools：它注册全部 50+ 工具，含
//! terminal/execute_code/desktop/browser/platform/project 等——这些在手机上
//! 要么无意义（没有 shell/cgroup/CDP Chrome）、要么危险（任意命令执行）。
//! 移动端只保留：文件读写、web 搜索/抽取、记忆、todo、会话检索、clarify。
//!
//! 🔴 web_search 必须覆盖：ulnclaw 原生后端是 duckduckgo/brave/tavily，
//! 前三者在中国大陆实测 HTTP=000 不可达（brave/tavily 还需付费 key，
//! 而移动端绝不内嵌 key）。zclaw 的教训：国内唯一零 key 可达源是 cn.bing.com。
//! 这里用同一套经过实证的 Bing HTML 抓取逻辑覆盖掉原生 web_search。
use std::sync::Arc;
use std::time::Duration;
use ulnclaw::tools::{tool, ToolContext, ToolRegistry};

/// 注册移动端安全工具子集。
pub fn register_mobile_tools(registry: &mut ToolRegistry) {
    // 安全子集：无 shell、无任意代码执行、无桌面/浏览器自动化。
    ulnclaw::tools::builtin::files::register(registry);
    ulnclaw::tools::builtin::web::register(registry); // web_search(将被覆盖) + web_extract
    ulnclaw::tools::builtin::memory::register(registry);
    ulnclaw::tools::builtin::todo::register(registry);
    ulnclaw::tools::builtin::session_search::register(registry);
    ulnclaw::tools::builtin::clarify::register(registry);
    ulnclaw::tools::builtin::skills::register(registry);

    // 用 Bing 覆盖原生 web_search（register 按名 HashMap insert，同名覆盖；
    // 先 unregister 原生版避免 toolset 名单出现重复项）。
    registry.unregister("web_search");
    registry.register(bing_search_tool());
}

const RESULT_CAP: usize = 1200;
const SNIPPET_CAP: usize = 400;

/// UTF-8 安全截断（zclaw 教训：`&s[..N]` 落在多字节字符中间会 panic，
/// 而 panic 跨 FFI 边界 = 杀 App）。
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn bing_search_tool() -> ulnclaw::tools::Tool {
    tool("web_search")
        .description(
            "Search the web (Bing). Returns titles, URLs and snippets, but results \
             may be irrelevant: for obscure or highly local queries (a residential \
             compound, a small shop, a street) search engines often return generic \
             city/travel pages instead. Judge relevance yourself — if the results do \
             not mention the specific thing you asked about, treat it as not found; \
             try at most one reworded query, then answer from your own knowledge and \
             state that it is unverified.",
        )
        .parameters(serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "搜索关键词，优先用短词组而非整句问题"},
                "max_results": {"type": "integer", "description": "最多返回几条（默认 5）"}
            },
            "required": ["query"]
        }))
        .handler(|args, _ctx: Arc<ToolContext>| async move {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
            if query.is_empty() {
                return Ok(serde_json::json!({"success": false, "error": "web_search: 'query' is required"}));
            }
            let max = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(5).min(10) as usize;
            match search_via_bing(query, max).await {
                Ok(text) => Ok(serde_json::json!({
                    "success": true,
                    "backend": "bing",
                    "data": {"web": text},
                })),
                Err(e) => Ok(serde_json::json!({"success": false, "error": e})),
            }
        })
        .build()
        .expect("build bing search tool")
}

/// cn.bing.com HTML 抓取。中国大陆可达（HTTP=200），无需 API key。
/// 选择器实测于 2026-09（与 zclaw 同源）：真链接在 `class="tilk"` 的 a 上，
/// 不在 h2 里；`<cite>` 只是显示格式，当 URL 会让后续 web_fetch 全失败。
async fn search_via_bing(query: &str, max: usize) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("client build: {}", e))?;

    let url = format!(
        "https://cn.bing.com/search?q={}&count=10",
        urlencoding::encode(query)
    );
    let resp = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36")
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|e| {
            // 连接失败/超时 = 源不可达。给分类状态，让模型决定别再重试。
            format!("web_search transport error (源不可达，勿重试): {}", e)
        })?;

    if !resp.status().is_success() {
        return Err(format!("web_search HTTP {}", resp.status().as_u16()));
    }
    let html = resp.text().await.unwrap_or_default();
    if html.is_empty() {
        return Err("web_search empty body".into());
    }

    let re_tag = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let re_h2 = regex::Regex::new(r"(?s)<h2[^>]*>(.*?)</h2>").unwrap();
    let re_tilk = regex::Regex::new(r#"(?s)<a[^>]*class="tilk"[^>]*href="([^"]+)""#).unwrap();
    let re_href = regex::Regex::new(r#"(?s)<h2[^>]*>\s*<a[^>]*href="([^"]+)""#).unwrap();
    let re_cite = regex::Regex::new(r"(?s)<cite[^>]*>(.*?)</cite>").unwrap();
    let re_p = regex::Regex::new(r"(?s)<p[^>]*>(.*?)</p>").unwrap();
    let strip = |s: &str| re_tag.replace_all(s, "").trim().to_string();

    let mut items: Vec<String> = Vec::new();
    for block in html.split(r#"<li class="b_algo""#).skip(1).take(max + 2) {
        let title = re_h2
            .captures(block)
            .map(|c| strip(&c[1]))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let url = re_tilk
            .captures(block)
            .map(|c| c[1].to_string())
            .or_else(|| re_href.captures(block).map(|c| c[1].to_string()))
            .filter(|u| u.starts_with("http"))
            .or_else(|| re_cite.captures(block).map(|c| strip(&c[1])))
            .unwrap_or_default();
        let snippet = re_p
            .captures(block)
            .map(|c| strip(&c[1]))
            .unwrap_or_default();
        items.push(if snippet.is_empty() {
            format!("- {}\n  {}", truncate_utf8(&title, RESULT_CAP), url)
        } else {
            format!(
                "- {}\n  {}\n  摘要: {}",
                truncate_utf8(&title, RESULT_CAP),
                url,
                truncate_utf8(&snippet, SNIPPET_CAP)
            )
        });
        if items.len() >= max {
            break;
        }
    }

    if items.is_empty() {
        // 抓到 HTML 但 0 结果：选择器变了或反爬页。
        // 🔴 不用 b_no 当反爬标记（实测降级页与正常页计数都是 1，无判别力）。
        let blocked = html.contains("captcha") || html.contains("g_eeconfig") || html.contains("请验证");
        return Err(if blocked {
            "web_search blocked (反爬验证页): 稍后重试或换更短查询词".into()
        } else {
            format!(
                "web_search: {} bytes HTML but 0 results parsed (可能 Bing 对该查询静默降级)",
                html.len()
            )
        });
    }

    Ok(items.join("\n\n"))
}

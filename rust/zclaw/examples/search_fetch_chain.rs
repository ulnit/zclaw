// examples/search_fetch_chain.rs — 验证「搜索→抓取」闭环：
// web_search 返回的 URL 必须能被 web_fetch 真正抓取，否则搜索结果是废链接。
use serde_json::json;

fn main() {
    let q = std::env::args().nth(1).unwrap_or_else(|| "上海今天天气".to_string());

    let ws = std::env::temp_dir().join(format!("zclaw_chain_{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("mkdir workspace");
    let ws_str = ws.to_string_lossy().to_string();
    let mem = zclaw::memory::MemoryStore::open(&ws_str).expect("open memory");
    let ctx = zclaw::tools::ToolCtx { workspace: ws.clone(), memory: &mem };

    println!("=== 第1步：web_search {:?} ===", q);
    let t0 = std::time::Instant::now();
    let out = zclaw::tools::execute(&ctx, "web_search_tool", &json!({"query": q}));
    println!("耗时 {:.2}s，字节 {}", t0.elapsed().as_secs_f64(), out.len());

    // 提取结果里的 URL（每条形如 "- 标题\n  URL"）
    let urls: Vec<String> = out.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| l.starts_with("http"))
        .collect();
    println!("提取到 {} 个 URL", urls.len());
    if urls.is_empty() {
        println!("❌ 搜索无可用 URL");
        println!("{}", out);
        let _ = std::fs::remove_dir_all(&ws);
        std::process::exit(1);
    }

    println!("\n=== 第2步：逐个 web_fetch（验证链接非废）===");
    let mut ok = 0usize;
    for (i, u) in urls.iter().take(3).enumerate() {
        let t1 = std::time::Instant::now();
        let r = zclaw::tools::execute(&ctx, "web_fetch", &json!({"url": u}));
        let el = t1.elapsed().as_secs_f64();
        let is_err = r.starts_with("Fetch failed") || r.starts_with("Error") || r.is_empty()
            || r.contains("failed to") || r.len() < 50;
        let mark = if is_err { "❌" } else { "✅" };
        if !is_err { ok += 1; }
        println!("  [{}] {} {:.2}s → {} 字节 {}",
            i + 1, mark, el, r.len(), u);
        if is_err {
            println!("      详情: {}", r.chars().take(160).collect::<String>().replace('\n', " "));
        } else {
            println!("      首行: {}", r.lines().next().unwrap_or("").chars().take(90).collect::<String>());
        }
    }

    println!("\n=== 判定 ===");
    println!("搜索: {} 条结果 / 抓取: {}/3 成功", urls.len(), ok);
    if ok >= 1 {
        println!("✅ 「搜索→抓取」链路闭环可用");
    } else {
        println!("⚠️ 搜索可用但抓取全失败，链接对模型无价值，需查 web_fetch");
    }
    let _ = std::fs::remove_dir_all(&ws);
}

// examples/search_probe.rs — 实测 web_search 在本机网络下能否返回真实结果。
// 用法: cargo run --release --example search_probe "查询词"
use serde_json::json;

fn main() {
    let q = std::env::args().nth(1).unwrap_or_else(|| "上海今天天气".to_string());
    println!("=== web_search 实测：query = {:?} ===\n", q);

    // MemoryStore::open 接收 &str 工作目录（签名：open(workspace_dir: &str)）
    let ws = std::env::temp_dir().join(format!("zclaw_probe_{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("mkdir workspace");
    let ws_str = ws.to_string_lossy().to_string();
    let mem = zclaw::memory::MemoryStore::open(&ws_str).expect("open memory");

    let ctx = zclaw::tools::ToolCtx { workspace: ws.clone(), memory: &mem };

    let t0 = std::time::Instant::now();
    let out = zclaw::tools::execute(&ctx, "web_search_tool", &json!({"query": q}));
    let elapsed = t0.elapsed();

    println!("耗时: {:.2}s", elapsed.as_secs_f64());
    println!("返回字节: {}", out.len());
    println!("--- 结果 ---");
    println!("{}", out);
    println!("--- 判定 ---");
    let n = out.lines().filter(|l| l.starts_with("- ")).count();
    if out.starts_with("Search failed") || out == "No results" || out.is_empty() {
        println!("❌ 搜索失败");
        let _ = std::fs::remove_dir_all(&ws);
        std::process::exit(1);
    } else if n >= 1 {
        println!("✅ 成功解析出 {} 条结果", n);
    } else {
        println!("⚠️ 有输出但未见 '- ' 结果行，需人工判读");
    }
    let _ = std::fs::remove_dir_all(&ws);
}

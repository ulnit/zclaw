//! 验证 dispatcher 的「迭代耗尽收口」修复。
//! 先启动 examples/mock_tool_loop（占 127.0.0.1:8931），它会：
//!   - 请求带 tools → 永远回 tool_call（模拟模型无限要求检索）
//!   - 请求不带 tools → 回真正的文本答案（模拟被强制收口）
//! 本用例设 max_iterations=3，断言：
//!   1) 拿到最终文本答案（不是空）
//!   2) 答案含收口轮标记 → 证明最后一轮确实没带 tools
//!   3) tool_call 次数 == max_iterations-1
//!
//! 注意 Chunk 的载荷字段是 `name`（不是 content）：Chunk::text(s) 把 s 放进 name，
//! 见 src/agent/dispatcher.rs 的 impl Chunk。
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use zclaw::agent::dispatcher::{Chunk, Dispatcher};
use zclaw::config::{AgentCfg, Config};
use zclaw::memory::MemoryStore;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().worker_threads(4).build().unwrap();
    rt.block_on(run());
}

async fn run() {
    let ws = std::env::temp_dir().join(format!("zclaw_iter_{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mem = Arc::new(MemoryStore::open(&ws.to_string_lossy()).expect("open memory"));

    const MAX_ITER: u32 = 3;
    let cfg = Config {
        api_url: "http://127.0.0.1:8931/v1".to_string(),
        api_key: "mock-key".to_string(),
        default_model: "mock".to_string(),
        temperature: 0.2,
        workspace_dir: ws.to_string_lossy().to_string(),
        agent: AgentCfg { max_iterations: MAX_ITER, system_prompt: "test".to_string() },
    };

    let disp = Dispatcher::new(cfg, mem.clone(), Arc::new(AtomicBool::new(false)));

    let texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let saw_done = Arc::new(AtomicBool::new(false));

    let t2 = texts.clone();
    let tc2 = tool_calls.clone();
    let d2 = saw_done.clone();
    let emit = move |c: Chunk| {
        match c.chunk_type {
            // 0=Text，载荷在 name 字段
            0 => t2.lock().unwrap().push(c.name.clone().unwrap_or_default()),
            1 => { tool_calls_dummy(&tc2); }
            3 => d2.store(true, Ordering::SeqCst),
            4 => eprintln!("  [收到 error chunk] {:?}", c.name),
            _ => {}
        }
    };

    disp.run_turn("sess-iter-test", "杭州二手房价格多少", &emit).await;

    let all = texts.lock().unwrap().concat();
    let ntc = tool_calls.load(Ordering::SeqCst);
    println!("=== 结果 ===");
    println!("  最终文本 ({} 字): {:?}", all.chars().count(), all.chars().take(90).collect::<String>());
    println!("  tool_call 次数: {} (期望 {} = max_iterations-1)", ntc, MAX_ITER - 1);
    println!("  收到 done: {}", saw_done.load(Ordering::SeqCst));
    println!();

    let mut ok = true;
    if all.contains("收口轮答案") {
        println!("  ✅ 断言1：拿到收口轮答案 → 强制收口生效（最后一轮未带 tools）");
    } else {
        println!("  ❌ 断言1：未拿到收口轮答案，实际={:?}", all.chars().take(60).collect::<String>());
        ok = false;
    }
    if ntc == (MAX_ITER - 1) as usize {
        println!("  ✅ 断言2：tool_call 次数正确，最后一轮未再请求工具");
    } else {
        println!("  ❌ 断言2：tool_call={} ≠ {}", ntc, MAX_ITER - 1);
        ok = false;
    }
    if !all.trim().is_empty() {
        println!("  ✅ 断言3：用户不会看到空气泡");
    } else {
        println!("  ❌ 断言3：文本为空 —— 用户仍会看到空气泡");
        ok = false;
    }
    println!();
    println!("{}", if ok { "🎉 全部断言通过" } else { "💥 存在失败断言" });
    let _ = std::fs::remove_dir_all(&ws);
    if !ok { std::process::exit(1); }
}

fn tool_calls_dummy(c: &AtomicUsize) { c.fetch_add(1, Ordering::SeqCst); }

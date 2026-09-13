//! 🔴 真后端端到端诊断：用真实 ai.ulnit.com + deepseek-v4.1-flash 跑完整 FFI 路径，
//! 复现真机「Who are you 卡…」——host 与设备走同一份 Rust 代码，
//! 若 host 也收不到 text chunk，bug 必在 FFI/ulnclaw 流式解析层。
//! key 从 ULNCLAW_TEST_KEY 环境变量读（不回显）。
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().worker_threads(4).build().unwrap();
    rt.block_on(run());
}

async fn run() {
    let key = std::env::var("ULNCLAW_TEST_KEY").expect("ULNCLAW_TEST_KEY 未设置");
    let model = std::env::var("ULNCLAW_TEST_MODEL")
        .unwrap_or_else(|_| "deepseek/deepseek-v4.1-flash".to_string());

    let ws = std::env::temp_dir().join(format!("ulnclaw_real_{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();

    let cfg = serde_json::json!({
        "api_url": "https://ai.ulnit.com/v1",
        "api_key": key,
        "default_model": model,
        "temperature": 0.7,
        "workspace_dir": ws.to_string_lossy(),
        "agent": {"max_iterations": 5}
    }).to_string();

    let c_cfg = std::ffi::CString::new(cfg).unwrap();
    let rc = unsafe { zclaw::zclaw_init(c_cfg.as_ptr()) };
    println!("zclaw_init → {}  (模型: {})", rc, model);
    assert_eq!(rc, 0);

    let msg = std::ffi::CString::new("Who are you").unwrap();
    let t0 = std::time::Instant::now();
    let rc = unsafe { zclaw::zclaw_chat(msg.as_ptr()) };
    println!("zclaw_chat → {}", rc);
    assert_eq!(rc, 0);

    let mut text = String::new();
    let mut thinking = String::new();
    let mut tools: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut saw_done = false;
    let mut polls = 0usize;
    let first_text = Arc::new(AtomicUsize::new(0));
    let ft = first_text.clone();

    // 轮询 120s 上限
    while t0.elapsed().as_secs() < 120 {
        polls += 1;
        let p = unsafe { zclaw::zclaw_poll_chunks() };
        let json = unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().to_string();
        unsafe { zclaw::zclaw_free(p) };
        if json != "[]" {
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&json) {
                for c in arr {
                    let kind = c["chunkType"].as_i64().unwrap_or(-1);
                    let name = c["name"].as_str().unwrap_or("").to_string();
                    match kind {
                        0 => {
                            if text.is_empty() {
                                ft.store(t0.elapsed().as_millis() as usize, Ordering::SeqCst);
                            }
                            text.push_str(&name);
                        }
                        1 => tools.push(name),
                        4 => errors.push(name),
                        5 => thinking.push_str(&name),
                        3 => saw_done = true,
                        _ => {}
                    }
                }
            }
        }
        if saw_done { break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    println!();
    println!("=== 真后端结果（{}）===", model);
    println!("  总耗时: {:.1}s  轮询次数: {}", t0.elapsed().as_secs_f64(), polls);
    println!("  首个 text chunk: {}ms", first_text.load(Ordering::SeqCst));
    println!("  text ({} 字): {:?}", text.chars().count(), text.chars().take(120).collect::<String>());
    println!("  thinking ({} 字): {:?}", thinking.chars().count(), thinking.chars().take(60).collect::<String>());
    println!("  tools: {:?}", tools);
    println!("  errors: {:?}", errors);
    println!("  done: {}", saw_done);
    println!();
    if !text.trim().is_empty() {
        println!("=== ✅ 正文到达：设备空气泡的根因不在 FFI 流式层 ===");
    } else {
        println!("=== ❌ 正文为空：复现设备 bug！根因在 FFI/ulnclaw 流式解析 ===");
        std::process::exit(1);
    }
}

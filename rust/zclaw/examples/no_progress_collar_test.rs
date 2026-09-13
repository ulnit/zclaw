//! 🔴 端到端验证「无进展提前收口」的**主循环接线**（单元测试只覆盖纯函数）。
//!
//! 复现真实生产故障：用户问「长沙市唯一星城小区是位于省图书馆对面吗」，
//! 模型每轮换一种措辞重搜，Bing 每次返回**逐字节相同**的泛化结果
//! （已用 search_probe.exe 实证：4 种措辞 → 1 个唯一 sha256，805B）。
//!
//! 本用例用 **mock LLM（确定性）+ 真实 Bing 工具执行（真实结果）**：
//!   - mock 每轮回一个不同 query 的 web_search_tool 调用（4 种措辞循环）
//!   - dispatcher 真的去调 tools::execute → 真的打 Bing
//!   - 不带 tools 的请求（= 收口轮）→ mock 回文本答案
//!
//! 断言：修复后第 3 次同结果即判定无进展 → 第 4 轮收口，
//!       tool_call 次数远小于 max_iterations-1（修复前会烧满）。
//!
//! 用法：cargo run --release --example no_progress_collar_test
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use zclaw::agent::dispatcher::{Chunk, Dispatcher};
use zclaw::config::{AgentCfg, Config};
use zclaw::memory::MemoryStore;

/// 4 种措辞 —— 与 search_probe 实证过的完全一致
const WORDINGS: [&str; 4] = [
    "长沙 唯一星城",
    "长沙 维一星城 小区",
    "长沙 唯一新城 小区 二手房",
    "长沙市唯一星城小区 是位于省图书馆对面吗",
];

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().worker_threads(4).build().unwrap();
    rt.block_on(run());
}

async fn run() {
    // ── 1. 起 mock LLM 服务器 ──
    let hits = Arc::new(AtomicUsize::new(0));
    let h2 = hits.clone();
    std::thread::spawn(move || mock_server(h2));
    // 等 mock 就绪
    for _ in 0..50 {
        if std::net::TcpStream::connect("127.0.0.1:8932").is_ok() { break; }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let ws = std::env::temp_dir().join(format!("zclaw_np_{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mem = Arc::new(MemoryStore::open(&ws.to_string_lossy()).expect("open memory"));

    const MAX_ITER: u32 = 10;
    // 🔴 system_prompt 必须用真实默认值（带工具使用策略），不能覆盖成 "test"。
    // AgentCfg 的 Default 已手工实现为 default_prompt()（见 config.rs），
    // 所以 `..Default::default()` 就能取到真实 prompt。
    let cfg = Config {
        api_url: "http://127.0.0.1:8932/v1".to_string(),
        api_key: "mock-key".to_string(),
        default_model: "mock".to_string(),
        temperature: 0.2,
        workspace_dir: ws.to_string_lossy().to_string(),
        agent: AgentCfg { max_iterations: MAX_ITER, ..Default::default() },
    };
    assert!(cfg.agent.system_prompt.contains("联网搜索只是"),
            "测试前提：必须加载真实策略 prompt，否则验的是空 prompt");

    let disp = Dispatcher::new(cfg, mem.clone(), Arc::new(AtomicBool::new(false)));

    let texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let tool_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let t2 = texts.clone();
    let tc2 = tool_calls.clone();
    let tn2 = tool_names.clone();
    let emit = move |c: Chunk| match c.chunk_type {
        0 => t2.lock().unwrap().push(c.name.clone().unwrap_or_default()),
        1 => {
            tc2.fetch_add(1, Ordering::SeqCst);
            tn2.lock().unwrap().push(c.name.clone().unwrap_or_default());
        }
        4 => eprintln!("  [error chunk] {:?}", c.name),
        _ => {}
    };

    println!("=== 跑真实场景（mock LLM + 真实 Bing 工具执行）===");
    let t0 = std::time::Instant::now();
    disp.run_turn("sess-np-test", "长沙市唯一星城小区是位于省图书馆对面吗", &emit).await;
    let elapsed = t0.elapsed();

    let all = texts.lock().unwrap().concat();
    let ntc = tool_calls.load(Ordering::SeqCst);
    let names = tool_names.lock().unwrap().clone();
    let mock_hits = hits.load(Ordering::SeqCst);

    println!("  耗时: {:.1}s", elapsed.as_secs_f64());
    println!("  mock LLM 请求轮数: {} (max_iterations={})", mock_hits, MAX_ITER);
    println!("  tool_call 次数: {}", ntc);
    println!("  工具序列: {:?}", names);
    println!("  最终文本 ({} 字): {:?}", all.chars().count(), all.chars().take(120).collect::<String>());
    println!();

    let mut ok = true;

    // 断言1：必须拿到答案，不能是空气泡
    if !all.trim().is_empty() && all.contains("收口轮答案") {
        println!("  ✅ 断言1：拿到收口轮答案 → 用户不会看到空气泡");
    } else {
        println!("  ❌ 断言1：未拿到收口轮答案");
        ok = false;
    }

    // 断言2：**提前**收口 —— 关键断言。
    // 无进展在第 3 次同结果后被判定 → 第 4 轮是收口轮（不带 tools）。
    // 所以 tool_call 应为 3，mock_hits 应为 4；修复前会是 9 / 10。
    let expect_tools = 3usize;
    if ntc == expect_tools {
        println!("  ✅ 断言2：tool_call={} → 第 {} 轮即提前收口（修复前会烧到 {} 轮）",
                 ntc, expect_tools + 1, MAX_ITER - 1);
    } else if ntc < MAX_ITER as usize - 1 {
        println!("  ⚠️ 断言2：tool_call={}（< {}，确实提前收口了，但不是预期的 {}）",
                 ntc, MAX_ITER - 1, expect_tools);
        println!("     可能 Bing 这几次返回了不同结果（no_progress 未命中），需复查");
        ok = false;
    } else {
        println!("  ❌ 断言2：tool_call={} = 烧满轮次，no_progress **没有**提前收口", ntc);
        ok = false;
    }

    // 断言3：绝不能出现那句道歉兜底文案
    if all.contains("抱歉，本轮尝试了多次检索") {
        println!("  ❌ 断言3：出现了被用户明确否决的道歉兜底文案");
        ok = false;
    } else {
        println!("  ✅ 断言3：无道歉兜底文案");
    }

    println!();
    if ok {
        println!("=== ✅ 全部断言通过：no_progress 提前收口已接线生效 ===");
    } else {
        println!("=== ❌ 存在失败断言 ===");
        std::process::exit(1);
    }
}

/// mock LLM：带 tools → 每轮换一种措辞要 web_search_tool；不带 tools → 回答案
fn mock_server(hits: Arc<AtomicUsize>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:8932").expect("bind 8932");
    for stream in listener.incoming() {
        let mut s = match stream { Ok(s) => s, Err(_) => continue };
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 { break; }
            let t = line.trim_end();
            if t.is_empty() { break; }
            if let Some(v) = t.to_lowercase().strip_prefix("content-length:") {
                content_len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_len];
        let _ = reader.read_exact(&mut body);
        let body_str = String::from_utf8_lossy(&body).to_string();

        let n = hits.fetch_add(1, Ordering::SeqCst);
        let has_tools = body_str.contains("\"tools\"");
        eprintln!("[mock] hit#{} has_tools={}", n + 1, has_tools);

        let sse = if has_tools {
            // 每轮换一种措辞（真实故障形态：模型不停改写查询）
            let q = WORDINGS[n % WORDINGS.len()];
            let chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "index": 0,
                            "id": format!("call_{}", n),
                            "type": "function",
                            "function": {
                                // 必须是 dispatcher 认可的工具名
                                "name": "web_search_tool",
                                "arguments": serde_json::json!({"query": q}).to_string(),
                            },
                        }],
                    },
                }],
            });
            format!("data: {}\n\ndata: [DONE]\n\n", chunk)
        } else {
            let chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": "【收口轮答案】没有联网查到该小区的确切位置，以下是基于我已有的了解，可能不准确：长沙市区叫「唯一星城」的住宅小区信息较少，建议以地图 App 或当地房产平台为准。",
                    },
                }],
            });
            format!("data: {}\n\ndata: [DONE]\n\n", chunk)
        };

        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            sse.len()
        );
        let _ = s.write_all(head.as_bytes());
        let _ = s.write_all(sse.as_bytes());
        let _ = s.flush();
    }
}

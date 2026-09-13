//! 🔴 端到端回归测试：证明 zclaw_chat → poll_chunks 能收到流式正文。
//!
//! 背景（真机实测 bug）：ulnclaw 的流式开关是 task-local STREAM_EMITTER
//! （`stream: STREAM_EMITTER.try_with(...).is_ok()`）。FFI 若不用 stream_scope
//! 包裹 run，provider 走**非流式**，on_stream_delta 永不触发，正文只在
//! RunResult.content —— 而 content 曾被 `let _ = final_text` 丢弃，
//! 导致 App 卡在「…」空气泡（后端日志 200 + is_stream:false + 178 tokens）。
//!
//! 本测试用 mock SSE 服务器（真流式）驱动完整 FFI 路径，断言：
//!   1) poll_chunks 能拿到 chunkType=0 的正文（= 流式通路打通）
//!   2) 拿到 chunkType=3 done
//!   3) 后端确实收到 "stream":true（证明 stream_scope 生效，这是根因所在）
//!   4) 正文内容完整拼接
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .unwrap();
    rt.block_on(run());
}

static HITS: AtomicUsize = AtomicUsize::new(0);
/// 记录后端收到的请求体里是否带 "stream":true（根因验证）
static SAW_STREAM_TRUE: AtomicUsize = AtomicUsize::new(0);

async fn run() {
    // 起 mock SSE 服务器
    std::thread::spawn(mock_server);
    for _ in 0..50 {
        if std::net::TcpStream::connect("127.0.0.1:8940").is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let ws = std::env::temp_dir().join(format!("ulnclaw_stream_{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();

    // ── 走真实 FFI 入口（与 App 完全相同的调用序列）──
    let cfg = serde_json::json!({
        "api_url": "http://127.0.0.1:8940/v1",
        "api_key": "mock-key",
        "default_model": "mock-model",
        "temperature": 0.7,
        "workspace_dir": ws.to_string_lossy(),
        "agent": {"max_iterations": 5}
    })
    .to_string();

    let c_cfg = std::ffi::CString::new(cfg).unwrap();
    let rc = unsafe { zclaw::zclaw_init(c_cfg.as_ptr()) };
    println!("zclaw_init → {}", rc);
    assert_eq!(rc, 0, "init 必须成功");

    let ver = unsafe { std::ffi::CStr::from_ptr(zclaw::zclaw_version()) }
        .to_string_lossy()
        .to_string();
    println!("zclaw_version → {}", ver);
    unsafe { zclaw::zclaw_free(zclaw::zclaw_version()) };

    let msg = std::ffi::CString::new("Who are you").unwrap();
    let rc = unsafe { zclaw::zclaw_chat(msg.as_ptr()) };
    println!("zclaw_chat → {}", rc);
    assert_eq!(rc, 0, "chat 必须被接受");

    // 轮询收集 chunk（与 App 的 pollChunks 循环同构）
    let mut text = String::new();
    let mut thinking = String::new();
    let mut saw_done = false;
    let mut saw_error: Option<String> = None;
    let mut chunks_seen = 0usize;

    for _ in 0..400 {
        // 40s 上限
        let p = unsafe { zclaw::zclaw_poll_chunks() };
        let json = unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().to_string();
        unsafe { zclaw::zclaw_free(p) };

        if json != "[]" {
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&json) {
                for c in arr {
                    chunks_seen += 1;
                    let kind = c["chunkType"].as_i64().unwrap_or(-1);
                    let name = c["name"].as_str().unwrap_or("").to_string();
                    match kind {
                        0 => text.push_str(&name),
                        4 => saw_error = Some(name),
                        5 => thinking.push_str(&name),
                        3 => saw_done = true,
                        _ => {}
                    }
                }
            }
        }
        if saw_done {
            break;
        }
        if unsafe { zclaw::zclaw_is_running() } == 0 && saw_done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    println!();
    println!("=== 结果 ===");
    println!("  chunk 总数: {}", chunks_seen);
    println!("  正文 text ({} 字): {:?}", text.chars().count(), text.chars().take(80).collect::<String>());
    println!("  思考 thinking ({} 字)", thinking.chars().count());
    println!("  收到 done: {}", saw_done);
    println!("  error: {:?}", saw_error);
    println!("  mock 请求数: {}", HITS.load(Ordering::SeqCst));
    println!("  带 stream:true 的请求数: {}", SAW_STREAM_TRUE.load(Ordering::SeqCst));
    println!();

    let mut ok = true;

    // 断言1：必须拿到正文（这就是真机上失败的点）
    if !text.trim().is_empty() {
        println!("  ✅ 断言1：poll_chunks 收到正文 → 流式通路打通");
    } else {
        println!("  ❌ 断言1：正文为空 —— 空气泡 bug 复现！");
        ok = false;
    }

    // 断言2：正文内容正确
    if text.contains("爻荚") || text.contains("UlnClaw") || text.contains("mock-answer") {
        println!("  ✅ 断言2：正文内容来自模型响应");
    } else {
        println!("  ⚠️ 断言2：正文内容意外: {:?}", text.chars().take(40).collect::<String>());
    }

    // 断言3：🔴 根因验证 —— 请求必须带 "stream":true（证明 stream_scope 生效）
    if SAW_STREAM_TRUE.load(Ordering::SeqCst) > 0 {
        println!("  ✅ 断言3：后端收到 stream:true → stream_scope 生效（根因已修）");
    } else {
        println!("  ❌ 断言3：后端从未收到 stream:true → 仍走非流式路径！");
        ok = false;
    }

    // 断言4：done chunk 必须到达（App 靠它结束 streaming 状态）
    if saw_done {
        println!("  ✅ 断言4：收到 done → UI 不会永远卡在「…」");
    } else {
        println!("  ❌ 断言4：没收到 done");
        ok = false;
    }

    // 断言5：无 error
    if saw_error.is_none() {
        println!("  ✅ 断言5：无 error chunk");
    } else {
        println!("  ❌ 断言5：{:?}", saw_error);
        ok = false;
    }

    // 断言6：🔴 正文不得重复（实测踩过的 bug：ulnclaw 在每个 delta 处同时调
    // emit_stream_event(Delta) 和 on_stream_delta 回调，两条通道都 push_chunk
    // 会让正文逐段翻倍 →「我是我是爻荚（爻荚（UlnClaw）UlnClaw）…」）。
    // mock 只发一次 "mock-answer"，所以正文里出现两次即为翻倍。
    let dup_count = text.matches("mock-answer").count();
    if dup_count == 1 {
        println!("  ✅ 断言6：正文无重复（mock-answer 出现 1 次）");
    } else {
        println!("  ❌ 断言6：正文重复！mock-answer 出现 {} 次（双通道推送 bug）", dup_count);
        ok = false;
    }
    // 同判据再用整段长度兜一次：mock 正文共 4 段拼成，翻倍则长度约 ×2
    let expect_len: usize = ["我是", "爻荚（", "UlnClaw）", "，mock-answer"].iter().map(|s| s.chars().count()).sum();
    if text.chars().count() == expect_len {
        println!("  ✅ 断言6b：正文长度精确匹配（{} 字符）", expect_len);
    } else {
        println!("  ❌ 断言6b：正文长度 {} ≠ 期望 {}", text.chars().count(), expect_len);
        ok = false;
    }

    println!();
    if ok {
        println!("=== ✅ 全部通过：流式 FFI 通路正常 ===");
    } else {
        println!("=== ❌ 存在失败断言 ===");
        std::process::exit(1);
    }
}

/// mock OpenAI 兼容 SSE 服务器：真流式（多 chunk 增量），并记录是否带 stream:true
fn mock_server() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:8940").expect("bind 8940");
    for stream in listener.incoming() {
        let mut s = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let t = line.trim_end();
            if t.is_empty() {
                break;
            }
            if let Some(v) = t.to_lowercase().strip_prefix("content-length:") {
                content_len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_len];
        let _ = reader.read_exact(&mut body);
        let body_str = String::from_utf8_lossy(&body).to_string();

        let n = HITS.fetch_add(1, Ordering::SeqCst) + 1;
        // 🔴 根因验证点：检查请求体是否带 "stream":true
        let is_stream = body_str.contains("\"stream\":true");
        if is_stream {
            SAW_STREAM_TRUE.fetch_add(1, Ordering::SeqCst);
        }
        eprintln!("[mock] hit#{} stream={}", n, is_stream);

        // 用 serde_json 构造（手写 JSON 字面量的括号转义曾导致测试假失败）
        let mut sse = String::new();
        let pieces = ["我是", "爻荚（", "UlnClaw）", "，mock-answer"];
        for (i, piece) in pieces.iter().enumerate() {
            let chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": piece},
                }],
            });
            sse.push_str(&format!("data: {}\n\n", chunk));
            let _ = i;
        }
        // 收尾 chunk（finish_reason）
        let fin = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18}
        });
        sse.push_str(&format!("data: {}\n\n", fin));
        sse.push_str("data: [DONE]\n\n");

        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            sse.len()
        );
        let _ = s.write_all(head.as_bytes());
        let _ = s.write_all(sse.as_bytes());
        let _ = s.flush();
    }
}

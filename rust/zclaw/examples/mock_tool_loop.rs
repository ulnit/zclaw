//! Mock OpenAI 兼容 SSE 服务器：模拟「模型每轮都要求调工具、永不给最终答案」，
//! 用于确定性验证 dispatcher 的迭代耗尽兜底（真实模型不会稳定复现连烧 max_iterations 轮）。
//!
//! 行为（匹配 src/providers/compatible.rs 的 stream_chat 解析契约）：
//!   - 请求体含 "tools" → 回一个 SSE 流，delta.tool_calls 里给 web_search 调用
//!     （永不给最终答案，模拟模型陷入无限检索）
//!   - 请求体不含 "tools"（= dispatcher 的收口轮）→ 回纯文本答案 SSE 流
//!
//! 用法：cargo run --release --example mock_tool_loop   （监听 127.0.0.1:8931）
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

static HITS: AtomicUsize = AtomicUsize::new(0);

fn main() {
    let addr = "127.0.0.1:8931";
    let listener = TcpListener::bind(addr).expect("bind 8931");
    println!("MOCK_READY {}", addr);
    for stream in listener.incoming() {
        let mut s = match stream { Ok(s) => s, Err(_) => continue };
        let mut reader = BufReader::new(s.try_clone().unwrap());

        // 读请求头
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
        // 读 body
        let mut body = vec![0u8; content_len];
        let _ = reader.read_exact(&mut body);
        let body_str = String::from_utf8_lossy(&body).to_string();

        let n = HITS.fetch_add(1, Ordering::SeqCst) + 1;
        let has_tools = body_str.contains("\"tools\"");
        eprintln!("[mock] hit#{} has_tools={} bytes={}", n, has_tools, body_str.len());

        // 构造 SSE。
        // 🔴 用 serde_json 构造 chunk，不要手写 JSON 字面量——之前手写在
        //    「无 tools」分支用了 `}}}]}`（三重括号是 format! 的转义习惯），
        //    但普通字符串字面量里 `}}` 不会折叠成 `}`，于是多出一个括号 →
        //    JSON 非法 → stream_chat 解析 continue → 收口轮答案丢失，
        //    测试假失败（生产兜底反而正常触发了）。serde_json 保证括号永远配对。
        let sse = if has_tools {
            // 永远要求调工具（模拟模型陷入无限检索）
            let chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": "让我再搜索一下。",
                        "tool_calls": [{
                            "index": 0,
                            "id": format!("call_{}", n),
                            "type": "function",
                            "function": {
                                "name": "web_search",
                                "arguments": "{\"query\":\"杭州 二手房 价格\"}",
                            },
                        }],
                    },
                }],
            });
            format!("data: {}\n\ndata: [DONE]\n\n", chunk)
        } else {
            // 收口轮（dispatcher 不再传 tools）：给真正的答案文本
            let chunk = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": "【收口轮答案】根据已检索到的信息：杭州二手房均价约 3.2 万/㎡。",
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

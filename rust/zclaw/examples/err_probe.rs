//! 验证 describe_reqwest_error 能挖出真因并正确分类。
//! 用三种必然失败的请求：DNS 不存在、连接被拒、TLS 证书不可信。
use zclaw::providers::compatible::{describe_reqwest_error, Client, ChatMessage, StreamEvent};

fn main() {
    // tokio 未启用 "macros" feature，手动建 runtime（rt-multi-thread 已启用）
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .unwrap();
    rt.block_on(run());
}

async fn run() {
    let client = Client::new();
    let msgs = vec![ChatMessage::user("hi")];

    let cases: Vec<(&str, &str)> = vec![
        ("DNS 不存在", "https://no-such-host-omnimind-test-9x7q.invalid/v1/chat/completions"),
        ("连接被拒", "https://127.0.0.1:1/v1/chat/completions"),
        ("TLS 自签/不可信", "https://self-signed.badssl.com/v1/chat/completions"),
        ("TLS 过期证书", "https://expired.badssl.com/v1/chat/completions"),
        ("正常站点(对照)", "https://ai.ulnit.com/v1/chat/completions"),
    ];

    for (label, url) in cases {
        println!("\n===== {} =====", label);
        println!("url = {}", url);
        // 直接用 reqwest 复现 stream_chat 内部的请求路径
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .unwrap();
        let body = serde_json::json!({
            "model": "test",
            "messages": msgs,
            "temperature": 0.7,
            "stream": true,
        });
        match http.post(url)
            .header("Authorization", "Bearer test")
            .header("Content-Type", "application/json")
            .json(&body)
            .send().await
        {
            Ok(r) => println!("OK status={} (对照组应到这里)", r.status()),
            Err(e) => {
                println!("旧输出: request failed: {}", e);
                println!("新输出: {}", describe_reqwest_error(&e, url));
            }
        }
        let _ = &client;
        let _ = StreamEvent::Done { content: String::new(), tool_calls: None };
    }
}

//! 验证：reqwest 的 .timeout() 是「总时长」超时，SSE 长流被掐断时报什么错。
//! 对照 .read_timeout()（只测字节间空闲）能否让同样的慢流走通。
fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all().worker_threads(2).build().unwrap();
    rt.block_on(run());
}

async fn run() {
    let url = "http://127.0.0.1:8901/v1/chat/completions";
    let body = serde_json::json!({
        "model": "mock", "stream": true,
        "messages": [{"role":"user","content":"hi"}],
    });

    // ── A: 复刻当前 Client::new() 的写法（总超时，这里设 3s 以便快速复现）──
    println!("===== A. .timeout(3s) —— 当前 zclaw 的写法（缩小到 3s 复现）=====");
    let a = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build().unwrap();
    report(read_stream(&a, url, &body).await);

    // ── B: 改为 read_timeout（字节间空闲超时）+ connect_timeout，不设总超时 ──
    println!("\n===== B. .read_timeout(3s) 无总超时 —— 修复后的写法 =====");
    let b = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(10))
        .build().unwrap();
    report(read_stream(&b, url, &body).await);

    // ── C: read_timeout 足够长，应完整读完 ──
    println!("\n===== C. .read_timeout(30s) —— 应完整收到 =====");
    let c = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(30))
        .build().unwrap();
    report(read_stream(&c, url, &body).await);
}

async fn read_stream(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> (String, Vec<String>, bool) {
    use futures_util::StreamExt;
    let mut got: Vec<String> = Vec::new();
    let mut err_text = String::new();
    let mut done_seen = false;

    let resp = match client.post(url)
        .header("Content-Type", "application/json")
        .json(body).send().await
    {
        Ok(r) => r,
        Err(e) => return (format!("SEND_ERR: {}", e), got, done_seen),
    };
    println!("  status={} headers_ok", resp.status());

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => {
                buf.push_str(&String::from_utf8_lossy(&c));
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim().to_string();
                    buf = buf[pos + 1..].to_string();
                    if !line.starts_with("data:") { continue; }
                    let p = line.trim_start_matches("data:").trim();
                    if p == "[DONE]" { done_seen = true; break; }
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(p) {
                        if let Some(s) = v["choices"][0]["delta"]["content"].as_str() {
                            got.push(s.to_string());
                        }
                    }
                }
            }
            Err(e) => {
                // 这就是 zclaw 里 format!("stream error: {}", e) 的 e
                err_text = e.to_string();
                break;
            }
        }
    }
    (err_text, got, done_seen)
}

fn report((err_text, got, done_seen): (String, Vec<String>, bool)) {
    println!("  收到 chunk 数 = {}", got.len());
    println!("  部分正文 = {:?}", got.concat());
    println!("  [DONE] = {}", done_seen);
    if err_text.is_empty() {
        println!("  stream 错误 = (无)");
    } else {
        println!("  stream 错误 = {:?}", err_text);
        println!("  >>> zclaw 会 emit: \"stream error: {}\"", err_text);
    }
}

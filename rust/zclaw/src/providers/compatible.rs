//! OpenAI-compatible provider with streaming (SSE) support.
//! Matches the endpoint contract https://ai.ulnit.com/v1/chat/completions.

use serde_json::json;
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(content: &str) -> Self {
        Self { role: "user".into(), content: Value::String(content.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn user_multimodal(text: &str, image_data_url: &str) -> Self {
        let mut parts = Vec::new();
        if !text.trim().is_empty() {
            parts.push(json!({"type":"text", "text": text}));
        }
        parts.push(json!({"type":"image_url", "image_url":{"url": image_data_url}}));
        Self { role: "user".into(), content: Value::Array(parts), tool_calls: None, tool_call_id: None }
    }
    pub fn system(content: &str) -> Self {
        Self { role: "system".into(), content: Value::String(content.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn assistant(content: &str) -> Self {
        Self { role: "assistant".into(), content: Value::String(content.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn assistant_with_tools(content: &str, tool_calls: Value) -> Self {
        Self { role: "assistant".into(), content: Value::String(content.into()), tool_calls: Some(tool_calls), tool_call_id: None }
    }
    pub fn tool(call_id: &str, content: &str) -> Self {
        Self { role: "tool".into(), content: Value::String(content.into()), tool_calls: None, tool_call_id: Some(call_id.into()) }
    }
}

/// One incremental event while streaming.
#[derive(Debug)]
pub enum StreamEvent {
    Delta(String),
    Thinking(String),
    ToolCall { id: String, name: String, arguments: String },
    Done { content: String, tool_calls: Option<Value> },
    Error(String),
}

/// Turn a reqwest transport error into a message that actually says WHY it failed.
///
/// reqwest's top-level Display is just "error sending request for url (...)"; the
/// real cause (TLS handshake failure, DNS, connect timeout, ...) lives in the
/// error's source chain.  Surfacing only the top level made an on-device TLS
/// problem indistinguishable from a network outage.
///
/// Also prefixes a stable category tag so callers can decide whether retrying via
/// a different HTTP stack is worthwhile (see the Android OkHttp fallback).
pub fn describe_reqwest_error(e: &reqwest::Error, url: &str) -> String {
    // Walk the whole source chain, collecting every layer's message.
    let mut parts: Vec<String> = Vec::new();
    parts.push(e.to_string());
    let mut src: Option<&dyn std::error::Error> = std::error::Error::source(e);
    let mut depth = 0;
    while let Some(s) = src {
        if depth > 8 { break; }
        let m = s.to_string();
        if !parts.iter().any(|p| p == &m) {
            parts.push(m);
        }
        src = std::error::Error::source(s);
        depth += 1;
    }
    let joined = parts.join(" <- ");

    // Classify.  Order matters: check the most specific first.
    let low = joined.to_lowercase();
    let category = if low.contains("certificate") || low.contains("cert") || low.contains("tls")
        || low.contains("ssl") || low.contains("handshake") || low.contains("unknownissuer")
        || low.contains("webpki") || low.contains("trust anchor") || low.contains("invalid peer") {
        "TLS/证书错误"
    } else if low.contains("dns") || low.contains("resolve") || low.contains("nodename nor servname")
        || low.contains("name or service not known") || low.contains("getaddrinfo")
        || low.contains("temporary failure in name resolution") {
        "DNS 解析失败"
    } else if low.contains("timed out") || low.contains("timeout") {
        "连接超时"
    } else if low.contains("connection refused") || low.contains("reset")
        || low.contains("unreachable") || low.contains("broken pipe") {
        "连接被拒绝/中断"
    } else if low.contains("proxy") {
        "代理错误"
    } else {
        "网络请求失败"
    };

    // Host helps distinguish "this site is unreachable" from "no network at all".
    // Parsed by hand so we don't need the `url` crate (reqwest only pulls it in
    // transitively, so it is not a direct dependency).
    format!("[{}] {} (host={})", category, joined, host_of(url))
}

/// Extract "host[:port]" from an http(s) URL without pulling in the url crate.
fn host_of(url: &str) -> &str {
    let after_scheme = url.find("://").map(|i| &url[i + 3..]).unwrap_or(url);
    let authority = after_scheme.split(|c| c == '/' || c == '?' || c == '#').next().unwrap_or("");
    if authority.is_empty() { "?" } else { authority }
}

/// Self-contained network/TLS probe, surfaced to the UI so a transport failure can
/// actually be diagnosed on-device instead of guessed at from a dev machine.
///
/// Reports each stage separately (DNS+TCP+TLS handshake vs. HTTP response), plus
/// the device clock — a wrong system clock makes every TLS handshake fail with a
/// certificate-expired error, which is otherwise indistinguishable from a broken
/// certificate chain.
pub async fn network_probe(url: &str, api_key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let host = host_of(url).to_string();

    // 0. Device clock — a skewed clock breaks TLS verification everywhere.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    out.push(format!("时钟: unix={} ({})", now,
        chrono::DateTime::from_timestamp(now as i64, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "?".to_string())));

    // 1. DNS resolution (bypasses the HTTP stack entirely).
    let dns_target = format!("https://{}/dns-probe", host);
    match tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let addrs: Vec<std::net::SocketAddr> = {
            // reqwest has no public resolve-only API; do a plain lookup via std.
            let h = host.split(':').next().unwrap_or(&host);
            let port: u16 = host.split(':').nth(1)
                .and_then(|p| p.parse().ok())
                .unwrap_or(443);
            let mut v = Vec::new();
            for ip in std::net::ToSocketAddrs::to_socket_addrs(&format!("{}:{}", h, port))
                .unwrap_or_else(|_| vec![].into_iter())
            {
                v.push(ip);
            }
            v
        };
        addrs
    }).await {
        Ok(addrs) if addrs.is_empty() => out.push(format!("DNS: {} 解析到 0 个地址 ✗", host)),
        Ok(addrs) => out.push(format!("DNS: {} -> {} ✓", host,
            addrs.iter().take(4).map(|a| a.ip().to_string()).collect::<Vec<_>>().join(", "))),
        Ok(_) => out.push(format!("DNS: {} 无结果 ✗", host)),
        Err(_) => out.push(format!("DNS: {} 超时(15s) ✗", host)),
    }

    // 2. TLS handshake + HTTP status via GET /v1/models (cheap, no tokens spent).
    let models_url = format!("https://{}/v1/models", host);
    let http = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => { out.push(format!("HTTP 客户端初始化失败: {}", describe_reqwest_error(&e, &models_url))); return out; }
    };
    let started = std::time::Instant::now();
    match http.get(&models_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send().await
    {
        Ok(r) => out.push(format!("TLS+HTTP: GET /v1/models -> {} ({} ms) ✓",
            r.status(), started.elapsed().as_millis())),
        Err(e) => out.push(format!("TLS+HTTP: GET /v1/models ✗ {}", describe_reqwest_error(&e, &models_url))),
    }

    // 3. The failing endpoint itself, with a minimal body — distinguishes
    //    "TLS fine but this path broken" from "no connectivity at all".
    let body = serde_json::json!({
        "model": "probe",
        "messages": [{"role": "user", "content": "ping"}],
        "stream": true,
        "max_tokens": 1,
    });
    let started2 = std::time::Instant::now();
    match http.post(&format!("https://{}/v1/chat/completions", host))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send().await
    {
        Ok(r) => out.push(format!("chat/completions: -> {} ({} ms) ✓ 通路正常",
            r.status(), started2.elapsed().as_millis())),
        Err(e) => out.push(format!("chat/completions: ✗ {}",
            describe_reqwest_error(&e, &format!("https://{}/v1/chat/completions", host)))),
    }
    let _ = dns_target;
    out
}



pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap(),
        }
    }

    /// Stream a chat completion, emitting deltas; returns the final assistant message parts.
    pub async fn stream_chat(
        &self,
        url: &str,
        api_key: &str,
        model: &str,
        temperature: f32,
        messages: &[ChatMessage],
        tools: Option<Value>,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> anyhow::Result<(String, Option<Value>)> {
        let mut body = json!({
            "model": model,
            "messages": messages,
            "temperature": temperature,
            "stream": true,
        });
        if let Some(t) = tools {
            if !t.as_array().map(|a| a.is_empty()).unwrap_or(true) {
                body["tools"] = t;
            }
        }

        let req = self.http
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body);

        let resp = req.send().await.map_err(|e| anyhow::anyhow!("{}", describe_reqwest_error(&e, url)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let msg = format!("HTTP {}: {}", status, &text[..text.len().min(300)]);
            on_event(StreamEvent::Error(msg.clone()));
            return Err(anyhow::anyhow!(msg));
        }

        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut full_content = String::new();
        let mut buf = String::new();
        // accumulate tool_call fragments per index
        let mut tool_calls: Vec<serde_json::Map<String, Value>> = Vec::new();

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    on_event(StreamEvent::Error(format!("stream error: {}", e)));
                    break;
                }
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // process complete SSE lines
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim().to_string();
                buf = buf[pos + 1..].to_string();
                if !line.starts_with("data:") { continue; }
                let payload = line.trim_start_matches("data:").trim();
                if payload == "[DONE]" { break; }
                let val: Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let delta = &val["choices"][0]["delta"];
                if let Some(c) = delta["content"].as_str() {
                    if !c.is_empty() {
                        full_content.push_str(c);
                        on_event(StreamEvent::Delta(c.to_string()));
                    }
                }
                if let Some(th) = delta["reasoning_content"].as_str() {
                    if !th.is_empty() {
                        on_event(StreamEvent::Thinking(th.to_string()));
                    }
                }
                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                        while tool_calls.len() <= idx { tool_calls.push(serde_json::Map::new()); }
                        let entry = &mut tool_calls[idx];
                        if let Some(id) = tc["id"].as_str() { entry.insert("id".into(), json!(id)); }
                        if let Some(n) = tc["function"]["name"].as_str() {
                            if !n.is_empty() { entry.insert("name".into(), json!(n)); }
                        }
                        if let Some(a) = tc["function"]["arguments"].as_str() {
                            let prev = entry.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                            entry.insert("arguments".into(), json!(format!("{}{}", prev, a)));
                        }
                        // emit tool call when we have id+name
                        if let (Some(id), Some(name)) = (entry.get("id").and_then(|v| v.as_str()), entry.get("name").and_then(|v| v.as_str())) {
                            if !name.is_empty() {
                                let args = entry.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                                on_event(StreamEvent::ToolCall { id: id.into(), name: name.into(), arguments: args.into() });
                            }
                        }
                    }
                }
            }
        }

        let tc_val: Option<Value> = if tool_calls.is_empty() {
            None
        } else {
            let arr: Vec<Value> = tool_calls.into_iter().map(|m| {
                let mut o = serde_json::Map::new();
                o.insert("id".into(), m.get("id").cloned().unwrap_or(json!("")));
                o.insert("type".into(), json!("function"));
                o.insert("function".into(), json!({
                    "name": m.get("name").cloned().unwrap_or(json!("")),
                    "arguments": m.get("arguments").cloned().unwrap_or(json!("")),
                }));
                Value::Object(o)
            }).collect();
            Some(json!(arr))
        };

        on_event(StreamEvent::Done { content: full_content.clone(), tool_calls: tc_val.clone() });
        Ok((full_content, tc_val))
    }
}

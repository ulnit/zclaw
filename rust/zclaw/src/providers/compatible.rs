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
    } else if low.contains("decoding response body") || low.contains("incomplete message")
        || low.contains("unexpected end of") || low.contains("connection closed")
        || low.contains("body write aborted") {
        // 响应体读取中途失败：流式(SSE)长对话被掐断的典型信号。
        // 常见成因＝客户端用了「总时长超时」而非「读取空闲超时」，或 HTTP/2 流被中间盒 RST。
        "响应流中途断开"
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
        // 注意：此处不需要再加 `Ok(_)` 分支——上面两条已穷尽所有 Ok，
        // 多写一条是死代码（编译器 unreachable pattern），且会让维护者误以为有第三种情况。
        Err(_) => out.push(format!("DNS: {} 超时(15s) ✗", host)),
    }

    // 2. TLS handshake + HTTP status via GET /v1/models (cheap, no tokens spent).
    //    顺便从这里取一个**该 key 真实可用**的模型名，给第 3 步的 chat 探针用。
    //    🔴 此前第 3 步硬编码 "model": "probe"，而后端按模型名做渠道分发，
    //    probe 不存在 → 必然 503「分组 default 下模型 probe 无可用渠道」。
    //    结果诊断永远报 chat 失败，而真实对话其实是好的（已用后端日志实证：
    //    同一时刻 aurora-5-mini 请求全部 200 / end_reason=done）。
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
    let mut probe_model: Option<String> = None;
    match http.get(&models_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send().await
    {
        Ok(r) => {
            let st = r.status();
            let ms = started.elapsed().as_millis();
            // 按状态码判定，不能无条件打 ✓（否则 401/503 也显示成功，自相矛盾）
            if st.is_success() {
                // 解析 data[].id，挑第一个非 probe 的模型给第 3 步用
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    probe_model = v["data"].as_array().and_then(|arr| {
                        arr.iter()
                            .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                            .find(|id| !id.is_empty())
                    });
                }
                out.push(format!("TLS+HTTP: GET /v1/models -> {} ({} ms) ✓", st, ms));
            } else {
                let text = r.text().await.unwrap_or_default();
                out.push(format!("TLS+HTTP: GET /v1/models -> {} ({} ms) ✗ {}", st, ms,
                    crate::tools::truncate_utf8(&text.replace('\n', " "), 160).0));
            }
        }
        Err(e) => out.push(format!("TLS+HTTP: GET /v1/models ✗ {}", describe_reqwest_error(&e, &models_url))),
    }

    // 3. The failing endpoint itself, with a minimal body — distinguishes
    //    "TLS fine but this path broken" from "no connectivity at all".
    //    模型名用第 2 步取到的真实模型；取不到就跳过这一步并说明原因，
    //    绝不拿不存在的模型名去探（那只会产出无意义的 503 误导排查）。
    let Some(model) = probe_model else {
        out.push("chat/completions: 跳过（未能从 /v1/models 取到可用模型名，无法构造有效探针请求）".into());
        let _ = dns_target;
        return out;
    };
    let body = serde_json::json!({
        "model": model,
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
        Ok(r) => {
            let st = r.status();
            let ms = started2.elapsed().as_millis();
            // 🔴 必须按状态码判定。此前无条件打「✓ 通路正常」，导致 503/401
            //    也显示成功（截图实证：`503 Service Unavailable (71 ms) ✓ 通路正常`），
            //    自相矛盾且把排查带偏。现在成功才 ✓，失败带上错误体片段。
            if st.is_success() {
                out.push(format!("chat/completions: [{}] -> {} ({} ms) ✓ 通路正常",
                    model, st, ms));
            } else {
                let text = r.text().await.unwrap_or_default();
                out.push(format!("chat/completions: [{}] -> {} ({} ms) ✗ {}",
                    model, st, ms, crate::tools::truncate_utf8(&text.replace('\n', " "), 200).0));
            }
        }
        Err(e) => out.push(format!("chat/completions: [{}] ✗ {}", model,
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
        // ⚠️ 不要用 .timeout()：reqwest 的 timeout 是「从连接建立到响应体读完」的
        // 总时长超时。SSE 流式对话的 body 会一直读，所以一次较长的生成（推理模型
        // 思考、长回答、多轮工具循环）只要超过这个总时长就会被掐断，报
        // `error decoding response body` —— 表现为对话中途失败，而服务端日志显示
        // 请求 200 且正常产出了内容。已用慢速 SSE mock 实证复现并验证修复。
        //
        // 正确做法：
        // - connect_timeout：只管建连
        // - read_timeout：只管「相邻两次收到字节之间的空闲」，只要上游持续吐 token
        //   就不会触发，无论整轮对话多长
        // - 不设总超时：对话长度由 agent 的 max_iterations 和上游模型自己决定
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .read_timeout(std::time::Duration::from_secs(300))
                // 移动网络切换/中间盒回收空闲连接时，保活能减少流中途被断
                .tcp_keepalive(std::time::Duration::from_secs(30))
                .pool_idle_timeout(std::time::Duration::from_secs(90))
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
            // 必须用安全截断：后端错误体常是中文 JSON，直接 &text[..300]
            // 落在多字节字符中间会 panic，而本库 panic="abort" → 杀死整个 App
            let msg = format!("HTTP {}: {}", status, crate::tools::truncate_utf8(&text, 300).0);
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
                    on_event(StreamEvent::Error(format!("stream error: {}", describe_reqwest_error(&e, url))));
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
                // 思考流字段兼容 —— 不同上游用不同字段名，必须全读：
                //   · DeepSeek 官方 / 多数 OpenAI 兼容端：reasoning_content
                //   · OpenRouter（本项目渠道）：reasoning + reasoning_details
                // reasoning_details 是结构化形式 [{"type":"reasoning.text","text":"…","index":0}]，
                // 仅在 reasoning 缺席时作为兜底，避免同一份思考被发射两次。
                //
                // 🔴 历史 bug：此前只读 reasoning_content，OpenRouter 渠道下永远匹配不到，
                // 思考流被 100% 丢弃；而思考期 delta.content 全是空字符串（被下方
                // !c.is_empty() 过滤），于是客户端在整个思考期收不到任何 chunk ——
                // 实测复杂问题思考期长达 24s~381s，UI 表现为「光标静止、永远等不到结束」。
                let mut think: Option<String> = None;
                for key in ["reasoning_content", "reasoning"] {
                    if let Some(th) = delta[key].as_str() {
                        if !th.is_empty() { think = Some(th.to_string()); break; }
                    }
                }
                if think.is_none() {
                    if let Some(arr) = delta["reasoning_details"].as_array() {
                        let mut buf = String::new();
                        for item in arr {
                            if let Some(t) = item["text"].as_str() { buf.push_str(t); }
                        }
                        if !buf.is_empty() { think = Some(buf); }
                    }
                }
                if let Some(th) = think {
                    on_event(StreamEvent::Thinking(th));
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

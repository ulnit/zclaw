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

        let resp = req.send().await.map_err(|e| anyhow::anyhow!("request failed: {}", e))?;
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

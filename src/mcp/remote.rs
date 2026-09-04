//! Remote MCP transports — Streamable HTTP and SSE (hermes `mcp_tool.py`
//! HTTP/SSE paths; the Python MCP SDK is replaced by a hand-rolled
//! JSON-RPC-over-HTTP client).
//!
//! - Streamable HTTP (default when `url` is set): every JSON-RPC message
//!   is POSTed to the server URL with
//!   `Accept: application/json, text/event-stream`; responses may be
//!   plain JSON or an SSE stream. The `Mcp-Session-Id` header returned by
//!   `initialize` is echoed on every later request.
//! - SSE (`transport = "sse"`, the pre-2025-03-26 protocol): a GET on the
//!   URL opens an SSE stream whose first `endpoint` event names the POST
//!   target; JSON-RPC messages are POSTed there and answers arrive as
//!   `message` events on the stream, correlated by id.

use crate::error::{AgentError, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

/// Protocol version for HTTP transports (hermes `LATEST_PROTOCOL_VERSION`
/// fallback — Streamable HTTP was introduced by 2025-03-26).
const PROTOCOL_VERSION: &str = "2025-03-26";
const REQUEST_TIMEOUT_SECS: u64 = 30;

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value>>>;

/// 401 recovery hook: returns a fresh bearer token (hermes
/// `MCPOAuthManager.handle_401` wiring point). Receives the FAILED
/// access token so recoveries can be deduped per token (hermes
/// `pending_401` keying).
pub type ReauthFn = std::sync::Arc<
    dyn Fn(
            Option<String>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>
        + Send
        + Sync,
>;

/// Pre-request disk-watch hook (hermes `HermesMCPOAuthProvider` pre-flow
/// `invalidate_if_disk_changed`): one cheap stat() per request; returns
/// a fresh token when an external process refreshed the tokens file,
/// `None` otherwise.
pub type TokenWatchFn = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

/// A running remote MCP connection (Streamable HTTP or SSE).
pub struct RemoteMcpClient {
    http: reqwest::Client,
    /// POST endpoint (the configured URL for Streamable HTTP; the
    /// announced endpoint for SSE).
    endpoint: Arc<Mutex<String>>,
    headers: HashMap<String, String>,
    session_id: Arc<Mutex<Option<String>>>,
    next_id: Arc<Mutex<u64>>,
    pending: Arc<Mutex<PendingMap>>,
    /// SSE stream reader (SSE transport only).
    reader_handle: Option<JoinHandle<()>>,
    transport: Transport,
    /// Current OAuth bearer token (when `auth = oauth`).
    bearer: Arc<Mutex<Option<String>>>,
    /// One-shot-per-request 401 recovery hook.
    reauth: Arc<Mutex<Option<ReauthFn>>>,
    /// Pre-request token disk-watch hook (cross-process refresh pickup).
    token_watch: Arc<Mutex<Option<TokenWatchFn>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    StreamableHttp,
    Sse,
}

/// One parsed SSE event.
#[derive(Debug, Default)]
struct SseEvent {
    event: String,
    data: String,
}

/// Split an SSE buffer into complete events + leftover.
fn split_sse_events(buffer: &str) -> (Vec<SseEvent>, String) {
    let mut events = Vec::new();
    let mut rest_start = 0usize;
    let bytes = buffer.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        let is_double_nl = bytes[i] == b'\n' && bytes[i + 1] == b'\n';
        let is_double_crlf = i + 3 < bytes.len() && &bytes[i..i + 4] == b"\r\n\r\n";
        if is_double_nl || is_double_crlf {
            let chunk = &buffer[rest_start..i];
            if let Some(event) = parse_sse_chunk(chunk) {
                events.push(event);
            }
            i += if is_double_crlf { 4 } else { 2 };
            rest_start = i;
            continue;
        }
        i += 1;
    }
    (events, buffer[rest_start..].to_string())
}

fn parse_sse_chunk(chunk: &str) -> Option<SseEvent> {
    let mut event = SseEvent::default();
    let mut has_data = false;
    for line in chunk.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event.event = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if has_data {
                event.data.push('\n');
            }
            event.data.push_str(value.trim_start());
            has_data = true;
        }
    }
    if event.event.is_empty() {
        event.event = "message".to_string();
    }
    has_data.then_some(event)
}

impl RemoteMcpClient {
    /// Connect with the transport selected by config (`transport = "sse"`
    /// → SSE, anything else → Streamable HTTP). Runs the initialize
    /// handshake.
    pub async fn connect(
        url: &str,
        transport: Option<&str>,
        headers: &HashMap<String, String>,
        bearer: Option<String>,
        reauth: Option<ReauthFn>,
        token_watch: Option<TokenWatchFn>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| AgentError::Tool(format!("MCP http client: {}", e)))?;
        let is_sse = transport
            .map(|t| t.eq_ignore_ascii_case("sse"))
            .unwrap_or(false);
        let mut client = Self {
            http,
            endpoint: Arc::new(Mutex::new(url.to_string())),
            headers: headers.clone(),
            session_id: Arc::new(Mutex::new(None)),
            next_id: Arc::new(Mutex::new(1)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            reader_handle: None,
            transport: if is_sse {
                Transport::Sse
            } else {
                Transport::StreamableHttp
            },
            bearer: Arc::new(Mutex::new(bearer)),
            reauth: Arc::new(Mutex::new(reauth)),
            token_watch: Arc::new(Mutex::new(token_watch)),
        };

        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "ulnclaw", "version": crate::VERSION}
        });
        if is_sse {
            client.start_sse_reader(url).await?;
        }
        client.request("initialize", init).await?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    /// SSE transport: open the GET stream, wait for the `endpoint` event,
    /// and spawn the reader task that dispatches `message` events to
    /// pending requests.
    async fn start_sse_reader(&mut self, url: &str) -> Result<()> {
        let mut request = self.http.get(url).header("Accept", "text/event-stream");
        if let Some(token) = self.bearer.lock().await.clone() {
            request = request.header("Authorization", format!("Bearer {}", token));
        }
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AgentError::Tool(format!("MCP SSE GET {}: {}", url, e)))?;
        if !response.status().is_success() {
            return Err(AgentError::Tool(format!(
                "MCP SSE GET {}: HTTP {}",
                url,
                response.status()
            )));
        }

        let (endpoint_tx, endpoint_rx) = oneshot::channel::<String>();
        let endpoint_slot = self.endpoint.clone();
        let pending = self.pending.clone();
        let base_url = url.to_string();
        let mut stream = response.bytes_stream();

        let reader_handle = tokio::spawn(async move {
            use futures::StreamExt;
            let mut buffer = String::new();
            let mut endpoint_tx = Some(endpoint_tx);
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                let (events, rest) = split_sse_events(&buffer);
                buffer = rest;
                for event in events {
                    match event.event.as_str() {
                        "endpoint" => {
                            let resolved = resolve_sse_endpoint(&base_url, event.data.trim());
                            if let Some(tx) = endpoint_tx.take() {
                                *endpoint_slot.lock().await = resolved.clone();
                                tx.send(resolved).ok();
                            }
                        }
                        "message" => {
                            let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
                                continue;
                            };
                            let Some(id) = message.get("id").and_then(|v| v.as_u64()) else {
                                continue;
                            };
                            let outcome = if let Some(result) = message.get("result").cloned() {
                                Ok(result)
                            } else if let Some(error) = message.get("error").cloned() {
                                Err(AgentError::Tool(format!("MCP error: {}", error)))
                            } else {
                                continue;
                            };
                            let mut pending = pending.lock().await;
                            if let Some(sender) = pending.remove(&id) {
                                sender.send(outcome).ok();
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
        self.reader_handle = Some(reader_handle);

        // The endpoint event must arrive before we can POST anything.
        let endpoint = tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), endpoint_rx)
            .await
            .map_err(|_| AgentError::Tool(format!("MCP SSE: no endpoint event from {}", url)))?
            .map_err(|_| AgentError::Tool("MCP SSE: endpoint channel closed".to_string()))?;
        *self.endpoint.lock().await = endpoint;
        Ok(())
    }

    async fn next_id(&self) -> u64 {
        let mut next = self.next_id.lock().await;
        let id = *next;
        *next += 1;
        id
    }

    /// Send one JSON-RPC request and await its response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id().await;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let outcome = match self.transport {
            Transport::StreamableHttp => {
                self.pending.lock().await.remove(&id);
                self.post_and_parse(&body, Some(id)).await
            }
            Transport::Sse => {
                self.post_message(&body).await?;
                match tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS), receiver)
                    .await
                {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(AgentError::Tool("MCP channel closed".into())),
                    Err(_) => {
                        self.pending.lock().await.remove(&id);
                        Err(AgentError::Tool(format!("MCP '{}' timed out", method)))
                    }
                }
            }
        };
        outcome
    }

    /// Send one JSON-RPC notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match self.transport {
            Transport::StreamableHttp => {
                self.post_and_parse::<Value>(&body, None).await.map(|_| ())
            }
            Transport::Sse => self.post_message(&body).await,
        }
    }

    /// POST one message to the endpoint (SSE transport).
    async fn post_message(&self, body: &Value) -> Result<()> {
        let endpoint = self.endpoint.lock().await.clone();
        let mut request = self.http.post(&endpoint).json(body);
        if let Some(token) = self.bearer.lock().await.clone() {
            request = request.header("Authorization", format!("Bearer {}", token));
        }
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AgentError::Tool(format!("MCP POST {}: {}", endpoint, e)))?;
        if !response.status().is_success() {
            return Err(AgentError::Tool(format!(
                "MCP POST {}: HTTP {}",
                endpoint,
                response.status()
            )));
        }
        Ok(())
    }

    /// POST with bearer auth + one 401 recovery retry (hermes
    /// `handle_401`: refresh/re-auth once, then replay the request).
    async fn post_with_auth(&self, endpoint: &str, body: &Value) -> Result<reqwest::Response> {
        let send = |bearer: Option<String>| {
            let mut request = self
                .http
                .post(endpoint)
                .header("Accept", "application/json, text/event-stream")
                .json(body);
            if let Some(token) = bearer {
                request = request.header("Authorization", format!("Bearer {}", token));
            }
            request
        };
        // Pre-request disk watch: pick up tokens an external process
        // refreshed on disk (hermes pre-flow invalidate_if_disk_changed).
        if let Some(watch) = self.token_watch.lock().await.clone() {
            if let Some(fresh) = watch().await {
                *self.bearer.lock().await = Some(fresh);
            }
        }
        let mut request = send(self.bearer.lock().await.clone());
        if let Some(session_id) = self.session_id.lock().await.clone() {
            request = request.header("Mcp-Session-Id", session_id);
        }
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AgentError::Tool(format!("MCP POST {}: {}", endpoint, e)))?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        // 401 — ask the recovery hook for a fresh token, retry once.
        // Pass the FAILED token so the manager can dedupe thundering-herd
        // 401s (hermes handle_401 keying).
        let failed_token = self.bearer.lock().await.clone();
        let hook = self.reauth.lock().await.clone();
        let Some(hook) = hook else {
            return Ok(response);
        };
        let fresh = hook(failed_token).await?;
        *self.bearer.lock().await = Some(fresh.clone());
        let mut request = send(Some(fresh));
        if let Some(session_id) = self.session_id.lock().await.clone() {
            request = request.header("Mcp-Session-Id", session_id);
        }
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        request
            .send()
            .await
            .map_err(|e| AgentError::Tool(format!("MCP POST {}: {}", endpoint, e)))
    }

    /// Streamable HTTP POST: parse a plain-JSON or SSE response body.
    /// `expect_id` = Some for requests (returns the matching reply), None
    /// for notifications (status check only).
    async fn post_and_parse<T>(&self, body: &Value, expect_id: Option<u64>) -> Result<T>
    where
        T: serde::de::DeserializeOwned + Default,
    {
        let endpoint = self.endpoint.lock().await.clone();
        let response = self.post_with_auth(&endpoint, body).await?;

        if let Some(session_header) = response.headers().get("mcp-session-id") {
            if let Ok(value) = session_header.to_str() {
                *self.session_id.lock().await = Some(value.to_string());
            }
        }

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = response
            .text()
            .await
            .map_err(|e| AgentError::Tool(format!("MCP read body: {}", e)))?;

        if !status.is_success() {
            return Err(AgentError::Tool(format!(
                "MCP POST {}: HTTP {} {}",
                endpoint,
                status,
                text.chars().take(200).collect::<String>()
            )));
        }
        let Some(id) = expect_id else {
            return Ok(T::default());
        };

        let message = if content_type.starts_with("text/event-stream") {
            let (events, _) = split_sse_events(&text);
            events
                .into_iter()
                .filter(|e| e.event == "message")
                .find_map(|e| serde_json::from_str::<Value>(&e.data).ok())
                .ok_or_else(|| AgentError::Tool("MCP SSE body carried no message".into()))?
        } else {
            serde_json::from_str::<Value>(&text)
                .map_err(|e| AgentError::Tool(format!("MCP bad JSON reply: {}", e)))?
        };

        if message.get("id").and_then(|v| v.as_u64()) != Some(id) {
            return Err(AgentError::Tool("MCP reply id mismatch".into()));
        }
        if let Some(error) = message.get("error").cloned() {
            return Err(AgentError::Tool(format!("MCP error: {}", error)));
        }
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        serde_json::from_value(result).map_err(|e| AgentError::Tool(format!("MCP result: {}", e)))
    }

    /// List the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<Value>> {
        let result: Value = self.request("tools/list", json!({})).await?;
        Ok(result
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Call a tool on the server.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    /// Close the connection (drops the SSE stream; HTTP is stateless).
    pub async fn close(&mut self) {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for RemoteMcpClient {
    fn drop(&mut self) {
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
        }
    }
}

/// Resolve the SSE `endpoint` event value against the stream URL (servers
/// usually send a path like `/messages?sessionId=...`).
fn resolve_sse_endpoint(base_url: &str, endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }
    let Ok(base) = reqwest::Url::parse(base_url) else {
        return endpoint.to_string();
    };
    base.join(endpoint)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| endpoint.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_event_parsing() {
        let buffer =
            "event: endpoint\ndata: /messages?x=1\n\nevent: message\ndata: {\"id\": 1}\n\npartial";
        let (events, rest) = split_sse_events(buffer);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "endpoint");
        assert_eq!(events[0].data, "/messages?x=1");
        assert_eq!(events[1].event, "message");
        assert_eq!(events[1].data, "{\"id\": 1}");
        assert_eq!(rest, "partial");
    }

    #[test]
    fn sse_default_event_name_is_message() {
        let (events, _) = split_sse_events("data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
        assert_eq!(events[0].data, "hello");
    }

    /// Minimal Streamable HTTP MCP server: JSON-RPC over POST with an
    /// `Mcp-Session-Id` header; notifications get 202.
    #[tokio::test]
    async fn streamable_http_end_to_end() {
        use axum::extract::Json;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use axum::Router;
        use serde_json::json;

        async fn rpc(Json(req): Json<Value>) -> (StatusCode, HeaderMap, Json<Value>) {
            let mut headers = HeaderMap::new();
            headers.insert("mcp-session-id", "sess-123".parse().unwrap());
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let Some(id) = req.get("id").cloned() else {
                // notification
                return (StatusCode::ACCEPTED, headers, Json(Value::Null));
            };
            let result = match method {
                "initialize" => {
                    json!({"serverInfo": {"name": "mock"}, "protocolVersion": "2025-03-26"})
                }
                "tools/list" => json!({"tools": [{
                    "name": "ping",
                    "description": "ping tool",
                    "inputSchema": {"type": "object", "properties": {}}
                }]}),
                "tools/call" => {
                    json!({"content": [{"type": "text", "text": "pong"}], "isError": false})
                }
                _ => json!({}),
            };
            (
                StatusCode::OK,
                headers,
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/mcp", post(rpc)))
                .await
                .ok();
        });

        let url = format!("http://127.0.0.1:{}/mcp", port);
        let client = RemoteMcpClient::connect(&url, None, &HashMap::new(), None, None, None)
            .await
            .expect("streamable connect");
        assert_eq!(
            client.session_id.lock().await.as_deref(),
            Some("sess-123"),
            "session header captured"
        );
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "ping");
        let result = client.call_tool("ping", json!({})).await.unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(|v| v.as_str()),
            Some("pong")
        );
    }

    /// Minimal SSE-transport MCP server: GET /sse announces the POST
    /// endpoint, then relays JSON-RPC replies as `message` events.
    #[tokio::test]
    async fn sse_transport_end_to_end() {
        use axum::extract::{Json, State};
        use axum::http::StatusCode;
        use axum::response::sse::{Event, Sse};
        use axum::routing::{get, post};
        use axum::Router;
        use futures::StreamExt;
        use serde_json::json;

        async fn messages(
            State(tx): State<tokio::sync::mpsc::UnboundedSender<String>>,
            Json(req): Json<Value>,
        ) -> StatusCode {
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let Some(id) = req.get("id").cloned() else {
                return StatusCode::ACCEPTED;
            };
            let result = match method {
                "initialize" => {
                    json!({"serverInfo": {"name": "mock-sse"}, "protocolVersion": "2024-11-05"})
                }
                "tools/list" => json!({"tools": [{
                    "name": "echo",
                    "description": "echo",
                    "inputSchema": {"type": "object", "properties": {}}
                }]}),
                "tools/call" => {
                    json!({"content": [{"type": "text", "text": "sse-pong"}], "isError": false})
                }
                _ => json!({}),
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            let _ = tx.send(reply);
            StatusCode::ACCEPTED
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let rx_slot: Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>> =
            Arc::new(tokio::sync::Mutex::new(Some(rx)));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let app = Router::new()
                .route(
                    "/sse",
                    get({
                        let rx_slot = rx_slot.clone();
                        move || {
                            let rx_slot = rx_slot.clone();
                            async move {
                                let endpoint = futures::stream::once(async {
                                    Ok::<_, std::convert::Infallible>(
                                        Event::default().event("endpoint").data("/messages"),
                                    )
                                });
                                let replies = futures::stream::unfold(rx_slot, |slot| async move {
                                    let reply = {
                                        let mut guard = slot.lock().await;
                                        guard.as_mut()?.recv().await
                                    };
                                    let reply = reply?;
                                    Some((
                                        Ok(Event::default().event("message").data(reply)),
                                        slot.clone(),
                                    ))
                                });
                                Sse::new(endpoint.chain(replies))
                            }
                        }
                    }),
                )
                .route("/messages", post(messages))
                .with_state(tx);
            axum::serve(listener, app).await.ok();
        });

        let url = format!("http://127.0.0.1:{}/sse", port);
        let client = RemoteMcpClient::connect(&url, Some("sse"), &HashMap::new(), None, None, None)
            .await
            .expect("sse connect");
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo");
        let result = client.call_tool("echo", json!({})).await.unwrap();
        assert_eq!(
            result.pointer("/content/0/text").and_then(|v| v.as_str()),
            Some("sse-pong")
        );
    }

    /// A stale bearer gets one 401, the reauth hook supplies a fresh
    /// token, and the retried request succeeds (hermes handle_401).
    #[tokio::test]
    async fn unauthorized_triggers_reauth_retry() {
        use axum::extract::Json;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use axum::Router;
        use serde_json::json;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        static REAUTH_CALLS: AtomicUsize = AtomicUsize::new(0);

        async fn rpc(headers: HeaderMap, Json(req): Json<Value>) -> (StatusCode, Json<Value>) {
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let Some(id) = req.get("id").cloned() else {
                return (StatusCode::ACCEPTED, Json(Value::Null));
            };
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if auth != "Bearer fresh-token" {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "unauthorized"})),
                );
            }
            let result = match method {
                "initialize" => {
                    json!({"serverInfo": {"name": "secure"}, "protocolVersion": "2025-03-26"})
                }
                "tools/list" => json!({"tools": []}),
                _ => json!({}),
            };
            (
                StatusCode::OK,
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result})),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/mcp", post(rpc)))
                .await
                .ok();
        });

        static FAILED_TOKENS: tokio::sync::Mutex<Vec<Option<String>>> =
            tokio::sync::Mutex::const_new(Vec::new());
        let reauth: ReauthFn = Arc::new(|failed: Option<String>| {
            Box::pin(async move {
                REAUTH_CALLS.fetch_add(1, Ordering::SeqCst);
                FAILED_TOKENS.lock().await.push(failed);
                Ok("fresh-token".to_string())
            })
        });
        let url = format!("http://127.0.0.1:{}/mcp", port);
        // connect() itself runs initialize — which 401s and recovers.
        let client = RemoteMcpClient::connect(
            &url,
            None,
            &HashMap::new(),
            Some("stale-token".into()),
            Some(reauth),
            None,
        )
        .await
        .expect("connect recovers from 401");
        assert!(client.list_tools().await.unwrap().is_empty());
        assert!(REAUTH_CALLS.load(Ordering::SeqCst) >= 1);
        // The hook received the FAILED token (hermes pending_401 keying).
        let seen = FAILED_TOKENS.lock().await;
        assert!(
            seen.iter().any(|t| t.as_deref() == Some("stale-token")),
            "{seen:?}"
        );
    }

    #[test]
    fn endpoint_resolution() {
        assert_eq!(
            resolve_sse_endpoint("http://127.0.0.1:9/sse", "/messages?x=1"),
            "http://127.0.0.1:9/messages?x=1"
        );
        assert_eq!(
            resolve_sse_endpoint("http://h/sse", "https://other/m"),
            "https://other/m"
        );
    }
}

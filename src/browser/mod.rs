//! Browser CDP client — the missing backend for the `browser_*` tools.
//!
//! Speaks the Chrome DevTools Protocol over WebSocket:
//!   - endpoint resolution (`ULNCLAW_BROWSER_CDP` ws:// URL or
//!     http://host:port discovery via `/json` + `/json/version`)
//!   - optional managed launch of a local Chrome/Chromium
//!   - cloud browser providers (Browserbase / Browser Use / Firecrawl) via
//!     the `cloud` submodule, selected by `[browser] cloud_provider`
//!   - page session with navigate/snapshot/click/type/scroll/press/
//!     screenshot/evaluate/dialog handling
//!
//! Snapshots produce Playwright-style accessibility listings with numeric
//! element refs (`[3] button "Submit"`); click/type resolve refs through
//! `DOM.resolveNode` + `Runtime.callFunctionOn`.

pub mod camofox;
pub mod cloud;
pub mod connect;
pub mod guard;

use crate::error::{AgentError, Result};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream};

type WsStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Resolved browser endpoint.
#[derive(Debug, Clone)]
pub struct BrowserEndpoint {
    /// WebSocket URL of the browser (browser-level) endpoint.
    pub browser_ws: Option<String>,
    /// HTTP discovery base (e.g. http://127.0.0.1:9222).
    pub http_base: Option<String>,
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// Resolve the endpoint from config/env (`ULNCLAW_BROWSER_CDP`).
///
/// Accepted forms:
///   - `ws://...` / `wss://...` — used directly as the browser WS endpoint
///   - `http://host:port` — discovery via `/json/version`
pub fn resolve_endpoint(raw: &str) -> Result<BrowserEndpoint> {
    let raw = raw.trim();
    if raw.starts_with("ws://") || raw.starts_with("wss://") {
        return Ok(BrowserEndpoint {
            browser_ws: Some(raw.to_string()),
            http_base: None,
        });
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Ok(BrowserEndpoint {
            browser_ws: None,
            http_base: Some(raw.trim_end_matches('/').to_string()),
        });
    }
    Err(AgentError::Config(format!(
        "ULNCLAW_BROWSER_CDP must be a ws://, wss://, or http(s):// endpoint, got: {}",
        raw
    )))
}

/// Fetch the browser-level WS url from an HTTP discovery endpoint.
pub async fn discover_browser_ws(http_base: &str) -> Result<String> {
    let url = format!("{}/json/version", http_base);
    let value: Value = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("CDP discovery {}: {}", url, e)))?
        .json()
        .await
        .map_err(|e| AgentError::Tool(format!("CDP discovery parse: {}", e)))?;
    value
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| AgentError::Tool("CDP /json/version missing webSocketDebuggerUrl".into()))
}

/// List page targets from an HTTP discovery endpoint.
pub async fn list_page_targets(http_base: &str) -> Result<Vec<Value>> {
    let url = format!("{}/json", http_base);
    let value: Value = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("CDP list {}: {}", url, e)))?
        .json()
        .await
        .map_err(|e| AgentError::Tool(format!("CDP list parse: {}", e)))?;
    Ok(value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
                .cloned()
                .collect()
        })
        .unwrap_or_default())
}

/// Create a new page target (`PUT /json/new`).
pub async fn create_page_target(http_base: &str, url: &str) -> Result<Value> {
    let endpoint = format!("{}/json/new?{}", http_base, url);
    let response = http_client()
        .put(&endpoint)
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("CDP new page: {}", e)))?;
    response
        .json()
        .await
        .map_err(|e| AgentError::Tool(format!("CDP new page parse: {}", e)))
}

// ---------------------------------------------------------------------------
// CDP client — WebSocket JSON-RPC with request/response demux + events
// ---------------------------------------------------------------------------

struct Pending {
    sender: tokio::sync::oneshot::Sender<Result<Value>>,
}

/// A live CDP connection.
pub struct CdpClient {
    out_tx: tokio::sync::mpsc::UnboundedSender<WsMessage>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    next_id: Arc<RwLock<u64>>,
    events: Arc<RwLock<Vec<(String, broadcast::Sender<Value>)>>>,
    /// Flipped when the read loop sees the socket close. A CDP connection
    /// has no keepalive of its own — without this flag a dead browser
    /// would wedge every later call on the 30s timeout (hermes session
    /// health checks recreate the session instead).
    closed: Arc<std::sync::atomic::AtomicBool>,
    _reader: tokio::task::JoinHandle<()>,
    _writer: tokio::task::JoinHandle<()>,
}

impl CdpClient {
    /// Connect to a CDP WebSocket endpoint.
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>> {
        let (socket, _) = connect_async(ws_url)
            .await
            .map_err(|e| AgentError::Tool(format!("CDP connect {}: {}", ws_url, e)))?;
        let (sink, stream) = socket.split();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<WsMessage>();

        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let events: Arc<RwLock<Vec<(String, broadcast::Sender<Value>)>>> =
            Arc::new(RwLock::new(Vec::new()));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let reader = tokio::spawn(Self::read_loop(
            stream,
            pending.clone(),
            events.clone(),
            closed.clone(),
        ));
        let writer = tokio::spawn(Self::write_loop(
            sink,
            out_rx,
            pending.clone(),
            closed.clone(),
        ));

        Ok(Arc::new(Self {
            out_tx,
            pending,
            next_id: Arc::new(RwLock::new(1)),
            events,
            closed,
            _reader: reader,
            _writer: writer,
        }))
    }

    async fn write_loop(
        mut sink: SplitSink<WsStream, WsMessage>,
        mut out_rx: tokio::sync::mpsc::UnboundedReceiver<WsMessage>,
        pending: Arc<Mutex<HashMap<u64, Pending>>>,
        closed: Arc<std::sync::atomic::AtomicBool>,
    ) {
        while let Some(message) = out_rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
        // The socket is gone — fail every in-flight call fast (covers the
        // race where a call was queued just as the connection died).
        closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut pending = pending.lock().await;
        for (_, entry) in pending.drain() {
            entry
                .sender
                .send(Err(AgentError::Tool("CDP connection closed".into())))
                .ok();
        }
    }

    async fn read_loop(
        mut stream: SplitStream<WsStream>,
        pending: Arc<Mutex<HashMap<u64, Pending>>>,
        events: Arc<RwLock<Vec<(String, broadcast::Sender<Value>)>>>,
        closed: Arc<std::sync::atomic::AtomicBool>,
    ) {
        while let Some(Ok(message)) = stream.next().await {
            let WsMessage::Text(text) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let (Some(id), Some(result)) = (
                value.get("id").and_then(|v| v.as_u64()),
                value.get("result").cloned(),
            ) {
                let mut pending = pending.lock().await;
                if let Some(entry) = pending.remove(&id) {
                    entry.sender.send(Ok(result)).ok();
                }
            } else if let (Some(id), Some(error)) = (
                value.get("id").and_then(|v| v.as_u64()),
                value.get("error").cloned(),
            ) {
                let mut pending = pending.lock().await;
                if let Some(entry) = pending.remove(&id) {
                    let message = error
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("CDP error");
                    entry
                        .sender
                        .send(Err(AgentError::Tool(format!("CDP error: {}", message))))
                        .ok();
                }
            } else if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
                let events = events.read().await;
                for (prefix, sender) in events.iter() {
                    if method.starts_with(prefix.as_str()) {
                        sender.send(value.clone()).ok();
                    }
                }
            }
        }
        // Socket closed (browser exited / network dropped). Fail pending
        // requests fast instead of letting them ride out the 30s timeout.
        closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut pending = pending.lock().await;
        for (_, entry) in pending.drain() {
            entry
                .sender
                .send(Err(AgentError::Tool("CDP connection closed".into())))
                .ok();
        }
    }

    /// False once the WebSocket has closed. Callers (the shared-session
    /// manager) use this to drop dead sessions and reconnect instead of
    /// wedging on per-call timeouts.
    pub fn is_connected(&self) -> bool {
        !self.closed.load(std::sync::atomic::Ordering::SeqCst) && !self.out_tx.is_closed()
    }

    /// Subscribe to events whose method starts with `prefix`.
    pub async fn subscribe(&self, prefix: &str) -> broadcast::Receiver<Value> {
        let (sender, receiver) = broadcast::channel(64);
        let mut events = self.events.write().await;
        events.push((prefix.to_string(), sender));
        receiver
    }

    /// Send a CDP command and await its result.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        if !self.is_connected() {
            return Err(AgentError::Tool("CDP connection closed".into()));
        }
        let id = {
            let mut next = self.next_id.write().await;
            let id = *next;
            *next += 1;
            id
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id, Pending { sender });
        let message = json!({"id": id, "method": method, "params": params});
        self.out_tx
            .send(WsMessage::Text(message.to_string()))
            .map_err(|e| AgentError::Tool(format!("CDP send: {}", e)))?;
        match tokio::time::timeout(std::time::Duration::from_secs(30), receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AgentError::Tool("CDP connection closed".into())),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(AgentError::Tool(format!("CDP '{}' timed out", method)))
            }
        }
    }

    /// Send a CDP event/notification (no response expected).
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({"method": method, "params": params});
        self.out_tx
            .send(WsMessage::Text(message.to_string()))
            .map_err(|e| AgentError::Tool(format!("CDP notify: {}", e)))
    }
}

// ---------------------------------------------------------------------------
// BrowserSession — one page target with automation helpers
// ---------------------------------------------------------------------------

/// Element ref from the last accessibility snapshot.
#[derive(Debug, Clone)]
pub struct ElementRef {
    pub index: usize,
    pub role: String,
    pub name: String,
    pub backend_node_id: u64,
}

/// A managed browser page.
pub struct BrowserSession {
    client: Arc<CdpClient>,
    pub target_id: String,
    pub page_url: String,
    refs: Arc<Mutex<Vec<ElementRef>>>,
    last_dialog: Arc<Mutex<Option<Value>>>,
}

impl BrowserSession {
    /// Connect to the endpoint, find (or create) a page target, and attach.
    pub async fn open(endpoint: &BrowserEndpoint) -> Result<Arc<Self>> {
        let http_base = match (&endpoint.http_base, &endpoint.browser_ws) {
            (Some(base), _) => Some(base.clone()),
            (None, Some(ws)) => {
                // Derive http base from ws://host:port/devtools/...
                let rest = ws.trim_start_matches("ws://").trim_start_matches("wss://");
                let host_port = rest.split('/').next().unwrap_or("");
                if host_port.is_empty() {
                    None
                } else {
                    Some(format!("http://{}", host_port))
                }
            }
            (None, None) => None,
        };

        let targets = if let Some(ref base) = http_base {
            list_page_targets(base).await.unwrap_or_default()
        } else {
            Vec::new()
        };

        let target = if let Some(first) = targets.into_iter().next() {
            first
        } else if let Some(ref base) = http_base {
            create_page_target(base, "about:blank").await?
        } else {
            return Err(AgentError::Tool(
                "no page target found and no HTTP discovery endpoint available".into(),
            ));
        };

        let ws_url = target
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Tool("page target missing webSocketDebuggerUrl".into()))?
            .to_string();
        let target_id = target
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let client = CdpClient::connect(&ws_url).await?;
        client.call("Page.enable", json!({})).await?;
        client.call("Runtime.enable", json!({})).await?;
        client.call("DOM.enable", json!({})).await?;
        client.call("Accessibility.enable", json!({})).await?;

        let session = Arc::new(Self {
            client,
            target_id,
            page_url: target
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank")
                .to_string(),
            refs: Arc::new(Mutex::new(Vec::new())),
            last_dialog: Arc::new(Mutex::new(None)),
        });

        // Track dialogs.
        let dialog_slot = session.last_dialog.clone();
        let mut events = session.client.subscribe("Page.javascriptDialog").await;
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if event.get("method").and_then(|v| v.as_str())
                    == Some("Page.javascriptDialogOpening")
                {
                    if let Some(params) = event.get("params").cloned() {
                        *dialog_slot.lock().await = Some(params);
                    }
                }
            }
        });

        Ok(session)
    }

    pub fn client(&self) -> &Arc<CdpClient> {
        &self.client
    }

    /// Navigate and wait for the load event (bounded).
    pub async fn navigate(&self, url: &str) -> Result<Value> {
        let mut load_events = self.client.subscribe("Page.loadEventFired").await;
        let result = self
            .client
            .call("Page.navigate", json!({"url": url}))
            .await?;
        if let Some(error) = result.get("errorText").and_then(|v| v.as_str()) {
            if !error.is_empty() {
                return Err(AgentError::Tool(format!("navigate: {}", error)));
            }
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), load_events.recv()).await;
        Ok(result)
    }

    pub async fn go_back(&self) -> Result<Value> {
        self.evaluate("window.history.back(); 'back'", None).await
    }

    /// Evaluate JS in the page.
    pub async fn evaluate(&self, expression: &str, timeout_ms: Option<u64>) -> Result<Value> {
        let mut params =
            json!({"expression": expression, "returnByValue": true, "awaitPromise": true});
        if let Some(timeout) = timeout_ms {
            params["timeout"] = json!(timeout);
        }
        let result = self.client.call("Runtime.evaluate", params).await?;
        if let Some(exception) = result.get("exceptionDetails") {
            let text = exception
                .pointer("/exception/description")
                .or_else(|| exception.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("JS exception");
            return Err(AgentError::Tool(format!("evaluate: {}", text)));
        }
        Ok(result.get("result").cloned().unwrap_or(json!(null)))
    }

    /// Accessibility snapshot with numbered element refs (Playwright-style).
    pub async fn snapshot(&self) -> Result<(String, Vec<ElementRef>)> {
        let tree = self
            .client
            .call("Accessibility.getFullAXTree", json!({}))
            .await?;
        let nodes = tree
            .get("nodes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        const INTERACTIVE: &[&str] = &[
            "link",
            "button",
            "textbox",
            "checkbox",
            "radio",
            "combobox",
            "menuitem",
            "tab",
            "switch",
            "searchbox",
            "slider",
            "spinbutton",
            "option",
            "treeitem",
            "cell",
            "menuitemcheckbox",
            "menuitemradio",
            "listbox",
        ];

        let mut lines = Vec::new();
        let mut refs = Vec::new();
        let mut next_ref = 1usize;
        for node in &nodes {
            let ignored = node
                .get("ignored")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if ignored {
                continue;
            }
            let role = node
                .pointer("/role/value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if role.is_empty() || role == "none" || role == "InlineTextBox" {
                continue;
            }
            let name = node
                .pointer("/name/value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let backend_node_id = node
                .get("backendDOMNodeId")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            if INTERACTIVE.contains(&role.as_str()) && backend_node_id > 0 {
                lines.push(format!("[{}] {} \"{}\"", next_ref, role, name));
                refs.push(ElementRef {
                    index: next_ref,
                    role: role.clone(),
                    name,
                    backend_node_id,
                });
                next_ref += 1;
            } else if matches!(
                role.as_str(),
                "heading" | "img" | "dialog" | "alert" | "table" | "article"
            ) {
                lines.push(format!("    {} \"{}\"", role, name));
            }
        }
        if lines.is_empty() {
            lines.push("(empty accessibility tree — the page may still be loading)".into());
        }

        let mut stored = self.refs.lock().await;
        *stored = refs.clone();
        Ok((lines.join("\n"), refs))
    }

    /// Resolve an element ref (or CSS selector) to a remote object id.
    async fn resolve_element(&self, element: &str) -> Result<String> {
        // Numeric ref from the last snapshot.
        if let Ok(index) = element.trim().parse::<usize>() {
            let refs = self.refs.lock().await;
            let found = refs.iter().find(|r| r.index == index).cloned();
            drop(refs);
            let Some(found) = found else {
                return Err(AgentError::Tool(format!(
                    "element ref [{}] not found — call browser_snapshot first",
                    index
                )));
            };
            let result = self
                .client
                .call(
                    "DOM.resolveNode",
                    json!({"backendNodeId": found.backend_node_id}),
                )
                .await?;
            return result
                .pointer("/object/objectId")
                .and_then(|v| v.as_str())
                .map(String::from)
                .ok_or_else(|| AgentError::Tool("element not attached to DOM".into()));
        }
        // CSS selector fallback.
        let doc = self.client.call("DOM.getDocument", json!({})).await?;
        let root_id = doc
            .pointer("/root/nodeId")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| AgentError::Tool("DOM.getDocument returned no root".into()))?;
        let found = self
            .client
            .call(
                "DOM.querySelector",
                json!({"nodeId": root_id, "selector": element}),
            )
            .await?;
        let node_id = found
            .get("nodeId")
            .and_then(|v| v.as_u64())
            .filter(|id| *id > 0)
            .ok_or_else(|| AgentError::Tool(format!("selector '{}' matched nothing", element)))?;
        let result = self
            .client
            .call("DOM.resolveNode", json!({"nodeId": node_id}))
            .await?;
        result
            .pointer("/object/objectId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| AgentError::Tool("element not attached to DOM".into()))
    }

    /// Click an element (ref `[N]` or CSS selector).
    pub async fn click(&self, element: &str) -> Result<Value> {
        let object_id = self.resolve_element(element).await?;
        self.client
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": "function() { this.scrollIntoView({block:'center'}); this.click(); }",
                    "returnByValue": true
                }),
            )
            .await?;
        Ok(json!({"clicked": element}))
    }

    /// Type text into an element.
    pub async fn type_text(&self, element: &str, text: &str) -> Result<Value> {
        let object_id = self.resolve_element(element).await?;
        self.client
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": format!(
                        r#"function() {{
                            this.scrollIntoView({{block:'center'}});
                            this.focus();
                            const text = {};
                            if (this.tagName === 'SELECT') {{
                                this.value = text;
                            }} else if (this.isContentEditable) {{
                                this.innerText = text;
                            }} else {{
                                this.value = text;
                            }}
                            this.dispatchEvent(new Event('input', {{bubbles: true}}));
                            this.dispatchEvent(new Event('change', {{bubbles: true}}));
                        }}"#,
                        serde_json::to_string(text).unwrap_or_default()
                    ),
                    "returnByValue": true
                }),
            )
            .await?;
        Ok(json!({"typed": text.len(), "into": element}))
    }

    pub async fn scroll(&self, direction: &str, pixels: u64) -> Result<Value> {
        let delta: i64 = if direction == "up" {
            -(pixels as i64)
        } else {
            pixels as i64
        };
        self.evaluate(&format!("window.scrollBy(0, {}); 'scrolled'", delta), None)
            .await
    }

    /// Press a keyboard key (Enter, Tab, Escape, Arrow*, or a single char).
    pub async fn press(&self, key: &str) -> Result<Value> {
        let lowered = key.to_lowercase();
        let (windows_key, code) = match lowered.as_str() {
            "enter" => ("Enter", "Enter"),
            "tab" => ("Tab", "Tab"),
            "escape" | "esc" => ("Escape", "Escape"),
            "backspace" => ("Backspace", "Backspace"),
            "delete" => ("Delete", "Delete"),
            "arrowup" | "up" => ("ArrowUp", "ArrowUp"),
            "arrowdown" | "down" => ("ArrowDown", "ArrowDown"),
            "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft"),
            "arrowright" | "right" => ("ArrowRight", "ArrowRight"),
            "home" => ("Home", "Home"),
            "end" => ("End", "End"),
            other => (other, ""),
        };
        for event_type in ["rawKeyDown", "keyUp"] {
            self.client
                .call(
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": event_type,
                        "key": windows_key,
                        "code": code,
                        "windowsVirtualKeyCode": windows_key.chars().next().map(|c| c as u32).unwrap_or(0),
                    }),
                )
                .await?;
            if event_type == "rawKeyDown" && windows_key.len() == 1 {
                self.client
                    .call(
                        "Input.dispatchKeyEvent",
                        json!({"type": "char", "text": windows_key}),
                    )
                    .await?;
            }
        }
        Ok(json!({"pressed": key}))
    }

    /// List images on the page.
    pub async fn get_images(&self) -> Result<Value> {
        self.evaluate(
            "Array.from(document.images).slice(0, 100).map((img, i) => ({index: i, src: img.src, alt: img.alt || '', width: img.naturalWidth, height: img.naturalHeight}))",
            None,
        )
        .await
    }

    /// PNG screenshot as base64.
    pub async fn screenshot(&self) -> Result<String> {
        let result = self
            .client
            .call("Page.captureScreenshot", json!({"format": "png"}))
            .await?;
        result
            .get("data")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| AgentError::Tool("screenshot returned no data".into()))
    }

    /// Current page title + URL.
    pub async fn page_info(&self) -> Result<Value> {
        let title = self.evaluate("document.title", None).await?;
        let url = self.evaluate("location.href", None).await?;
        Ok(json!({
            "title": title.get("value").and_then(|v| v.as_str()).unwrap_or(""),
            "url": url.get("value").and_then(|v| v.as_str()).unwrap_or(""),
        }))
    }

    /// Handle the most recent JavaScript dialog.
    pub async fn handle_dialog(&self, accept: bool, prompt_text: Option<&str>) -> Result<Value> {
        let pending = {
            let mut slot = self.last_dialog.lock().await;
            slot.take()
        };
        if pending.is_none() {
            return Ok(json!({"handled": false, "note": "no dialog is currently open"}));
        }
        let mut params = json!({"accept": accept});
        if let Some(text) = prompt_text {
            params["promptText"] = json!(text);
        }
        self.client
            .call("Page.handleJavaScriptDialog", params)
            .await?;
        Ok(json!({"handled": true, "accept": accept}))
    }
}

// ---------------------------------------------------------------------------
// Browser supervisor — managed launch of a local Chrome/Chromium
// (hermes runs an external agent-browser daemon; ulnclaw launches directly)
// ---------------------------------------------------------------------------

/// A browser process launched by the supervisor.
pub struct ManagedBrowser {
    child: tokio::process::Child,
    /// HTTP discovery base, e.g. `http://127.0.0.1:9333`.
    pub http_base: String,
    /// Port the DevTools server listens on.
    pub port: u16,
}

impl ManagedBrowser {
    /// True while the browser process is still running.
    pub async fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Terminate the browser process.
    pub async fn stop(&mut self) {
        self.child.kill().await.ok();
    }
}

fn managed_slot() -> &'static RwLock<Option<Arc<RwLock<ManagedBrowser>>>> {
    static SLOT: std::sync::OnceLock<RwLock<Option<Arc<RwLock<ManagedBrowser>>>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Candidate binary names, most specific first.
const BROWSER_CANDIDATES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
    "headless_shell",
    "brave-browser",
    "brave-browser-stable",
    "brave",
    "microsoft-edge",
    "microsoft-edge-stable",
    "msedge",
];

/// Extra well-known install locations beyond PATH.
const BROWSER_EXTRA_PATHS: &[&str] = &[
    "/usr/bin",
    "/usr/local/bin",
    "/opt/google/chrome",
    "/snap/bin",
    "/opt/brave.com/brave",
    "/opt/brave-bin",
    "/opt/microsoft/msedge",
    "/Applications/Google Chrome.app/Contents/MacOS",
    "/Applications/Chromium.app/Contents/MacOS",
    "/Applications/Brave Browser.app/Contents/MacOS",
    "/Applications/Microsoft Edge.app/Contents/MacOS",
];

/// Locate a Chrome/Chromium binary (env override `ULNCLAW_BROWSER_PATH`).
pub fn find_browser_binary() -> Option<std::path::PathBuf> {
    if let Some(custom) = crate::config::get_env_value("ULNCLAW_BROWSER_PATH") {
        let path = std::path::PathBuf::from(custom);
        if path.exists() {
            return Some(path);
        }
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path_var).collect();
    for extra in BROWSER_EXTRA_PATHS {
        let dir = std::path::PathBuf::from(extra);
        if dir.is_dir() {
            dirs.push(dir);
        }
    }
    for dir in &dirs {
        for name in BROWSER_CANDIDATES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// True when the sandbox bypass flag is needed (root without CAP_SYS_ADMIN).
fn needs_sandbox_bypass() -> bool {
    #[cfg(unix)]
    {
        libc_getuid() == 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
fn libc_getuid() -> u32 {
    // Avoid a libc dependency just for the uid: read /proc.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                let uid_line = line.strip_prefix("Uid:")?;
                uid_line.split_whitespace().next()?.parse::<u32>().ok()
            })
        })
        .unwrap_or(1)
}

/// Launch (or reuse) a managed headless browser and return its HTTP base.
pub async fn launch_managed_browser() -> Result<String> {
    // Reuse a live managed browser.
    {
        let slot = managed_slot().read().await;
        if let Some(browser) = slot.as_ref() {
            let mut guard = browser.write().await;
            if guard.alive().await {
                return Ok(guard.http_base.clone());
            }
        }
    }

    let binary = find_browser_binary().ok_or_else(|| {
        AgentError::Tool(
            "no Chrome/Chromium binary found (set ULNCLAW_BROWSER_PATH, install chromium, \
             or point ULNCLAW_BROWSER_CDP at an existing DevTools endpoint)"
                .into(),
        )
    })?;

    // Grab a free port.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| AgentError::Tool(format!("bind ephemeral port: {}", e)))?;
        listener
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| AgentError::Tool(e.to_string()))?
    };

    let user_data_dir = std::env::temp_dir().join(format!("ulnclaw-browser-{}", port));
    std::fs::create_dir_all(&user_data_dir).ok();

    let mut args = vec![
        format!("--remote-debugging-port={}", port),
        "--remote-debugging-address=127.0.0.1".to_string(),
        "--headless=new".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-sync".to_string(),
        "--disable-extensions".to_string(),
        format!("--user-data-dir={}", user_data_dir.display()),
    ];
    if needs_sandbox_bypass() {
        args.push("--no-sandbox".to_string());
    }
    if let Some(extra) = crate::config::get_env_value("ULNCLAW_BROWSER_ARGS") {
        for arg in extra.split_whitespace() {
            args.push(arg.to_string());
        }
    }

    let child = tokio::process::Command::new(&binary)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| AgentError::Tool(format!("launch {}: {}", binary.display(), e)))?;

    let http_base = format!("http://127.0.0.1:{}", port);
    // Wait for the DevTools endpoint to come up.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if std::time::Instant::now() > deadline {
            return Err(AgentError::Tool(format!(
                "managed browser ({}) did not open the DevTools port {} within 30s",
                binary.display(),
                port
            )));
        }
        if discover_browser_ws(&http_base).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let managed = Arc::new(RwLock::new(ManagedBrowser {
        child,
        http_base: http_base.clone(),
        port,
    }));
    {
        let mut slot = managed_slot().write().await;
        *slot = Some(managed.clone());
    }
    tracing::info!(
        "managed browser launched: {} on port {}",
        binary.display(),
        port
    );
    Ok(http_base)
}

/// Stop the managed browser, if one is running.
pub async fn stop_managed_browser() {
    let mut slot = managed_slot().write().await;
    if let Some(browser) = slot.take() {
        browser.write().await.stop().await;
        // Drop any page session attached to it.
        let session_slot = global_session_slot();
        *session_slot.write().await = None;
    }
}

/// True when the configured endpoint is (or defaults to) managed-launch mode.
pub fn is_auto_mode(raw: &str) -> bool {
    matches!(raw.trim(), "" | "auto" | "launch" | "managed")
}

// ---------------------------------------------------------------------------
// Global session manager — one shared page across tool calls
// ---------------------------------------------------------------------------

fn global_session_slot() -> &'static RwLock<Option<Arc<BrowserSession>>> {
    static SLOT: std::sync::OnceLock<RwLock<Option<Arc<BrowserSession>>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

// Live CDP endpoint override — hermes `/browser connect` sets
// BROWSER_CDP_URL for the process lifetime; the gateway exposes the same
// via POST /v1/browser/connect. Precedence: live override > env var.
fn override_slot() -> &'static std::sync::RwLock<Option<String>> {
    static SLOT: std::sync::OnceLock<std::sync::RwLock<Option<String>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::RwLock::new(None))
}

/// Set the live CDP endpoint override (validated). Hermes `/browser connect`.
pub fn set_cdp_override(raw: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AgentError::tool("browser connect: URL is required"));
    }
    if !is_auto_mode(raw) {
        resolve_endpoint(raw)?;
    }
    *override_slot().write().unwrap() = Some(raw.to_string());
    Ok(())
}

/// Clear the live CDP endpoint override. Hermes `/browser disconnect`.
pub fn clear_cdp_override() {
    *override_slot().write().unwrap() = None;
}

/// Current live override + its source, if any:
/// ("override" | "env" | "config", raw). Precedence mirrors hermes
/// `_get_cdp_override`: live `/browser connect` override >
/// `ULNCLAW_BROWSER_CDP` env > `[browser] cdp_url` config.
pub fn endpoint_with_source() -> Option<(&'static str, String)> {
    if let Some(raw) = override_slot().read().unwrap().clone() {
        return Some(("override", raw));
    }
    if let Some(raw) = crate::config::get_env_value("ULNCLAW_BROWSER_CDP") {
        return Some(("env", raw));
    }
    crate::config::UlncLawConfig::load(None)
        .ok()
        .and_then(|c| c.browser.cdp_url.clone())
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .map(|raw| ("config", raw))
}

/// Whether a managed (auto-launched) browser is currently running.
pub async fn managed_running() -> bool {
    managed_slot().read().await.is_some()
}

/// Read the configured CDP endpoint (live override > env > config), if
/// any. Absent or `auto`/`launch`/`managed` means "cloud provider, else
/// launch a local browser".
pub fn configured_endpoint_raw() -> Option<String> {
    endpoint_with_source().map(|(_, raw)| raw)
}

/// Get (or open) the shared browser session.
pub async fn with_session<F, Fut, T>(func: F) -> Result<T>
where
    F: FnOnce(Arc<BrowserSession>) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let raw = configured_endpoint_raw().unwrap_or_else(|| "auto".to_string());
    let endpoint = if is_auto_mode(&raw) {
        // Hermes cloud-mode precedence: when a cloud browser provider
        // resolves (explicit `[browser] cloud_provider` or the legacy
        // availability walk) its session CDP URL wins over a local launch.
        match cloud::cloud_endpoint().await {
            Ok(Some(cdp_url)) => resolve_endpoint(&cdp_url)?,
            Ok(None) => {
                // Supervisor: launch (or reuse) a managed local browser.
                let http_base = launch_managed_browser().await?;
                BrowserEndpoint {
                    browser_ws: None,
                    http_base: Some(http_base),
                }
            }
            Err(e) => return Err(AgentError::tool(e)),
        }
    } else {
        resolve_endpoint(&raw)?
    };

    // Reuse an existing live session. A dead CDP connection (browser
    // restarted, socket dropped) is dropped here so the next open builds a
    // fresh session — hermes session-health parity.
    {
        let slot = global_session_slot().read().await;
        if let Some(session) = slot.as_ref() {
            if session.client().is_connected() {
                return func(session.clone()).await;
            }
        }
    }
    {
        let mut slot = global_session_slot().write().await;
        if slot
            .as_ref()
            .map(|session| !session.client().is_connected())
            .unwrap_or(false)
        {
            *slot = None;
        }
    }

    let session = BrowserSession::open(&endpoint).await?;
    {
        let mut slot = global_session_slot().write().await;
        *slot = Some(session.clone());
    }
    func(session).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_endpoint_forms() {
        let ws = resolve_endpoint("ws://127.0.0.1:9222/devtools/browser/abc").unwrap();
        assert!(ws.browser_ws.is_some());
        assert!(ws.http_base.is_none());

        let http = resolve_endpoint("http://127.0.0.1:9222").unwrap();
        assert_eq!(http.http_base.as_deref(), Some("http://127.0.0.1:9222"));

        assert!(resolve_endpoint("ftp://nope").is_err());
    }

    /// Full CDP round-trip against a mock browser server.
    #[tokio::test]
    async fn test_cdp_client_against_mock_server() {
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                let WsMessage::Text(text) = message else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let Some(id) = value.get("id") else { continue };
                let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let result = match method {
                    "Runtime.evaluate" => {
                        json!({"result": {"type": "string", "value": "mock-title"}})
                    }
                    "Accessibility.getFullAXTree" => json!({"nodes": [
                        {"role": {"value": "WebArea"}, "name": {"value": "Page"}, "backendDOMNodeId": 1, "ignored": false},
                        {"role": {"value": "button"}, "name": {"value": "Submit"}, "backendDOMNodeId": 7, "ignored": false},
                        {"role": {"value": "link"}, "name": {"value": "Home"}, "backendDOMNodeId": 9, "ignored": false},
                        {"role": {"value": "none"}, "ignored": true}
                    ]}),
                    "DOM.resolveNode" => json!({"object": {"objectId": "{\"node\":7}"}}),
                    "Runtime.callFunctionOn" => json!({"result": {"type": "undefined"}}),
                    _ => json!({}),
                };
                let reply = json!({"id": id, "result": result});
                ws.send(WsMessage::Text(reply.to_string())).await.unwrap();
            }
        });

        let client = CdpClient::connect(&format!("ws://{}/", addr))
            .await
            .unwrap();

        let evaluated = client
            .call("Runtime.evaluate", json!({"expression": "document.title"}))
            .await
            .unwrap();
        assert_eq!(
            evaluated.pointer("/result/value").and_then(|v| v.as_str()),
            Some("mock-title")
        );

        // Snapshot parsing through a session-like flow (refs mapping).
        let tree = client
            .call("Accessibility.getFullAXTree", json!({}))
            .await
            .unwrap();
        let nodes = tree.get("nodes").and_then(|v| v.as_array()).unwrap();
        let interactive: Vec<&Value> = nodes
            .iter()
            .filter(|n| {
                !n.get("ignored").and_then(|v| v.as_bool()).unwrap_or(false)
                    && matches!(
                        n.pointer("/role/value").and_then(|v| v.as_str()),
                        Some("button") | Some("link")
                    )
            })
            .collect();
        assert_eq!(interactive.len(), 2);

        // Click path: resolve ref then callFunctionOn.
        let resolved = client
            .call("DOM.resolveNode", json!({"backendNodeId": 7}))
            .await
            .unwrap();
        assert!(resolved.pointer("/object/objectId").is_some());
        let clicked = client
            .call(
                "Runtime.callFunctionOn",
                json!({"objectId": resolved.pointer("/object/objectId").unwrap(), "functionDeclaration": "function() { this.click(); }"}),
            )
            .await
            .unwrap();
        assert!(clicked.is_object());
    }

    /// When the browser closes the socket, `is_connected` flips to false
    /// and in-flight calls fail fast instead of riding out the timeout.
    #[tokio::test]
    async fn test_cdp_client_detects_closed_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Accept one connection and drop it immediately.
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            drop(ws);
        });

        let client = CdpClient::connect(&format!("ws://{}/", addr))
            .await
            .unwrap();
        assert!(client.is_connected());

        // Wait for the read loop to observe the close.
        for _ in 0..100 {
            if !client.is_connected() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!client.is_connected());

        // Calls fail fast with a connection-closed error (no 30s wait).
        let error = client
            .call("Runtime.evaluate", json!({"expression": "1"}))
            .await
            .expect_err("dead connection must error");
        assert!(error.to_string().contains("closed"));
    }
}

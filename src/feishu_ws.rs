//! Feishu/Lark WebSocket long-connection transport — port of the
//! lark_oapi WS client protocol used by hermes `platforms.feishu` @
//! v2026.8.3 (`connection_mode = "websocket"`, the SDK default).
//!
//! Protocol (lark_oapi `ws/client.py` + `pbbp2.proto`):
//!
//! 1. **Endpoint handshake** — `POST {domain}/callback/ws/endpoint`
//!    with `{"AppID", "AppSecret"}` returns a one-time connection URL
//!    (query params `device_id` + `service_id`) and a `ClientConfig`
//!    (reconnect count/interval/nonce, ping interval).
//! 2. **Framing** — every WS binary message is a protobuf `Frame`
//!    (`SeqID`, `LogID`, `service`, `method`, repeated `Header`,
//!    `payload_encoding`, `payload_type`, `payload`, `LogIDNew`).
//!    `method` 0 = CONTROL (ping/pong), 1 = DATA (event/card).
//! 3. **Ping loop** — every `PingInterval` (default 120 s) a CONTROL
//!    frame with header `type: ping`; CONTROL `pong` frames may carry a
//!    JSON `ClientConfig` that re-tunes the session.
//! 4. **Data frames** — `sum > 1` marks split packets reassembled by
//!    `message_id`/`seq` (5 s TTL); complete `event` payloads are the
//!    same schema-2.0 envelope as the webhook transport and are
//!    dispatched **without** verification-token/signature checks (the
//!    SDK's `_do_without_validation`), then acknowledged by echoing the
//!    frame back with a `biz_rt` header and `{"code":200}` payload.
//! 5. **Reconnect** — random jitter up to `ReconnectNonce` (30 s) then
//!    retries every `ReconnectInterval` (120 s); `ReconnectCount = -1`
//!    means retry forever.

use crate::feishu::{FeishuConfig, ResolvedFeishu};
use crate::messaging::Dispatcher;
use crate::yuanbao_proto::{decode_varint, encode_varint};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as WsMessage;

const GEN_ENDPOINT_URI: &str = "/callback/ws/endpoint";
const CONNECT_TIMEOUT_SECS: u64 = 30;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Split-packet reassembly TTL (lark_oapi `ExpiringCache` timeout).
const COMBINE_TTL: Duration = Duration::from_secs(5);

/// `Frame.method` values (lark_oapi `FrameType`).
pub const FRAME_METHOD_CONTROL: i32 = 0;
pub const FRAME_METHOD_DATA: i32 = 1;

/// Frame header `type` values (lark_oapi `MessageType`).
pub const MSG_TYPE_EVENT: &str = "event";
pub const MSG_TYPE_CARD: &str = "card";
pub const MSG_TYPE_PING: &str = "ping";
pub const MSG_TYPE_PONG: &str = "pong";

/// Frame header keys (lark_oapi `ws/const.py`).
pub const HEADER_TYPE: &str = "type";
pub const HEADER_MESSAGE_ID: &str = "message_id";
pub const HEADER_SUM: &str = "sum";
pub const HEADER_SEQ: &str = "seq";
pub const HEADER_TRACE_ID: &str = "trace_id";
pub const HEADER_BIZ_RT: &str = "biz_rt";

/// Local reconnect/ping defaults — the Feishu endpoint authoritatively
/// replaces these on every handshake (lark_oapi `Client.__init__`).
pub const DEFAULT_RECONNECT_COUNT: i64 = -1;
pub const DEFAULT_RECONNECT_INTERVAL_SECS: u64 = 120;
pub const DEFAULT_RECONNECT_NONCE_SECS: u64 = 30;
pub const DEFAULT_PING_INTERVAL_SECS: u64 = 120;

const FEISHU_DOMAIN_URL: &str = "https://open.feishu.cn";
const LARK_DOMAIN_URL: &str = "https://open.larksuite.com";

/// Map the `domain` config (`"feishu"` | `"lark"` | full URL) to the
/// Open API base used for the endpoint handshake.
pub fn ws_domain_url(domain: &str) -> &'static str {
    match domain.trim().to_lowercase().as_str() {
        "lark" | "larksuite" => LARK_DOMAIN_URL,
        _ => FEISHU_DOMAIN_URL,
    }
}

// ---------------------------------------------------------------------------
// Frame protobuf codec (pbbp2.proto, hand-rolled wire format)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LarkHeader {
    pub key: String,
    pub value: String,
}

/// The lark_oapi `Frame` message (`pbbp2.proto`, proto2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LarkFrame {
    pub seq_id: u64,
    pub log_id: u64,
    pub service: i32,
    pub method: i32,
    pub headers: Vec<LarkHeader>,
    pub payload_encoding: String,
    pub payload_type: String,
    pub payload: Vec<u8>,
    pub log_id_new: String,
}

impl LarkFrame {
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }
}

fn push_varint_field(out: &mut Vec<u8>, field: u64, value: u64) {
    out.extend(encode_varint((field << 3) | 0));
    out.extend(encode_varint(value));
}

fn push_len_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    out.extend(encode_varint((field << 3) | 2));
    out.extend(encode_varint(bytes.len() as u64));
    out.extend_from_slice(bytes);
}

fn encode_header(header: &LarkHeader) -> Vec<u8> {
    let mut out = Vec::new();
    if !header.key.is_empty() {
        push_len_field(&mut out, 1, header.key.as_bytes());
    }
    if !header.value.is_empty() {
        push_len_field(&mut out, 2, header.value.as_bytes());
    }
    out
}

/// Serialize a `Frame` (proto2 default-valued scalars omitted, matching
/// protobuf semantics; the ping frame's explicit zero SeqID/LogID are
/// therefore elided, exactly like `SerializeToString`).
pub fn encode_frame(frame: &LarkFrame) -> Vec<u8> {
    let mut out = Vec::new();
    if frame.seq_id != 0 {
        push_varint_field(&mut out, 1, frame.seq_id);
    }
    if frame.log_id != 0 {
        push_varint_field(&mut out, 2, frame.log_id);
    }
    if frame.service != 0 {
        push_varint_field(&mut out, 3, frame.service as u64);
    }
    if frame.method != 0 {
        push_varint_field(&mut out, 4, frame.method as u64);
    }
    for header in &frame.headers {
        let encoded = encode_header(header);
        push_len_field(&mut out, 5, &encoded);
    }
    if !frame.payload_encoding.is_empty() {
        push_len_field(&mut out, 6, frame.payload_encoding.as_bytes());
    }
    if !frame.payload_type.is_empty() {
        push_len_field(&mut out, 7, frame.payload_type.as_bytes());
    }
    if !frame.payload.is_empty() {
        push_len_field(&mut out, 8, &frame.payload);
    }
    if !frame.log_id_new.is_empty() {
        push_len_field(&mut out, 9, frame.log_id_new.as_bytes());
    }
    out
}

fn decode_header(data: &[u8]) -> Result<LarkHeader, String> {
    let mut header = LarkHeader::default();
    let mut pos = 0;
    while pos < data.len() {
        let (key, next) = decode_varint(data, pos).ok_or("header: bad tag")?;
        pos = next;
        let field = key >> 3;
        match (field, key & 7) {
            (1, 2) | (2, 2) => {
                let (len, next) = decode_varint(data, pos).ok_or("header: bad len")?;
                pos = next;
                let len = len as usize;
                if pos + len > data.len() {
                    return Err("header: truncated string".into());
                }
                let value = String::from_utf8_lossy(&data[pos..pos + len]).to_string();
                pos += len;
                if field == 1 {
                    header.key = value;
                } else {
                    header.value = value;
                }
            }
            (_, 0) => {
                let (_, next) = decode_varint(data, pos).ok_or("header: bad varint")?;
                pos = next;
            }
            (_, 2) => {
                let (len, next) = decode_varint(data, pos).ok_or("header: bad len")?;
                pos = next + len as usize;
            }
            (_, 1) => pos += 8,
            (_, 5) => pos += 4,
            _ => return Err(format!("header: bad wire type {}", key & 7)),
        }
        if pos > data.len() {
            return Err("header: truncated".into());
        }
    }
    Ok(header)
}

/// Parse one protobuf `Frame`; unknown fields are skipped.
pub fn decode_frame(data: &[u8]) -> Result<LarkFrame, String> {
    let mut frame = LarkFrame::default();
    let mut pos = 0;
    while pos < data.len() {
        let (key, next) = decode_varint(data, pos).ok_or("frame: bad tag")?;
        pos = next;
        let field = key >> 3;
        let wire = key & 7;
        match (field, wire) {
            (1, 0) => {
                let (v, next) = decode_varint(data, pos).ok_or("frame: bad SeqID")?;
                frame.seq_id = v;
                pos = next;
            }
            (2, 0) => {
                let (v, next) = decode_varint(data, pos).ok_or("frame: bad LogID")?;
                frame.log_id = v;
                pos = next;
            }
            (3, 0) => {
                let (v, next) = decode_varint(data, pos).ok_or("frame: bad service")?;
                frame.service = v as i32;
                pos = next;
            }
            (4, 0) => {
                let (v, next) = decode_varint(data, pos).ok_or("frame: bad method")?;
                frame.method = v as i32;
                pos = next;
            }
            (5, 2) => {
                let (len, next) = decode_varint(data, pos).ok_or("frame: bad header len")?;
                pos = next;
                let len = len as usize;
                if pos + len > data.len() {
                    return Err("frame: truncated header".into());
                }
                frame.headers.push(decode_header(&data[pos..pos + len])?);
                pos += len;
            }
            (6, 2) | (7, 2) | (9, 2) => {
                let (len, next) = decode_varint(data, pos).ok_or("frame: bad len")?;
                pos = next;
                let len = len as usize;
                if pos + len > data.len() {
                    return Err("frame: truncated string".into());
                }
                let value = String::from_utf8_lossy(&data[pos..pos + len]).to_string();
                pos += len;
                match field {
                    6 => frame.payload_encoding = value,
                    7 => frame.payload_type = value,
                    _ => frame.log_id_new = value,
                }
            }
            (8, 2) => {
                let (len, next) = decode_varint(data, pos).ok_or("frame: bad len")?;
                pos = next;
                let len = len as usize;
                if pos + len > data.len() {
                    return Err("frame: truncated payload".into());
                }
                frame.payload = data[pos..pos + len].to_vec();
                pos += len;
            }
            (_, 0) => {
                let (_, next) = decode_varint(data, pos).ok_or("frame: bad varint")?;
                pos = next;
            }
            (_, 2) => {
                let (len, next) = decode_varint(data, pos).ok_or("frame: bad len")?;
                pos = next + len as usize;
            }
            (_, 1) => pos += 8,
            (_, 5) => pos += 4,
            _ => return Err(format!("frame: bad wire type {wire}")),
        }
        if pos > data.len() {
            return Err("frame: truncated".into());
        }
    }
    Ok(frame)
}

/// CONTROL ping frame sent on the heartbeat interval (lark_oapi
/// `_new_ping_frame`).
pub fn new_ping_frame(service_id: i32) -> LarkFrame {
    LarkFrame {
        service: service_id,
        method: FRAME_METHOD_CONTROL,
        headers: vec![LarkHeader {
            key: HEADER_TYPE.into(),
            value: MSG_TYPE_PING.into(),
        }],
        ..Default::default()
    }
}

/// Ack for a processed DATA frame: the original frame echoed back with
/// a `biz_rt` header and a `{"code":200}` response payload.
pub fn new_ack_frame(frame: &LarkFrame, biz_rt_ms: u64) -> LarkFrame {
    let mut ack = frame.clone();
    ack.headers.push(LarkHeader {
        key: HEADER_BIZ_RT.into(),
        value: biz_rt_ms.to_string(),
    });
    ack.payload = br#"{"code":200}"#.to_vec();
    ack
}

// ---------------------------------------------------------------------------
// Endpoint handshake
// ---------------------------------------------------------------------------

/// Session tuning pushed by the endpoint handshake and by CONTROL pong
/// frames (lark_oapi `ClientConfig`).
#[derive(Debug, Clone, PartialEq)]
pub struct WsClientConfig {
    pub reconnect_count: i64,
    pub reconnect_interval_secs: u64,
    pub reconnect_nonce_secs: u64,
    pub ping_interval_secs: u64,
}

impl Default for WsClientConfig {
    fn default() -> Self {
        Self {
            reconnect_count: DEFAULT_RECONNECT_COUNT,
            reconnect_interval_secs: DEFAULT_RECONNECT_INTERVAL_SECS,
            reconnect_nonce_secs: DEFAULT_RECONNECT_NONCE_SECS,
            ping_interval_secs: DEFAULT_PING_INTERVAL_SECS,
        }
    }
}

fn parse_client_config(value: &Value) -> WsClientConfig {
    let conf = value.get("ClientConfig").unwrap_or(value);
    WsClientConfig {
        reconnect_count: conf
            .get("ReconnectCount")
            .and_then(|v| v.as_i64())
            .unwrap_or(DEFAULT_RECONNECT_COUNT),
        reconnect_interval_secs: conf
            .get("ReconnectInterval")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_RECONNECT_INTERVAL_SECS),
        reconnect_nonce_secs: conf
            .get("ReconnectNonce")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_RECONNECT_NONCE_SECS),
        ping_interval_secs: conf
            .get("PingInterval")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_PING_INTERVAL_SECS),
    }
}

/// Endpoint handshake failure; `fatal` marks credential/permission
/// errors (lark_oapi `ClientException`) that must not be retried.
#[derive(Debug)]
pub struct EndpointError {
    pub fatal: bool,
    pub msg: String,
}

/// Parse the `/callback/ws/endpoint` response body (lark_oapi
/// `_get_conn_url` semantics: code 0 ok, 1 system busy, 1000040343
/// internal error — both retryable; anything else is a fatal client
/// error).
pub fn parse_endpoint_response(body: &str) -> Result<(String, WsClientConfig), EndpointError> {
    let value: Value = serde_json::from_str(body).map_err(|e| EndpointError {
        fatal: false,
        msg: format!("endpoint JSON: {e}"),
    })?;
    let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    let msg = value
        .get("msg")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if code == 1 {
        return Err(EndpointError {
            fatal: false,
            msg: "system busy".into(),
        });
    }
    if code != 0 && code != 1000040343 {
        return Err(EndpointError {
            fatal: true,
            msg: format!("endpoint error {code}: {msg}"),
        });
    }
    if code != 0 {
        return Err(EndpointError {
            fatal: false,
            msg: format!("endpoint error {code}: {msg}"),
        });
    }
    let data = value.get("data").ok_or_else(|| EndpointError {
        fatal: false,
        msg: "endpoint response missing data".into(),
    })?;
    let url = data
        .get("URL")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| EndpointError {
            fatal: false,
            msg: "endpoint response missing URL".into(),
        })?
        .to_string();
    let conf = parse_client_config(data);
    Ok((url, conf))
}

/// Perform the endpoint handshake (lark_oapi `_get_conn_url`).
pub async fn fetch_ws_endpoint(
    client: &reqwest::Client,
    domain_url: &str,
    app_id: &str,
    app_secret: &str,
) -> Result<(String, WsClientConfig), EndpointError> {
    let resp = client
        .post(format!("{domain_url}{GEN_ENDPOINT_URI}"))
        .header("locale", "zh")
        .json(&json!({"AppID": app_id, "AppSecret": app_secret}))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| EndpointError {
            fatal: false,
            msg: format!("endpoint request: {e}"),
        })?;
    if !resp.status().is_success() {
        return Err(EndpointError {
            fatal: false,
            msg: format!("endpoint HTTP {} (system busy)", resp.status()),
        });
    }
    let body = resp.text().await.map_err(|e| EndpointError {
        fatal: false,
        msg: format!("endpoint body: {e}"),
    })?;
    parse_endpoint_response(&body)
}

/// Extract `device_id` (connection id) and `service_id` from the
/// handshake URL query (lark_oapi `_connect`).
pub fn parse_conn_url(url: &str) -> Result<(String, i32), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("conn url: {e}"))?;
    let mut device_id = None;
    let mut service_id = None;
    for (key, value) in parsed.query_pairs() {
        if key == "device_id" {
            device_id = Some(value.to_string());
        } else if key == "service_id" {
            service_id = value.parse::<i32>().ok();
        }
    }
    match (device_id, service_id) {
        (Some(device), Some(service)) => Ok((device, service)),
        _ => Err("conn url missing device_id/service_id".into()),
    }
}

// ---------------------------------------------------------------------------
// Split-packet reassembly (lark_oapi `_combine`)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CombineBuffer {
    parts: HashMap<String, (Instant, Vec<Option<Vec<u8>>>)>,
}

impl CombineBuffer {
    /// Store one chunk; returns the full payload once all `sum` parts
    /// arrived for `msg_id` (parts expire after [`COMBINE_TTL`]).
    fn add(&mut self, msg_id: &str, sum: usize, seq: usize, chunk: Vec<u8>) -> Option<Vec<u8>> {
        self.parts.retain(|_, (ts, _)| ts.elapsed() < COMBINE_TTL);
        let entry = self
            .parts
            .entry(msg_id.to_string())
            .or_insert_with(|| (Instant::now(), vec![None::<Vec<u8>>; sum]));
        let (ts, slots) = entry;
        *ts = Instant::now();
        if slots.len() != sum {
            // Conflicting sum for the same message id — start over.
            *slots = vec![None::<Vec<u8>>; sum];
        }
        if seq < slots.len() {
            slots[seq] = Some(chunk);
        }
        if slots.iter().all(|slot| slot.is_some()) {
            let (_, slots) = self.parts.remove(msg_id)?;
            Some(
                slots
                    .into_iter()
                    .map(|slot| slot.unwrap_or_default())
                    .collect::<Vec<Vec<u8>>>()
                    .concat(),
            )
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Connection loop
// ---------------------------------------------------------------------------

enum RunOutcome {
    /// Transport error — reconnect per ClientConfig.
    Disconnected(String),
    /// Credential/permission failure — stop the transport.
    Fatal(String),
}

fn random_fraction() -> f64 {
    let mut bytes = [0u8; 4];
    crate::feishu::fill_random_bytes(&mut bytes);
    u32::from_le_bytes(bytes) as f64 / u32::MAX as f64
}

/// Serve one WS connection until it drops (lark_oapi
/// `_connect`/`_receive_message_loop`/`_ping_loop` fused into a single
/// select loop).
async fn run_once(
    client: &reqwest::Client,
    domain_url: &str,
    cfg: &FeishuConfig,
    resolved: &ResolvedFeishu,
    dispatcher: &std::sync::Arc<Dispatcher>,
    pairing: &Option<std::sync::Arc<crate::pairing::PairingStore>>,
    conf: &mut WsClientConfig,
) -> RunOutcome {
    let (conn_url, new_conf) =
        match fetch_ws_endpoint(client, domain_url, &resolved.app_id, &resolved.app_secret).await {
            Ok(v) => v,
            Err(e) => {
                return if e.fatal {
                    RunOutcome::Fatal(e.msg)
                } else {
                    RunOutcome::Disconnected(e.msg)
                }
            }
        };
    *conf = new_conf;
    let (conn_id, service_id) = match parse_conn_url(&conn_url) {
        Ok(v) => v,
        Err(e) => return RunOutcome::Disconnected(e),
    };
    let ws = match tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS),
        tokio_tungstenite::connect_async(&conn_url),
    )
    .await
    {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => return RunOutcome::Disconnected(format!("ws connect: {e}")),
        Err(_) => return RunOutcome::Disconnected("ws connect timeout".into()),
    };
    eprintln!("[feishu] websocket connected (conn_id={conn_id})");
    let (mut sink, mut stream) = ws.split();
    let mut combine = CombineBuffer::default();
    let ping_interval = Duration::from_secs(conf.ping_interval_secs.max(1));
    let mut ping_deadline = tokio::time::Instant::now() + ping_interval;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(ping_deadline) => {
                let frame = new_ping_frame(service_id);
                if sink.send(WsMessage::Binary(encode_frame(&frame))).await.is_err() {
                    return RunOutcome::Disconnected("ping send failed".into());
                }
                ping_deadline = tokio::time::Instant::now() + ping_interval;
            }
            msg = stream.next() => {
                let Some(raw) = msg else {
                    return RunOutcome::Disconnected("connection closed".into());
                };
                let raw = match raw {
                    Ok(WsMessage::Binary(data)) => data,
                    Ok(WsMessage::Close(_)) => {
                        return RunOutcome::Disconnected("closed by server".into());
                    }
                    Ok(_) => continue,
                    Err(e) => return RunOutcome::Disconnected(format!("ws error: {e}")),
                };
                let frame = match decode_frame(&raw) {
                    Ok(frame) => frame,
                    Err(e) => {
                        eprintln!("[feishu] bad frame dropped: {e}");
                        continue;
                    }
                };
                match frame.method {
                    FRAME_METHOD_CONTROL => {
                        if frame.header(HEADER_TYPE) == Some(MSG_TYPE_PONG)
                            && !frame.payload.is_empty()
                        {
                            if let Ok(value) = serde_json::from_slice::<Value>(&frame.payload) {
                                let new_conf = parse_client_config(&value);
                                eprintln!("[feishu] pong re-configured session (ping_interval={}s)", new_conf.ping_interval_secs);
                                *conf = new_conf;
                                ping_deadline = tokio::time::Instant::now()
                                    + Duration::from_secs(conf.ping_interval_secs.max(1));
                            }
                        }
                    }
                    FRAME_METHOD_DATA => {
                        let Some(msg_id) = frame.header(HEADER_MESSAGE_ID).map(|s| s.to_string())
                        else {
                            eprintln!("[feishu] data frame without message_id dropped");
                            continue;
                        };
                        let sum: usize = frame
                            .header(HEADER_SUM)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(1);
                        let seq: usize = frame
                            .header(HEADER_SEQ)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        let msg_type = frame
                            .header(HEADER_TYPE)
                            .unwrap_or("")
                            .to_string();
                        let payload = if sum > 1 {
                            match combine.add(&msg_id, sum, seq, frame.payload.clone()) {
                                Some(full) => full,
                                None => continue,
                            }
                        } else {
                            frame.payload.clone()
                        };
                        if msg_type != MSG_TYPE_EVENT {
                            // hermes parity: the lark SDK WebSocket client
                            // drops CARD frames (card actions only work in
                            // webhook mode), so non-event frames are
                            // skipped without an ack.
                            continue;
                        }
                        let trace_id = frame
                            .header(HEADER_TRACE_ID)
                            .unwrap_or("")
                            .to_string();
                        let start = Instant::now();
                        match serde_json::from_slice::<Value>(&payload) {
                            Ok(envelope) => {
                                dispatch_event_envelope(cfg, dispatcher, pairing, &envelope);
                            }
                            Err(e) => {
                                eprintln!(
                                    "[feishu] event payload not JSON (message_id={msg_id}, trace_id={trace_id}): {e}"
                                );
                            }
                        }
                        let biz_rt_ms = start.elapsed().as_millis() as u64;
                        let ack = new_ack_frame(&frame, biz_rt_ms);
                        if sink.send(WsMessage::Binary(encode_frame(&ack))).await.is_err() {
                            return RunOutcome::Disconnected("ack send failed".into());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Route one WS event envelope through the same path as the webhook
/// transport (no verification-token/signature checks — lark_oapi
/// `_do_without_validation`).
fn dispatch_event_envelope(
    cfg: &FeishuConfig,
    dispatcher: &std::sync::Arc<Dispatcher>,
    pairing: &Option<std::sync::Arc<crate::pairing::PairingStore>>,
    envelope: &Value,
) {
    let event_type = envelope
        .pointer("/header/event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if event_type == "drive.notice.comment_add_v1" || event_type == "vc.bot.meeting_invited_v1" {
        let cfg = cfg.clone();
        let dispatcher = dispatcher.clone();
        let envelope = envelope.clone();
        let event_type = event_type.to_string();
        tokio::spawn(async move {
            crate::feishu_comment::dispatch_aux_event(&cfg, &dispatcher, &event_type, &envelope)
                .await;
        });
        return;
    }
    if event_type == "im.message.reaction.created_v1"
        || event_type == "im.message.reaction.deleted_v1"
    {
        let event_id = envelope
            .pointer("/header/event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if event_id.is_empty() || crate::feishu::remember_event_id(&event_id) {
            let cfg = cfg.clone();
            let dispatcher = dispatcher.clone();
            let envelope = envelope.clone();
            let event_type = event_type.to_string();
            tokio::spawn(async move {
                crate::feishu::handle_reaction_event(&cfg, &dispatcher, &envelope, &event_type)
                    .await;
            });
        }
        return;
    }
    if event_type == "im.message.message_read_v1" {
        // hermes `_on_message_read_event`: read receipts are ignored.
        return;
    }
    if event_type != "im.message.receive_v1" {
        return;
    }
    let event_id = envelope
        .pointer("/header/event_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !event_id.is_empty() && !crate::feishu::remember_event_id(&event_id) {
        return;
    }
    let cfg = cfg.clone();
    let dispatcher = dispatcher.clone();
    let pairing = pairing.clone();
    let envelope = envelope.clone();
    tokio::spawn(async move {
        crate::feishu::handle_message_event(&cfg, &dispatcher, pairing.as_deref(), &envelope).await;
    });
}

/// Feishu WebSocket transport entry point — spawned from
/// `run_messaging` when `[messaging.feishu] connection_mode` is
/// `"websocket"` (the hermes default).
pub async fn run(
    cfg: FeishuConfig,
    dispatcher: std::sync::Arc<Dispatcher>,
    pairing: Option<std::sync::Arc<crate::pairing::PairingStore>>,
) {
    crate::feishu::register_sender(&cfg);
    let resolved = cfg.resolve();
    if resolved.app_id.is_empty() || resolved.app_secret.is_empty() {
        eprintln!(
            "[feishu] websocket mode requires app_id + app_secret (FEISHU_APP_ID/FEISHU_APP_SECRET); transport not started"
        );
        return;
    }
    let domain_url = ws_domain_url(&resolved.domain);
    let client = reqwest::Client::new();
    let mut conf = WsClientConfig::default();
    let mut attempts: i64 = 0;
    loop {
        match run_once(
            &client,
            domain_url,
            &cfg,
            &resolved,
            &dispatcher,
            &pairing,
            &mut conf,
        )
        .await
        {
            RunOutcome::Fatal(msg) => {
                eprintln!("[feishu] websocket transport stopped: {msg}");
                return;
            }
            RunOutcome::Disconnected(msg) => {
                eprintln!("[feishu] websocket disconnected: {msg}");
            }
        }
        if conf.reconnect_count >= 0 && attempts >= conf.reconnect_count {
            eprintln!(
                "[feishu] giving up after {} reconnect attempt(s)",
                conf.reconnect_count
            );
            return;
        }
        attempts += 1;
        if conf.reconnect_nonce_secs > 0 {
            let jitter =
                Duration::from_secs_f64(random_fraction() * conf.reconnect_nonce_secs as f64);
            tokio::time::sleep(jitter).await;
        }
        tokio::time::sleep(Duration::from_secs(conf.reconnect_interval_secs.max(1))).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> LarkFrame {
        LarkFrame {
            seq_id: 42,
            log_id: 7,
            service: 3,
            method: FRAME_METHOD_DATA,
            headers: vec![
                LarkHeader {
                    key: HEADER_TYPE.into(),
                    value: MSG_TYPE_EVENT.into(),
                },
                LarkHeader {
                    key: HEADER_MESSAGE_ID.into(),
                    value: "m-1".into(),
                },
            ],
            payload_encoding: "json".into(),
            payload_type: "event".into(),
            payload: br#"{"schema":"2.0"}"#.to_vec(),
            log_id_new: "log-new".into(),
        }
    }

    #[test]
    fn frame_codec_round_trip() {
        let frame = sample_frame();
        let decoded = decode_frame(&encode_frame(&frame)).expect("decodes");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn empty_frame_round_trip() {
        let frame = LarkFrame::default();
        let encoded = encode_frame(&frame);
        assert!(encoded.is_empty());
        assert_eq!(decode_frame(&encoded).expect("decodes"), frame);
    }

    #[test]
    fn decode_skips_unknown_fields() {
        // Field 15 varint + field 16 length-delimited prepended to a
        // ping frame encoding.
        let mut bytes = Vec::new();
        push_varint_field(&mut bytes, 15, 99);
        push_len_field(&mut bytes, 16, b"junk");
        bytes.extend(encode_frame(&new_ping_frame(5)));
        let decoded = decode_frame(&bytes).expect("decodes");
        assert_eq!(decoded, new_ping_frame(5));
    }

    #[test]
    fn ping_frame_shape() {
        let frame = new_ping_frame(12);
        assert_eq!(frame.method, FRAME_METHOD_CONTROL);
        assert_eq!(frame.service, 12);
        assert_eq!(frame.seq_id, 0);
        assert_eq!(frame.log_id, 0);
        assert_eq!(frame.header(HEADER_TYPE), Some(MSG_TYPE_PING));
    }

    #[test]
    fn ack_frame_echoes_headers_and_sets_response() {
        let frame = sample_frame();
        let ack = new_ack_frame(&frame, 17);
        assert_eq!(ack.seq_id, frame.seq_id);
        assert_eq!(ack.payload, br#"{"code":200}"#);
        assert_eq!(ack.header(HEADER_BIZ_RT), Some("17"));
        // Original headers preserved.
        assert_eq!(ack.header(HEADER_MESSAGE_ID), Some("m-1"));
    }

    #[test]
    fn conn_url_query_parsing() {
        let url = "wss://open.feishu.cn/callback/ws?device_id=dev-1&service_id=3&extra=x";
        let (device, service) = parse_conn_url(url).expect("parses");
        assert_eq!(device, "dev-1");
        assert_eq!(service, 3);
        assert!(parse_conn_url("wss://x/callback/ws?device_id=d").is_err());
        assert!(parse_conn_url("not a url").is_err());
    }

    #[test]
    fn endpoint_response_ok() {
        let body = r#"{"code":0,"msg":"ok","data":{"URL":"wss://h/ws?device_id=d1&service_id=2","ClientConfig":{"ReconnectCount":-1,"ReconnectInterval":120,"ReconnectNonce":30,"PingInterval":60}}}"#;
        let (url, conf) = parse_endpoint_response(body).expect("parses");
        assert!(url.contains("device_id=d1"));
        assert_eq!(conf.ping_interval_secs, 60);
        assert_eq!(conf.reconnect_count, -1);
        assert_eq!(conf.reconnect_nonce_secs, 30);
    }

    #[test]
    fn endpoint_response_system_busy_retries() {
        let err = parse_endpoint_response(r#"{"code":1,"msg":"busy"}"#).unwrap_err();
        assert!(!err.fatal);
        assert_eq!(err.msg, "system busy");
    }

    #[test]
    fn endpoint_response_fatal_error() {
        let err = parse_endpoint_response(r#"{"code":1000040350,"msg":"exceed conn limit"}"#)
            .unwrap_err();
        assert!(err.fatal);
        // Internal error is retryable (lark_oapi ServerException).
        let err = parse_endpoint_response(r#"{"code":1000040343,"msg":"internal"}"#).unwrap_err();
        assert!(!err.fatal);
    }

    #[test]
    fn pong_payload_reconfigures() {
        let payload =
            r#"{"ReconnectCount":5,"ReconnectInterval":30,"ReconnectNonce":10,"PingInterval":45}"#;
        let value: Value = serde_json::from_str(payload).unwrap();
        let conf = parse_client_config(&value);
        assert_eq!(conf.reconnect_count, 5);
        assert_eq!(conf.ping_interval_secs, 45);
    }

    #[test]
    fn combine_reassembles_out_of_order() {
        let mut combine = CombineBuffer::default();
        assert!(combine.add("m1", 3, 1, b"World".to_vec()).is_none());
        assert!(combine.add("m1", 3, 0, b"Hello, ".to_vec()).is_none());
        let full = combine.add("m1", 3, 2, b"!".to_vec()).expect("complete");
        assert_eq!(full, b"Hello, World!");
        // Buffer consumed.
        assert!(combine.parts.is_empty());
    }

    #[test]
    fn combine_keeps_message_ids_separate() {
        let mut combine = CombineBuffer::default();
        assert!(combine.add("a", 2, 0, b"a0".to_vec()).is_none());
        assert!(combine.add("b", 2, 0, b"b0".to_vec()).is_none());
        let full = combine.add("b", 2, 1, b"b1".to_vec()).expect("complete");
        assert_eq!(full, b"b0b1");
        assert!(combine.parts.contains_key("a"));
    }

    #[test]
    fn domain_mapping() {
        assert_eq!(ws_domain_url("feishu"), FEISHU_DOMAIN_URL);
        assert_eq!(ws_domain_url("lark"), LARK_DOMAIN_URL);
        assert_eq!(ws_domain_url("LARK"), LARK_DOMAIN_URL);
        assert_eq!(ws_domain_url(""), FEISHU_DOMAIN_URL);
    }
}

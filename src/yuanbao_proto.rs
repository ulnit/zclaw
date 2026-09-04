//! Yuanbao WebSocket protocol codec — port of hermes
//! `gateway/platforms/yuanbao_proto.py` @ v2026.8.3.
//!
//! Protocol layering:
//!
//! ```text
//! WebSocket frame
//!   └── ConnMsg (protobuf: trpc.yuanbao.conn_common.ConnMsg)
//!         ├── head: Head (cmd_type, cmd, seq_no, msg_id, module, ...)
//!         └── data: bytes (business payload, standard protobuf)
//!               └── InboundMessagePush / SendC2CMessageReq / ...
//! ```
//!
//! Each WebSocket frame carries exactly one ConnMsg (no framing glue).
//! The codec is hand-rolled protobuf wire format (varint + length
//! delimited), mirroring the hermes implementation — no protobuf crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

// conn-layer cmd_type values (ConnMsg.Head.cmd_type).
pub const CMD_TYPE_REQUEST: u64 = 0;
pub const CMD_TYPE_RESPONSE: u64 = 1;
pub const CMD_TYPE_PUSH: u64 = 2;
pub const CMD_TYPE_PUSH_ACK: u64 = 3;

// Built-in command words.
pub const CMD_AUTH_BIND: &str = "auth-bind";
pub const CMD_PING: &str = "ping";

// Built-in module names.
pub const MODULE_CONN_ACCESS: &str = "conn_access";

// Business-layer package (short name, matching the TS client).
pub const BIZ_PKG: &str = "yuanbao_openclaw_proxy";

/// openclaw instance_id (fixed value 17 in hermes).
pub const INSTANCE_ID: u32 = 17;

// Reply-heartbeat status constants.
pub const WS_HEARTBEAT_RUNNING: u64 = 1;
#[allow(dead_code)]
pub const WS_HEARTBEAT_FINISH: u64 = 2;

// Wire types.
const WT_VARINT: u64 = 0;
#[allow(dead_code)]
const WT_64BIT: u64 = 1;
const WT_LEN: u64 = 2;
#[allow(dead_code)]
const WT_32BIT: u64 = 5;

static SEQ_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Incrementing sequence number (wrapping at u32).
pub fn next_seq_no() -> u32 {
    SEQ_COUNTER.fetch_add(1, Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Protobuf wire-format primitives
// ---------------------------------------------------------------------------

pub fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

pub fn decode_varint(data: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    let mut i = pos;
    while i < data.len() {
        let byte = data[i];
        result |= ((byte & 0x7F) as u64) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            return Some((result, i));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

fn encode_field(field_number: u64, wire_type: u64, value: &[u8]) -> Vec<u8> {
    let mut out = encode_varint((field_number << 3) | wire_type);
    out.extend_from_slice(value);
    out
}

fn encode_string_field(field_number: u64, value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut payload = encode_varint(bytes.len() as u64);
    payload.extend_from_slice(bytes);
    encode_field(field_number, WT_LEN, &payload)
}

fn encode_bytes_field(field_number: u64, value: &[u8]) -> Vec<u8> {
    let mut payload = encode_varint(value.len() as u64);
    payload.extend_from_slice(value);
    encode_field(field_number, WT_LEN, &payload)
}

fn encode_message_field(field_number: u64, message: &[u8]) -> Vec<u8> {
    encode_bytes_field(field_number, message)
}

fn encode_varint_field(field_number: u64, value: u64) -> Vec<u8> {
    encode_field(field_number, WT_VARINT, &encode_varint(value))
}

/// Parsed protobuf field: (field_number, wire_type, payload).
#[derive(Debug, Clone)]
pub enum FieldValue {
    Varint(u64),
    Bytes(Vec<u8>),
    Other,
}

pub fn parse_fields(mut data: &[u8]) -> Vec<(u64, FieldValue)> {
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let Some((tag, next)) = decode_varint(data, pos) else {
            break;
        };
        pos = next;
        let field_number = tag >> 3;
        let wire_type = tag & 0x7;
        match wire_type {
            WT_VARINT => {
                let Some((value, next)) = decode_varint(data, pos) else {
                    break;
                };
                pos = next;
                fields.push((field_number, FieldValue::Varint(value)));
            }
            WT_LEN => {
                let Some((len, next)) = decode_varint(data, pos) else {
                    break;
                };
                pos = next;
                let len = len as usize;
                if pos + len > data.len() {
                    break;
                }
                fields.push((
                    field_number,
                    FieldValue::Bytes(data[pos..pos + len].to_vec()),
                ));
                pos += len;
            }
            WT_64BIT => {
                if pos + 8 > data.len() {
                    break;
                }
                pos += 8;
                fields.push((field_number, FieldValue::Other));
            }
            WT_32BIT => {
                if pos + 4 > data.len() {
                    break;
                }
                pos += 4;
                fields.push((field_number, FieldValue::Other));
            }
            _ => break,
        }
    }
    let _ = &mut data;
    fields
}

type FieldMap = HashMap<u64, Vec<FieldValue>>;

fn fields_to_dict(fields: Vec<(u64, FieldValue)>) -> FieldMap {
    let mut map: FieldMap = HashMap::new();
    for (fn_, value) in fields {
        map.entry(fn_).or_default().push(value);
    }
    map
}

fn get_string(map: &FieldMap, field_number: u64) -> String {
    map.get(&field_number)
        .and_then(|values| values.first())
        .and_then(|value| match value {
            FieldValue::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn get_varint(map: &FieldMap, field_number: u64) -> u64 {
    map.get(&field_number)
        .and_then(|values| values.first())
        .and_then(|value| match value {
            FieldValue::Varint(v) => Some(*v),
            _ => None,
        })
        .unwrap_or(0)
}

fn get_bytes(map: &FieldMap, field_number: u64) -> Vec<u8> {
    map.get(&field_number)
        .and_then(|values| values.first())
        .and_then(|value| match value {
            FieldValue::Bytes(bytes) => Some(bytes.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn get_repeated_bytes(map: &FieldMap, field_number: u64) -> Vec<Vec<u8>> {
    map.get(&field_number)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| match value {
                    FieldValue::Bytes(bytes) => Some(bytes.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// ConnMsg.Head
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnHead {
    pub cmd_type: u64,
    pub cmd: String,
    pub seq_no: u64,
    pub msg_id: String,
    pub module: String,
    pub need_ack: bool,
    pub status: u64,
}

fn encode_head(head: &ConnHead) -> Vec<u8> {
    let mut buf = Vec::new();
    if head.cmd_type != 0 {
        buf.extend(encode_varint_field(1, head.cmd_type));
    }
    if !head.cmd.is_empty() {
        buf.extend(encode_string_field(2, &head.cmd));
    }
    if head.seq_no != 0 {
        buf.extend(encode_varint_field(3, head.seq_no));
    }
    if !head.msg_id.is_empty() {
        buf.extend(encode_string_field(4, &head.msg_id));
    }
    if !head.module.is_empty() {
        buf.extend(encode_string_field(5, &head.module));
    }
    if head.need_ack {
        buf.extend(encode_varint_field(6, 1));
    }
    if head.status != 0 {
        buf.extend(encode_varint_field(10, head.status));
    }
    buf
}

pub fn decode_head(data: &[u8]) -> ConnHead {
    let map = fields_to_dict(parse_fields(data));
    ConnHead {
        cmd_type: get_varint(&map, 1),
        cmd: get_string(&map, 2),
        seq_no: get_varint(&map, 3),
        msg_id: get_string(&map, 4),
        module: get_string(&map, 5),
        need_ack: get_varint(&map, 6) != 0,
        status: get_varint(&map, 10),
    }
}

/// Decoded ConnMsg envelope.
#[derive(Debug, Clone)]
pub struct ConnMsg {
    pub head: ConnHead,
    pub data: Vec<u8>,
}

/// Encode a full ConnMsg (hermes `encode_conn_msg_full`).
pub fn encode_conn_msg_full(head: &ConnHead, data: &[u8]) -> Vec<u8> {
    let head_bytes = encode_head(head);
    let mut buf = encode_message_field(1, &head_bytes);
    if !data.is_empty() {
        buf.extend(encode_bytes_field(2, data));
    }
    buf
}

/// Decode a ConnMsg (hermes `decode_conn_msg`).
pub fn decode_conn_msg(data: &[u8]) -> ConnMsg {
    let map = fields_to_dict(parse_fields(data));
    let head_bytes = get_bytes(&map, 1);
    let payload = get_bytes(&map, 2);
    let head = if head_bytes.is_empty() {
        ConnHead::default()
    } else {
        decode_head(&head_bytes)
    };
    ConnMsg {
        head,
        data: payload,
    }
}

/// Business-layer view over a ConnMsg (hermes `decode_biz_msg`).
#[derive(Debug, Clone)]
pub struct BizMsg {
    pub service: String,
    pub method: String,
    pub req_id: String,
    pub body: Vec<u8>,
    pub is_response: bool,
    pub head: ConnHead,
}

pub fn decode_biz_msg(data: &[u8]) -> BizMsg {
    let msg = decode_conn_msg(data);
    BizMsg {
        service: msg.head.module.clone(),
        method: msg.head.cmd.clone(),
        req_id: msg.head.msg_id.clone(),
        body: msg.data.clone(),
        is_response: msg.head.cmd_type == CMD_TYPE_RESPONSE,
        head: msg.head,
    }
}

// ---------------------------------------------------------------------------
// MsgContent / MsgBodyElement
// ---------------------------------------------------------------------------

/// One `MsgContent` entry (subset of hermes fields used by the text path +
/// inbound decode).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MsgContent {
    pub text: String,
    pub uuid: String,
    pub data: String,
    pub desc: String,
    pub ext: String,
    pub url: String,
    pub file_name: String,
    pub file_size: u64,
    /// TIM `image_format` (field 3) — hermes `_MIME_TO_IMAGE_FORMAT`.
    pub image_format: u32,
    /// TIM `image_info_array` (field 8) — hermes
    /// `build_image_msg_body` entries.
    pub image_info_array: Vec<ImageInfo>,
    /// TIM `index` (field 9) — TIMFaceElem face/sticker index; 0 for
    /// Yuanbao catalog stickers (hermes omits zero varints on encode).
    pub index: u64,
    /// TIMCustomElem `ext_map` (field 999, `map<string, string>`) —
    /// carries the base64-encoded ForwardMsgData payload for forwarded
    /// WeChat chat records (hermes elem_type 1009).
    pub ext_map: Vec<(String, String)>,
}

/// One `image_info_array` entry (hermes `build_image_msg_body`: type
/// 1 = original).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageInfo {
    pub info_type: u64,
    pub size: u64,
    pub width: u64,
    pub height: u64,
    pub url: String,
}

fn encode_msg_content(content: &MsgContent) -> Vec<u8> {
    let mut buf = Vec::new();
    if !content.text.is_empty() {
        buf.extend(encode_string_field(1, &content.text));
    }
    if !content.uuid.is_empty() {
        buf.extend(encode_string_field(2, &content.uuid));
    }
    if content.image_format != 0 {
        buf.extend(encode_varint_field(3, content.image_format as u64));
    }
    if !content.data.is_empty() {
        buf.extend(encode_string_field(4, &content.data));
    }
    if !content.desc.is_empty() {
        buf.extend(encode_string_field(5, &content.desc));
    }
    if !content.ext.is_empty() {
        buf.extend(encode_string_field(6, &content.ext));
    }
    for info in &content.image_info_array {
        let mut img_buf = Vec::new();
        if info.info_type != 0 {
            img_buf.extend(encode_varint_field(1, info.info_type));
        }
        if info.size != 0 {
            img_buf.extend(encode_varint_field(2, info.size));
        }
        if info.width != 0 {
            img_buf.extend(encode_varint_field(3, info.width));
        }
        if info.height != 0 {
            img_buf.extend(encode_varint_field(4, info.height));
        }
        if !info.url.is_empty() {
            img_buf.extend(encode_string_field(5, &info.url));
        }
        buf.extend(encode_message_field(8, &img_buf));
    }
    if content.index != 0 {
        buf.extend(encode_varint_field(9, content.index));
    }
    if !content.url.is_empty() {
        buf.extend(encode_string_field(10, &content.url));
    }
    if content.file_size != 0 {
        buf.extend(encode_varint_field(11, content.file_size));
    }
    if !content.file_name.is_empty() {
        buf.extend(encode_string_field(12, &content.file_name));
    }
    for (key, value) in &content.ext_map {
        let mut entry = Vec::new();
        entry.extend(encode_string_field(1, key));
        entry.extend(encode_string_field(2, value));
        buf.extend(encode_message_field(999, &entry));
    }
    buf
}

pub fn decode_msg_content(data: &[u8]) -> MsgContent {
    let map = fields_to_dict(parse_fields(data));
    let image_info_array = get_repeated_bytes(&map, 8)
        .into_iter()
        .map(|bytes| {
            let img = fields_to_dict(parse_fields(&bytes));
            ImageInfo {
                info_type: get_varint(&img, 1),
                size: get_varint(&img, 2),
                width: get_varint(&img, 3),
                height: get_varint(&img, 4),
                url: get_string(&img, 5),
            }
        })
        .collect();
    MsgContent {
        text: get_string(&map, 1),
        uuid: get_string(&map, 2),
        data: get_string(&map, 4),
        desc: get_string(&map, 5),
        ext: get_string(&map, 6),
        url: get_string(&map, 10),
        file_size: get_varint(&map, 11),
        file_name: get_string(&map, 12),
        image_format: get_varint(&map, 3) as u32,
        image_info_array,
        index: get_varint(&map, 9),
        ext_map: get_repeated_bytes(&map, 999)
            .into_iter()
            .map(|entry| {
                let entry_map = fields_to_dict(parse_fields(&entry));
                (get_string(&entry_map, 1), get_string(&entry_map, 2))
            })
            .collect(),
    }
}

/// One `MsgBodyElement` (msg_type + content).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MsgBodyElement {
    pub msg_type: String,
    pub msg_content: MsgContent,
}

pub fn encode_msg_body_element(element: &MsgBodyElement) -> Vec<u8> {
    let mut buf = Vec::new();
    if !element.msg_type.is_empty() {
        buf.extend(encode_string_field(1, &element.msg_type));
    }
    let content_bytes = encode_msg_content(&element.msg_content);
    if !content_bytes.is_empty() {
        buf.extend(encode_message_field(2, &content_bytes));
    }
    buf
}

pub fn decode_msg_body_element(data: &[u8]) -> MsgBodyElement {
    let map = fields_to_dict(parse_fields(data));
    let content_bytes = get_bytes(&map, 2);
    MsgBodyElement {
        msg_type: get_string(&map, 1),
        msg_content: if content_bytes.is_empty() {
            MsgContent::default()
        } else {
            decode_msg_content(&content_bytes)
        },
    }
}

fn encode_log_ext(trace_id: &str) -> Vec<u8> {
    if trace_id.is_empty() {
        return Vec::new();
    }
    encode_string_field(1, trace_id)
}

fn decode_log_ext(data: &[u8]) -> String {
    let map = fields_to_dict(parse_fields(data));
    get_string(&map, 1)
}

// ---------------------------------------------------------------------------
// ForwardMsgData (hermes `decode_forward_msg_data` + sub-messages) —
// the base64-decoded ext_map payload of forwarded WeChat chat records
// (TIMCustomElem elem_type 1009).
// ---------------------------------------------------------------------------

/// One `multimedia` entry inside a forwarded record (hermes
/// `_decode_forward_multimedia`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ForwardMultimedia {
    pub media_type: String,
    pub url: String,
    pub file_name: String,
    pub file_size: u64,
    pub media_id: String,
}

/// One `msgContent` entry inside a forwarded message (hermes
/// `_decode_forward_msg_content`): 1 = TEXT, 2 = MULTIMEDIA,
/// 3 = nested FORWARD_MSG.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ForwardMsgContent {
    pub content_type: u64,
    pub text: String,
    pub multimedia: Vec<ForwardMultimedia>,
}

/// One forwarded message (hermes `_decode_forward_msg`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ForwardMsg {
    pub sender: String,
    pub time: u64,
    pub plain_text: String,
    pub msg_content: Vec<ForwardMsgContent>,
}

/// Forwarded chat-record payload (hermes `decode_forward_msg_data`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ForwardMsgData {
    pub sub_type: u64,
    pub begin_time: u64,
    pub end_time: u64,
    pub nick_name: String,
    pub msg: Vec<ForwardMsg>,
}

fn decode_forward_multimedia(data: &[u8]) -> ForwardMultimedia {
    let map = fields_to_dict(parse_fields(data));
    ForwardMultimedia {
        media_type: get_string(&map, 1),
        url: get_string(&map, 2),
        file_name: get_string(&map, 4),
        file_size: get_varint(&map, 5),
        media_id: get_string(&map, 15),
    }
}

fn decode_forward_msg_content(data: &[u8]) -> ForwardMsgContent {
    let map = fields_to_dict(parse_fields(data));
    ForwardMsgContent {
        content_type: get_varint(&map, 1),
        text: get_string(&map, 2),
        multimedia: get_repeated_bytes(&map, 3)
            .into_iter()
            .map(|bytes| decode_forward_multimedia(&bytes))
            .collect(),
    }
}

fn decode_forward_msg(data: &[u8]) -> ForwardMsg {
    let map = fields_to_dict(parse_fields(data));
    ForwardMsg {
        sender: get_string(&map, 1),
        time: get_varint(&map, 2),
        plain_text: get_string(&map, 3),
        msg_content: get_repeated_bytes(&map, 4)
            .into_iter()
            .map(|bytes| decode_forward_msg_content(&bytes))
            .collect(),
    }
}

/// hermes `decode_forward_msg_data` — parse the base64-decoded ext_map
/// value; `None` on empty input.
pub fn decode_forward_msg_data(data: &[u8]) -> Option<ForwardMsgData> {
    if data.is_empty() {
        return None;
    }
    let map = fields_to_dict(parse_fields(data));
    Some(ForwardMsgData {
        sub_type: get_varint(&map, 1),
        begin_time: get_varint(&map, 2),
        end_time: get_varint(&map, 3),
        nick_name: get_string(&map, 4),
        msg: get_repeated_bytes(&map, 5)
            .into_iter()
            .map(|bytes| decode_forward_msg(&bytes))
            .collect(),
    })
}

/// hermes `_encode_forward_multimedia` (mock/test builder).
pub fn encode_forward_multimedia(media: &ForwardMultimedia) -> Vec<u8> {
    let mut buf = Vec::new();
    if !media.media_type.is_empty() {
        buf.extend(encode_string_field(1, &media.media_type));
    }
    if !media.url.is_empty() {
        buf.extend(encode_string_field(2, &media.url));
    }
    if !media.file_name.is_empty() {
        buf.extend(encode_string_field(4, &media.file_name));
    }
    if media.file_size != 0 {
        buf.extend(encode_varint_field(5, media.file_size));
    }
    if !media.media_id.is_empty() {
        buf.extend(encode_string_field(15, &media.media_id));
    }
    buf
}

/// hermes `_encode_forward_msg_content` (mock/test builder).
pub fn encode_forward_msg_content(content: &ForwardMsgContent) -> Vec<u8> {
    let mut buf = Vec::new();
    if content.content_type != 0 {
        buf.extend(encode_varint_field(1, content.content_type));
    }
    if !content.text.is_empty() {
        buf.extend(encode_string_field(2, &content.text));
    }
    for media in &content.multimedia {
        buf.extend(encode_message_field(3, &encode_forward_multimedia(media)));
    }
    buf
}

/// hermes `_encode_forward_msg` (mock/test builder).
pub fn encode_forward_msg(msg: &ForwardMsg) -> Vec<u8> {
    let mut buf = Vec::new();
    if !msg.sender.is_empty() {
        buf.extend(encode_string_field(1, &msg.sender));
    }
    if msg.time != 0 {
        buf.extend(encode_varint_field(2, msg.time));
    }
    if !msg.plain_text.is_empty() {
        buf.extend(encode_string_field(3, &msg.plain_text));
    }
    for content in &msg.msg_content {
        buf.extend(encode_message_field(
            4,
            &encode_forward_msg_content(content),
        ));
    }
    buf
}

/// hermes `encode_forward_msg_data` — "mainly used to build mock / test
/// data; production code never needs to encode this".
pub fn encode_forward_msg_data(data: &ForwardMsgData) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend(encode_varint_field(1, data.sub_type));
    if data.begin_time != 0 {
        buf.extend(encode_varint_field(2, data.begin_time));
    }
    if data.end_time != 0 {
        buf.extend(encode_varint_field(3, data.end_time));
    }
    if !data.nick_name.is_empty() {
        buf.extend(encode_string_field(4, &data.nick_name));
    }
    for msg in &data.msg {
        buf.extend(encode_message_field(5, &encode_forward_msg(msg)));
    }
    buf
}

// ---------------------------------------------------------------------------
// InboundMessagePush decode
// ---------------------------------------------------------------------------

/// Decoded inbound message push (hermes `decode_inbound_push` fields).
#[derive(Debug, Clone, Default)]
pub struct InboundPush {
    pub callback_command: String,
    pub from_account: String,
    pub to_account: String,
    pub sender_nickname: String,
    pub group_id: String,
    pub group_code: String,
    pub group_name: String,
    pub msg_seq: u64,
    pub msg_random: u64,
    pub msg_time: u64,
    pub msg_key: String,
    pub msg_id: String,
    pub msg_body: Vec<MsgBodyElement>,
    pub cloud_custom_data: String,
    pub bot_owner_id: String,
    pub claw_msg_type: u64,
    pub private_from_group_code: String,
    pub trace_id: String,
}

pub fn decode_inbound_push(data: &[u8]) -> Option<InboundPush> {
    let map = fields_to_dict(parse_fields(data));
    let msg_body: Vec<MsgBodyElement> = get_repeated_bytes(&map, 13)
        .iter()
        .map(|bytes| decode_msg_body_element(bytes))
        .collect();
    let log_ext_bytes = get_bytes(&map, 20);
    let trace_id = if log_ext_bytes.is_empty() {
        String::new()
    } else {
        decode_log_ext(&log_ext_bytes)
    };
    Some(InboundPush {
        callback_command: get_string(&map, 1),
        from_account: get_string(&map, 2),
        to_account: get_string(&map, 3),
        sender_nickname: get_string(&map, 4),
        group_id: get_string(&map, 5),
        group_code: get_string(&map, 6),
        group_name: get_string(&map, 7),
        msg_seq: get_varint(&map, 8),
        msg_random: get_varint(&map, 9),
        msg_time: get_varint(&map, 10),
        msg_key: get_string(&map, 11),
        msg_id: get_string(&map, 12),
        msg_body,
        cloud_custom_data: get_string(&map, 14),
        bot_owner_id: get_string(&map, 16),
        claw_msg_type: get_varint(&map, 18),
        private_from_group_code: get_string(&map, 19),
        trace_id,
    })
}

// ---------------------------------------------------------------------------
// Outbound message encoding
// ---------------------------------------------------------------------------

/// Encode a SendC2CMessageReq biz payload (hermes `_encode_send_c2c_req`).
pub fn encode_send_c2c_req(
    to_account: &str,
    from_account: &str,
    msg_body: &[MsgBodyElement],
    msg_id: &str,
    msg_random: u64,
    msg_seq: Option<u64>,
    group_code: &str,
    trace_id: &str,
) -> Vec<u8> {
    let mut buf = Vec::new();
    if !msg_id.is_empty() {
        buf.extend(encode_string_field(1, msg_id));
    }
    buf.extend(encode_string_field(2, to_account));
    if !from_account.is_empty() {
        buf.extend(encode_string_field(3, from_account));
    }
    if msg_random != 0 {
        buf.extend(encode_varint_field(4, msg_random));
    }
    for element in msg_body {
        buf.extend(encode_message_field(5, &encode_msg_body_element(element)));
    }
    if !group_code.is_empty() {
        buf.extend(encode_string_field(6, group_code));
    }
    if let Some(seq) = msg_seq {
        buf.extend(encode_varint_field(7, seq));
    }
    if !trace_id.is_empty() {
        buf.extend(encode_message_field(8, &encode_log_ext(trace_id)));
    }
    buf
}

/// Full ConnMsg for `send_c2c_message` (hermes `encode_send_c2c_message`).
pub fn encode_send_c2c_message(
    to_account: &str,
    msg_body: &[MsgBodyElement],
    from_account: &str,
    msg_id: &str,
    msg_random: u64,
    msg_seq: Option<u64>,
    group_code: &str,
    trace_id: &str,
) -> Vec<u8> {
    let biz = encode_send_c2c_req(
        to_account,
        from_account,
        msg_body,
        msg_id,
        msg_random,
        msg_seq,
        group_code,
        trace_id,
    );
    let req_id = if msg_id.is_empty() {
        format!("c2c_{}", next_seq_no())
    } else {
        msg_id.to_string()
    };
    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: "send_c2c_message".into(),
            seq_no: next_seq_no() as u64,
            msg_id: req_id,
            module: BIZ_PKG.into(),
            ..Default::default()
        },
        &biz,
    )
}

/// Encode a SendGroupMessageReq biz payload (hermes `_encode_send_group_req`).
pub fn encode_send_group_req(
    group_code: &str,
    from_account: &str,
    msg_body: &[MsgBodyElement],
    msg_id: &str,
    to_account: &str,
    random: &str,
    msg_seq: Option<u64>,
    ref_msg_id: &str,
    trace_id: &str,
) -> Vec<u8> {
    let mut buf = Vec::new();
    if !msg_id.is_empty() {
        buf.extend(encode_string_field(1, msg_id));
    }
    buf.extend(encode_string_field(2, group_code));
    if !from_account.is_empty() {
        buf.extend(encode_string_field(3, from_account));
    }
    if !to_account.is_empty() {
        buf.extend(encode_string_field(4, to_account));
    }
    if !random.is_empty() {
        buf.extend(encode_string_field(5, random));
    }
    for element in msg_body {
        buf.extend(encode_message_field(6, &encode_msg_body_element(element)));
    }
    if !ref_msg_id.is_empty() {
        buf.extend(encode_string_field(7, ref_msg_id));
    }
    if let Some(seq) = msg_seq {
        buf.extend(encode_varint_field(8, seq));
    }
    if !trace_id.is_empty() {
        buf.extend(encode_message_field(9, &encode_log_ext(trace_id)));
    }
    buf
}

/// Full ConnMsg for `send_group_message` (hermes `encode_send_group_message`).
pub fn encode_send_group_message(
    group_code: &str,
    msg_body: &[MsgBodyElement],
    from_account: &str,
    msg_id: &str,
    to_account: &str,
    random: &str,
    msg_seq: Option<u64>,
    ref_msg_id: &str,
    trace_id: &str,
) -> Vec<u8> {
    let biz = encode_send_group_req(
        group_code,
        from_account,
        msg_body,
        msg_id,
        to_account,
        random,
        msg_seq,
        ref_msg_id,
        trace_id,
    );
    let req_id = if msg_id.is_empty() {
        format!("grp_{}", next_seq_no())
    } else {
        msg_id.to_string()
    };
    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: "send_group_message".into(),
            seq_no: next_seq_no() as u64,
            msg_id: req_id,
            module: BIZ_PKG.into(),
            ..Default::default()
        },
        &biz,
    )
}

// ---------------------------------------------------------------------------
// AuthBind / Ping / PushAck / heartbeats
// ---------------------------------------------------------------------------

/// hermes `encode_auth_bind`.
#[allow(clippy::too_many_arguments)]
pub fn encode_auth_bind(
    biz_id: &str,
    uid: &str,
    source: &str,
    token: &str,
    msg_id: &str,
    app_version: &str,
    operation_system: &str,
    bot_version: &str,
    route_env: &str,
) -> Vec<u8> {
    // AuthInfo: uid=1, source=2, token=3
    let mut auth_buf = encode_string_field(1, uid);
    auth_buf.extend(encode_string_field(2, source));
    auth_buf.extend(encode_string_field(3, token));

    // DeviceInfo: app_version=1, app_operation_system=2, instance_id=10,
    // bot_version=24
    let mut dev_buf = Vec::new();
    if !app_version.is_empty() {
        dev_buf.extend(encode_string_field(1, app_version));
    }
    if !operation_system.is_empty() {
        dev_buf.extend(encode_string_field(2, operation_system));
    }
    dev_buf.extend(encode_string_field(10, &INSTANCE_ID.to_string()));
    if !bot_version.is_empty() {
        dev_buf.extend(encode_string_field(24, bot_version));
    }

    let mut req_buf = encode_string_field(1, biz_id);
    req_buf.extend(encode_message_field(2, &auth_buf));
    req_buf.extend(encode_message_field(3, &dev_buf));
    if !route_env.is_empty() {
        req_buf.extend(encode_string_field(5, route_env));
    }

    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: CMD_AUTH_BIND.into(),
            seq_no: next_seq_no() as u64,
            msg_id: msg_id.to_string(),
            module: MODULE_CONN_ACCESS.into(),
            ..Default::default()
        },
        &req_buf,
    )
}

/// hermes `encode_ping` (PingReq is an empty message).
pub fn encode_ping(msg_id: &str) -> Vec<u8> {
    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: CMD_PING.into(),
            seq_no: next_seq_no() as u64,
            msg_id: msg_id.to_string(),
            module: MODULE_CONN_ACCESS.into(),
            ..Default::default()
        },
        &[],
    )
}

/// hermes `encode_push_ack`.
pub fn encode_push_ack(original_head: &ConnHead) -> Vec<u8> {
    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_PUSH_ACK,
            cmd: original_head.cmd.clone(),
            seq_no: next_seq_no() as u64,
            msg_id: original_head.msg_id.clone(),
            module: original_head.module.clone(),
            ..Default::default()
        },
        &[],
    )
}

/// hermes `encode_send_private_heartbeat`.
pub fn encode_send_private_heartbeat(
    from_account: &str,
    to_account: &str,
    heartbeat: u64,
) -> Vec<u8> {
    let mut buf = encode_string_field(1, from_account);
    buf.extend(encode_string_field(2, to_account));
    buf.extend(encode_varint_field(3, heartbeat));
    let req_id = format!("hb_priv_{}", next_seq_no());
    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: "send_private_heartbeat".into(),
            seq_no: next_seq_no() as u64,
            msg_id: req_id,
            module: BIZ_PKG.into(),
            ..Default::default()
        },
        &buf,
    )
}

/// hermes `encode_send_group_heartbeat`.
pub fn encode_send_group_heartbeat(
    from_account: &str,
    group_code: &str,
    heartbeat: u64,
    send_time_ms: u64,
) -> Vec<u8> {
    let ts = if send_time_ms == 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    } else {
        send_time_ms
    };
    let mut buf = encode_string_field(1, from_account);
    buf.extend(encode_string_field(2, "")); // to_account empty for group
    buf.extend(encode_string_field(3, group_code));
    buf.extend(encode_varint_field(4, ts));
    buf.extend(encode_varint_field(5, heartbeat));
    let req_id = format!("hb_grp_{}", next_seq_no());
    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: "send_group_heartbeat".into(),
            seq_no: next_seq_no() as u64,
            msg_id: req_id,
            module: BIZ_PKG.into(),
            ..Default::default()
        },
        &buf,
    )
}

/// AuthBindRsp parse: Ok(connect_id) when code == 0 (hermes
/// `_extract_connect_id`).
pub fn decode_auth_bind_rsp(data: &[u8]) -> std::result::Result<String, String> {
    let map = fields_to_dict(parse_fields(data));
    let code = get_varint(&map, 1);
    if code != 0 {
        let message = get_string(&map, 2);
        return Err(format!(
            "AuthBindRsp error: code={code} message={message:?}"
        ));
    }
    let connect_id = get_string(&map, 3);
    if connect_id.is_empty() {
        return Err("AuthBindRsp missing connectId".into());
    }
    Ok(connect_id)
}

/// SendC2CMessageRsp / SendGroupMessageRsp: field 1 carries the result
/// code (0 = success).
pub fn decode_send_rsp_code(data: &[u8]) -> u64 {
    let map = fields_to_dict(parse_fields(data));
    get_varint(&map, 1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ============================================================
// Group queries (hermes encode_query_group_info /
// encode_get_group_member_list + rsp decoders) — biz RPCs used by
// the yb_* tools (P248).
// ============================================================

/// Encode a QueryGroupInfoReq ConnMsg (hermes `encode_query_group_info`).
///
/// QueryGroupInfoReq fields: 1 = group_code.
pub fn encode_query_group_info(group_code: &str, req_id: &str) -> Vec<u8> {
    let body = encode_string_field(1, group_code);
    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: "query_group_info".into(),
            seq_no: next_seq_no() as u64,
            msg_id: req_id.to_string(),
            module: BIZ_PKG.into(),
            ..Default::default()
        },
        &body,
    )
}

/// Encode a GetGroupMemberListReq ConnMsg (hermes
/// `encode_get_group_member_list`).
///
/// GetGroupMemberListReq fields: 1 = group_code, 2 = offset, 3 = limit.
pub fn encode_get_group_member_list(
    group_code: &str,
    offset: u64,
    limit: u64,
    req_id: &str,
) -> Vec<u8> {
    let mut body = encode_string_field(1, group_code);
    if offset != 0 {
        body.extend(encode_varint_field(2, offset));
    }
    body.extend(encode_varint_field(3, limit));
    encode_conn_msg_full(
        &ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: "get_group_member_list".into(),
            seq_no: next_seq_no() as u64,
            msg_id: req_id.to_string(),
            module: BIZ_PKG.into(),
            ..Default::default()
        },
        &body,
    )
}

/// Decoded QueryGroupInfoRsp (hermes `decode_query_group_info_rsp`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryGroupInfoRsp {
    pub code: u64,
    pub message: String,
    pub group_name: String,
    pub owner_id: String,
    pub owner_nickname: String,
    pub member_count: u64,
}

/// Decode a QueryGroupInfoRsp biz payload.
///
/// QueryGroupInfoRsp: 1 = code, 2 = message, 3 = GroupInfo{1 = name,
/// 2 = owner user id, 3 = owner nickname, 4 = group size}.
pub fn decode_query_group_info_rsp(data: &[u8]) -> QueryGroupInfoRsp {
    let map = fields_to_dict(parse_fields(data));
    let mut rsp = QueryGroupInfoRsp {
        code: get_varint(&map, 1),
        message: get_string(&map, 2),
        ..Default::default()
    };
    let group_info = get_bytes(&map, 3);
    if !group_info.is_empty() {
        let gi = fields_to_dict(parse_fields(&group_info));
        rsp.group_name = get_string(&gi, 1);
        rsp.owner_id = get_string(&gi, 2);
        rsp.owner_nickname = get_string(&gi, 3);
        rsp.member_count = get_varint(&gi, 4);
    }
    rsp
}

/// One group member (hermes MemberInfo: 1 = user_id, 2 = nickname,
/// 3 = role/user_type, 4 = join_time, 5 = name_card).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberInfo {
    pub user_id: String,
    pub nickname: String,
    pub role: u64,
    pub join_time: u64,
    pub name_card: String,
}

/// Decoded GetGroupMemberListRsp (hermes
/// `decode_get_group_member_list_rsp`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberListRsp {
    pub code: u64,
    pub message: String,
    pub members: Vec<MemberInfo>,
    pub next_offset: u64,
    pub is_complete: bool,
}

/// Decode a GetGroupMemberListRsp biz payload.
pub fn decode_get_group_member_list_rsp(data: &[u8]) -> MemberListRsp {
    let map = fields_to_dict(parse_fields(data));
    let mut rsp = MemberListRsp {
        code: get_varint(&map, 1),
        message: get_string(&map, 2),
        next_offset: get_varint(&map, 4),
        is_complete: get_varint(&map, 5) != 0,
        ..Default::default()
    };
    if let Some(entries) = map.get(&3) {
        for entry in entries {
            let FieldValue::Bytes(member_bytes) = entry else {
                continue;
            };
            let m = fields_to_dict(parse_fields(member_bytes));
            rsp.members.push(MemberInfo {
                user_id: get_string(&m, 1),
                nickname: get_string(&m, 2),
                role: get_varint(&m, 3),
                join_time: get_varint(&m, 4),
                name_card: get_string(&m, 5),
            });
        }
    }
    rsp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for value in [0u64, 1, 127, 128, 300, 16_384, u32::MAX as u64, u64::MAX] {
            let encoded = encode_varint(value);
            let (decoded, pos) = decode_varint(&encoded, 0).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(pos, encoded.len());
        }
    }

    #[test]
    fn conn_msg_roundtrip() {
        let head = ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: "send_c2c_message".into(),
            seq_no: 42,
            msg_id: "req-123".into(),
            module: BIZ_PKG.into(),
            need_ack: true,
            status: 0,
        };
        let payload = b"business-bytes";
        let encoded = encode_conn_msg_full(&head, payload);
        let decoded = decode_conn_msg(&encoded);
        assert_eq!(decoded.head.cmd_type, CMD_TYPE_REQUEST);
        assert_eq!(decoded.head.cmd, "send_c2c_message");
        assert_eq!(decoded.head.seq_no, 42);
        assert_eq!(decoded.head.msg_id, "req-123");
        assert_eq!(decoded.head.module, BIZ_PKG);
        assert!(decoded.head.need_ack);
        assert_eq!(decoded.data, payload);
    }

    #[test]
    fn conn_msg_empty_data() {
        let head = ConnHead {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: CMD_PING.into(),
            seq_no: 7,
            msg_id: "ping-1".into(),
            module: MODULE_CONN_ACCESS.into(),
            ..Default::default()
        };
        let encoded = encode_conn_msg_full(&head, &[]);
        let decoded = decode_conn_msg(&encoded);
        assert_eq!(decoded.head.cmd, CMD_PING);
        assert!(decoded.data.is_empty());
    }

    #[test]
    fn biz_msg_view() {
        let head = ConnHead {
            cmd_type: CMD_TYPE_RESPONSE,
            cmd: "send_c2c_message".into(),
            seq_no: 1,
            msg_id: "req-9".into(),
            module: BIZ_PKG.into(),
            ..Default::default()
        };
        let encoded = encode_conn_msg_full(&head, b"rsp");
        let biz = decode_biz_msg(&encoded);
        assert_eq!(biz.service, BIZ_PKG);
        assert_eq!(biz.method, "send_c2c_message");
        assert_eq!(biz.req_id, "req-9");
        assert!(biz.is_response);
        assert_eq!(biz.body, b"rsp");
    }

    #[test]
    fn msg_content_roundtrip() {
        let content = MsgContent {
            text: "你好，世界".into(),
            uuid: "u-1".into(),
            file_name: "doc.pdf".into(),
            file_size: 1234,
            ..Default::default()
        };
        let encoded = encode_msg_content(&content);
        let decoded = decode_msg_content(&encoded);
        assert_eq!(decoded.text, "你好，世界");
        assert_eq!(decoded.uuid, "u-1");
        assert_eq!(decoded.file_name, "doc.pdf");
        assert_eq!(decoded.file_size, 1234);
    }

    #[test]
    fn msg_body_element_roundtrip() {
        let element = MsgBodyElement {
            msg_type: "TIMTextElem".into(),
            msg_content: MsgContent {
                text: "hello".into(),
                ..Default::default()
            },
        };
        let decoded = decode_msg_body_element(&encode_msg_body_element(&element));
        assert_eq!(decoded, element);
    }

    #[test]
    fn inbound_push_decode() {
        // Hand-build an InboundMessagePush payload (fields per the hermes
        // proto mapping).
        let text_element = encode_msg_body_element(&MsgBodyElement {
            msg_type: "TIMTextElem".into(),
            msg_content: MsgContent {
                text: "ping".into(),
                ..Default::default()
            },
        });
        let mut payload = Vec::new();
        payload.extend(encode_string_field(2, "user-1")); // from_account
        payload.extend(encode_string_field(4, "Alice")); // sender_nickname
        payload.extend(encode_string_field(6, "group-9")); // group_code
        payload.extend(encode_varint_field(8, 77)); // msg_seq
        payload.extend(encode_string_field(11, "key-1")); // msg_key
        payload.extend(encode_string_field(12, "msg-1")); // msg_id
        payload.extend(encode_message_field(13, &text_element)); // msg_body
        payload.extend(encode_message_field(20, &encode_log_ext("trace-1")));

        let push = decode_inbound_push(&payload).unwrap();
        assert_eq!(push.from_account, "user-1");
        assert_eq!(push.sender_nickname, "Alice");
        assert_eq!(push.group_code, "group-9");
        assert_eq!(push.msg_seq, 77);
        assert_eq!(push.msg_key, "key-1");
        assert_eq!(push.msg_id, "msg-1");
        assert_eq!(push.trace_id, "trace-1");
        assert_eq!(push.msg_body.len(), 1);
        assert_eq!(push.msg_body[0].msg_type, "TIMTextElem");
        assert_eq!(push.msg_body[0].msg_content.text, "ping");
    }

    #[test]
    fn send_c2c_message_envelope() {
        let element = MsgBodyElement {
            msg_type: "TIMTextElem".into(),
            msg_content: MsgContent {
                text: "hi".into(),
                ..Default::default()
            },
        };
        let bytes = encode_send_c2c_message("user-1", &[element], "bot-1", "m-1", 5, None, "", "");
        let msg = decode_conn_msg(&bytes);
        assert_eq!(msg.head.cmd_type, CMD_TYPE_REQUEST);
        assert_eq!(msg.head.cmd, "send_c2c_message");
        assert_eq!(msg.head.module, BIZ_PKG);
        assert_eq!(msg.head.msg_id, "m-1");

        // Decode the SendC2CMessageReq payload.
        let map = fields_to_dict(parse_fields(&msg.data));
        assert_eq!(get_string(&map, 1), "m-1");
        assert_eq!(get_string(&map, 2), "user-1");
        assert_eq!(get_string(&map, 3), "bot-1");
        assert_eq!(get_varint(&map, 4), 5);
        let body = get_repeated_bytes(&map, 5);
        assert_eq!(body.len(), 1);
        let element = decode_msg_body_element(&body[0]);
        assert_eq!(element.msg_content.text, "hi");
    }

    #[test]
    fn send_group_message_envelope() {
        let element = MsgBodyElement {
            msg_type: "TIMTextElem".into(),
            msg_content: MsgContent {
                text: "hello group".into(),
                ..Default::default()
            },
        };
        let bytes = encode_send_group_message(
            "group-9",
            &[element],
            "bot-1",
            "m-2",
            "",
            "",
            Some(3),
            "ref-1",
            "trace-2",
        );
        let msg = decode_conn_msg(&bytes);
        assert_eq!(msg.head.cmd, "send_group_message");

        let map = fields_to_dict(parse_fields(&msg.data));
        assert_eq!(get_string(&map, 1), "m-2");
        assert_eq!(get_string(&map, 2), "group-9");
        assert_eq!(get_string(&map, 3), "bot-1");
        assert_eq!(get_string(&map, 7), "ref-1");
        assert_eq!(get_varint(&map, 8), 3);
        let body = get_repeated_bytes(&map, 6);
        assert_eq!(
            decode_msg_body_element(&body[0]).msg_content.text,
            "hello group"
        );
    }

    #[test]
    fn auth_bind_envelope() {
        let bytes = encode_auth_bind(
            "ybBot", "bot-1", "bot", "tok-1", "auth-1", "1.0", "linux", "1.0", "",
        );
        let msg = decode_conn_msg(&bytes);
        assert_eq!(msg.head.cmd, CMD_AUTH_BIND);
        assert_eq!(msg.head.module, MODULE_CONN_ACCESS);
        assert_eq!(msg.head.msg_id, "auth-1");

        let map = fields_to_dict(parse_fields(&msg.data));
        assert_eq!(get_string(&map, 1), "ybBot");
        let auth = fields_to_dict(parse_fields(&get_bytes(&map, 2)));
        assert_eq!(get_string(&auth, 1), "bot-1");
        assert_eq!(get_string(&auth, 2), "bot");
        assert_eq!(get_string(&auth, 3), "tok-1");
        let dev = fields_to_dict(parse_fields(&get_bytes(&map, 3)));
        assert_eq!(get_string(&dev, 10), "17"); // instance_id
    }

    #[test]
    fn auth_bind_rsp_decode() {
        // code=0, connectId="conn-1"
        let mut data = encode_varint_field(1, 0);
        data.extend(encode_string_field(3, "conn-1"));
        assert_eq!(decode_auth_bind_rsp(&data).unwrap(), "conn-1");

        // code=500 with message
        let mut bad = encode_varint_field(1, 500);
        bad.extend(encode_string_field(2, "invalid token"));
        let err = decode_auth_bind_rsp(&bad).unwrap_err();
        assert!(err.contains("500") && err.contains("invalid token"));
    }

    #[test]
    fn ping_and_push_ack() {
        let ping = decode_conn_msg(&encode_ping("p-1"));
        assert_eq!(ping.head.cmd, CMD_PING);
        assert_eq!(ping.head.module, MODULE_CONN_ACCESS);
        assert!(ping.data.is_empty());

        let original = ConnHead {
            cmd_type: CMD_TYPE_PUSH,
            cmd: "InboundMessagePush".into(),
            seq_no: 0,
            msg_id: "push-1".into(),
            module: BIZ_PKG.into(),
            need_ack: true,
            ..Default::default()
        };
        let ack = decode_conn_msg(&encode_push_ack(&original));
        assert_eq!(ack.head.cmd_type, CMD_TYPE_PUSH_ACK);
        assert_eq!(ack.head.cmd, "InboundMessagePush");
        assert_eq!(ack.head.msg_id, "push-1");
    }

    #[test]
    fn heartbeat_encodes() {
        let hb = decode_conn_msg(&encode_send_private_heartbeat(
            "bot-1",
            "user-1",
            WS_HEARTBEAT_RUNNING,
        ));
        assert_eq!(hb.head.cmd, "send_private_heartbeat");
        assert_eq!(hb.head.module, BIZ_PKG);

        let ghb = decode_conn_msg(&encode_send_group_heartbeat(
            "bot-1",
            "group-9",
            WS_HEARTBEAT_RUNNING,
            0,
        ));
        assert_eq!(ghb.head.cmd, "send_group_heartbeat");
    }

    #[test]
    fn send_rsp_code() {
        assert_eq!(decode_send_rsp_code(&encode_varint_field(1, 0)), 0);
        assert_eq!(decode_send_rsp_code(&encode_varint_field(1, 42)), 42);
        assert_eq!(decode_send_rsp_code(&[]), 0);
    }

    #[test]
    fn msg_content_index_field_round_trips() {
        let content = MsgContent {
            data: "{\"sticker_id\":\"225\"}".into(),
            index: 7,
            ..Default::default()
        };
        let element = MsgBodyElement {
            msg_type: "TIMFaceElem".into(),
            msg_content: content,
        };
        let decoded = decode_msg_body_element(&encode_msg_body_element(&element));
        assert_eq!(decoded.msg_type, "TIMFaceElem");
        assert_eq!(decoded.msg_content.index, 7);
        assert_eq!(decoded.msg_content.data, "{\"sticker_id\":\"225\"}");
    }

    #[test]
    fn group_info_request_round_trips() {
        let bytes = encode_query_group_info("328306697", "qgi_1");
        let msg = decode_conn_msg(&bytes);
        assert_eq!(msg.head.cmd, "query_group_info");
        assert_eq!(msg.head.cmd_type, CMD_TYPE_REQUEST);
        assert_eq!(msg.head.msg_id, "qgi_1");
        assert_eq!(msg.head.module, BIZ_PKG);
        let map = fields_to_dict(parse_fields(&msg.data));
        assert_eq!(get_string(&map, 1), "328306697");
    }

    #[test]
    fn member_list_request_round_trips() {
        let bytes = encode_get_group_member_list("777", 0, 200, "gml_2");
        let msg = decode_conn_msg(&bytes);
        assert_eq!(msg.head.cmd, "get_group_member_list");
        assert_eq!(msg.head.msg_id, "gml_2");
        let map = fields_to_dict(parse_fields(&msg.data));
        assert_eq!(get_string(&map, 1), "777");
        // offset=0 omitted (hermes parity); limit always present.
        assert!(map.get(&2).is_none());
        assert_eq!(get_varint(&map, 3), 200);
    }

    #[test]
    fn group_info_rsp_decodes_nested_group_info() {
        let gi = {
            let mut buf = encode_string_field(1, "派名");
            buf.extend(encode_string_field(2, "owner-u"));
            buf.extend(encode_string_field(3, "OwnerNick"));
            buf.extend(encode_varint_field(4, 42));
            buf
        };
        let mut payload = encode_varint_field(1, 0);
        payload.extend(encode_string_field(2, "ok"));
        payload.extend(encode_bytes_field(3, &gi));
        let rsp = decode_query_group_info_rsp(&payload);
        assert_eq!(rsp.code, 0);
        assert_eq!(rsp.message, "ok");
        assert_eq!(rsp.group_name, "派名");
        assert_eq!(rsp.owner_id, "owner-u");
        assert_eq!(rsp.owner_nickname, "OwnerNick");
        assert_eq!(rsp.member_count, 42);
    }

    #[test]
    fn member_list_rsp_decodes_repeated_members() {
        let member = {
            let mut buf = encode_string_field(1, "u1");
            buf.extend(encode_string_field(2, "Alice"));
            buf.extend(encode_varint_field(3, 2));
            buf.extend(encode_varint_field(4, 1700000000));
            buf.extend(encode_string_field(5, "阿里"));
            buf
        };
        let mut payload = encode_varint_field(1, 0);
        payload.extend(encode_string_field(2, ""));
        payload.extend(encode_bytes_field(3, &member));
        payload.extend(encode_bytes_field(3, &member));
        payload.extend(encode_varint_field(4, 2));
        payload.extend(encode_varint_field(5, 1));
        let rsp = decode_get_group_member_list_rsp(&payload);
        assert_eq!(rsp.code, 0);
        assert_eq!(rsp.members.len(), 2);
        assert_eq!(rsp.members[0].user_id, "u1");
        assert_eq!(rsp.members[0].nickname, "Alice");
        assert_eq!(rsp.members[0].role, 2);
        assert_eq!(rsp.members[0].name_card, "阿里");
        assert_eq!(rsp.next_offset, 2);
        assert!(rsp.is_complete);
    }

    #[test]
    fn msg_content_zero_index_omitted_like_hermes() {
        // hermes `_encode_msg_content` skips zero varints, so catalog
        // stickers (index=0) put only the data JSON on the wire.
        let element = MsgBodyElement {
            msg_type: "TIMFaceElem".into(),
            msg_content: MsgContent {
                data: "{\"sticker_id\":\"225\"}".into(),
                ..Default::default()
            },
        };
        let content_bytes = encode_msg_content(&element.msg_content);
        let decoded = decode_msg_content(&content_bytes);
        assert_eq!(decoded.index, 0);
        assert_eq!(decoded.data, "{\"sticker_id\":\"225\"}");
        // Only the data field (tag 0x22 + len + payload) may be on the
        // wire — no field-9 varint present.
        assert_eq!(content_bytes.len(), 2 + decoded.data.len());
    }
}

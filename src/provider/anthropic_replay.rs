//! Anthropic thinking-block replay: capture carriers, rebuild assistant turns,
//! and apply the per-route signature policy.
//!
//! Verbatim port of the replay half of `agent/anthropic_message_convert.py`
//! (hermes v0.22.2) plus the capture half of
//! `agent/transports/anthropic.py::normalize_response`.
//!
//! WHY THIS EXISTS: Anthropic signs each `thinking` block against the blocks
//! PRECEDING it. Replaying an assistant turn without its `signature` — or with
//! the blocks in the wrong order — is HTTP 400 ("Invalid signature in thinking
//! block"). Two carriers exist upstream and both are ported here:
//!
//! * `reasoning_details` — the thinking/redacted_thinking blocks, persisted.
//! * `anthropic_content_blocks` — the FULL ordered block list, stored only when
//!   signed thinking interleaves with `tool_use` (the one shape the parallel
//!   lists reconstruct wrongly). Runtime-only upstream: there is no DB column
//!   for it, so a reloaded session falls back to `reasoning_details`.
//!
//! 🔴 Whitelist, not blacklist: SDK response blocks carry output-only fields the
//! INPUT schema forbids (`parsed_output`, `caller`, `citations=None`). Sanitizing
//! at CAPTURE is what keeps them from ever being persisted and replayed.

use crate::provider::anthropic_thinking::{anthropic_thinking_route, ThinkingRoute};
use crate::provider::{Message, ToolCall};
use serde_json::{json, Value};

/// hermes `_THINKING_TYPES` (`anthropic_message_convert.py:18`).
pub const THINKING_TYPES: &[&str] = &["thinking", "redacted_thinking"];

/// hermes `_CACHEABLE_TYPES` (`:19`) — the block types a cache breakpoint may
/// ride on. Thinking blocks are deliberately NOT cacheable: `cache_control` on
/// them interferes with signature validation.
const CACHEABLE_TYPES: &[&str] = &["text", "tool_use"];

/// hermes `_EMPTY_TEXT_PLACEHOLDER` — Anthropic rejects empty text blocks.
const EMPTY_TEXT_PLACEHOLDER: &str = "(empty)";

/// hermes `_block_type`: the `type` field as a trimmed lowercase string.
pub fn block_type(b: &Value) -> String {
    b.get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn is_thinking(b: &Value) -> bool {
    THINKING_TYPES.contains(&block_type(b).as_str())
}

/// Non-empty string field, else `None`. Upstream truthiness on `b.get("signature")`
/// treats `""`/`None` alike as absent; a non-string is not a usable signature.
fn nonempty_str(b: &Value, key: &str) -> Option<String> {
    b.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// hermes `_cache_control_of` — only a dict-valued marker counts.
fn cache_control_of(b: &Value) -> Option<Value> {
    b.get("cache_control")
        .filter(|v| v.is_object())
        .cloned()
}

/// hermes `_carry_cache_control`.
fn carry_cache_control(out: &mut Value, b: &Value) {
    if let Some(cc) = cache_control_of(b) {
        out["cache_control"] = cc;
    }
}

/// hermes `_is_blank_text_block` (`:34-37`) — Anthropic 400s on text blocks whose
/// text is not a non-whitespace string.
fn is_blank_text_block(b: &Value) -> bool {
    block_type(b) == "text"
        && !b
            .get("text")
            .and_then(|t| t.as_str())
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false)
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// hermes `_replay_thinking` (`:293-295`): the signature rides along ONLY when
/// non-empty. `thinking` defaults to `""` (upstream `b.get("thinking", "")`).
fn replay_thinking(b: &Value) -> Value {
    let mut out = json!({
        "type": "thinking",
        "thinking": b.get("thinking").and_then(|t| t.as_str()).unwrap_or(""),
    });
    if let Some(sig) = nonempty_str(b, "signature") {
        out["signature"] = json!(sig);
    }
    out
}

/// hermes `_replay_redacted_thinking` (`:298-299`): `data` IS the signature for a
/// redacted block; without it the block is unusable and dropped.
fn replay_redacted_thinking(b: &Value) -> Option<Value> {
    nonempty_str(b, "data").map(|data| json!({"type": "redacted_thinking", "data": data}))
}

/// hermes `_sanitize_tool_id` (`:115-117`): Anthropic requires ids matching
/// `[a-zA-Z0-9_-]`; replace the rest, never empty.
fn sanitize_tool_id(tool_id: &str) -> String {
    if tool_id.is_empty() {
        return String::new();
    }
    let cleaned: String = tool_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "tool_0".to_string()
    } else {
        cleaned
    }
}

/// hermes `_replay_tool_use`.
fn replay_tool_use(b: &Value) -> Value {
    let mut out = json!({
        "type": "tool_use",
        "id": sanitize_tool_id(b.get("id").and_then(|v| v.as_str()).unwrap_or("")),
        "name": b.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        "input": b.get("input").cloned().unwrap_or_else(|| json!({})),
    });
    carry_cache_control(&mut out, b);
    out
}

/// hermes `_sanitize_replay_block` (`:317-325`) — per-type whitelist so future
/// SDK fields cannot reintroduce the "Extra inputs are not permitted" 400.
/// Unknown types are dropped.
pub fn sanitize_replay_block(b: &Value) -> Option<Value> {
    if !b.is_object() {
        return None;
    }
    match block_type(b).as_str() {
        "text" => {
            if is_blank_text_block(b) {
                return None;
            }
            let mut out = text_block(b.get("text").and_then(|t| t.as_str()).unwrap_or(""));
            // hermes `_text_block_with_citations`: citations ride along ONLY as a
            // non-empty list (the only input-valid shape). `citations: null` and
            // `citations: []` from the SDK must NOT be forwarded.
            if let Some(c) = b.get("citations").and_then(|c| c.as_array()) {
                if !c.is_empty() {
                    out["citations"] = Value::Array(c.clone());
                }
            }
            carry_cache_control(&mut out, b);
            Some(out)
        }
        "thinking" => Some(replay_thinking(b)),
        "redacted_thinking" => replay_redacted_thinking(b),
        "tool_use" => Some(replay_tool_use(b)),
        "image" => b
            .get("source")
            .filter(|s| s.is_object())
            .map(|src| json!({"type": "image", "source": src})),
        _ => None,
    }
}

/// The carriers `normalize_response` derives from a response's content blocks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThinkingCarriers {
    /// `provider_data["reasoning_details"]` — persisted.
    pub reasoning_details: Option<Vec<Value>>,
    /// `provider_data["anthropic_content_blocks"]` — runtime-only ordered sidecar.
    pub anthropic_content_blocks: Option<Vec<Value>>,
    /// `reasoning` — the readable mirror (`"\n\n".join(thinking parts)`).
    pub reasoning: Option<String>,
}

/// hermes `AnthropicTransport.normalize_response`
/// (`agent/transports/anthropic.py:65-106`), carrier half.
///
/// 🔴 The ordered channel is emitted ONLY for the shape the parallel lists
/// reconstruct wrongly: SIGNED thinking interleaved with `tool_use`. Emitting it
/// always would pin every turn to a verbatim block list and lose the
/// tool-argument redaction that the normal path applies.
pub fn thinking_carriers_from_blocks(blocks: &[Value]) -> ThinkingCarriers {
    let mut ordered: Vec<Value> = Vec::new();
    let mut details: Vec<Value> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    for b in blocks {
        let clean = sanitize_replay_block(b);
        if let Some(ref c) = clean {
            ordered.push(c.clone());
        }
        match block_type(b).as_str() {
            "thinking" => {
                if let Some(t) = b.get("thinking").and_then(|t| t.as_str()) {
                    reasoning_parts.push(t.to_string());
                }
                // raw only if sanitize dropped it (upstream `detail` fallback)
                details.push(clean.clone().unwrap_or_else(|| b.clone()));
            }
            "redacted_thinking" => {
                details.push(clean.clone().unwrap_or_else(|| b.clone()));
            }
            _ => {}
        }
    }
    let signed = ordered
        .iter()
        .any(|b| is_thinking(b) && (nonempty_str(b, "signature").is_some() || nonempty_str(b, "data").is_some()));
    let has_tool_use = ordered.iter().any(|b| block_type(b) == "tool_use");
    ThinkingCarriers {
        reasoning_details: if details.is_empty() { None } else { Some(details) },
        anthropic_content_blocks: if signed && has_tool_use {
            Some(ordered)
        } else {
            None
        },
        reasoning: if reasoning_parts.is_empty() {
            None
        } else {
            Some(reasoning_parts.join("\n\n"))
        },
    }
}

/// hermes `_extract_preserved_thinking_blocks` (`:242-251`) — deep-copied
/// thinking blocks out of `reasoning_details`, no sanitizing (capture already did).
pub fn extract_preserved_thinking_blocks(message: &Message) -> Vec<Value> {
    message
        .reasoning_details
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|d| d.is_object() && is_thinking(d))
        .cloned()
        .collect()
}

/// hermes `_apply_assistant_cache_control_to_last_cacheable_block`.
fn apply_cache_control_to_last_cacheable(blocks: &mut [Value], cc: Option<Value>) {
    let Some(cc) = cc.filter(|c| c.is_object()) else {
        return;
    };
    for b in blocks.iter_mut().rev() {
        if CACHEABLE_TYPES.contains(&block_type(b).as_str()) {
            if b.get("cache_control").is_none() {
                b["cache_control"] = cc;
            }
            break;
        }
    }
}

fn has_block_type(blocks: &[Value], types: &[&str]) -> bool {
    blocks.iter().any(|b| types.contains(&block_type(b).as_str()))
}

/// hermes `_replay_ordered_blocks` (`:338-378`).
///
/// 🔴 `tool_use.input` is RE-SOURCED from `tool_calls` (redacted at storage time),
/// never from the captured block (raw API response, NOT redacted) — otherwise a
/// secret the model inlined into a tool call goes back on the wire.
fn replay_ordered_blocks(message: &Message, ordered_blocks: &[Value]) -> Option<Vec<Value>> {
    let mut replayed: Vec<Value> = Vec::new();
    let mut relocated_cc: Option<Value> = None;
    let mut dropped_blank_text = false;
    for b in ordered_blocks {
        let Some(mut clean) = sanitize_replay_block(b) else {
            dropped_blank_text = dropped_blank_text || block_type(b) == "text";
            if let Some(cc) = cache_control_of(b) {
                relocated_cc = Some(cc);
            }
            continue;
        };
        if block_type(&clean) == "tool_use" {
            let id = clean.get("id").and_then(|v| v.as_str()).unwrap_or("");
            // Match against the sanitized tool_call id (upstream builds
            // `redacted_input_by_id` with `_sanitize_tool_id`).
            if let Some(tc) = message
                .tool_calls
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .find(|tc| sanitize_tool_id(&tc.id) == id)
            {
                clean["input"] = parse_tool_args(&tc.function.arguments);
            }
        }
        replayed.push(clean);
    }
    if !has_block_type(&replayed, CACHEABLE_TYPES)
        && (dropped_blank_text || relocated_cc.is_some())
    {
        replayed.push(text_block(EMPTY_TEXT_PLACEHOLDER));
    }
    if replayed.is_empty() {
        return None;
    }
    apply_cache_control_to_last_cacheable(&mut replayed, relocated_cc);
    Some(replayed)
}

/// hermes `_parse_tool_args` — malformed arguments become `{}` rather than a 400.
fn parse_tool_args(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
}

/// hermes `assistant_replay_carrier` (`:380-389`) — single owner of carrier
/// precedence: the ordered sidecar when anything survives it, else the
/// `reasoning_details` thinking blocks.
pub fn assistant_replay_carrier(message: &Message) -> (bool, Vec<Value>) {
    if let Some(ordered) = message.anthropic_content_blocks.as_deref() {
        if !ordered.is_empty() {
            if let Some(replayed) = replay_ordered_blocks(message, ordered) {
                return (true, replayed);
            }
        }
    }
    (false, extract_preserved_thinking_blocks(message))
}

/// hermes `_convert_assistant_message` (`:392-434`), block-building half.
/// Cache-marker relocation is handled by the caller's prompt-caching pass.
pub fn assistant_blocks(message: &Message, tool_calls: Option<&[ToolCall]>) -> Vec<Value> {
    let (ordered, mut blocks) = assistant_replay_carrier(message);
    if ordered {
        return blocks;
    }
    if let Some(content) = message.content.as_deref() {
        if !content.trim().is_empty() {
            blocks.push(text_block(content));
        }
    }
    for tc in tool_calls.unwrap_or(&[]) {
        blocks.push(json!({
            "type": "tool_use",
            "id": tc.id,
            "name": tc.function.name,
            "input": parse_tool_args(&tc.function.arguments),
        }));
    }
    // Kimi's /coding endpoint requires reasoning_content on replayed tool-call
    // turns — even "". Prepend (thinking must precede text/tool_use). SKIP when
    // signed thinking blocks already came from the carrier: a duplicate unsigned
    // one would be demoted to a spurious text block. hermes-agent#13848.
    if !has_block_type(&blocks, THINKING_TYPES) {
        if let Some(reasoning) = message.reasoning.as_deref() {
            blocks.insert(0, json!({"type": "thinking", "thinking": reasoning}));
        }
    }
    if blocks.is_empty() {
        blocks.push(text_block(EMPTY_TEXT_PLACEHOLDER));
    }
    blocks
}

/// hermes `_strip_thinking` (`:65-66`).
fn strip_thinking(blocks: &[Value]) -> Vec<Value> {
    blocks.iter().filter(|b| !is_thinking(b)).cloned().collect()
}

/// hermes `_keep_valid_thinking` (`:555-570`): keep valid signed thinking, demote
/// readable unsigned/invalidated thinking to plain text. A dataless redacted
/// block is dropped (there is nothing readable to demote).
fn keep_valid_thinking(blocks: &[Value], signature_dead: bool) -> Vec<Value> {
    let mut out = Vec::new();
    for b in blocks {
        if !is_thinking(b) {
            out.push(b.clone());
            continue;
        }
        let is_redacted = block_type(b) == "redacted_thinking";
        // redacted 'data' IS the signature
        let signed = if is_redacted {
            nonempty_str(b, "data")
        } else {
            nonempty_str(b, "signature")
        };
        if signed.is_some() && !signature_dead {
            out.push(b.clone());
        } else if (signature_dead || !is_redacted)
            && b.get("thinking")
                .and_then(|t| t.as_str())
                .map(|t| !t.is_empty())
                .unwrap_or(false)
        {
            out.push(text_block(b["thinking"].as_str().unwrap_or("")));
        }
    }
    out
}

/// hermes `_current_tool_loop_start` (`:573-584`): index AFTER the last user
/// message carrying no tool result. Every assistant turn from there on belongs
/// to the in-flight tool loop, whose thinking Anthropic requires back.
fn current_tool_loop_start(result: &[Value]) -> usize {
    for i in (0..result.len()).rev() {
        let m = &result[i];
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let has_tool_result = m
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| blocks.iter().any(|b| block_type(b) == "tool_result"))
            .unwrap_or(false);
        if !has_tool_result {
            return i + 1;
        }
    }
    0
}

/// hermes `_manage_thinking_signatures` (`:587-625`). Mutates `result` in place.
///
/// 🔴 The cut is at the START OF THE IN-FLIGHT TOOL LOOP, not at the latest
/// assistant message: cutting later broke the loop's earlier steps, because the
/// API requires those turns' thinking back and strips older turns itself. The
/// latest assistant turn always keeps its thinking even once a new user message
/// closed its loop — hence the `min` with the last assistant index.
pub fn manage_thinking_signatures(result: &mut [Value], base_url: &str, model: &str) {
    let route = anthropic_thinking_route(base_url, model);
    let last_assistant = result
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        .unwrap_or(result.len());
    let loop_start = current_tool_loop_start(result).min(last_assistant);
    let preserve_prior = route.replays_prior_thinking(model);

    for idx in 0..result.len() {
        if result[idx].get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = result[idx].get("content").and_then(|c| c.as_array()).cloned() else {
            continue;
        };
        let new_content: Vec<Value> = match route {
            // Kimi replays as-is; the shared cleanup below still strips markers.
            ThinkingRoute::Kimi => content,
            // DeepSeek needs unsigned blocks round-tripped but rejects signed ones.
            ThinkingRoute::DeepSeek => {
                let kept: Vec<Value> = content
                    .into_iter()
                    .filter(|b| {
                        !is_thinking(b)
                            || (nonempty_str(b, "signature").is_none()
                                && nonempty_str(b, "data").is_none())
                    })
                    .collect();
                if kept.is_empty() {
                    vec![text_block(EMPTY_TEXT_PLACEHOLDER)]
                } else {
                    kept
                }
            }
            _ => {
                if route == ThinkingRoute::ThirdParty || (idx < loop_start && !preserve_prior) {
                    let kept = strip_thinking(&content);
                    if kept.is_empty() {
                        vec![text_block("(thinking elided)")]
                    } else {
                        kept
                    }
                } else {
                    // `_thinking_signature_invalidated` is the 400-healing flag
                    // (slice2f); no producer yet, so it reads false here.
                    let kept = keep_valid_thinking(&content, false);
                    if kept.is_empty() {
                        vec![text_block(EMPTY_TEXT_PLACEHOLDER)]
                    } else {
                        kept
                    }
                }
            }
        };
        let mut new_content = new_content;
        // cache_control on thinking blocks interferes with signature validation.
        for b in new_content.iter_mut() {
            if is_thinking(b) {
                if let Some(obj) = b.as_object_mut() {
                    obj.remove("cache_control");
                }
            }
        }
        result[idx]["content"] = Value::Array(new_content);
    }
}

/// Streaming block accumulator — hermes `AnthropicStreamAccumulator`
/// (`agent/relay_llm.py:603-669`), narrowed to the replay carriers.
///
/// 🔴 The signature is NOT in `content_block_start`; it arrives as one or more
/// `signature_delta` fragments that must be APPENDED to the block's `signature`
/// field (`_ANTHROPIC_APPEND_DELTAS`). Taking the last fragment, or ignoring the
/// event, yields a truncated/absent signature and the next turn 400s.
///
/// Blocks are keyed by the wire `index` and emitted in index order at finalize,
/// so interleaved thinking/text/tool_use keeps the order Anthropic signed.
#[derive(Debug, Default)]
pub struct StreamBlockAccumulator {
    blocks: std::collections::BTreeMap<usize, Value>,
    /// Index of the currently open text block (deltas carry no index).
    current_text: Option<usize>,
    /// Index of the currently open thinking block (`thinking_delta` has no index).
    current_thinking: Option<usize>,
}

impl StreamBlockAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    fn append_field(&mut self, index: usize, field: &str, fragment: &str) {
        let block = self.blocks.entry(index).or_insert_with(|| json!({}));
        let existing = block.get(field).and_then(|v| v.as_str()).unwrap_or("");
        block[field] = json!(format!("{}{}", existing, fragment));
    }

    /// Feed one parsed chunk. Non-Anthropic chunks are a no-op.
    pub fn observe(&mut self, chunk: &crate::provider::StreamChunk) {
        if let Some((index, block)) = &chunk.thinking_block_start {
            self.blocks.insert(*index, block.clone());
            self.current_thinking = Some(*index);
            self.current_text = None;
        }
        if let Some((index, text)) = &chunk.text_block_start {
            self.blocks.insert(*index, text_block(text));
            self.current_text = Some(*index);
            self.current_thinking = None;
        }
        if let Some((index, signature)) = &chunk.signature_delta {
            self.append_field(*index, "signature", signature);
        }
        if let Some(delta) = &chunk.delta_reasoning {
            if let Some(index) = self.current_thinking {
                self.append_field(index, "thinking", delta);
            }
        }
        if let Some(delta) = &chunk.delta_content {
            if let Some(index) = self.current_text {
                self.append_field(index, "text", delta);
            }
        }
        for delta in &chunk.tool_call_deltas {
            self.current_text = None;
            self.current_thinking = None;
            let block = self
                .blocks
                .entry(delta.index)
                .or_insert_with(|| json!({"type": "tool_use", "input": {}}));
            block["type"] = json!("tool_use");
            if let Some(id) = &delta.id {
                block["id"] = json!(id);
            }
            if let Some(name) = &delta.name_delta {
                let existing = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                block["name"] = json!(format!("{}{}", existing, name));
            }
            if let Some(args) = &delta.arguments_delta {
                let existing = block
                    .get("_partial_json")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                block["_partial_json"] = json!(format!("{}{}", existing, args));
            }
        }
    }

    /// Ordered block list (hermes `finalize`): `_partial_json` becomes `input`.
    pub fn blocks(&self) -> Vec<Value> {
        self.blocks
            .values()
            .map(|b| {
                let mut b = b.clone();
                let partial = b
                    .get("_partial_json")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                if let Some(partial) = partial {
                    if let Some(obj) = b.as_object_mut() {
                        obj.remove("_partial_json");
                    }
                    b["input"] = parse_tool_args(&partial);
                }
                b
            })
            .collect()
    }

    /// Carriers for the finished turn, via the same capture path as non-streaming.
    pub fn carriers(&self) -> ThinkingCarriers {
        thinking_carriers_from_blocks(&self.blocks())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, Role};

    fn assistant() -> Message {
        Message {
            role: Role::Assistant,
            content: Some("answer".into()),
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_details: None,
            anthropic_content_blocks: None,
        }
    }

    fn signed_thinking(text: &str, sig: &str) -> Value {
        json!({"type": "thinking", "thinking": text, "signature": sig})
    }

    // ---- capture -----------------------------------------------------------

    #[test]
    fn sanitize_whitelists_and_drops_output_only_fields() {
        let b = json!({
            "type": "thinking", "thinking": "hmm", "signature": "sig",
            "parsed_output": {"x": 1}, "caller": "sdk"
        });
        let clean = sanitize_replay_block(&b).unwrap();
        assert_eq!(clean, json!({"type":"thinking","thinking":"hmm","signature":"sig"}));
        // blank text and unknown types are dropped, not coerced
        assert!(sanitize_replay_block(&json!({"type":"text","text":"   "})).is_none());
        assert!(sanitize_replay_block(&json!({"type":"server_tool_use"})).is_none());
        // citations: null must not be forwarded (input schema forbids it)
        let t = sanitize_replay_block(&json!({"type":"text","text":"hi","citations":null})).unwrap();
        assert!(t.get("citations").is_none());
    }

    #[test]
    fn empty_signature_is_not_replayed() {
        let b = json!({"type":"thinking","thinking":"hmm","signature":""});
        assert_eq!(
            sanitize_replay_block(&b).unwrap(),
            json!({"type":"thinking","thinking":"hmm"})
        );
    }

    #[test]
    fn dataless_redacted_thinking_is_dropped_but_still_captured_raw() {
        assert!(sanitize_replay_block(&json!({"type":"redacted_thinking"})).is_none());
        let carriers = thinking_carriers_from_blocks(&[json!({"type":"redacted_thinking"})]);
        // upstream `detail = clean if clean is not None else block_dict`
        assert_eq!(carriers.reasoning_details.unwrap().len(), 1);
        assert!(carriers.anthropic_content_blocks.is_none());
    }

    #[test]
    fn ordered_sidecar_only_for_signed_thinking_interleaved_with_tool_use() {
        let tool = json!({"type":"tool_use","id":"t1","name":"terminal","input":{"command":"ls"}});
        // signed thinking + tool_use -> sidecar
        let c = thinking_carriers_from_blocks(&[signed_thinking("a", "s"), tool.clone()]);
        assert!(c.anthropic_content_blocks.is_some());
        assert_eq!(c.reasoning.as_deref(), Some("a"));
        // signed thinking WITHOUT tool_use -> no sidecar (parallel lists are fine)
        let c = thinking_carriers_from_blocks(&[signed_thinking("a", "s"), text_block("hi")]);
        assert!(c.anthropic_content_blocks.is_none());
        assert!(c.reasoning_details.is_some());
        // UNSIGNED thinking + tool_use -> no sidecar
        let c = thinking_carriers_from_blocks(&[
            json!({"type":"thinking","thinking":"a"}),
            tool.clone(),
        ]);
        assert!(c.anthropic_content_blocks.is_none());
    }

    #[test]
    fn reasoning_mirror_joins_with_blank_line() {
        let c = thinking_carriers_from_blocks(&[
            signed_thinking("first", "s1"),
            signed_thinking("second", "s2"),
        ]);
        assert_eq!(c.reasoning.as_deref(), Some("first\n\nsecond"));
    }

    // ---- replay ------------------------------------------------------------

    #[test]
    fn carrier_precedence_prefers_ordered_sidecar() {
        let mut m = assistant();
        m.reasoning_details = Some(vec![signed_thinking("from-details", "s")]);
        m.anthropic_content_blocks = Some(vec![
            signed_thinking("from-sidecar", "s"),
            json!({"type":"tool_use","id":"t1","name":"terminal","input":{}}),
        ]);
        let (ordered, blocks) = assistant_replay_carrier(&m);
        assert!(ordered);
        assert_eq!(blocks[0]["thinking"], "from-sidecar");
    }

    #[test]
    fn ordered_replay_resources_tool_input_from_redacted_tool_calls() {
        let mut m = assistant();
        m.tool_calls = Some(vec![ToolCall {
            id: "t1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "terminal".into(),
                arguments: r#"{"command":"echo REDACTED"}"#.into(),
            },
        }]);
        m.anthropic_content_blocks = Some(vec![
            signed_thinking("a", "s"),
            // captured block holds the RAW (unredacted) input
            json!({"type":"tool_use","id":"t1","name":"terminal","input":{"command":"echo secret"}}),
        ]);
        let (ordered, blocks) = assistant_replay_carrier(&m);
        assert!(ordered);
        assert_eq!(blocks[1]["input"]["command"], "echo REDACTED");
    }

    #[test]
    fn thinking_precedes_text_and_tool_use_on_replay() {
        let mut m = assistant();
        m.reasoning_details = Some(vec![signed_thinking("ponder", "sig")]);
        m.tool_calls = Some(vec![ToolCall {
            id: "t1".into(),
            call_type: "function".into(),
            function: FunctionCall { name: "terminal".into(), arguments: "{}".into() },
        }]);
        let blocks = assistant_blocks(&m, m.tool_calls.as_deref());
        assert_eq!(block_type(&blocks[0]), "thinking");
        assert_eq!(blocks[0]["signature"], "sig");
        assert_eq!(block_type(&blocks[1]), "text");
        assert_eq!(block_type(&blocks[2]), "tool_use");
    }

    #[test]
    fn unsigned_reasoning_pad_injected_only_without_thinking_carrier() {
        let mut m = assistant();
        m.reasoning = Some("readable thought".into());
        let blocks = assistant_blocks(&m, None);
        assert_eq!(block_type(&blocks[0]), "thinking");
        assert!(blocks[0].get("signature").is_none());
        // with a signed carrier present, no duplicate unsigned block
        m.reasoning_details = Some(vec![signed_thinking("signed", "s")]);
        let blocks = assistant_blocks(&m, None);
        assert_eq!(blocks.iter().filter(|b| is_thinking(b)).count(), 1);
    }

    #[test]
    fn empty_assistant_turn_gets_placeholder() {
        let mut m = assistant();
        m.content = Some("   ".into());
        let blocks = assistant_blocks(&m, None);
        assert_eq!(blocks, vec![text_block(EMPTY_TEXT_PLACEHOLDER)]);
    }

    // ---- route policy ------------------------------------------------------

    fn wire(blocks: Vec<Value>) -> Value {
        json!({"role": "assistant", "content": blocks})
    }

    #[test]
    fn third_party_relay_strips_all_thinking() {
        let mut result = vec![
            json!({"role":"user","content":[text_block("q")]}),
            wire(vec![signed_thinking("a", "s"), text_block("answer")]),
        ];
        manage_thinking_signatures(&mut result, "https://relay.example.com/v1", "claude-opus-4-6");
        let blocks = result[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(block_type(&blocks[0]), "text");
    }

    #[test]
    fn thinking_only_turn_on_stripping_route_gets_elided_placeholder() {
        let mut result = vec![wire(vec![signed_thinking("a", "s")])];
        manage_thinking_signatures(&mut result, "https://relay.example.com", "claude-opus-4-6");
        assert_eq!(result[0]["content"][0]["text"], "(thinking elided)");
    }

    #[test]
    fn native_keeps_signed_thinking_inside_the_tool_loop() {
        // user question, assistant(thinking+tool_use), user(tool_result),
        // assistant(thinking+text): both assistant turns are in-flight loop.
        let mut result = vec![
            json!({"role":"user","content":[text_block("q")]}),
            wire(vec![signed_thinking("step1", "s1"), json!({"type":"tool_use","id":"t","name":"n","input":{}})]),
            json!({"role":"user","content":[json!({"type":"tool_result","tool_use_id":"t","content":"ok"})]}),
            wire(vec![signed_thinking("step2", "s2"), text_block("done")]),
        ];
        manage_thinking_signatures(&mut result, "https://api.anthropic.com", "claude-opus-4-6");
        assert_eq!(result[1]["content"][0]["signature"], "s1");
        assert_eq!(result[3]["content"][0]["signature"], "s2");
    }

    #[test]
    fn native_older_model_strips_thinking_before_the_loop_but_keeps_latest() {
        // claude-3-7-sonnet does not preserve prior thinking: the closed loop's
        // turns lose thinking, the latest assistant turn keeps it.
        let mut result = vec![
            json!({"role":"user","content":[text_block("q1")]}),
            wire(vec![signed_thinking("old", "s1"), text_block("a1")]),
            json!({"role":"user","content":[text_block("q2")]}),
            wire(vec![signed_thinking("new", "s2"), text_block("a2")]),
        ];
        manage_thinking_signatures(&mut result, "https://api.anthropic.com", "claude-3-7-sonnet");
        let old = result[1]["content"].as_array().unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(block_type(&old[0]), "text");
        assert_eq!(result[3]["content"][0]["signature"], "s2");
    }

    #[test]
    fn deepseek_keeps_unsigned_and_drops_signed() {
        let mut result = vec![wire(vec![
            signed_thinking("signed", "s"),
            json!({"type":"thinking","thinking":"unsigned"}),
            text_block("answer"),
        ])];
        manage_thinking_signatures(&mut result, "https://api.deepseek.com/anthropic", "deepseek-r1");
        let blocks = result[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["thinking"], "unsigned");
        assert!(blocks[0].get("signature").is_none());
    }

    #[test]
    fn kimi_replays_as_is_but_loses_cache_markers_on_thinking() {
        let mut thinking = signed_thinking("a", "s");
        thinking["cache_control"] = json!({"type":"ephemeral"});
        let mut result = vec![wire(vec![thinking, text_block("answer")])];
        manage_thinking_signatures(&mut result, "https://api.kimi.com/coding", "kimi-k2.5");
        let blocks = result[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["signature"], "s");
        assert!(blocks[0].get("cache_control").is_none());
    }

    #[test]
    fn minimax_replays_prior_thinking_regardless_of_model() {
        let mut result = vec![
            json!({"role":"user","content":[text_block("q1")]}),
            wire(vec![signed_thinking("old", "s1"), text_block("a1")]),
            json!({"role":"user","content":[text_block("q2")]}),
            wire(vec![signed_thinking("new", "s2"), text_block("a2")]),
        ];
        manage_thinking_signatures(&mut result, "https://api.minimaxi.com/anthropic", "MiniMax-M2");
        assert_eq!(result[1]["content"][0]["signature"], "s1");
        assert_eq!(result[3]["content"][0]["signature"], "s2");
    }

    #[test]
    fn unsigned_thinking_is_demoted_to_text_on_signed_routes() {
        let mut result = vec![wire(vec![
            json!({"type":"thinking","thinking":"readable"}),
            text_block("answer"),
        ])];
        manage_thinking_signatures(&mut result, "https://api.anthropic.com", "claude-opus-4-6");
        let blocks = result[0]["content"].as_array().unwrap();
        assert_eq!(block_type(&blocks[0]), "text");
        assert_eq!(blocks[0]["text"], "readable");
    }

    #[test]
    fn dataless_redacted_is_dropped_not_demoted() {
        let mut result = vec![wire(vec![
            json!({"type":"redacted_thinking","thinking":"not readable"}),
            text_block("answer"),
        ])];
        manage_thinking_signatures(&mut result, "https://api.anthropic.com", "claude-opus-4-6");
        let blocks = result[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], "answer");
    }
}
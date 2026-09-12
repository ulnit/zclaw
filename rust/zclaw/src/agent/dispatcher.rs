//! Agent dispatcher — the chat-with-tools loop. Emits Chunk events that the
//! FFI layer turns into the poll_chunks JSON stream.

use crate::config::Config;
use crate::memory::MemoryStore;
use crate::providers::compatible::{ChatMessage, Client, StreamEvent};
use crate::tools::{self, ToolCtx};
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Chunk types — must match HarmonyOS ZClawApi.ets switch(chunk.chunkType).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChunkType {
    Text = 0,
    ToolCall = 1,
    ToolResult = 2,
    Done = 3,
    Error = 4,
    Thinking = 5,
}

#[derive(Debug, Clone, Serialize)]
pub struct Chunk {
    #[serde(rename = "chunkType")]
    pub chunk_type: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

impl Chunk {
    pub fn text(s: &str) -> Self { Self { chunk_type: 0, name: Some(s.into()), args: None, result: None } }
    pub fn tool_call(name: &str, args: &str) -> Self { Self { chunk_type: 1, name: Some(name.into()), args: Some(args.into()), result: None } }
    pub fn tool_result(name: &str, result: &str) -> Self { Self { chunk_type: 2, name: Some(name.into()), args: None, result: Some(result.into()) } }
    pub fn done() -> Self { Self { chunk_type: 3, name: None, args: None, result: None } }
    pub fn error(msg: &str) -> Self { Self { chunk_type: 4, name: Some(msg.into()), args: None, result: None } }
    pub fn thinking(s: &str) -> Self { Self { chunk_type: 5, name: Some(s.into()), args: None, result: None } }
}

pub struct Dispatcher {
    pub config: Config,
    pub memory: Arc<MemoryStore>,
    pub cancelled: Arc<AtomicBool>,
    client: Client,
}

impl Dispatcher {
    pub fn new(config: Config, memory: Arc<MemoryStore>, cancelled: Arc<AtomicBool>) -> Self {
        Self { config, memory, cancelled, client: Client::new() }
    }

    /// Run one user turn: stream deltas, execute tool calls, loop until a final
    /// answer or max_iterations. Emits chunks via `emit`.
    pub async fn run_turn(
        &self,
        session_id: &str,
        user_text: &str,
        emit: &(dyn Fn(Chunk) + Send + Sync),
    ) {
        let ws = std::path::PathBuf::from(&self.config.workspace_dir);
        let ctx = ToolCtx { workspace: ws, memory: &self.memory };

        let mut history: Vec<ChatMessage> = vec![
            ChatMessage::system(&self.config.agent.system_prompt),
        ];
        // #9490: trim history at complete-turn boundaries only — keep whole
        // user/assistant turns, never a dangling half-turn.
        let prior = self.memory.list_messages(session_id);
        let mut kept: Vec<&crate::memory::Message> = prior.iter().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
        while !kept.is_empty() && kept[0].role != "user" {
            kept.remove(0);
        }
        for m in kept {
            if m.role == "user" {
                history.push(ChatMessage::user(&m.content));
            } else {
                history.push(ChatMessage::assistant(&m.content));
            }
        }
        // Multimodal FFI envelope. The image is sent only in this request and is
        // intentionally not persisted to SQLite.
        let envelope = serde_json::from_str::<Value>(user_text).ok()
            .filter(|v| v["__zclaw_multimodal"].as_bool() == Some(true));
        let (persisted_text, user_message) = if let Some(v) = envelope {
            let text = v["text"].as_str().unwrap_or("");
            let image = v["image_data_url"].as_str().unwrap_or("");
            if image.starts_with("data:image/") {
                let persisted = if text.trim().is_empty() { "[图片]".to_string() } else { format!("{}\n[图片]", text) };
                (persisted, ChatMessage::user_multimodal(text, image))
            } else {
                (text.to_string(), ChatMessage::user(text))
            }
        } else {
            (user_text.to_string(), ChatMessage::user(user_text))
        };
        history.push(user_message);
        self.memory.save_message(session_id, "user", &persisted_text);

        let max_iter = self.config.agent.max_iterations.max(1);
        let tools = tools::tool_schemas();
        let mut final_answer = String::new();

        for iter in 0..max_iter {
            if self.cancelled.load(Ordering::SeqCst) {
                emit(Chunk::error("cancelled"));
                return;
            }

            // 🔴 最后一轮强制收口：不再提供工具，模型只能用已收集到的信息作答。
            //    没有这一步时，循环耗尽后 final_answer 仍为空 → 只发 done，
            //    用户看到一个「全是工具重试叙述、没有任何结论」的空气泡
            //    （实测：思考流 557 字连续 "Let me try more specific searches…"
            //    直到 10 轮用尽，发送按钮灰着，零答案）。
            //    典型成因：搜索引擎对冷门本地 POI 静默降级返回泛化结果，
            //    模型误判「没查到」而无限换措辞重试。
            let is_last = iter + 1 == max_iter;
            let tools_arg = if is_last { None } else { Some(tools.clone()) };
            if is_last {
                history.push(ChatMessage::user(
                    "（系统提示：已达到本轮工具调用上限，不能再调用任何工具。\
                     请立刻基于目前已获得的信息直接回答用户的问题；\
                     如果信息不足，就如实说明查到了什么、没查到什么，\
                     并给出你能给的最佳判断或下一步建议。不要再尝试检索。）"
                ));
            }

            let result = self.client.stream_chat(
                &self.config.chat_url(),
                &self.config.api_key,
                &self.config.default_model,
                self.config.temperature,
                &history,
                tools_arg,
                &|ev| match ev {
                    // #9007: avoid duplicate streamed narration — suppress
                    // empty and byte-identical consecutive deltas.
                    StreamEvent::Delta(t) => emit(Chunk::text(&t)),
                    StreamEvent::Thinking(t) => emit(Chunk::thinking(&t)),
                    StreamEvent::ToolCall { name, arguments, .. } => {
                        emit(Chunk::tool_call(&name, &arguments));
                    }
                    StreamEvent::Done { .. } => {}
                    StreamEvent::Error(e) => emit(Chunk::error(&e)),
                },
            ).await;

            let (content, tool_calls) = match result {
                Ok(parts) => parts,
                Err(e) => {
                    emit(Chunk::error(&e.to_string()));
                    return;
                }
            };

            // Extract tool calls from the final value
            let call_list: Vec<(String, String, String)> = tool_calls
                .as_ref()
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|tc| {
                            let id = tc["id"].as_str().unwrap_or_default().to_string();
                            let name = tc["function"]["name"].as_str().unwrap_or_default().to_string();
                            let args = tc["function"]["arguments"].as_str().unwrap_or_default().to_string();
                            (id, name, args)
                        })
                        .collect()
                })
                .unwrap_or_default();

            // No tool calls → final answer
            if call_list.is_empty() {
                final_answer = content;
                break;
            }

            // Record assistant message with tool_calls, then execute each tool
            history.push(ChatMessage::assistant_with_tools(&content, tool_calls.unwrap()));

            for (id, name, arguments) in &call_list {
                if self.cancelled.load(Ordering::SeqCst) {
                    emit(Chunk::error("cancelled"));
                    return;
                }
                let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Object(Default::default()));
                let result_text = tools::execute(&ctx, name, &args);
                emit(Chunk::tool_result(name, &result_text));
                history.push(ChatMessage::tool(id, &result_text));
            }
        }

        // 最终兜底：正常路径下上面的「收口轮」会产出答案。但若模型连收口轮都返回
        // 空（上游异常/被截断），绝不能只发 done —— 用户会对着空气泡无从判断。
        // 此时发一条可读的说明文本，并持久化，避免会话里留下一条空 assistant 消息。
        if final_answer.is_empty() {
            let fallback = "抱歉，本轮尝试了多次检索仍未拿到足以回答你问题的信息。\
                            可能是该信息在公开网络上较少，或搜索引擎返回了不相关的结果。\
                            你可以换个说法（例如只给小区名或加上市/区）再问一次，\
                            或补充更多线索，我再帮你查。";
            emit(Chunk::text(fallback));
            final_answer = fallback.to_string();
        }

        if !final_answer.is_empty() {
            self.memory.save_message(session_id, "assistant", &final_answer);
        }
        emit(Chunk::done());
    }
}

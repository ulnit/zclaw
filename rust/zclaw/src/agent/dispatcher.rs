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
        for m in self.memory.list_messages(session_id).iter().rev().take(12).rev() {
            if m.role == "user" {
                history.push(ChatMessage::user(&m.content));
            } else {
                history.push(ChatMessage::assistant(&m.content));
            }
        }
        history.push(ChatMessage::user(user_text));
        self.memory.save_message(session_id, "user", user_text);

        let max_iter = self.config.agent.max_iterations.max(1);
        let tools = tools::tool_schemas();
        let mut final_answer = String::new();

        for _ in 0..max_iter {
            if self.cancelled.load(Ordering::SeqCst) {
                emit(Chunk::error("cancelled"));
                return;
            }

            let result = self.client.stream_chat(
                &self.config.chat_url(),
                &self.config.api_key,
                &self.config.default_model,
                self.config.temperature,
                &history,
                Some(tools.clone()),
                &|ev| match ev {
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

        if !final_answer.is_empty() {
            self.memory.save_message(session_id, "assistant", &final_answer);
        }
        emit(Chunk::done());
    }
}

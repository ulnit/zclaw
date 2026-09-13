//! Chunk 协议 —— 与 zclaw v0.2 逐字节对齐，让 App 端 Kotlin/ArkTS 解析器零改动。
//!
//! App 解析（ZClawNativeApi.kt / ZClawApi.ets）只认这套字段：
//!   chunkType: 0=text, 1=tool_call, 2=tool_result, 3=done, 4=error, 5=thinking
//!   name / args / result（可选，按 kind 取用）
//! 序列化为 JSON 数组经 zclaw_poll_chunks() 返回。
use serde::Serialize;

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
    pub fn text(s: &str) -> Self {
        Self { chunk_type: 0, name: Some(s.into()), args: None, result: None }
    }
    pub fn tool_call(name: &str, args: &str) -> Self {
        Self { chunk_type: 1, name: Some(name.into()), args: Some(args.into()), result: None }
    }
    pub fn tool_result(name: &str, result: &str) -> Self {
        Self { chunk_type: 2, name: Some(name.into()), args: None, result: Some(result.into()) }
    }
    pub fn done() -> Self {
        Self { chunk_type: 3, name: None, args: None, result: None }
    }
    pub fn error(msg: &str) -> Self {
        Self { chunk_type: 4, name: Some(msg.into()), args: None, result: None }
    }
    pub fn thinking(s: &str) -> Self {
        Self { chunk_type: 5, name: Some(s.into()), args: None, result: None }
    }
}

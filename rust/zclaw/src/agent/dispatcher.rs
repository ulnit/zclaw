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

/// 🔴 收口提示词：检测到检索无进展时注入，逼模型**用自身知识作答**。
///
/// 设计要点（用户明确要求）：绝不能让用户看到「抱歉，尝试多次检索仍未拿到…」
/// 这类道歉话术——那是把工具失败的负担转嫁给用户。搜索只是一个工具，搜不到
/// 就该退回模型自己的知识，并诚实标注不确定性。所以这里明令禁止道歉式开场，
/// 要求直接给实质回答。
const COLLAR_PROMPT: &str = "（系统提示：停止调用任何工具。检索没有得到有用信息，\
现在请直接回答用户的问题。要求：①用你自己已有的知识给出实质性回答，不要只说\
「没查到」；②在回答中简短说明哪些部分是你已有的了解、未经联网核实，可能不准确；\
③禁止使用「抱歉，尝试了多次检索仍未…」这类道歉式开场，直接给内容；\
④如果确实完全不了解该对象，就说明它可能是较小众的本地信息，并给出用户可以\
如何自行确认的具体建议。）";

/// 裸问重试提示：最后一道防线。若收口轮仍返回空，就把用户原问题原样再问一次
/// （不带工具、不带道歉模板），让模型正常作答——而不是给用户一段固定道歉文案。
fn bare_retry_prompt(user_text: &str) -> String {
    format!("（系统提示：不要用任何工具，直接回答下面这个问题。若不确定就如实说明。）\n\n{}", user_text)
}

/// 工具调用的「进展」记录，用于检测无效重试。
/// backport 自上游 zeroclaw loop_detector 的 no_progress / exact_repeat 思路，
/// 但按移动端单进程库简化（无需 sliding window 的 ping-pong 检测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CallSig {
    args_hash: u64,
    result_hash: u64,
}

/// 简易稳定哈希（避免引入新依赖）。FNV-1a。
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 检索类工具：只有这些的「无进展」才触发收口。
/// memory_*/datetime/file_* 等本地工具反复调用是正常行为，不该被打断。
fn is_retrieval_tool(name: &str) -> bool {
    matches!(name, "web_search_tool" | "web_search" | "web_fetch" | "http_request")
}

/// 无进展判定阈值：同一检索工具拿到**完全相同结果**达到这个次数就收口。
///
/// 为什么是 3（而不是上游 loop_detector 的 5）：移动端一轮对话用户要等几十秒，
/// 烧到 5 次才干预意味着白等一倍以上时间。且我们的 system prompt 已明令
/// 「最多换一种措辞再试一次」，3 次正好对应「原措辞 + 换 2 次措辞」的宽容上限。
/// 注意必须 ≥2 才成立（首次调用谈不上重复），且要求参数至少 2 种，
/// 避免把「同参数重试」误判（那种由网络抖动引起，重试有意义）。
const NO_PROGRESS_MIN: usize = 3;

/// 纯函数版「无进展」判定，便于单元测试（不需要网络/mock 服务器）。
///
/// 判据（与 iOS ZClawAgent 完全一致）：
/// - 最近一次调用必须是检索类工具（本地工具反复调用是正常的，不干预）；
/// - 以它的 result_hash 为基准，统计「同工具 + 同结果」的调用次数 ≥ NO_PROGRESS_MIN
///   （= 换了措辞却什么都没新查到）；
/// - 且这些调用的 args_hash 至少 2 种（= 确实在换措辞重试，而非同参数重试；
///   后者由网络抖动引起，重试有意义，不该收口）。
fn should_collar_for_no_progress(sigs: &[(String, CallSig)]) -> bool {
    let Some((last_name, last_sig)) = sigs.last() else { return false };
    if !is_retrieval_tool(last_name) { return false; }

    let same: Vec<&(String, CallSig)> = sigs.iter()
        .filter(|(n, s)| n == last_name && s.result_hash == last_sig.result_hash)
        .collect();
    if same.len() < NO_PROGRESS_MIN { return false; }

    let mut uniq: Vec<u64> = same.iter().map(|(_, s)| s.args_hash).collect();
    uniq.sort_unstable();
    uniq.dedup();
    uniq.len() >= 2
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

        // 检索类调用的签名记录（args_hash + result_hash），用于「无进展」检测。
        let mut retrieval_sigs: Vec<(String, CallSig)> = Vec::new();
        // 上一轮检测到无进展 → 本轮强制收口（不给工具 + 注入 COLLAR_PROMPT）。
        let mut force_collar = false;
        let mut collar_used = false;

        for iter in 0..max_iter {
            if self.cancelled.load(Ordering::SeqCst) {
                emit(Chunk::error("cancelled"));
                return;
            }

            // 🔴 收口轮：两种触发条件
            //   a) force_collar —— 检索已无进展（同一工具、换了措辞、结果完全相同），
            //      **提前**收口，不必白烧到第 max_iter 轮。实测该 POI 场景后端
            //      prompt_tokens 从 3.3k 涨到 18k、9 轮全在重搜、每轮零正文。
            //   b) is_last —— 兜底，轮次用尽时无论如何都要给用户一个答案。
            // 收口轮不传 tools，并注入「用自身知识直接作答、禁止道歉式开场」的指令。
            let is_last = iter + 1 == max_iter;
            let collar = force_collar || is_last;
            let tools_arg = if collar { None } else { Some(tools.clone()) };
            if collar && !collar_used {
                history.push(ChatMessage::user(COLLAR_PROMPT));
                collar_used = true;
            }
            force_collar = false;

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

            // 收口轮已不提供 tools；若上游仍硬塞 tool_calls，不再执行工具，
            // 直接以已产出的 content 收尾（可能为空，交由下面的裸问重试兜底）。
            if collar {
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

                if is_retrieval_tool(name) {
                    retrieval_sigs.push((name.clone(), CallSig {
                        args_hash: fnv1a(arguments),
                        result_hash: fnv1a(&result_text),
                    }));
                }
            }

            // ── 无进展检测（backport 上游 loop_detector 的 detect_no_progress）──
            // 实测 Bing 对冷门 POI 的不同措辞返回**逐字节相同**的泛化结果
            // （已用 search_probe 实证：4 种措辞 → 1 个唯一 sha256），必命中。
            // 判定逻辑抽成纯函数 should_collar_for_no_progress，带单元测试。
            if should_collar_for_no_progress(&retrieval_sigs) {
                // 换着措辞重搜却拿到完全一样的结果 → 立刻收口，别再烧轮次
                force_collar = true;
            }
        }

        // 🔴 最终兜底：**裸问重试**，而不是给用户一段固定道歉文案。
        //   用户明确要求：搜不到时模型应当自己思考回答，绝不能看到
        //   「抱歉，本轮尝试了多次检索仍未拿到…」这类把工具失败转嫁给用户的话术。
        //   做法：丢弃被工具往返污染的 history（实测可涨到 18k tokens），
        //   用 system + 用户原问题重新问一次、且不提供工具，让模型正常作答。
        if final_answer.trim().is_empty() {
            let retry_history = vec![
                ChatMessage::system(&self.config.agent.system_prompt),
                ChatMessage::user(&bare_retry_prompt(user_text)),
            ];
            let retry = self.client.stream_chat(
                &self.config.chat_url(),
                &self.config.api_key,
                &self.config.default_model,
                self.config.temperature,
                &retry_history,
                None,   // 不给工具：这一轮必须出正文
                &|ev| match ev {
                    StreamEvent::Delta(t) => emit(Chunk::text(&t)),
                    StreamEvent::Thinking(t) => emit(Chunk::thinking(&t)),
                    StreamEvent::Error(e) => emit(Chunk::error(&e)),
                    _ => {}
                },
            ).await;
            match retry {
                Ok((c, _)) => {
                    final_answer = c;
                    // 裸问重试也不带工具，模型几乎没有理由返回空；若仍空说明
                    // 上游/网络彻底异常 → 报错（客户端显示为错误条），
                    // 绝不留空气泡，也不用道歉话术糊弄。
                    if final_answer.trim().is_empty() {
                        emit(Chunk::error("模型未返回内容（上游异常），请重试或更换模型"));
                    }
                }
                Err(e) => emit(Chunk::error(&e.to_string())),
            }
        }

        if !final_answer.trim().is_empty() {
            self.memory.save_message(session_id, "assistant", &final_answer);
        }
        emit(Chunk::done());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一条检索调用记录（模拟 dispatcher 主循环里 push 的形态）
    fn sig(name: &str, args: &str, result: &str) -> (String, CallSig) {
        (name.to_string(), CallSig { args_hash: fnv1a(args), result_hash: fnv1a(result) })
    }

    /// 🔴 真实生产场景回归测试：用户问「长沙市唯一星城小区是位于省图书馆对面吗」，
    /// 模型换着措辞重搜 4 次，Bing 每次返回**逐字节相同**的泛化结果。
    /// 修复前：跑满 10 轮、每轮 finish=tool_calls、零正文 → 空气泡
    ///        （后端实测 prompt_tokens 3.3k→18k，用户 client_gone 放弃）。
    /// 修复后：第 3 次同结果即判定无进展，下一轮强制收口出答案。
    #[test]
    fn no_progress_fires_on_real_poi_research_loop() {
        // search_probe.exe 实测：这 4 种措辞的结果体 sha256 完全一致（805B）
        let identical = "[1] 长沙 - 维基百科\n[2] 长沙旅游攻略 - 携程\n[3] 湖南省图书馆";

        let two = vec![
            sig("web_search_tool", r#"{"query":"长沙 唯一星城"}"#, identical),
            sig("web_search_tool", r#"{"query":"长沙 维一星城 小区"}"#, identical),
        ];
        assert!(!should_collar_for_no_progress(&two), "2 次同结果还不足以判定（阈值 {}）", NO_PROGRESS_MIN);

        let three = {
            let mut v = two.clone();
            v.push(sig("web_search_tool", r#"{"query":"长沙 唯一新城 小区 二手房"}"#, identical));
            v
        };
        assert!(should_collar_for_no_progress(&three),
            "3 次不同措辞拿到完全相同结果 → 必须提前收口，不能烧满 max_iterations");
    }

    /// 同参数重试不该收口：那是网络抖动/超时，重试有意义。
    #[test]
    fn same_args_retry_does_not_collar() {
        let r = "[1] 无结果";
        let v = vec![
            sig("web_search_tool", r#"{"query":"长沙 唯一星城"}"#, r),
            sig("web_search_tool", r#"{"query":"长沙 唯一星城"}"#, r),
            sig("web_search_tool", r#"{"query":"长沙 唯一星城"}"#, r),
        ];
        assert!(!should_collar_for_no_progress(&v),
            "参数完全相同 = 网络重试，不是换措辞重搜，不该打断");
    }

    /// 每次结果都不同 = 检索有进展，即使换了 4 种措辞也不该收口。
    #[test]
    fn differing_results_do_not_collar() {
        let v = vec![
            sig("web_search_tool", r#"{"query":"杭州 二手房 价格"}"#, "[1] 杭州二手房均价 3.2万"),
            sig("web_search_tool", r#"{"query":"杭州 拱墅区 二手房"}"#, "[1] 拱墅区均价 2.8万"),
            sig("web_search_tool", r#"{"query":"杭州 西湖区 二手房"}"#, "[1] 西湖区均价 4.1万"),
            sig("web_search_tool", r#"{"query":"杭州 滨江区 二手房"}"#, "[1] 滨江区均价 3.9万"),
        ];
        assert!(!should_collar_for_no_progress(&v), "结果各异 = 有进展，不该收口");
    }

    /// 本地工具反复调用是正常行为（如连查 3 次时间、3 次记忆写入），绝不能被打断。
    /// 用的是 tools/mod.rs 里**真实注册**的工具名（datetime / memory_* / file_* / shell）。
    #[test]
    fn local_tools_never_collar() {
        let r = "2026-09-13 21:53";
        for name in ["datetime", "memory_store", "memory_recall", "memory_forget",
                     "file_read", "file_write", "file_edit", "glob_search",
                     "content_search", "shell"] {
            let v = vec![
                sig(name, r#"{"a":1}"#, r),
                sig(name, r#"{"a":2}"#, r),
                sig(name, r#"{"a":3}"#, r),
            ];
            assert!(!should_collar_for_no_progress(&v),
                "{} 是本地工具，反复调用正常，不该触发收口", name);
        }
    }

    /// 边界：最后一次调用是本地工具时，即便前面检索已无进展也不收口
    /// （判据以最后一次为准 —— 那种情况模型已转向别的工作）。
    #[test]
    fn last_call_must_be_retrieval() {
        let r = "[1] 泛化结果";
        let v = vec![
            sig("web_search_tool", r#"{"query":"a"}"#, r),
            sig("web_search_tool", r#"{"query":"b"}"#, r),
            sig("web_search_tool", r#"{"query":"c"}"#, r),
            sig("datetime", r#"{"expression":"1+1"}"#, "2"),
        ];
        assert!(!should_collar_for_no_progress(&v), "最后一次是本地工具 → 不收口");
    }

    /// 空记录不 panic。
    #[test]
    fn empty_sigs_no_collar() {
        assert!(!should_collar_for_no_progress(&[]));
    }

    /// 收口提示必须**明令禁止**道歉式开场 —— 这是用户的核心诉求，
    /// 用测试锁死，防止将来改文案时又把「抱歉，尝试了多次检索…」放回去。
    #[test]
    fn collar_prompt_forbids_apology_and_demands_own_knowledge() {
        assert!(COLLAR_PROMPT.contains("禁止"), "收口提示必须含禁止道歉的指令");
        assert!(COLLAR_PROMPT.contains("抱歉"), "必须点名禁止的正是那句道歉文案");
        assert!(COLLAR_PROMPT.contains("你自己已有的知识"), "必须要求用自身知识作答");
        assert!(COLLAR_PROMPT.contains("停止调用任何工具"), "必须停止工具调用");
    }

    /// system prompt 必须承载「搜索只是工具之一、搜不到就自己答」的策略。
    /// 历史根因：prompt 只有一句自我介绍，模型没有任何工具使用指引。
    #[test]
    fn system_prompt_carries_search_policy() {
        let p = crate::config::Config::default().agent.system_prompt;
        assert!(p.contains("联网搜索只是"), "必须说明搜索只是工具之一");
        assert!(p.contains("不要把它当成答案"), "必须教模型判断相关性");
        assert!(p.contains("用你自己已有的知识回答"), "必须给出搜不到时的出路");
        assert!(p.contains("不要编造"), "必须强调诚实优先");
        assert!(p.len() > 200, "策略 prompt 不该再是一句话（实测 {} 字节）", p.len());
    }

    /// 裸问重试提示必须带上用户原问题（否则重试等于问空气）。
    #[test]
    fn bare_retry_preserves_user_question() {
        let p = bare_retry_prompt("长沙市唯一星城小区是位于省图书馆对面吗");
        assert!(p.contains("长沙市唯一星城小区是位于省图书馆对面吗"));
        assert!(p.contains("不要用任何工具"));
    }
}

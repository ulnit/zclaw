//! mobile-ffi — 把 ulnclaw 引擎包成 zclaw v0.2 C ABI 的 libzclaw.so。
//!
//! 设计目标：App 端 JNI/NAPI 桥与 Kotlin/ArkTS 解析器**零改动**。
//! 符号、chunk 协议、config JSON、sessions/messages JSON 全部与
//! zclaw 0.5.2 逐字段对齐（契约见 chunk.rs 注释）。
//!
//! 架构：ulnclaw 作为 path 依赖（rlib），本 crate 只加移动端外壳：
//!   - ThinkingCapture provider wrapper：抽取 delta_reasoning → thinking chunk
//!     （ulnclaw 的 on_thinking 回调是无参信号，拿不到文本）
//!   - 移动端安全工具子集 + Bing 覆盖 web_search（境内 ddg/brave 不可达）
//!   - 工具策略 system prompt + no_progress 提前收口 + 裸问重试
//!     （移植 zclaw v0.5.2 的搜索治理，防止换引擎后 bug 回归）
//!   - panic 用 catch_unwind 拦在 FFI 边界内，绝不跨边界杀 App
//!
//! 与 zclaw 的关键差异：ulnclaw Agent 自带 SQLite 会话持久化、上下文压缩、
//! max_iterations 耗尽时返回可读文本（不会空气泡）。
mod chunk;
mod prompt;
mod provider;
mod tools;

use chunk::Chunk;
use provider::{ChunkSink, ThinkingCapture};
use std::ffi::{c_char, CStr, CString};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use ulnclaw::agent::{stream_scope, Agent, AgentCallbacks, AgentConfig, StreamEvent};
use ulnclaw::config::UlncLawConfig;
use ulnclaw::provider::{Message, Role};
use ulnclaw::session::sqlite::SqliteSessionStore;
use ulnclaw::tools::context::ToolContext;
use ulnclaw::tools::ToolRegistry;

const VERSION: &str = "0.6.1-ulnclaw-mobile";

// ─────────────────────────── state ───────────────────────────

struct State {
    runtime: tokio::runtime::Runtime,
    cfg: MobileCfg,
    store: Arc<SqliteSessionStore>,
    sink: ChunkSink,
    running: AtomicBool,
    current_session: Mutex<String>,
    /// 当前内层 agent 任务的 abort 手柄（zclaw_cancel / no_progress 收口用）
    abort_slot: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
}

static STATE: OnceLock<Mutex<Option<Arc<State>>>> = OnceLock::new();

fn state_slot() -> &'static Mutex<Option<Arc<State>>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn with_state<R>(f: impl FnOnce(&Arc<State>) -> R) -> Option<R> {
    let guard = state_slot().lock().ok()?;
    guard.as_ref().map(|st| f(st))
}

/// App 端 init 传入的 config JSON（与 zclaw_jni.cpp / zclaw_napi.cpp 拼的字段一致）。
#[derive(serde::Deserialize, Default)]
struct MobileCfg {
    #[serde(default)]
    api_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    default_model: String,
    #[serde(default = "default_temp")]
    temperature: f32,
    #[serde(default)]
    workspace_dir: String,
    #[serde(default)]
    agent: AgentJson,
}

#[derive(serde::Deserialize, Default)]
struct AgentJson {
    #[serde(default)]
    max_iterations: Option<usize>,
}

fn default_temp() -> f32 {
    0.7
}

// ─────────────────────────── helpers ───────────────────────────

fn to_cstring(s: &str) -> *const c_char {
    CString::new(s)
        .unwrap_or_else(|_| CString::new("").unwrap())
        .into_raw() as *const c_char
}

fn cstr_to_string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string())
}

/// FNV-1a（与 zclaw dispatcher / iOS ZClawAgent 同判据）
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 检索类工具（no_progress 判据只针对这些；本地工具反复调用是正常行为）
fn is_retrieval_tool(name: &str) -> bool {
    matches!(name, "web_search" | "web_extract")
}

const NO_PROGRESS_MIN: usize = 3;

/// 无进展判定：同工具+结果逐字节相同 ≥3 次且参数 ≥2 种（换措辞重搜）。
/// 实证前提（zclaw search_probe）：Bing 对冷门 POI 的 4 种措辞返回同一 sha256。
fn no_progress(sigs: &[(String, u64, u64)]) -> bool {
    let Some((last_name, _, last_res)) = sigs.last() else {
        return false;
    };
    if !is_retrieval_tool(last_name) {
        return false;
    }
    let same: Vec<&(String, u64, u64)> = sigs
        .iter()
        .filter(|(n, _, r)| n == last_name && *r == *last_res)
        .collect();
    if same.len() < NO_PROGRESS_MIN {
        return false;
    }
    let mut uniq: Vec<u64> = same.iter().map(|(_, a, _)| *a).collect();
    uniq.sort_unstable();
    uniq.dedup();
    uniq.len() >= 2
}

fn push_chunk(st: &State, c: Chunk) {
    if let Ok(mut q) = st.sink.lock() {
        q.push(c);
    }
}

// ─────────────────────────── agent 构造 ───────────────────────────

fn build_agent(st: &Arc<State>, with_tools: bool) -> Result<Agent, String> {
    let provider_inner = ulnclaw::provider::openai::OpenAiProvider::builder()
        .endpoint(&st.cfg.api_url)
        .api_key(&st.cfg.api_key)
        .model(&st.cfg.default_model)
        .name("openai-compatible")
        .temperature(st.cfg.temperature)
        .build()
        .map_err(|e| format!("build provider failed: {}", e))?;
    let provider: Arc<dyn ulnclaw::provider::Provider> =
        Arc::new(ThinkingCapture::new(provider_inner, st.sink.clone()));

    let mut registry = ToolRegistry::new();
    if with_tools {
        tools::register_mobile_tools(&mut registry);
    }

    let home = std::path::PathBuf::from(&st.cfg.workspace_dir);
    // 🔴 不能用 UlncLawConfig::default()：它的 model.model = "gpt-5.2"
    // （ulnclaw 的 DEFAULT_MODEL），而辅助任务（title_generator / 上下文压缩 /
    // approval guardian）经 resolve_aux_task 的「未覆盖 → 继承主运行时」分支
    // 读的正是 config.model.model。真机实证：主对话 200 成功后紧跟 3 个
    // `503 分组 default 下模型 gpt-5.2 无可用渠道` —— 后端没有这个模型名。
    // 所以必须把用户实际选的 model/api_key/base_url/temperature 灌进去。
    let mut uln_cfg = UlncLawConfig::default();
    uln_cfg.model.model = st.cfg.default_model.clone();
    uln_cfg.model.base_url = Some(st.cfg.api_url.clone());
    uln_cfg.model.api_key = Some(st.cfg.api_key.clone());
    uln_cfg.model.temperature = Some(st.cfg.temperature);
    let max_iter = st.cfg.agent.max_iterations.unwrap_or(10).clamp(1, 30);
    uln_cfg.agent.max_iterations = max_iter;
    uln_cfg.agent.approval = false;
    let context = ToolContext::new()
        .with_home(home.clone())
        .with_workdir(home)
        .with_config(uln_cfg)
        .with_store(st.store.clone())
        .with_provider(provider.clone());
    context.set_tool_definitions(registry.definitions());

    let agent = Agent::new(provider, registry).with_config(AgentConfig {
        max_iterations: max_iter,
        approval: false, // 移动端无审批 UI；工具面已裁剪到安全子集
        persist: with_tools,
        source: "mobile".to_string(),
        environment_probe: false, // 移动端没有 Python 工具链可探测
        system_prompt: Some(prompt::mobile_system_prompt()),
        // 🔴 显式指定 model：effective_model() 优先取 config.model，
        // 否则回落到 provider.model()。两者这里一致，但写明避免歧义。
        model: Some(st.cfg.default_model.clone()),
        ..Default::default()
    });
    Ok(agent.with_tool_context(context).with_store(st.store.clone()))
}

fn stream_callbacks(st: &Arc<State>, streamed_any: Arc<AtomicBool>) -> AgentCallbacks {
    // no_progress 跟踪状态（每次 chat 一份，随 callbacks 闭包捕获）
    let sigs: Arc<Mutex<Vec<(String, u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let last_args: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let mut cb = AgentCallbacks::default();

    // 🔴 **不要**在这里设 on_stream_delta 推 text chunk！
    // ulnclaw 在每个 delta 处**同时**调两个通道（agent/mod.rs:1311-1316）：
    //   emit_stream_event(StreamEvent::Delta(visible))  ← stream_scope emitter
    //   callbacks.on_stream_delta(&visible)             ← 本回调
    // 两处都 push_chunk 会让正文**逐段翻倍**（端到端测试实测：
    // 「我是我是爻荚（爻荚（UlnClaw）UlnClaw），mock-answer，mock-answer」）。
    // 正文统一由 stream_scope 的 emitter 推送——那条通道同时是
    // `stream: STREAM_EMITTER.try_with(...).is_ok()` 的开关，必须在。
    // on_stream_delta 仅用于置 streamed_any 标志（供 content 兜底判断），不推数据。
    {
        cb.on_stream_delta = Some(Box::new(move |_t: &str| {
            streamed_any.store(true, Ordering::SeqCst);
        }));
    }

    // 工具开始 → tool_call chunk，并记住 args（no_progress 判据需要）
    {
        let st = st.clone();
        let last_args = last_args.clone();
        cb.on_tool_start = Some(Box::new(move |name: &str, args: &serde_json::Value| {
            let args_str = args.to_string();
            if let Ok(mut la) = last_args.lock() {
                *la = args_str.clone();
            }
            push_chunk(&st, Chunk::tool_call(name, &args_str));
        }));
    }

    // 工具完成 → tool_result chunk；检索类工具记签名，命中 no_progress 即中止本轮
    {
        let st = st.clone();
        let sigs = sigs.clone();
        let last_args = last_args.clone();
        cb.on_tool_complete = Some(Box::new(move |name: &str, result: &serde_json::Value| {
            let result_str = serde_json::to_string(result).unwrap_or_default();
            push_chunk(&st, Chunk::tool_result(name, &truncate_chars(&result_str, 2000)));

            if !is_retrieval_tool(name) {
                return;
            }
            let args_str = last_args.lock().map(|g| g.clone()).unwrap_or_default();
            let hit = {
                let mut v = sigs.lock().unwrap_or_else(|e| e.into_inner());
                v.push((name.to_string(), fnv1a(&args_str), fnv1a(&result_str)));
                no_progress(&v)
            };
            if hit {
                // 换措辞重搜 ≥3 次拿到逐字节相同结果 → 中止本轮，外层裸问重试。
                // ulnclaw 的 Agent 循环无法从 callback 注入收口提示，
                // abort + bare-retry 是 FFI 层的等价实现。
                if let Ok(slot) = st.abort_slot.lock() {
                    if let Some(h) = slot.as_ref() {
                        h.abort();
                    }
                }
            }
        }));
    }

    cb
}

/// 显示用截断（按字符，绝不按字节切 CJK）
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

// ─────────────────────────── chat 主流程 ───────────────────────────

async fn run_chat(st: Arc<State>, session_id: String, text: String) {
    // 会话建档（zclaw touch_session 等价物）：标题取首条消息前 30 字。
    st.store
        .ensure_session(&session_id, "mobile", Some(&st.cfg.default_model), Some(&st.cfg.workspace_dir))
        .ok();

    // 历史：从 store 加载，过滤 system（agent 每轮自己注入）
    let history: Vec<Message> = st
        .store
        .load_messages(&session_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.role != Role::System)
        .collect();
    let history_arg = if history.is_empty() { None } else { Some(history) };

    let agent = match build_agent(&st, true) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            push_chunk(&st, Chunk::error(&format!("引擎初始化失败: {}", e)));
            st.running.store(false, Ordering::SeqCst);
            push_chunk(&st, Chunk::done());
            return;
        }
    };
    agent.wire_runners();
    // 🔴 streamed_any：标记本轮是否流式吐出过正文。结束后若为 false 而
    // RunResult.content 非空（非流式路径），把 content 一次性补发为 text chunk。
    let streamed_any = Arc::new(AtomicBool::new(false));
    let _ = agent
        .set_callbacks(stream_callbacks(&st, streamed_any.clone()))
        .await;

    // 🔴 根因修复：ulnclaw 的流式开关是 task-local STREAM_EMITTER ——
    //    `stream: STREAM_EMITTER.try_with(|_| ()).is_ok()`（agent/mod.rs:1270）。
    //    不包 stream_scope 时 provider 走**非流式**（后端日志实证 is_stream:false），
    //    on_stream_delta 永不触发，正文只存在于 RunResult.content ——
    //    此前 content 又被 `let _ = final_text` 丢弃 → App 收不到一个字，
    //    卡在「…」空气泡（真机实测 Who are you 无响应，后端 200 + 178 tokens）。
    //    修复：emitter 把 StreamEvent::Delta 推进 chunk 队列（与 on_stream_delta
    //    双通道，任一到达即算 streamed_any），整个 run 包进 scope。
    let sink_for_scope = st.sink.clone();
    let streamed_for_scope = streamed_any.clone();
    let emitter = Arc::new(move |ev: StreamEvent| {
        if let StreamEvent::Delta(t) = ev {
            if !t.is_empty() {
                streamed_for_scope.store(true, Ordering::SeqCst);
                if let Ok(mut q) = sink_for_scope.lock() {
                    q.push(Chunk::text(&t));
                }
            }
        }
    });

    // 内层任务可被 abort（用户取消 / no_progress 收口），外层负责收尾。
    let agent2 = agent.clone();
    let sid = session_id.clone();
    let msg = text.clone();
    let inner = tokio::spawn(async move {
        // stream_scope 必须在 run 所在 task 内包裹（task-local 语义）
        stream_scope(
            emitter,
            agent2.run_with_session(&msg, history_arg, Some(&sid)),
        )
        .await
    });
    if let Ok(mut slot) = st.abort_slot.lock() {
        *slot = Some(inner.abort_handle());
    }

    let outcome = inner.await;
    if let Ok(mut slot) = st.abort_slot.lock() {
        *slot = None;
    }

    let mut final_text = String::new();
    let mut need_bare_retry = false;
    let mut transport_error: Option<String> = None;

    match outcome {
        Ok(Ok(result)) => {
            final_text = result.content.clone();
            // 🔴 content 兜底：若整轮一个字都没流式吐出（如非流式降级路径、
            // 或 provider 不支持 stream）而 RunResult 有正文，一次性补发。
            // streamed_any=true 时绝不补发（delta 已经推过，补发=重复渲染）。
            if !streamed_any.load(Ordering::SeqCst) && !final_text.trim().is_empty() {
                push_chunk(&st, Chunk::text(&final_text));
                streamed_any.store(true, Ordering::SeqCst);
            }
            // ulnclaw 烧满迭代时返回这句固定文本 —— 视同没收口，裸问重试
            if final_text.trim().is_empty() || final_text.starts_with("Reached the iteration budget")
            {
                need_bare_retry = true;
            }
        }
        Ok(Err(e)) => {
            // 模型/网络错误：把分类文本发给 App。Kotlin isTransportError 按关键字
            // 决定是否回退 OkHttp（要求含 transport/timeout/dns/tls 等词）。
            let s = format!("{}", e);
            transport_error = Some(s.clone());
            push_chunk(&st, Chunk::error(&classify_error(&s)));
        }
        Err(join_err) if join_err.is_cancelled() => {
            // 两种取消来源：用户 zclaw_cancel，或 no_progress 收口。
            if st_cancelled_by_user() {
                push_chunk(&st, Chunk::error("cancelled"));
            } else {
                need_bare_retry = true;
            }
        }
        Err(join_err) => {
            // 内层任务 panic：绝不让它跨 FFI。给可读错误。
            push_chunk(&st, Chunk::error(&format!("agent task panicked: {}", join_err)));
        }
    }

    // 🔴 裸问重试（zclaw v0.5.2 同款兜底）：不给道歉文案，丢工具重问一遍。
    if need_bare_retry && transport_error.is_none() {
        let retry_agent = match build_agent(&st, false) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                push_chunk(&st, Chunk::error(&format!("重试引擎初始化失败: {}", e)));
                st.running.store(false, Ordering::SeqCst);
                push_chunk(&st, Chunk::done());
                return;
            }
        };
        retry_agent.wire_runners();
        let mut cb = AgentCallbacks::default();
        let streamed2 = Arc::new(AtomicBool::new(false));
        let streamed2_cb = streamed2.clone();
        // 🔴 同主路径：on_stream_delta 不推数据（emitter2 已推），仅置标志，
        // 否则正文逐段翻倍。
        cb.on_stream_delta = Some(Box::new(move |_t: &str| {
            streamed2_cb.store(true, Ordering::SeqCst);
        }));
        let _ = retry_agent.set_callbacks(cb).await;
        // 同样包 stream_scope（否则又走非流式、delta 回调不触发）
        let sink2 = st.sink.clone();
        let streamed2_scope = streamed2.clone();
        let emitter2 = Arc::new(move |ev: StreamEvent| {
            if let StreamEvent::Delta(t) = ev {
                if !t.is_empty() {
                    streamed2_scope.store(true, Ordering::SeqCst);
                    if let Ok(mut q) = sink2.lock() {
                        q.push(Chunk::text(&t));
                    }
                }
            }
        });
        // 🔴 E0716：bare_retry_prompt 的临时 String 必须先 let 绑定，
        // 否则在 retry_fut（借用它）存活期间就被释放。
        let retry_prompt = prompt::bare_retry_prompt(&text);
        let retry_fut = retry_agent.run(&retry_prompt, None);
        let outcome2 = stream_scope(emitter2, retry_fut).await;
        match outcome2 {
            Ok(r) => final_text = r.content,
            Err(e) => push_chunk(&st, Chunk::error(&format!("重试失败: {}", e))),
        }
        // content 兜底（与主路径同理：非流式时 delta 回调不触发）
        if !streamed2.load(Ordering::SeqCst) && !final_text.trim().is_empty() {
            push_chunk(&st, Chunk::text(&final_text));
        }
        if final_text.trim().is_empty() {
            push_chunk(&st, Chunk::error("模型未返回内容（上游异常），请重试或更换模型"));
        } else {
            // retry agent persist=false，手动落库 assistant 消息
            st.store
                .append_message(
                    &session_id,
                    &Message {
                        role: Role::Assistant,
                        content: Some(final_text.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    },
                )
                .ok();
        }
    }

    st.running.store(false, Ordering::SeqCst);
    push_chunk(&st, Chunk::done());
    let _ = final_text;
}

/// 用户主动取消标记（与 no_progress abort 区分）
static USER_CANCELLED: AtomicBool = AtomicBool::new(false);
fn st_cancelled_by_user() -> bool {
    USER_CANCELLED.swap(false, Ordering::SeqCst)
}

/// 把底层错误串归类成 Kotlin isTransportError 能识别的关键字形态。
fn classify_error(raw: &str) -> String {
    let low = raw.to_lowercase();
    if low.contains("timed out") || low.contains("timeout") {
        format!("request timed out (连接超时): {}", raw)
    } else if low.contains("dns") || low.contains("resolve") {
        format!("dns resolve failed (DNS 解析失败): {}", raw)
    } else if low.contains("tls") || low.contains("certificate") || low.contains("ssl") {
        format!("TLS/certificate error (证书错误): {}", raw)
    } else if low.contains("connection") {
        format!("error sending request (连接失败): {}", raw)
    } else {
        raw.to_string()
    }
}

// ─────────────────────────── C ABI ───────────────────────────

#[no_mangle]
pub extern "C" fn zclaw_init(config_json: *const c_char) -> i32 {
    // catch_unwind：panic 绝不跨 FFI 边界（zclaw panic=abort 的教训）
    let r = panic::catch_unwind(|| {
        let json = match cstr_to_string(config_json) {
            Some(j) => j,
            None => return -1,
        };
        let mut cfg: MobileCfg = match serde_json::from_str(&json) {
            Ok(c) => c,
            Err(_) => return -1,
        };
        if cfg.api_url.is_empty() {
            cfg.api_url = "https://ai.ulnit.com/v1".to_string();
        }
        if cfg.workspace_dir.is_empty() {
            cfg.workspace_dir = ".".to_string();
        }
        std::fs::create_dir_all(&cfg.workspace_dir).ok();

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
        {
            Ok(r) => r,
            Err(_) => return -1,
        };
        let db_path = std::path::Path::new(&cfg.workspace_dir).join("ulnclaw_sessions.db");
        let store = match SqliteSessionStore::open(&db_path) {
            Ok(s) => Arc::new(s),
            Err(_) => return -1,
        };

        let st = Arc::new(State {
            runtime,
            cfg,
            store,
            sink: Arc::new(Mutex::new(Vec::new())),
            running: AtomicBool::new(false),
            current_session: Mutex::new(String::new()),
            abort_slot: Arc::new(Mutex::new(None)),
        });
        *state_slot().lock().unwrap() = Some(st);
        0
    });
    r.unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn zclaw_chat(message: *const c_char) -> i32 {
    let r = panic::catch_unwind(|| {
        let text = match cstr_to_string(message) {
            Some(t) if !t.trim().is_empty() => t,
            _ => return -1,
        };
        let Some(st) = state_slot().lock().ok().and_then(|g| g.clone()) else {
            return -1;
        };
        if st.running.load(Ordering::SeqCst) {
            return -2; // busy
        }
        let session_id = {
            let mut s = st.current_session.lock().unwrap();
            if s.is_empty() {
                *s = uuid::Uuid::new_v4().to_string();
            }
            s.clone()
        };
        st.running.store(true, Ordering::SeqCst);
        if let Ok(mut q) = st.sink.lock() {
            q.clear();
        }
        let st2 = st.clone();
        st.runtime.spawn(async move {
            run_chat(st2, session_id, text).await;
        });
        0
    });
    r.unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn zclaw_set_session(session_id: *const c_char) -> i32 {
    let r = panic::catch_unwind(|| {
        let id = match cstr_to_string(session_id) {
            Some(i) if !i.trim().is_empty() => i,
            _ => return -1,
        };
        match with_state(|st| {
            *st.current_session.lock().unwrap() = id;
        }) {
            Some(()) => 0,
            None => -1,
        }
    });
    r.unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn zclaw_poll_chunks() -> *const c_char {
    let r = panic::catch_unwind(|| {
        let chunks: Vec<Chunk> = with_state(|st| {
            std::mem::take(&mut *st.sink.lock().unwrap_or_else(|e| e.into_inner()))
        })
        .unwrap_or_default();
        let json = serde_json::to_string(&chunks).unwrap_or_else(|_| "[]".to_string());
        to_cstring(&json)
    });
    r.unwrap_or_else(|_| to_cstring("[]"))
}

#[no_mangle]
pub extern "C" fn zclaw_is_running() -> i32 {
    let r = panic::catch_unwind(|| {
        with_state(|st| {
            if st.running.load(Ordering::SeqCst) {
                1
            } else {
                0
            }
        })
        .unwrap_or(0)
    });
    r.unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn zclaw_cancel() -> i32 {
    let r = panic::catch_unwind(|| {
        with_state(|st| {
            if !st.running.load(Ordering::SeqCst) {
                return -1;
            }
            USER_CANCELLED.store(true, Ordering::SeqCst);
            if let Ok(slot) = st.abort_slot.lock() {
                if let Some(h) = slot.as_ref() {
                    h.abort();
                }
            }
            0
        })
        .unwrap_or(-1)
    });
    r.unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn zclaw_get_sessions() -> *const c_char {
    let r = panic::catch_unwind(|| {
        let json = with_state(|st| {
            let rows = st.store.list_session_rows(50).unwrap_or_default();
            let arr: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "title": r.title.unwrap_or_default(),
                        "model_name": r.model.unwrap_or_default(),
                        // App 端 Kotlin 按毫秒解析 created_at/updated_at
                        "created_at": (r.started_at * 1000.0) as i64,
                        "updated_at": (r.last_activity_at * 1000.0) as i64,
                    })
                })
                .collect();
            serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
        })
        .unwrap_or_else(|| "[]".to_string());
        to_cstring(&json)
    });
    r.unwrap_or_else(|_| to_cstring("[]"))
}

#[no_mangle]
pub extern "C" fn zclaw_get_messages(session_id: *const c_char) -> *const c_char {
    let r = panic::catch_unwind(|| {
        let Some(id) = cstr_to_string(session_id) else {
            return to_cstring("[]");
        };
        let json = with_state(|st| {
            let rows = st.store.load_message_rows(&id).unwrap_or_default();
            let arr: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|m| {
                    serde_json::json!({
                        "role": m.role,
                        "content": m.content,
                        "created_at": (m.timestamp * 1000.0) as i64,
                    })
                })
                .collect();
            serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
        })
        .unwrap_or_else(|| "[]".to_string());
        to_cstring(&json)
    });
    r.unwrap_or_else(|_| to_cstring("[]"))
}

#[no_mangle]
pub extern "C" fn zclaw_free(ptr: *const c_char) {
    if ptr.is_null() {
        return;
    }
    let _ = panic::catch_unwind(|| unsafe {
        drop(CString::from_raw(ptr as *mut c_char));
    });
}

#[no_mangle]
pub extern "C" fn zclaw_version() -> *const c_char {
    to_cstring(VERSION)
}

/// 网络自诊断（App 失败路径调用；Kotlin 解析 JSON 字符串数组）。
/// MVP：DNS+TLS+HTTP GET /v1/models（不耗 token）。阻塞调用，须在 IO 线程。
#[no_mangle]
pub extern "C" fn zclaw_network_probe(api_url: *const c_char, api_key: *const c_char) -> *const c_char {
    let r = panic::catch_unwind(|| {
        let url = cstr_to_string(api_url).filter(|s| !s.is_empty())
            .unwrap_or_else(|| "https://ai.ulnit.com/v1".to_string());
        let key = cstr_to_string(api_key).unwrap_or_default();
        let handle = with_state(|st| st.runtime.handle().clone());
        let lines = match handle {
            Some(h) => h.block_on(probe(&url, &key)),
            None => match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
            {
                Ok(rt) => rt.block_on(probe(&url, &key)),
                Err(e) => vec![format!("无法创建诊断 runtime: {}", e)],
            },
        };
        let json =
            serde_json::to_string(&lines).unwrap_or_else(|_| "[\"诊断结果序列化失败\"]".to_string());
        to_cstring(&json)
    });
    r.unwrap_or_else(|_| to_cstring("[\"诊断异常\"]"))
}

async fn probe(url: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "api host".to_string());

    // 1. DNS
    let models_url = if url.ends_with("/v1") || url.ends_with("/v1/") {
        format!("{}/models", url.trim_end_matches('/'))
    } else {
        format!("{}/v1/models", url.trim_end_matches('/'))
    };
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::task::spawn_blocking({
            let h = host.clone();
            move || {
                use std::net::ToSocketAddrs;
                format!("{:?}", (h.as_str(), 443).to_socket_addrs().map(|mut a| a.next()).unwrap_or(None))
            }
        }),
    )
    .await
    {
        Ok(Ok(s)) if s.contains("Some") => out.push(format!("✓ DNS 解析正常: {} → {}", host, s)),
        Ok(Ok(_)) => out.push(format!("✗ DNS 解析失败: {}", host)),
        Ok(Err(e)) => out.push(format!("✗ DNS 探测异常: {}", e)),
        Err(_) => out.push(format!("✗ DNS 解析超时(10s): {}", host)),
    }

    // 2. TLS+HTTP GET /v1/models（带 key 则应 200）
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build();
    match client {
        Ok(c) => {
            let t0 = std::time::Instant::now();
            let mut req = c.get(&models_url);
            if !key.is_empty() {
                // 🔴 key 是 &str：key.clone() 仍是 &str，与 format! 的 String 分支
                // 类型不匹配（E0308 实测）。统一 to_string()。
                let bearer = if key.starts_with("sk-") {
                    key.to_string()
                } else {
                    format!("sk-{}", key)
                };
                req = req.bearer_auth(bearer);
            }
            match req.send().await {
                Ok(resp) => {
                    let ms = t0.elapsed().as_millis();
                    let status = resp.status().as_u16();
                    if resp.status().is_success() {
                        out.push(format!("✓ TLS+HTTP 正常: {} ({}ms)", status, ms));
                    } else {
                        out.push(format!("✗ HTTP {} ({}ms) — 服务端拒绝，检查 Key/额度", status, ms));
                    }
                }
                Err(e) => out.push(format!("✗ TLS/连接失败: {}", classify_error(&format!("{}", e)))),
            }
        }
        Err(e) => out.push(format!("✗ HTTP client 构建失败: {}", e)),
    }
    out
}

const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    // ── chunk 协议序列化：逐字段对齐 zclaw v0.2（App 解析器契约）──
    #[test]
    fn chunk_json_matches_zclaw_contract() {
        let t = serde_json::to_value(Chunk::text("你好")).unwrap();
        assert_eq!(t["chunkType"], 0);
        assert_eq!(t["name"], "你好");
        assert!(t.get("args").is_none());
        assert!(t.get("result").is_none());

        let tc = serde_json::to_value(Chunk::tool_call("web_search", "{}")).unwrap();
        assert_eq!(tc["chunkType"], 1);
        assert_eq!(tc["args"], "{}");

        let tr = serde_json::to_value(Chunk::tool_result("web_search", "r")).unwrap();
        assert_eq!(tr["chunkType"], 2);
        assert_eq!(tr["result"], "r");

        assert_eq!(serde_json::to_value(Chunk::done()).unwrap()["chunkType"], 3);
        assert_eq!(serde_json::to_value(Chunk::error("e")).unwrap()["chunkType"], 4);
        assert_eq!(serde_json::to_value(Chunk::thinking("t")).unwrap()["chunkType"], 5);
    }

    // ── no_progress 判据（与 zclaw v0.5.2 / iOS ZClawAgent 同判据）──
    fn s(name: &str, args: &str, res: &str) -> (String, u64, u64) {
        (name.to_string(), fnv1a(args), fnv1a(res))
    }

    /// 真实故障形态回归：Bing 对冷门 POI 的 4 种措辞返回逐字节相同结果
    /// （search_probe 实证 1 个唯一 sha256）。修复前烧满迭代零正文。
    #[test]
    fn no_progress_fires_on_reworded_identical_results() {
        let same = "[1] 长沙 - 维基百科";
        let two = vec![
            s("web_search", r#"{"query":"长沙 唯一星城"}"#, same),
            s("web_search", r#"{"query":"长沙 维一星城 小区"}"#, same),
        ];
        assert!(!no_progress(&two), "阈值 {} 未到不判定", NO_PROGRESS_MIN);
        let three = {
            let mut v = two.clone();
            v.push(s("web_search", r#"{"query":"长沙 唯一新城 二手房"}"#, same));
            v
        };
        assert!(no_progress(&three), "3 次同结果+不同措辞 → 必须收口");
    }

    #[test]
    fn no_progress_guards() {
        // 同参数重试（网络抖动）不收口
        let r = "[1] 无结果";
        let retry = vec![
            s("web_search", r#"{"q":"a"}"#, r),
            s("web_search", r#"{"q":"a"}"#, r),
            s("web_search", r#"{"q":"a"}"#, r),
        ];
        assert!(!no_progress(&retry));
        // 结果各异 = 有进展
        let progress = vec![
            s("web_search", "a", "r1"),
            s("web_search", "b", "r2"),
            s("web_search", "c", "r3"),
        ];
        assert!(!no_progress(&progress));
        // 本地工具永不收口（todo/memory/files 反复调用正常）
        for name in ["todo", "memory", "read_file", "session_search", "clarify", "skills"] {
            let v = vec![s(name, "a", "same"), s(name, "b", "same"), s(name, "c", "same")];
            assert!(!no_progress(&v), "{} 是本地工具不该收口", name);
        }
        assert!(!no_progress(&[]));
    }

    // ── prompt 文案锁：道歉兜底不许回归（用户明确否决过）──
    #[test]
    fn prompts_lock_no_apology_policy() {
        let p = prompt::mobile_system_prompt();
        assert!(p.contains("联网搜索只是"));
        assert!(p.contains("用你自己已有的知识回答"));
        assert!(p.contains("不要编造") || p.contains("更不要编造"));
        assert!(p.len() > 200, "策略 prompt 不该退化成一句话");
        let c = prompt::COLLAR_PROMPT;
        assert!(c.contains("禁止") && c.contains("抱歉"), "必须点名禁止道歉式开场");
        assert!(c.contains("你自己已有的知识"));
        let b = prompt::bare_retry_prompt("唯一星城在哪");
        assert!(b.contains("唯一星城在哪"));
        assert!(b.contains("不要用任何工具"));
    }

    // ── UTF-8 安全截断（CJK 边界，zclaw panic=abort 教训）──
    #[test]
    fn truncate_chars_never_splits_cjk() {
        assert_eq!(truncate_chars("你好世界", 2), "你好");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("", 5), "");
    }

    #[test]
    fn error_classification_keeps_kotlin_keywords() {
        // Kotlin isTransportError 按关键字回退 OkHttp：timeout/dns/tls/connection
        assert!(classify_error("operation timed out").contains("超时"));
        assert!(classify_error("dns error: failed to resolve").to_lowercase().contains("dns"));
        assert!(classify_error("invalid certificate").contains("证书"));
        assert!(classify_error("connection refused").contains("连接"));
        // 非传输错误原样保留（401/quota 不该触发回退）
        assert_eq!(classify_error("http 401 unauthorized"), "http 401 unauthorized");
    }
}

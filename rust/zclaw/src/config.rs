use serde::Deserialize;

fn default_temp() -> f32 { 0.7 }
fn default_max_iter() -> u32 { 10 }
/// 🔴 这是三端唯一的 system prompt 来源（单一事实源）。
///
/// 历史 bug：此前 prompt 只有一句 "You are ZClaw, a helpful pocket AI assistant."，
/// 完全没有「工具使用策略」。而 web_search 的工具描述又过度承诺
/// "Returns relevant search results"。结果模型误以为「一定能搜到，搜不到就是我
/// 措辞不对」，对冷门本地 POI（小区名）陷入「换措辞→重搜→还是泛化结果→再换」
/// 的死循环：后端日志实测 prompt_tokens 从 3.3k 一路涨到 18k、9 轮全在重搜、
/// 每轮 finish=tool_calls 零正文，用户最终放弃（client_gone），看到空气泡。
///
/// 关键认知（用户点破）：**联网搜索只是众多工具之一**。搜不到时模型应当退回
/// 自身知识作答，而不是把检索当成必须完成的前置条件。下面把这条策略写死进 prompt，
/// 从源头减少无谓重试；loop_detector（dispatcher.rs）再作为第二道防线兜底。
///
/// 改这里即对 Android/Harmony 生效（两端的 JNI/NAPI 已不再硬编码 prompt，见下）。
/// iOS 走纯 Swift，需在 ZClawAgent.swift 同步同一份文案。
fn default_prompt() -> String {
    "你是爻荚（UlnClaw），一个装在口袋里的 AI 助手。\n\
     \n\
     关于工具：联网搜索只是你可用的工具之一，不是回答问题前必须完成的步骤。\n\
     - 简单常识、推理、计算、写作类问题：直接回答，不必检索。\n\
     - 确实需要实时/具体信息时才搜索。\n\
     - 搜索结果可能不相关：搜索引擎对冷门或本地化的查询（如某个小区名、\n\
     \x20 某条街、某个小商户）常常返回泛化的城市介绍或无关页面。\n\
     - 判断结果是否真的回答了问题：若标题和摘要里没有你查的那个具体对象，\n\
     \x20 就是没搜到，不要把它当成答案。\n\
     \n\
     搜索没结果时的正确做法（重要）：\n\
     1. 最多换一种措辞再试一次（例如去掉修饰词、只用核心名称、或加上城市/区名）。\n\
     2. 仍然没有 → 立即停止检索，用你自己已有的知识回答，并明确说明：\n\
     \x20 「我没有联网查到确切信息，以下是基于我已有的了解，可能不准确」。\n\
     3. 绝不要为了同一个问题反复搜索超过两三次；反复重试不会带来新信息，\n\
     \x20 只会浪费用户时间。\n\
     \n\
     诚实优先：宁可说「查不到」或「不确定」，也不要把不相关的搜索结果硬凑成答案，\n\
     更不要编造具体细节（地址、价格、时间等）。"
        .to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentCfg {
    #[serde(default = "default_max_iter")]
    pub max_iterations: u32,
    #[serde(default = "default_prompt")]
    pub system_prompt: String,
}

/// 🔴 手工实现 Default，不能用 #[derive(Default)]。
///
/// `#[serde(default = "default_prompt")]` **只在反序列化缺字段时生效**，
/// 派生的 `Default::default()` 给的是空字符串。两条路径不一致的话：
/// Android/Harmony 的 JSON（已不再传 system_prompt）走 serde → 拿到策略 prompt；
/// 而任何 `Config::default()` 的调用点 → 拿到**空 prompt**，工具使用策略全丢。
/// 单元测试正是用后者，才把这个隐患暴露出来。
impl Default for AgentCfg {
    fn default() -> Self {
        Self { max_iterations: default_max_iter(), system_prompt: default_prompt() }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub api_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default = "default_temp")]
    pub temperature: f32,
    #[serde(default)]
    pub workspace_dir: String,
    #[serde(default)]
    pub agent: AgentCfg,
}

impl Config {
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.api_url.trim_end_matches('/'))
    }
}

//! 移动端 Provider wrapper：把 ulnclaw 的 OpenAiProvider 包一层，
//! 在流式转发时抽取 `delta_reasoning` 推给 chunk 队列（chunkType=5）。
//!
//! 为什么需要它：ulnclaw 的 AgentCallbacks.on_thinking 是**无参**信号，
//! reasoning 文本只在 agent 内部累积（ProviderResponse.reasoning），
//! 不流给任何 callback、不进 StreamEvent。App 的思考流面板需要增量文本，
//! 所以必须在 Provider 层拦截 chat_completion_stream 的每个 chunk。
//!
//! 这是移动端 FFI 对上游的唯一行为增量，通过组合（而非修改）实现，
//! 上游 git pull 不受影响。
use crate::chunk::Chunk;
use std::sync::Arc;
// 🔴 必须用 std Mutex 而非 tokio Mutex：sink 同时被
//   a) 同步 FFI 入口（zclaw_poll_chunks/push_chunk）——tokio .lock() 返回 Future 没法同步用
//   b) 异步 provider wrapper——临界区只是 push 一个 Chunk，纳秒级，
//      std Mutex 在 async 里短暂持有完全安全（无 await 跨越）
use std::sync::Mutex;
use ulnclaw::error::Result;
use ulnclaw::provider::{
    Provider, ProviderRequest, ProviderResponse, ProviderStream,
};

pub type ChunkSink = Arc<Mutex<Vec<Chunk>>>;

/// 包裹任意上游 Provider，流式转发时抽取 reasoning delta。
pub struct ThinkingCapture<P: Provider + 'static> {
    inner: Arc<P>,
    sink: ChunkSink,
}

impl<P: Provider + 'static> ThinkingCapture<P> {
    pub fn new(inner: P, sink: ChunkSink) -> Self {
        Self { inner: Arc::new(inner), sink }
    }
}

#[async_trait::async_trait]
impl<P: Provider + 'static> Provider for ThinkingCapture<P> {
    async fn chat_completion(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        self.inner.chat_completion(request).await
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    async fn chat_completion_stream(&self, request: ProviderRequest) -> Result<ProviderStream> {
        use futures::StreamExt;
        let stream = self.inner.chat_completion_stream(request).await?;
        let sink = self.sink.clone();

        // then_each：每个 chunk 过一手（只读 reasoning，不改内容），再原样转发。
        // std Mutex 临界区内只 push 一个 Chunk（无 await 跨越），async 安全。
        let tapped = stream.then(move |item| {
            let sink = sink.clone();
            async move {
                if let Ok(chunk) = &item {
                    if let Some(r) = &chunk.delta_reasoning {
                        if !r.is_empty() {
                            if let Ok(mut q) = sink.lock() {
                                q.push(Chunk::thinking(r));
                            }
                        }
                    }
                }
                item
            }
        });
        Ok(Box::pin(tapped))
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn is_available(&self) -> bool {
        self.inner.is_available().await
    }

    async fn analyze_image(&self, prompt: &str, image_url: &str) -> Result<String> {
        self.inner.analyze_image(prompt, image_url).await
    }
}

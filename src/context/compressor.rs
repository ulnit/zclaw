//! Context compression — aligned with hermes' agent/conversation_compression.py
//!
//! When the conversation exceeds the token budget, the middle turns are
//! summarized by a secondary model call and replaced with a compact
//! "[CONTEXT SUMMARY]" message, keeping the system prompt, the first user
//! message, and the most recent turns intact.

use crate::provider::{Message, Provider, Role};

/// Hard share of the context budget the verbatim tail may occupy
/// (hermes `TAIL_MAX_CONTEXT_FRACTION`, `agent/context_compressor.py`).
///
/// Without this cap a small-window model spends its *entire* budget on the
/// "protected" tail, and compression becomes a no-op that still costs a summary
/// call. hermes documents exactly this failure:
///
/// > The lean floor alone is 61% of a 16K window and 122% of an 8K one, so on a
/// > local 27B the "protected" tail WAS the whole request and every compaction
/// > pass summarised six rows and reclaimed nothing.
///
/// ulnclaw's `keep_recent: 12` had the same shape of bug from the other side: a
/// fixed *message count* is unbounded in tokens, so twelve tool-heavy turns can
/// exceed any budget.
pub const TAIL_MAX_CONTEXT_FRACTION: f32 = 0.20;

/// Never shrink the tail below this many messages, so a compaction can't leave the
/// model with no recent context at all (hermes `compressible_tail_cap = max(3, …)`).
const TAIL_MIN_MESSAGES: usize = 3;

const SUMMARY_PROMPT: &str = "Summarize the conversation segment below into a compact briefing \
that preserves: decisions made, facts learned, file paths touched, pending work, and any \
constraints the user stated. Be dense and factual; bullet points are fine. Do not address the \
user — this summary is injected back into the model's context.\n\nCONVERSATION SEGMENT:\n";

pub struct ContextCompressor {
    /// Maximum context tokens before compression kicks in.
    pub max_context_tokens: usize,
    /// Target ratio of the budget the compressed result should fit under.
    pub target_ratio: f32,
    /// Number of recent messages always kept verbatim.
    pub keep_recent: usize,
    /// IANA timezone for the summary's temporal anchoring (hermes
    /// compressor `Current date` line via hermes_time).
    pub timezone: Option<String>,
}

impl Default for ContextCompressor {
    fn default() -> Self {
        Self::new(100_000)
    }
}

impl ContextCompressor {
    pub fn new(max_context_tokens: usize) -> Self {
        Self {
            max_context_tokens,
            target_ratio: 0.5,
            keep_recent: 12,
            timezone: None,
        }
    }

    /// Set the timezone used for the summary's `Current date` anchor.
    pub fn with_timezone(mut self, timezone: Option<String>) -> Self {
        self.timezone = timezone;
        self
    }

    /// Estimate token count (rough approximation: 4 chars per token).
    pub fn estimate_tokens(messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|m| {
                let content_len = m.content.as_ref().map(|c| c.len()).unwrap_or(0);
                let tool_len = m
                    .tool_calls
                    .as_ref()
                    .map(|calls| serde_json::to_string(calls).map(|s| s.len()).unwrap_or(0))
                    .unwrap_or(0);
                (content_len + tool_len) / 4
            })
            .sum()
    }

    /// Check if compression is needed.
    pub fn needs_compression(&self, messages: &[Message]) -> bool {
        Self::estimate_tokens(messages) > self.max_context_tokens
    }

    /// How many trailing messages to keep verbatim for this conversation.
    ///
    /// `keep_recent` is the *requested* count; this additionally enforces
    /// [`TAIL_MAX_CONTEXT_FRACTION`] of the budget in tokens, then clamps to
    /// [`TAIL_MIN_MESSAGES`] so a compaction never leaves the model without any
    /// recent context. Both bounds matter in opposite directions:
    ///
    /// * without the token cap, twelve tool-heavy turns can exceed the whole
    ///   budget — compression summarises the middle and reclaims almost nothing;
    /// * without the floor, a tiny budget would shrink the tail to zero and the
    ///   model would lose the turn it is currently answering.
    ///
    /// Returns 0 only when the conversation is too short to compress at all.
    pub fn effective_tail_len(&self, messages: &[Message]) -> usize {
        let n = messages.len();
        let want = self.keep_recent.min(n);
        if want == 0 {
            return 0;
        }
        let token_cap = (self.max_context_tokens as f32 * TAIL_MAX_CONTEXT_FRACTION) as usize;
        // Walk newest-first, accumulating until the next message would break the cap.
        let mut used = 0usize;
        let mut count = 0usize;
        for m in messages.iter().rev() {
            let cost = Self::estimate_tokens(std::slice::from_ref(m));
            if count > 0 && used + cost > token_cap {
                break;
            }
            used += cost;
            count += 1;
            if count >= want {
                break;
            }
        }
        // Floor: never let the cap shrink the tail below TAIL_MIN_MESSAGES (or
        // below what actually exists), otherwise the model loses the turn it is
        // currently answering.
        let floor = TAIL_MIN_MESSAGES.min(n);
        count.max(floor)
    }

    /// Compress using a provider call. Returns None when compression fails or
    /// is not applicable (keeps the original messages).
    pub async fn compress_with_provider(
        &self,
        messages: Vec<Message>,
        provider: &dyn Provider,
    ) -> Option<Vec<Message>> {
        self.compress_with_model(messages, provider, provider.model())
            .await
    }

    /// Compress with an explicit model id (auxiliary model routing — the
    /// hermes compressor calls `get_text_auxiliary_client("compression")`
    /// and summarizes with the resolved aux model).
    pub async fn compress_with_model(
        &self,
        messages: Vec<Message>,
        provider: &dyn Provider,
        model: &str,
    ) -> Option<Vec<Message>> {
        // Token-aware tail: `keep_recent` is only the *requested* count; the
        // effective one is additionally capped at TAIL_MAX_CONTEXT_FRACTION of the
        // budget so a small-window model doesn't spend its whole context on the
        // "protected" tail and reclaim nothing (see the constant's docs).
        let tail_len = self.effective_tail_len(&messages);
        if messages.len() <= tail_len + 2 {
            return None;
        }
        // Keep: system prompt + first user message + recent tail.
        let system = messages.iter().find(|m| m.role == Role::System).cloned();
        let first_user = messages.iter().find(|m| m.role == Role::User).cloned();
        let tail_start = messages.len() - tail_len;
        let middle: Vec<&Message> = messages
            .iter()
            .enumerate()
            .filter(|(i, m)| m.role != Role::System && *i > 0 && *i < tail_start)
            .map(|(_, m)| m)
            .collect();
        if middle.is_empty() {
            return None;
        }

        let mut segment = String::new();
        for message in &middle {
            let content = message.content.as_deref().unwrap_or("");
            if content.is_empty() {
                continue;
            }
            // Bound each message to keep the summary prompt manageable.
            let bounded: String = content.chars().take(2000).collect();
            segment.push_str(&format!("[{}] {}\n", message.role, bounded));
        }
        if segment.is_empty() {
            return None;
        }

        let summary_request = crate::provider::ProviderRequest {
            messages: vec![Message {
                reasoning: None,
                role: Role::User,
                content: Some(format!(
                    "Current date: {}\n\n{}{}",
                    crate::hermes_time::today_iso(self.timezone.as_deref()),
                    SUMMARY_PROMPT,
                    segment
                )),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            tools: vec![],
            model: model.to_string(),
            max_tokens: Some(1024),
            temperature: Some(0.2),
            stream: false,
            stop: None,

            images: None,
        };

        let summary = match provider.chat_completion(summary_request).await {
            Ok(response) => response.content.unwrap_or_default(),
            Err(e) => {
                tracing::warn!("compression failed: {}", e);
                return None;
            }
        };
        if summary.trim().is_empty() {
            return None;
        }

        let mut compressed = Vec::new();
        if let Some(system) = system {
            compressed.push(system);
        }
        if let Some(first_user) = first_user {
            compressed.push(first_user);
        }
        compressed.push(Message {
            reasoning: None,
            role: Role::User,
            content: Some(format!(
                "[CONTEXT SUMMARY — earlier turns compressed]\n{}",
                summary.trim()
            )),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
        compressed.push(Message {
            reasoning: None,
            role: Role::Assistant,
            content: Some("Understood — continuing from the summary.".to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
        compressed.extend(messages.into_iter().skip(tail_start));
        Some(compressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> Message {
        Message {
            reasoning: None,
            role: Role::User,
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    fn assistant(content: &str) -> Message {
        Message {
            reasoning: None,
            role: Role::Assistant,
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// A conversation of `n` alternating turns, each message `chars` long.
    fn convo(n: usize, chars: usize) -> Vec<Message> {
        let text = "x".repeat(chars);
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    user(&text)
                } else {
                    assistant(&text)
                }
            })
            .collect()
    }

    #[test]
    fn test_estimation_and_threshold() {
        let compressor = ContextCompressor::new(10);
        let messages = vec![Message {
            reasoning: None,
            role: Role::User,
            content: Some("x".repeat(100)), // ~25 tokens
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }];
        assert!(compressor.needs_compression(&messages));
        let big = ContextCompressor::new(100_000);
        assert!(!big.needs_compression(&messages));
    }

    /// Below the cap, `keep_recent` is honoured exactly — the token cap must not
    /// change behaviour for conversations that already fit.
    #[test]
    fn tail_len_uses_keep_recent_when_under_the_token_cap() {
        let c = ContextCompressor::new(100_000); // 20% = 20k tokens, plenty
        let messages = convo(40, 100); // ~25 tokens each
        assert_eq!(c.effective_tail_len(&messages), c.keep_recent);
        assert_eq!(c.keep_recent, 12);
    }

    /// 🔴 The hermes failure mode this ports: a tool-heavy tail that alone exceeds
    /// the budget. `keep_recent: 12` messages of ~1000 tokens each = ~12k tokens,
    /// which is *the whole* 10k budget — the "protected" tail was the entire
    /// request and compaction reclaimed nothing. The cap must shrink it to ~20%.
    #[test]
    fn tail_len_is_capped_at_twenty_percent_of_budget() {
        let c = ContextCompressor::new(10_000); // 20% = 2000 tokens
        let messages = convo(40, 4_000); // 1000 tokens each
        let tail = c.effective_tail_len(&messages);
        assert!(
            tail < c.keep_recent,
            "a 12-message tail of 1000-token messages must be capped, got {tail}"
        );
        let tail_tokens = ContextCompressor::estimate_tokens(&messages[messages.len() - tail..]);
        // One message (1000) always fits; the second would reach 2000 = cap exactly.
        assert!(
            tail_tokens <= 2_000 + 1_000,
            "tail must stay near 20% of the budget, got {tail_tokens} tokens ({tail} msgs)"
        );
        assert!(tail >= TAIL_MIN_MESSAGES, "floor must hold, got {tail}");
    }

    /// The floor keeps a tiny window workable instead of shrinking the tail to 0
    /// (hermes `compressible_tail_cap = max(3, …)`).
    #[test]
    fn tail_len_never_drops_below_the_floor() {
        let c = ContextCompressor::new(10); // 20% = 2 tokens: nothing fits
        let messages = convo(20, 400);
        assert_eq!(
            c.effective_tail_len(&messages),
            TAIL_MIN_MESSAGES,
            "even an absurdly small budget must keep the floor"
        );
    }

    /// Never reports more messages than exist (would underflow `len - tail_start`).
    #[test]
    fn tail_len_never_exceeds_the_conversation() {
        let c = ContextCompressor::new(100_000);
        let short = convo(2, 100);
        assert_eq!(c.effective_tail_len(&short), 2);
        assert_eq!(c.effective_tail_len(&[]), 0);
        // floor is clamped to what exists, so a 2-message convo can't report 3
        let tiny_budget = ContextCompressor::new(10);
        assert_eq!(tiny_budget.effective_tail_len(&short), 2);
    }

    /// `keep_recent: 0` must stay 0 — a caller that deliberately wants no
    /// verbatim tail must not have the floor forced on it.
    #[test]
    fn zero_keep_recent_is_honoured() {
        let mut c = ContextCompressor::new(100_000);
        c.keep_recent = 0;
        let messages = convo(20, 100);
        assert_eq!(c.effective_tail_len(&messages), 0);
    }

    /// 🔴 Regression guard for the real bug: with a tool-heavy conversation and a
    /// small window, compression must actually *reclaim* tokens. Before the cap the
    /// tail alone consumed the budget, so `compress_with_model` would either bail or
    /// return something no smaller than the input.
    #[tokio::test]
    async fn compression_actually_reclaims_tokens_on_a_small_window() {
        // Minimal mock: the trait defaults cover everything else (streaming,
        // vision, availability); compress_with_model only calls chat_completion.
        struct Echo;
        #[async_trait::async_trait]
        impl Provider for Echo {
            async fn chat_completion(
                &self,
                _req: crate::provider::ProviderRequest,
            ) -> crate::error::Result<crate::provider::ProviderResponse> {
                Ok(crate::provider::ProviderResponse {
                    content: Some("SUMMARY-OF-EARLIER-TURNS".to_string()),
                    tool_calls: vec![],
                    usage: None,
                    model: "echo-model".to_string(),
                    reasoning: None,
                    finish_reason: Some("stop".to_string()),
                })
            }
            fn model(&self) -> &str {
                "echo-model"
            }
            fn name(&self) -> &str {
                "echo"
            }
        }

        let c = ContextCompressor::new(10_000);
        let messages = convo(40, 4_000); // 40 x 1000 tokens = 40k
        let before = ContextCompressor::estimate_tokens(&messages);

        let out = c
            .compress_with_model(messages.clone(), &Echo, "echo-model")
            .await
            .expect("compression must produce a result");

        let after = ContextCompressor::estimate_tokens(&out);
        assert!(
            after < before / 2,
            "compression must reclaim a majority of the window: {before} -> {after}"
        );
        assert!(out.len() < messages.len(), "message count must drop");
        assert!(
            out.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.contains("SUMMARY-OF-EARLIER-TURNS"))),
            "the summary must be present in the compressed output"
        );
    }
}

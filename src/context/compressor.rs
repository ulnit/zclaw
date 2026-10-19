//! Context compression — aligned with hermes' agent/conversation_compression.py
//!
//! When the conversation exceeds the token budget, the middle turns are
//! summarized by a secondary model call and replaced with a compact
//! "[CONTEXT SUMMARY]" message, keeping the system prompt, the first user
//! message, and the most recent turns intact.

use crate::provider::{Message, Provider, Role, Usage};

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

/// How many provider-confirmed *ineffective* compactions trip the anti-thrash
/// breaker (hermes `_tripped`: `_ineffective_compression_count >= 2`).
const INEFFECTIVE_COMPRESSION_LIMIT: usize = 2;

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
    /// Provider-reported prompt tokens from the most recent response
    /// (hermes `last_real_prompt_tokens`). `None` until the first reading.
    last_real_prompt_tokens: Option<u32>,
    /// `messages.len()` at the moment [`Self::last_real_prompt_tokens`] was
    /// read, so a usage-anchored figure can add only what was appended since
    /// (hermes: "real prompt count + delta of what was appended since").
    anchor_len: usize,
    /// A compaction just replaced history, so the last real reading describes a
    /// transcript that no longer exists (hermes
    /// `awaiting_real_usage_after_compression`). 🔴 Without this latch the stale
    /// reading is still over threshold and the next iteration compresses again —
    /// the death spiral hermes fixed in #4750/#2153.
    awaiting_real_usage: bool,
    /// A completed response carried no usage, so there is nothing to anchor on
    /// (hermes `_provider_omits_usage`, set by `note_usage_less_response`).
    provider_omits_usage: bool,
    /// Armed when a compaction runs; judged on the next real reading.
    verify_cleared_threshold: bool,
    /// Provider-confirmed ineffective compactions in a row.
    ineffective_count: usize,
    /// Anti-thrash breaker tripped: stop compressing, it cannot help.
    breaker_tripped: bool,
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
            last_real_prompt_tokens: None,
            anchor_len: 0,
            awaiting_real_usage: false,
            provider_omits_usage: false,
            verify_cleared_threshold: false,
            ineffective_count: 0,
            breaker_tripped: false,
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
    ///
    /// 🔴 **Usage-anchored, not estimate-anchored** (hermes #104555: "Compaction
    /// triggers on real token counts — provider-reported usage, not bytes/4").
    /// [`Self::estimate_tokens`] counts only message content and tool-call
    /// arguments, so it systematically **under-counts** the real prompt: the
    /// system prompt and the tool *schemas* never appear in `messages` at all
    /// (hermes #14695 puts that incompressible floor at 20–30K tokens with 50+
    /// tools). Once a provider reading exists it is authoritative and the
    /// estimate only covers what was appended since.
    ///
    /// Two guards make this safe rather than merely more accurate:
    /// * `awaiting_real_usage` — right after a compaction the last reading
    ///   describes a transcript that no longer exists. Compressing on it again is
    ///   the death spiral hermes fixed in #4750/#2153.
    /// * `breaker_tripped` — after two provider-confirmed *ineffective*
    ///   compactions, stop. The incompressible floor alone exceeds the threshold,
    ///   so shrinking messages can never help and every pass would be wasted
    ///   money and lost context.
    ///
    /// **Deliberately not ported:** hermes' `should_defer_preflight_to_real_usage`
    /// lets a rough over-estimate *wait one request* for the provider to
    /// adjudicate, and its docstring says so explicitly — "Let the provider
    /// adjudicate; its overflow error still triggers reactive recovery". ulnclaw
    /// has **no reactive context-overflow recovery**, so deferring would turn a
    /// preventive compaction into a hard 400. Without that recovery, the estimate
    /// must keep its preventive role.
    pub fn needs_compression(&self, messages: &[Message]) -> bool {
        if self.breaker_tripped || self.awaiting_real_usage {
            return false;
        }
        match self.last_real_prompt_tokens {
            // Anchored: the provider's own count for the bulk, plus an estimate
            // of only what was appended since that reading.
            Some(real) => {
                let from = self.anchor_len.min(messages.len());
                let delta = Self::estimate_tokens(&messages[from..]);
                real as usize + delta > self.max_context_tokens
            }
            // No reading yet (first request of the run, or a provider that omits
            // usage): the rough estimate is the only signal available.
            None => Self::estimate_tokens(messages) > self.max_context_tokens,
        }
    }

    /// Feed the provider's real usage back in. Call once per **completed**
    /// response, with `request_messages` = `messages.len()` as sent.
    ///
    /// Mirrors hermes `update_from_response` + `_apply_real_prompt_verdict`:
    /// the latch is consumed on *every* completed response (even one without
    /// usage) so it can never stay armed forever (#2153), and the
    /// ineffective-compaction verdict is judged **here**, because this is the
    /// only place that sees the provider's real count for a just-compacted
    /// conversation. hermes is explicit that the verdict must not live in the
    /// trigger check: that runs twice per turn against two different measures
    /// (rough preflight vs real post-response, #36718) and the rough one can dip
    /// below the threshold and reset the strike every turn, re-opening the loop.
    pub fn note_response_usage(&mut self, usage: Option<&Usage>, request_messages: usize) {
        match usage {
            Some(u) if u.prompt_tokens > 0 => {
                let real = u.prompt_tokens as usize;
                self.last_real_prompt_tokens = Some(u.prompt_tokens);
                self.anchor_len = request_messages;
                // A real reading proves this provider does report usage.
                self.provider_omits_usage = false;
                if real < self.max_context_tokens {
                    // Any real reading below the trigger proves the prompt fits.
                    self.ineffective_count = 0;
                    self.breaker_tripped = false;
                } else if self.verify_cleared_threshold {
                    // Compaction ran and the provider still reports over threshold.
                    self.ineffective_count += 1;
                    if self.ineffective_count >= INEFFECTIVE_COMPRESSION_LIMIT {
                        self.breaker_tripped = true;
                        tracing::warn!(
                            "compaction did not clear the threshold {}/{} times: {} real prompt \
                             tokens still >= {} budget — the incompressible prompt (system + tool \
                             schemas) may already exceed it, so shrinking messages cannot help; \
                             disabling auto-compression for this run",
                            self.ineffective_count,
                            INEFFECTIVE_COMPRESSION_LIMIT,
                            real,
                            self.max_context_tokens
                        );
                    }
                }
            }
            // No usage in a completed response: nothing to anchor on, so rough
            // estimates must decide instead of waiting forever (#2153).
            _ => self.provider_omits_usage = true,
        }
        // Consume both flags on every completed response so neither can stay armed.
        self.awaiting_real_usage = false;
        self.verify_cleared_threshold = false;
    }

    /// A compaction just replaced the history. Arm the latch and discard the now
    /// stale reading — it describes a transcript that no longer exists.
    pub fn note_compaction_ran(&mut self) {
        self.awaiting_real_usage = true;
        self.verify_cleared_threshold = true;
        self.last_real_prompt_tokens = None;
        self.anchor_len = 0;
    }

    /// Whether the anti-thrash breaker has stopped auto-compression.
    pub fn breaker_tripped(&self) -> bool {
        self.breaker_tripped
    }

    /// Whether a completed response proved this provider omits `usage`.
    pub fn provider_omits_usage(&self) -> bool {
        self.provider_omits_usage
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

    // ---------------------------------------------------------------
    // P3-5b: usage-anchored trigger + latch + anti-thrash breaker
    // ---------------------------------------------------------------

    fn usage(prompt: u32) -> crate::provider::Usage {
        crate::provider::Usage {
            prompt_tokens: prompt,
            completion_tokens: 0,
            total_tokens: prompt,
        }
    }

    /// 🔴 The core fix: a conversation whose *content* is small but whose real
    /// prompt is over budget (system prompt + tool schemas are an incompressible
    /// floor that never appears in `messages`) MUST trigger. The old chars/4
    /// estimate could never see this — hermes #104555/#14695.
    #[test]
    fn real_usage_triggers_where_the_estimate_could_not() {
        let mut c = ContextCompressor::new(10_000);
        let messages = convo(6, 40); // ~60 tokens of content: far under budget
        assert!(
            !c.needs_compression(&messages),
            "sanity: without a reading, a tiny transcript must not trigger"
        );
        // Provider says the real prompt is 12k (schemas + system prompt included).
        c.note_response_usage(Some(&usage(12_000)), messages.len());
        assert!(
            c.needs_compression(&messages),
            "a real over-budget reading must trigger even though content is tiny"
        );
    }

    /// The anchor must add only what was appended *since* the reading — not
    /// re-count the whole transcript (that would double-count everything).
    #[test]
    fn anchor_counts_only_messages_appended_since_the_reading() {
        let mut c = ContextCompressor::new(10_000);
        let messages = convo(10, 400); // ~100 tokens each
        c.note_response_usage(Some(&usage(9_000)), messages.len());
        // Nothing appended: 9_000 + 0 <= 10_000.
        assert!(!c.needs_compression(&messages));
        // Append ~2 messages (~200 tokens): 9_200 <= 10_000 still fits.
        let mut grown = messages.clone();
        grown.push(user(&"y".repeat(400)));
        grown.push(assistant(&"y".repeat(400)));
        assert!(!c.needs_compression(&grown));
        // Append enough to cross: 9_000 + ~1_200 > 10_000.
        for _ in 0..10 {
            grown.push(user(&"y".repeat(400)));
        }
        assert!(
            c.needs_compression(&grown),
            "delta since the anchor must be added to the real reading"
        );
    }

    /// 🔴 The death spiral (hermes #4750/#2153): after a compaction the last
    /// reading describes a transcript that no longer exists. Without the latch the
    /// next iteration would compress again on stale numbers, forever.
    #[test]
    fn compaction_latch_suppresses_the_next_check_until_real_usage_arrives() {
        let mut c = ContextCompressor::new(10_000);
        let messages = convo(20, 400);
        c.note_response_usage(Some(&usage(12_000)), messages.len());
        assert!(c.needs_compression(&messages));

        c.note_compaction_ran();
        assert!(
            !c.needs_compression(&messages),
            "armed latch must suppress compression on the stale reading"
        );

        // A completed response consumes the latch — even one with no usage,
        // otherwise it could stay armed forever (#2153).
        c.note_response_usage(None, messages.len());
        assert!(
            c.provider_omits_usage(),
            "a usage-less response must be recorded"
        );
        // Latch consumed → back to the estimate path (no reading was taken).
        assert!(!c.needs_compression(&convo(2, 40)));
    }

    /// The latch is consumed by ANY completed response, including one that does
    /// carry usage (hermes: "consume the flag once real usage arrives even
    /// without prompt_tokens, so it can't stay armed").
    #[test]
    fn latch_is_consumed_by_a_response_that_carries_usage() {
        let mut c = ContextCompressor::new(10_000);
        c.note_compaction_ran();
        assert!(!c.needs_compression(&convo(4, 40)));
        c.note_response_usage(Some(&usage(500)), 4);
        // Latch gone and a fresh under-budget reading anchors the next decision.
        assert!(!c.needs_compression(&convo(4, 40)));
        assert!(!c.breaker_tripped());
    }

    /// 🔴 Anti-thrash: two provider-confirmed *ineffective* compactions trip the
    /// breaker. The incompressible floor (system prompt + tool schemas) alone
    /// exceeds the threshold, so shrinking messages can never help (hermes #14695).
    #[test]
    fn breaker_trips_after_two_provider_confirmed_ineffective_compactions() {
        let mut c = ContextCompressor::new(10_000);
        let messages = convo(20, 400);

        // Compaction 1: runs, provider still reports over threshold.
        c.note_compaction_ran();
        c.note_response_usage(Some(&usage(11_000)), messages.len());
        assert!(!c.breaker_tripped(), "one strike is not enough");
        assert!(c.needs_compression(&messages), "still compressible");

        // Compaction 2: runs, provider STILL reports over threshold → trip.
        c.note_compaction_ran();
        c.note_response_usage(Some(&usage(10_500)), messages.len());
        assert!(c.breaker_tripped(), "two confirmed strikes must trip");
        assert!(
            !c.needs_compression(&messages),
            "a tripped breaker must stop compressing — it cannot help"
        );
    }

    /// The breaker must reset when a real reading comes back under threshold,
    /// otherwise one bad stretch would disable compression for the whole run.
    #[test]
    fn breaker_resets_when_a_real_reading_comes_back_under_threshold() {
        let mut c = ContextCompressor::new(10_000);
        let messages = convo(20, 400);
        c.note_compaction_ran();
        c.note_response_usage(Some(&usage(11_000)), messages.len());
        c.note_compaction_ran();
        c.note_response_usage(Some(&usage(10_500)), messages.len());
        assert!(c.breaker_tripped());
        // A later response finally under the budget clears it.
        c.note_response_usage(Some(&usage(4_000)), messages.len());
        assert!(
            !c.breaker_tripped(),
            "a good reading must clear the breaker"
        );
        assert!(!c.needs_compression(&convo(2, 40)));
    }

    /// The verdict must be judged on the REAL reading only, never on the rough
    /// estimate: hermes is explicit that keying the strike on an estimate lets it
    /// reset every turn and re-opens the loop (#36718).
    #[test]
    fn ineffective_strike_needs_the_verify_flag_armed_by_a_compaction() {
        let mut c = ContextCompressor::new(10_000);
        let messages = convo(20, 400);
        // Two over-threshold readings WITHOUT any compaction in between must not
        // trip: nothing was ineffective, compression simply has not run yet.
        c.note_response_usage(Some(&usage(11_000)), messages.len());
        c.note_response_usage(Some(&usage(11_000)), messages.len());
        assert!(
            !c.breaker_tripped(),
            "strikes require a compaction to have actually run first"
        );
        assert!(
            c.needs_compression(&messages),
            "and it must still be willing to try"
        );
    }

    /// A `prompt_tokens: 0` reading must not be mistaken for a real one — that
    /// would anchor at 0 and disable compression entirely.
    #[test]
    fn zero_prompt_tokens_is_treated_as_no_reading() {
        let mut c = ContextCompressor::new(10_000);
        let messages = convo(4, 40);
        c.note_response_usage(Some(&usage(0)), messages.len());
        assert!(
            c.provider_omits_usage(),
            "0 prompt tokens = nothing to anchor on"
        );
        // Estimate path still decides.
        assert!(!c.needs_compression(&messages));
        let mut big = ContextCompressor::new(10);
        big.note_response_usage(Some(&usage(0)), messages.len());
        assert!(
            big.needs_compression(&messages),
            "with no usable reading the estimate must still be able to fire"
        );
    }

    /// The anchor index must never exceed the current length (a compaction can
    /// shrink the transcript below the recorded anchor).
    #[test]
    fn anchor_len_is_clamped_when_the_transcript_shrinks() {
        let mut c = ContextCompressor::new(10_000);
        c.note_response_usage(Some(&usage(9_000)), 50);
        let short = convo(3, 40);
        // 50 > 3: clamping must not panic on the slice.
        assert!(!c.needs_compression(&short));
    }

    /// Default construction starts clean: no reading, no latch, no breaker.
    #[test]
    fn fresh_compressor_starts_on_the_estimate_path() {
        let c = ContextCompressor::new(10_000);
        assert!(!c.breaker_tripped());
        assert!(!c.provider_omits_usage());
        let messages = convo(4, 40);
        assert!(!c.needs_compression(&messages));
        let big = convo(4, 400_000); // ~400k tokens
        assert!(c.needs_compression(&big), "estimate path must still fire");
    }
}

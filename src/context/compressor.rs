//! Context compression — aligned with hermes' agent/conversation_compression.py
//!
//! When the conversation exceeds the token budget, the middle turns are
//! summarized by a secondary model call and replaced with a compact
//! "[CONTEXT SUMMARY]" message, keeping the system prompt, the first user
//! message, and the most recent turns intact.

use crate::provider::{Message, Provider, Role, Usage};
use std::sync::Arc;

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

/// How long the tripped breaker stays closed before it allows ONE probe
/// (hermes `_ANTI_THRASH_RECOVERY_SECONDS = 300.0`, `context_compressor.py:2726`).
///
/// 🔴 The back-off must not be permanent (#14694): the tripped verdict was made
/// against the transcript *as it existed then*, but the conversation keeps
/// growing and can accumulate plenty of compressible material later. Without a
/// recovery path the session never auto-compacts again and rides into the
/// provider's hard context limit. Recovery is a *probation probe*, not a reset:
/// the count drops to 1 strike, so if the probe is ineffective again the very
/// next verdict re-trips — at most one compaction attempt per window, bounded
/// rather than thrashing.
pub const ANTI_THRASH_RECOVERY_SECONDS: f64 = 300.0;

/// Durable per-session anti-thrash state, bound to one session row.
///
/// hermes gives its compressor `_session_db` + `_session_id` and reaches the
/// columns through `_durable_read`/`_durable_write`. This trait is the same
/// contract expressed the Rust-natural way, so `context/` does not have to
/// depend on `session/` (the agent, which holds both, supplies the impl).
///
/// 🔴 Why durable at all: the compressor is **rebuilt** — P3-5d's registry
/// LRU-evicts past 64 sessions, and any restart rebuilds it. A process-local
/// deadline restarts the wait on every rebuild, so a tripped session never
/// earns its probe and "stayed blocked forever" (hermes #100185). The strike
/// count is durable for the same reason and for #54923 "restart must not
/// disarm" / #69872.
pub trait CompressionGuardStore: Send + Sync {
    /// Persisted strike count; 0 when absent.
    fn ineffective_count(&self) -> i64;
    /// Persist the strike count (best-effort; must not fail a turn).
    fn set_ineffective_count(&self, count: i64);
    /// Persisted wall-clock recovery deadline (epoch seconds); 0.0 = unarmed.
    fn recovery_deadline(&self) -> f64;
    /// Persist (or with 0.0, disarm) the recovery deadline.
    fn set_recovery_deadline(&self, deadline: f64);
}

/// Wall clock in epoch seconds. A free function so tests can reason about the
/// values they store; the deadline is wall clock by requirement, never
/// monotonic (it is compared across process/compressor rebuilds).
fn wall_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

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
    /// Wall-clock epoch when the tripped breaker next allows one probe
    /// (hermes `_anti_thrash_recovery_deadline`). `0.0` = unarmed. Mirrors the
    /// durable column so the value survives a compressor rebuild — see
    /// [`CompressionGuardStore`].
    recovery_deadline: f64,
    /// Durable per-session anti-thrash state, bound when the agent knows which
    /// session this compressor serves (hermes `_session_db` + `_session_id`).
    /// `None` = an ephemeral run: the clock still works in memory, it just
    /// cannot survive a rebuild.
    guard: Option<Arc<dyn CompressionGuardStore>>,
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
            recovery_deadline: 0.0,
            guard: None,
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
    /// **Deliberately not the place for hermes' preflight deferral:** upstream
    /// keeps `should_compress_info` (`context_compressor.py:2994`) free of
    /// `should_defer_preflight_to_real_usage` — the gate layer
    /// (`turn_preflight_gate.py:97`, `context_engine.py:192`) asks that question
    /// separately. Folding it in here would silently change this predicate's
    /// contract for every caller. See [`Self::defer_preflight`].
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

    /// The preflight gate's question: should preventive compression for THIS
    /// request wait one round for the provider's real usage instead of trusting
    /// the rough estimate?
    ///
    /// hermes asks this in the gate layer, not inside `should_compress`
    /// (`turn_preflight_gate.py:97`, `context_engine.py:192`), so it lives here as
    /// a separate predicate and [`Self::needs_compression`] keeps its contract.
    ///
    /// Usage-anchored figures never defer: the bulk of that number is the
    /// provider's own count, not a guess, so there is nothing to wait for.
    ///
    /// 🔴 **P3-6 is what makes deferring safe.** Until the agent loop recovered
    /// from a provider overflow verdict, skipping a preventive compaction would
    /// have turned it into a hard 400 that failed the turn — which is exactly why
    /// the estimate kept its preventive role before. With reactive recovery in
    /// place (P3-6 slices 1-3) the provider adjudicates: if the rough estimate was
    /// right, the overflow error triggers compaction and a retry anyway.
    pub fn defer_preflight(&self, messages: &[Message]) -> bool {
        if self.last_real_prompt_tokens.is_some() {
            return false;
        }
        self.should_defer_preflight_to_real_usage(Self::estimate_tokens(messages))
    }

    /// True when a whole-context ROUGH estimate over threshold must wait ONE
    /// request for the provider's real usage.
    ///
    /// Port of hermes `should_defer_preflight_to_real_usage`
    /// (`agent/context_compressor.py:2975`), verbatim in order and in each
    /// condition. Callers skip this for usage-anchored figures (real prompt count
    /// + delta of what was appended since), which never defer. A rough figure
    /// defers right after a local or native compaction (the last real reading is
    /// stale — the latch) and on any transcript the anchor does not cover (first
    /// request, rewind/edit-resend, reloaded history): the next response
    /// re-anchors it. It never defers once the provider has proven it omits
    /// usage, or the estimate would be the only signal and compression could
    /// never fire (#2153); the overflow handler compacts reactively in every
    /// case.
    pub fn should_defer_preflight_to_real_usage(&self, rough_tokens: usize) -> bool {
        if rough_tokens < self.max_context_tokens {
            return false;
        }
        if self.awaiting_real_usage {
            return true;
        }
        // Estimate magnitude is not evidence of overflow, even past the full window.
        // Let the provider adjudicate; its overflow error still triggers reactive recovery.
        let anchored = self
            .last_real_prompt_tokens
            .map(|t| t as usize)
            .unwrap_or(0);
        if anchored >= self.max_context_tokens {
            return false;
        }
        !self.provider_omits_usage
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
                    // Any real reading below the trigger proves the prompt fits,
                    // so the strikes clear — durably, because a sibling agent or
                    // a rebuilt compressor reads that row to decide whether it is
                    // still blocked (hermes `_refresh_durable_guards`).
                    self.record_ineffective_verdict(0);
                } else if self.verify_cleared_threshold {
                    // Compaction ran and the provider still reports over threshold.
                    self.record_ineffective_verdict(self.ineffective_count + 1);
                    if self.breaker_tripped {
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

    /// Bind this compressor to a session's durable guard state and load it
    /// (hermes binds `_session_db`/`_session_id` then calls
    /// `_load_anti_thrash_recovery_deadline` + `_load_ineffective_compression_count`
    /// during construction).
    ///
    /// 🔴 This is what makes #54923 "restart must not disarm" hold: a rebuilt
    /// compressor inherits the armed/tripped guard instead of starting clean.
    pub fn bind_guard(&mut self, guard: Arc<dyn CompressionGuardStore>) {
        self.guard = Some(guard);
        self.load_durable_guard_state();
    }

    /// Re-read the durable guard state into memory. Also used as upstream's
    /// `_refresh_durable_guards`: when a gate is about to block, a sibling agent
    /// on the same session row may already have cleared it.
    fn load_durable_guard_state(&mut self) {
        let Some(guard) = self.guard.as_ref() else {
            return;
        };
        let count = guard.ineffective_count().max(0) as usize;
        self.ineffective_count = count;
        self.breaker_tripped = count >= INEFFECTIVE_COMPRESSION_LIMIT;
        self.recovery_deadline = guard.recovery_deadline().max(0.0);
    }

    /// Set the strike count, persisting **only on change** (hermes
    /// `_record_ineffective_compression_verdict`: "persists only on change").
    fn record_ineffective_verdict(&mut self, count: usize) {
        if count == self.ineffective_count {
            return;
        }
        self.ineffective_count = count;
        self.breaker_tripped = count >= INEFFECTIVE_COMPRESSION_LIMIT;
        if let Some(guard) = self.guard.as_ref() {
            guard.set_ineffective_count(count as i64);
        }
    }

    /// Arm/disarm the recovery clock, persisting only on change (hermes
    /// `_set_anti_thrash_recovery_deadline`'s early return keeps an unarmed
    /// evaluation from writing the row on every turn).
    fn set_recovery_deadline(&mut self, deadline: f64) {
        if (deadline - self.recovery_deadline).abs() < f64::EPSILON {
            return;
        }
        self.recovery_deadline = deadline;
        if let Some(guard) = self.guard.as_ref() {
            guard.set_recovery_deadline(deadline);
        }
    }

    /// The recovery clock on in-memory state only (hermes
    /// `_automatic_compression_blocked_locally`).
    fn anti_thrash_blocked_locally(&mut self) -> bool {
        if !self.breaker_tripped {
            // Guard not tripped: disarm any pending clock so a later trip
            // starts a full window.
            self.set_recovery_deadline(0.0);
            return false;
        }
        let now = wall_now();
        if self.recovery_deadline <= 0.0
            || self.recovery_deadline - now > ANTI_THRASH_RECOVERY_SECONDS
        {
            // Unarmed → arm lazily on the first blocked evaluation. The second
            // leg is a backwards clock jump: never wait longer than one window.
            self.set_recovery_deadline(now + ANTI_THRASH_RECOVERY_SECONDS);
        } else if now >= self.recovery_deadline {
            // Window elapsed → allow ONE probation probe by dropping to a
            // single strike, persisted so sibling agents on the same row
            // unblock too. If the probe is ineffective again the very next
            // verdict re-trips, so the worst case is one attempt per window.
            self.set_recovery_deadline(0.0);
            if self.ineffective_count >= INEFFECTIVE_COMPRESSION_LIMIT {
                self.record_ineffective_verdict(1);
            }
            tracing::info!(
                "anti-thrash recovery: {:.0}s elapsed since the guard tripped — allowing one \
                 compaction probe (ineffective={})",
                ANTI_THRASH_RECOVERY_SECONDS,
                self.ineffective_count
            );
            return false;
        }
        tracing::warn!(
            "compression skipped — repeated compaction attempts did not restore healthy context \
             (ineffective={}); auto-compaction will retry once in {:.0}s",
            self.ineffective_count,
            (self.recovery_deadline - now).max(0.0)
        );
        true
    }

    /// Whether auto-compaction is blocked by the anti-thrash guard, running the
    /// recovery clock (hermes `_automatic_compression_blocked`).
    ///
    /// 🔴 Call this **before** [`Self::needs_compression`] each iteration: it is
    /// what clears `breaker_tripped` once the window elapses, so the probe can
    /// actually be taken. Blocked-then-refresh mirrors upstream: durable rows
    /// may have been cleared by another agent since the local read.
    pub fn anti_thrash_blocked(&mut self) -> bool {
        if !self.anti_thrash_blocked_locally() {
            return false;
        }
        self.load_durable_guard_state();
        self.anti_thrash_blocked_locally()
    }

    /// The persisted strike count (test/inspection aid).
    pub fn ineffective_count(&self) -> usize {
        self.ineffective_count
    }

    /// The armed recovery deadline as a wall-clock epoch, `0.0` when unarmed
    /// (test/inspection aid).
    pub fn recovery_deadline(&self) -> f64 {
        self.recovery_deadline
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

    // ---- P3-5c: anti-thrash recovery clock + durable guard -----------------

    /// In-memory [`CompressionGuardStore`] so the clock is tested without
    /// SQLite or real wall-clock waits — tests pre-seed a past/future deadline.
    /// Mutex (not Cell) because the trait bound is Send + Sync.
    #[derive(Default)]
    struct MockGuard(std::sync::Mutex<(i64, f64)>);

    impl CompressionGuardStore for MockGuard {
        fn ineffective_count(&self) -> i64 {
            self.0.lock().unwrap().0
        }
        fn set_ineffective_count(&self, count: i64) {
            self.0.lock().unwrap().0 = count;
        }
        fn recovery_deadline(&self) -> f64 {
            self.0.lock().unwrap().1
        }
        fn set_recovery_deadline(&self, deadline: f64) {
            self.0.lock().unwrap().1 = deadline;
        }
    }

    /// Trip the breaker the honest way: two provider-confirmed ineffective
    /// compactions (the same setup the existing breaker tests use).
    fn tripped_compressor() -> ContextCompressor {
        let mut c = ContextCompressor::new(10_000);
        let messages = convo(20, 400);
        c.note_compaction_ran();
        c.note_response_usage(Some(&usage(11_000)), messages.len());
        c.note_compaction_ran();
        c.note_response_usage(Some(&usage(10_500)), messages.len());
        assert!(c.breaker_tripped());
        c
    }

    /// A tripped breaker blocks, and the first blocked evaluation arms a
    /// wall-clock deadline ~one window out (hermes arms it lazily).
    #[test]
    fn p35c_tripped_breaker_blocks_and_arms_the_recovery_clock() {
        // 🔴 Seed the DURABLE row first, then bind — production order: the row
        // is already tripped and a (re)built compressor inherits it. Tripping
        // in memory before binding an empty row would let the bind correctly
        // overwrite memory with the durable 0 (#54923 makes the row the
        // authority), which is the behaviour under test, not a setup step.
        let guard = Arc::new(MockGuard::default());
        guard.set_ineffective_count(2);
        let mut c = ContextCompressor::new(10_000);
        c.bind_guard(guard.clone());
        assert!(
            c.breaker_tripped(),
            "the durable tripped state is inherited"
        );

        assert!(c.anti_thrash_blocked(), "tripped => blocked");
        let deadline = guard.recovery_deadline();
        let now = wall_now();
        assert!(deadline > now, "the clock must be armed into the future");
        assert!(
            (deadline - now - ANTI_THRASH_RECOVERY_SECONDS).abs() < 5.0,
            "armed ~one window out, got {:+.1}s",
            deadline - now - ANTI_THRASH_RECOVERY_SECONDS
        );
        // Still blocked while the window is open.
        assert!(c.anti_thrash_blocked());
    }

    /// 🔴 The whole point of #14694: the back-off is NOT permanent. Once the
    /// window elapses, exactly ONE probe is allowed by dropping to a single
    /// strike and disarming the clock — so the next ineffective verdict re-trips
    /// (bounded: one attempt per window, not a thrash).
    #[test]
    fn p35c_elapsed_window_allows_one_probe_and_re_arms_on_failure() {
        let mut c = tripped_compressor();
        let guard = Arc::new(MockGuard::default());
        // Pre-seed a deadline already in the past: the window has elapsed.
        guard.set_recovery_deadline(wall_now() - 1.0);
        guard.set_ineffective_count(2);
        c.bind_guard(guard.clone());

        // Probe allowed (not blocked); strike count dropped to 1; clock disarmed.
        assert!(
            !c.anti_thrash_blocked(),
            "elapsed window must allow a probe"
        );
        assert_eq!(c.ineffective_count(), 1, "one probation strike remains");
        assert_eq!(guard.ineffective_count(), 1, "drop is persisted");
        assert_eq!(c.recovery_deadline(), 0.0, "clock disarmed after the probe");
        assert!(
            !c.breaker_tripped(),
            "a single strike does not hold the breaker"
        );

        // The probe compacts but is ineffective again -> one more strike re-trips.
        let messages = convo(20, 400);
        c.note_compaction_ran();
        c.note_response_usage(Some(&usage(10_500)), messages.len());
        assert!(
            c.breaker_tripped(),
            "an ineffective probe re-trips immediately"
        );
        assert_eq!(guard.ineffective_count(), 2);
    }

    /// 🔴 #54923 "restart must not disarm" + #69872: a rebuilt compressor bound
    /// to the same row inherits the tripped guard. This is what P3-5d's LRU
    /// eviction and any process restart must not undo.
    #[test]
    fn p35d_rebuilt_compressor_inherits_the_durable_tripped_guard() {
        let guard = Arc::new(MockGuard::default());
        guard.set_ineffective_count(2); // durable tripped state
        {
            let mut c = ContextCompressor::new(10_000);
            c.bind_guard(guard.clone());
            assert!(c.breaker_tripped(), "bind inherits the tripped guard");
            assert!(c.anti_thrash_blocked()); // arms the clock durably
        } // c dropped (simulating eviction / restart)
        assert!(guard.ineffective_count() >= 2, "tripped state is durable");
        assert!(guard.recovery_deadline() > 0.0, "deadline is durable");

        // A brand-new compressor on the same row starts blocked, inheriting the
        // SAME armed window rather than restarting it.
        let mut c2 = ContextCompressor::new(10_000);
        c2.bind_guard(guard.clone());
        assert!(c2.breaker_tripped(), "guard survives the rebuild");
        assert!(c2.anti_thrash_blocked());
        assert_eq!(
            c2.recovery_deadline(),
            guard.recovery_deadline(),
            "the rebuilt compressor resumes the same window, not a fresh one"
        );
    }

    /// #100185: a durable deadline already in the past lets a rebuilt compressor
    /// earn its probe immediately — the exact "blocked forever" bug upstream
    /// fixed, which a process-local (monotonic) clock would reintroduce.
    #[test]
    fn p35c_durable_past_deadline_lets_a_rebuilt_compressor_probe() {
        let guard = Arc::new(MockGuard::default());
        guard.set_ineffective_count(2);
        guard.set_recovery_deadline(wall_now() - 10.0); // armed long ago, elapsed

        let mut c = ContextCompressor::new(10_000); // fresh, e.g. after restart
        c.bind_guard(guard.clone());
        assert!(c.breaker_tripped(), "durable tripped state loaded");
        assert!(
            !c.anti_thrash_blocked(),
            "an elapsed durable window must grant the probe after a rebuild"
        );
        assert_eq!(c.ineffective_count(), 1);
    }

    /// Blocked-then-refresh (hermes `_refresh_durable_guards`): when the local
    /// state would block but a sibling agent already cleared the durable row,
    /// the re-read unblocks this compressor too.
    #[test]
    fn p35c_blocked_then_refresh_honours_a_sibling_clearing_the_row() {
        let guard = Arc::new(MockGuard::default());
        guard.set_ineffective_count(2); // durable tripped state
        let mut c = ContextCompressor::new(10_000);
        c.bind_guard(guard.clone());
        // First evaluation arms the clock and blocks.
        assert!(c.anti_thrash_blocked());
        assert!(c.breaker_tripped());

        // A sibling agent on the same session row clears the strikes durably.
        guard.set_ineffective_count(0);
        guard.set_recovery_deadline(0.0);

        // Now blocked-then-refresh re-reads the row and unblocks.
        assert!(
            !c.anti_thrash_blocked(),
            "a sibling clearing the durable row must unblock this compressor"
        );
        assert!(
            !c.breaker_tripped(),
            "the refresh adopted the cleared count"
        );
    }

    /// A backwards clock jump must never make the wait exceed one window
    /// (hermes re-arms when `deadline - now > window`).
    #[test]
    fn p35c_backwards_clock_jump_rearms_to_at_most_one_window() {
        let guard = Arc::new(MockGuard::default());
        guard.set_ineffective_count(2); // durable tripped state
                                        // A deadline absurdly far in the future, as if the wall clock jumped back.
        guard.set_recovery_deadline(wall_now() + 10.0 * ANTI_THRASH_RECOVERY_SECONDS);
        let mut c = ContextCompressor::new(10_000);
        c.bind_guard(guard.clone());
        assert!(c.breaker_tripped(), "bind inherits the tripped guard");
        assert!(c.anti_thrash_blocked());
        let now = wall_now();
        assert!(
            c.recovery_deadline() - now <= ANTI_THRASH_RECOVERY_SECONDS + 5.0,
            "the clock must be clamped to one window, got {:.0}s",
            c.recovery_deadline() - now
        );
    }

    /// With no guard bound (an ephemeral run) the clock still works in memory;
    /// it simply cannot survive a rebuild, which is the correct degraded mode.
    #[test]
    fn p35c_unbound_compressor_runs_the_clock_in_memory_only() {
        let mut c = tripped_compressor(); // no bind_guard
        assert!(c.anti_thrash_blocked(), "still blocks in memory");
        assert!(c.recovery_deadline() > 0.0, "armed in memory");
        assert_eq!(c.ineffective_count(), 2, "count is local-only");
    }

    // ---- P3-6 slice 4: preflight deferral to real usage ---------------------
    //
    // hermes `should_defer_preflight_to_real_usage` (context_compressor.py:2975).
    // Four conditions, in upstream's order — each one tested separately because
    // getting any of them wrong either wastes a summarization call on a
    // transcript that fits, or lets the estimate become the only signal so
    // compression can never fire (#2153).

    /// Below the threshold there is nothing to defer: the estimate alone says
    /// the request fits.
    #[test]
    fn p36_defer_is_false_below_threshold() {
        let c = ContextCompressor::new(10_000);
        assert!(!c.should_defer_preflight_to_real_usage(9_999));
        // And `defer_preflight` agrees on the same transcript.
        assert!(!c.defer_preflight(&convo(4, 40)));
    }

    /// 🔴 Right after a compaction the latch is armed and the last real reading
    /// describes a transcript that no longer exists — the rough figure must wait
    /// one request (#4750 death spiral).
    #[test]
    fn p36_defer_is_true_while_the_compaction_latch_is_armed() {
        let mut c = ContextCompressor::new(10_000);
        c.note_compaction_ran();
        assert!(c.should_defer_preflight_to_real_usage(20_000));
    }

    /// 🔴 Estimate magnitude is not evidence of overflow, even far past the full
    /// window: with no latch armed, let the provider adjudicate. If it really
    /// does not fit, the overflow error drives reactive compaction (slice 3).
    #[test]
    fn p36_defer_lets_the_provider_adjudicate_a_huge_estimate() {
        let c = ContextCompressor::new(10_000);
        // One million tokens of rough estimate — still deferred, by design.
        assert!(c.should_defer_preflight_to_real_usage(1_000_000));
        // And the gate-level predicate agrees on a real over-budget transcript.
        let big = convo(4, 400_000); // ~400k tokens
        assert!(c.needs_compression(&big), "estimate path still fires");
        assert!(
            c.defer_preflight(&big),
            "but the gate must defer it to real usage"
        );
    }

    /// 🔴 Once the provider has proven it omits usage, the estimate is the ONLY
    /// signal there will ever be — deferring would mean compression can never
    /// fire (#2153).
    #[test]
    fn p36_defer_is_false_when_the_provider_omits_usage() {
        let mut c = ContextCompressor::new(10_000);
        // A completed response with no usage proves the omission.
        c.note_response_usage(None, 4);
        assert!(c.provider_omits_usage());
        assert!(!c.should_defer_preflight_to_real_usage(20_000));
        assert!(!c.defer_preflight(&convo(4, 400_000)));
    }

    /// 🔴 A real reading at or above the threshold already proves the context
    /// does not fit — there is nothing left to wait for, so the estimate-based
    /// deferral must not suppress compression.
    #[test]
    fn p36_defer_is_false_once_a_real_reading_is_over_threshold() {
        let mut c = ContextCompressor::new(10_000);
        c.note_response_usage(Some(&usage(12_000)), 4);
        assert!(!c.should_defer_preflight_to_real_usage(20_000));
        // And the gate predicate short-circuits on any anchored transcript,
        // because an anchored figure is not a rough estimate at all.
        assert!(!c.defer_preflight(&convo(4, 400_000)));
    }

    /// 🔴 Usage-anchored figures NEVER defer — hermes is explicit that callers
    /// skip the deferral for (real prompt count + delta appended since), because
    /// the bulk of that number is the provider's own count, not a guess.
    #[test]
    fn p36_anchored_transcripts_never_defer() {
        let mut c = ContextCompressor::new(10_000);
        // A small real reading anchors the compressor.
        c.note_response_usage(Some(&usage(1_000)), 4);
        // The transcript then grows far past the budget, but the figure is
        // anchored: the gate must not defer, and compression must fire.
        let big = convo(40, 400_000);
        assert!(c.needs_compression(&big));
        assert!(!c.defer_preflight(&big));
    }

    /// 🔴 `needs_compression` keeps its original contract — the deferral is
    /// asked by the gate layer (`turn_preflight_gate.py:97`), NOT folded into the
    /// predicate. Folding it in silently changed behaviour for every caller and
    /// broke three pre-existing tests; this pins the separation.
    #[test]
    fn p36_deferral_is_not_folded_into_needs_compression() {
        let c = ContextCompressor::new(10_000);
        let big = convo(4, 400_000);
        assert!(
            c.needs_compression(&big),
            "the predicate itself must not defer"
        );
        assert!(
            c.defer_preflight(&big),
            "the gate predicate does defer — they are separate questions"
        );
    }
}

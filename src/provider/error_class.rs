//! Provider error classification — the slice that decides *compress vs failover*.
//!
//! Port of hermes `agent/error_classifier.py`, scoped to the one decision the
//! reactive context-overflow recovery (P3-6) needs: when a provider rejects a
//! request because the context is too large, the agent must **compact and retry**
//! instead of burning a fallback provider. Upstream names this
//! `FailoverReason.context_overflow` — "Context too large — compress, not
//! failover" (`error_classifier.py:46`).
//!
//! # 🔴 The pattern list is not the contract — the ORDER is
//!
//! Matching `_CONTEXT_OVERFLOW_PATTERNS` alone misclassifies, and the failure mode
//! is *destructive*: compressing throws away conversation history. Every rule
//! below exists to keep a specific non-overflow error out of the compression
//! branch, in upstream's order:
//!
//! * **`max_tokens` is load-bearing in two directions.** It is an overflow
//!   pattern, but GPT-5's `"Unsupported parameter: 'max_tokens'"` is a request
//!   *validation* rejection — compression cannot fix it. Hence
//!   `_REQUEST_VALIDATION_PATTERNS` runs first (`error_classifier.py:1186-1188`).
//! * **Memory ceiling ≠ context window.** oMLX prefill-memory errors say
//!   "reduce context length" in their remediation tail but measure **bytes**, and
//!   compression cannot lower a prefill peak — it burns the budget, re-hits the
//!   wedged server, and ends in a session reset (#52261). → `Overloaded`.
//! * **Empty-response advisories must not enter compression** (:377, and
//!   `_OVERFLOW_AS_5XX_RULES` puts them first for the same reason).
//! * **Encrypted-reasoning replay rejections** trip the overflow wording
//!   ("could not be verified") but need a replay-state strip, not a compaction.
//! * **A 429 that mentions tokens is a throttle.** Bedrock's
//!   "Throttling error: Too many tokens" contains an overflow phrase; upstream
//!   matches rate limit first so the throttle wins (:144).
//!
//! # Ordering differs between the two upstream chains
//!
//! `_by_status` runs **before** `_by_message` in `_STAGES`, and returns a verdict
//! for every known status code — so `_MESSAGE_TAIL_RULES` only ever sees
//! **status-less** errors. The two chains disagree on purpose:
//!
//! * **HTTP 400** (`_classify_400` → `_400_TAIL_RULES`): empty-response →
//!   memory-ceiling → **overflow** → policy → model-not-found → rate-limit →
//!   billing. Overflow precedes rate-limit/billing here.
//! * **5xx / 503 / 529** (`_status_5xx`, `_OVERFLOW_AS_5XX_RULES`):
//!   empty-response → memory-ceiling → **overflow**. llama.cpp reports overflow as
//!   **500**, and busy/model-load OOM arrives as 503, so 5xx overflow is real.
//! * **status-less** (`_by_message` → `_MESSAGE_TAIL_RULES`): overloaded → billing
//!   → rate-limit → empty-response → **overflow** → auth → … Here rate-limit and
//!   billing precede overflow ("overload before rate_limit/billing so 'overloaded'
//!   backs off instead of rotating").
//!
//! Both chains are implemented separately below rather than flattened into one
//! list; flattening would silently change which rule wins for 400s or for
//! status-less errors.
//!
//! # Scope — NOT YET PORTED
//!
//! This module answers exactly one question: *is this a context overflow that
//! compression can fix?* Everything else collapses to [`ProviderErrorClass::Other`]
//! and keeps following the path it follows today (`agent::failover`, or the
//! provider's own retry ladder for `retriable_status`). Deliberately absent, each
//! because it needs upstream machinery the port does not have:
//!
//! * the **large-session heuristic** (`_Ctx.large_session(frac, tokens, messages)`,
//!   used for "generic 400 + large session → probable overflow" and
//!   "disconnect + large session → overflow"). It needs `approx_tokens` /
//!   `context_length` / `num_messages` context, so a pure `f(status, body)`
//!   classifier cannot express it.
//! * `_by_error_code`'s provider-scoped native code tables (Gemini gRPC names,
//!   Anthropic error types — #70414 keeps them scoped so a coincidentally named
//!   code from another backend stays unknown).
//! * the plugin / profile / MoA verdict hooks.
//! * the remaining ~20 `FailoverReason` variants (auth, billing, policy,
//!   image/tool shapes, role alternation, …). Only the two codes
//!   `context_length_exceeded` / `max_tokens_exceeded` are mapped, since both mean
//!   overflow.
//!
//! Matching semantics mirror upstream exactly: the message is **lowercased** once
//! (`msg = (error_msg or "").lower()`, :541) and rules are **substring** tests
//! (`any(p in error_msg for p in patterns)`, `_first_match`).

/// What a provider rejection means for recovery.
///
/// Only the distinction that changes behaviour is modelled; upstream's 25-variant
/// `FailoverReason` is not replicated because nothing in the port consumes the
/// other variants yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorClass {
    /// Context too large — **compress, do not fail over**
    /// (`FailoverReason.context_overflow`, `should_compress=True`).
    ContextOverflow,
    /// The request body itself is too large (HTTP 413 /
    /// `FailoverReason.payload_too_large`). Also `should_compress=True` upstream,
    /// but 🔴 it has its own byte-scored recovery path and must not be merged with
    /// `ContextOverflow` (`chat_completion_helpers.py:3929-3931` scopes that branch
    /// to `context_overflow` ONLY). Kept distinct here so the agent can route it
    /// separately once that path is ported.
    PayloadTooLarge,
    /// Provider is overloaded / at capacity — back off, do not compress.
    Overloaded,
    /// Throttled or out of credit — retry/rotate, never compress.
    RateLimitOrBilling,
    /// Anything else. Stays on today's path (failover / provider retry ladder).
    Other,
}

impl ProviderErrorClass {
    /// Whether reactive compaction is the correct recovery.
    pub fn should_compress(self) -> bool {
        matches!(self, Self::ContextOverflow | Self::PayloadTooLarge)
    }
}

/// hermes `_CONTEXT_OVERFLOW_PATTERNS` (`error_classifier.py:253-266`), verbatim
/// and in order. Includes the Chinese wordings and the Together/Fireworks phrasing
/// ("Input length 131393 exceeds the maximum allowed input length of 131040
/// tokens"), which no other pattern matches — ported there from
/// anomalyco/opencode#37848.
const CONTEXT_OVERFLOW_PATTERNS: &[&str] = &[
    "context length",
    "context size",
    "maximum context",
    "token limit",
    "too many tokens",
    "reduce the length",
    "exceeds the limit",
    "context window",
    "prompt is too long",
    "prompt exceeds max length",
    "max_tokens",
    "maximum number of tokens",
    "exceeds the max_model_len",
    "max_model_len",
    "prompt length",
    "input is too long",
    "maximum model length",
    "context length exceeded",
    "truncating input",
    "slot context",
    "n_ctx_slot",
    "超过最大长度",
    "上下文长度",
    "tokens in request more than max tokens allowed",
    "max input token",
    "input token",
    "exceeds the maximum number of input tokens",
    "maximum allowed input length",
];

/// hermes `_MEMORY_CEILING_PATTERNS` (:238-244). Every token names memory or
/// allocation in **bytes**, never a token count, which is what keeps this list
/// disjoint from the overflow patterns. Must be checked BEFORE overflow and before
/// the usage-limit disambiguation — "memory limit exceeded" contains
/// "limit exceeded", which would otherwise read as billing.
const MEMORY_CEILING_PATTERNS: &[&str] = &[
    "memory guard",
    "memory limit exceeded",
    "memory_guard_tier",
    "dynamic ceiling",
    "memory ceiling",
    "available memory",
    "out of memory",
    "insufficient memory",
    "prefill would require",
    "predicted peak would",
    "prefill safety cap",
    "metal_cap",
];

/// hermes `_MEMORY_CEILING_ERROR_CODES` (:248-250).
const MEMORY_CEILING_ERROR_CODES: &[&str] = &[
    "prefill_memory_exceeded",
    "prefill_memory_aborted",
    "omlx_prefill_memory_exceeded",
];

/// hermes `_EMPTY_PROVIDER_RESPONSE_PATTERNS` (:379-382). Checked before overflow
/// on every chain: an empty-response advisory is a server fault, and compressing
/// for it destroys history for nothing.
const EMPTY_PROVIDER_RESPONSE_PATTERNS: &[&str] = &[
    "returned an empty response",
    "empty response despite retries",
    "provider returned an empty response",
    "model returning empty responses",
    "empty response stream",
];

/// hermes `_RATE_LIMIT_PATTERNS` (:145-151).
const RATE_LIMIT_PATTERNS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "too many requests",
    "throttled",
    "requests per minute",
    "tokens per minute",
    "requests per day",
    "try again in",
    "please retry after",
    "resource exhausted",
    "resource_exhausted",
    "resource-exhausted",
    "resourceexhausted",
    "rate increased too quickly",
    "throttlingexception",
    "too many concurrent requests",
    "servicequotaexceededexception",
    "throttling",
];

/// hermes `_BILLING_PATTERNS` (:103-118). The nested `_FREE_TIER_REFUSAL_PATTERNS`
/// expansion is NOT included — it is a separate list this port has not read yet, so
/// free-tier refusals fall through to `Other` rather than to a guessed billing
/// match. Recorded as NOT YET PORTED rather than invented.
const BILLING_PATTERNS: &[&str] = &[
    "insufficient credits",
    "insufficient_quota",
    "insufficient balance",
    "credit balance",
    "credits exhausted",
    "credits have been exhausted",
    "requires available credits",
    "account balance is too low",
    "no usable credits",
    "top up your credits",
    "payment required",
    "billing hard limit",
    "exceeded your current quota",
    "account is deactivated",
    "plan does not include",
    "out of extra usage",
    "out of funds",
    "run out of funds",
    "balance_depleted",
    "budget limit exceeded",
    "hard billing limit",
];

/// hermes `_OVERLOADED_PATTERNS` (:156-164).
const OVERLOADED_PATTERNS: &[&str] = &[
    "overloaded",
    "temporarily overloaded",
    "service is temporarily overloaded",
    "service may be temporarily overloaded",
    "server is overloaded",
    "server overloaded",
    "server overload",
    "server_overload",
    "service overloaded",
    "service is overloaded",
    "upstream overloaded",
    "currently overloaded",
    "upstream model provider is temporarily unavailable. please try again in a moment.",
    "at capacity",
    "over capacity",
];

/// hermes `_PAYLOAD_TOO_LARGE_PATTERNS` (:176-182).
const PAYLOAD_TOO_LARGE_PATTERNS: &[&str] = &[
    "request entity too large",
    "payload too large",
    "error code: 413",
    "request_too_large",
    "request exceeds the maximum size",
];

/// hermes `_REQUEST_VALIDATION_PATTERNS` (:320-323). 🔴 Must precede overflow:
/// `"Unsupported parameter: 'max_tokens'"` contains an overflow pattern but
/// compression cannot fix a rejected request shape.
const REQUEST_VALIDATION_PATTERNS: &[&str] = &[
    "unknown parameter",
    "unsupported parameter",
    "unrecognized request argument",
    "invalid_request_error",
    "unknown_parameter",
    "unsupported_parameter",
];

/// hermes `_400_VALIDATION_CODES` (:665).
const VALIDATION_ERROR_CODES: &[&str] = &["unknown_parameter", "unsupported_parameter"];

/// Structured error codes that mean overflow regardless of wording
/// (hermes `_ERROR_CODE_VERDICTS`, :645).
const OVERFLOW_ERROR_CODES: &[&str] = &["context_length_exceeded", "max_tokens_exceeded"];

/// Markers for an encrypted-reasoning replay rejection. 🔴 Must precede overflow:
/// upstream comments that `"encrypted content … could not be verified"` otherwise
/// trips the overflow wording, and the fix is stripping replay state, not
/// compacting (:1157-1171).
const INVALID_ENCRYPTED_MARKERS: &[&str] = &[
    "invalid_encrypted_content",
    "could not decrypt the provided encrypted_content",
    "encrypted content could not be decrypted or parsed",
    "conflicting authenticated continuation identities",
];

/// hermes `_USAGE_LIMIT_PATTERNS` (:~430) — used for the 402 disambiguation.
const USAGE_LIMIT_PATTERNS: &[&str] = &[
    "usage limit",
    "quota",
    "limit exceeded",
    "key limit exceeded",
];

/// hermes `_USAGE_LIMIT_TRANSIENT_SIGNALS` — a periodic quota says "try again in
/// 5 minutes", which is a throttle, not billing.
const USAGE_LIMIT_TRANSIENT_SIGNALS: &[&str] = &[
    "try again",
    "retry",
    "resets at",
    "reset in",
    "resets in",
    "reset after",
    "available in",
    "wait",
    "requests remaining",
    "periodic",
    "window",
    "per minute",
    "per second",
];

/// `_first_match`: true when any pattern is a substring of the (already
/// lowercased) message.
fn any_of(haystack: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| haystack.contains(p))
}

/// Classify a provider rejection.
///
/// `status` is the HTTP status when the provider gave one (`None` for a
/// status-less transport/API error); `message` is the provider's error text;
/// `error_code` is a structured code if the body carried one. The message is
/// lowercased here, exactly as upstream does once per classification.
///
/// 🔴 Order is the contract — see the module docs for why each guard exists.
pub fn classify_provider_error(
    status: Option<u16>,
    message: &str,
    error_code: Option<&str>,
) -> ProviderErrorClass {
    let msg = message.to_lowercase();
    let code = error_code.unwrap_or("").to_lowercase();

    // Structured codes win the same way `_ERROR_CODE_VERDICTS` settles a rejection
    // whose wording a proxy flattened away.
    if OVERFLOW_ERROR_CODES.contains(&code.as_str()) {
        return ProviderErrorClass::ContextOverflow;
    }
    if MEMORY_CEILING_ERROR_CODES.contains(&code.as_str()) {
        return ProviderErrorClass::Overloaded;
    }

    match status {
        // ── HTTP 400: `_classify_400` ────────────────────────────────────
        Some(400) => classify_400(&msg, &code),
        // ── 413: payload size, its own recovery path ─────────────────────
        Some(413) => ProviderErrorClass::PayloadTooLarge,
        // ── 402: periodic quota vs exhausted credit ──────────────────────
        Some(402) => classify_usage_limit(&msg),
        // ── 429: throttle. 🔴 Bedrock's body says "Too many tokens", which is
        // an overflow pattern — the status settles it first, so throttle wins.
        Some(429) => ProviderErrorClass::RateLimitOrBilling,
        // ── 503 / 529: `_OVERFLOW_AS_5XX_RULES or _V_OVERLOADED` ─────────
        Some(503) | Some(529) => overflow_as_5xx(&msg).unwrap_or(ProviderErrorClass::Overloaded),
        // ── 500 / 502: `_status_5xx` — llama.cpp reports overflow as 500 ──
        Some(500) | Some(502) => {
            if any_of(&msg, REQUEST_VALIDATION_PATTERNS)
                || VALIDATION_ERROR_CODES.contains(&code.as_str())
            {
                return ProviderErrorClass::Other;
            }
            overflow_as_5xx(&msg).unwrap_or(ProviderErrorClass::Other)
        }
        // ── Any other status: no overflow decision to make ───────────────
        Some(_) => ProviderErrorClass::Other,
        // ── Status-less: `_by_message` (HEAD rules, then 402 disambiguation,
        // then the TAIL chain where rate-limit/billing precede overflow) ───
        None => classify_statusless(&msg),
    }
}

/// hermes `_OVERFLOW_AS_5XX_RULES` (:~600): empty-response → memory-ceiling →
/// overflow. Used by 500/502 (`_status_5xx`) and 503/529.
fn overflow_as_5xx(msg: &str) -> Option<ProviderErrorClass> {
    if any_of(msg, EMPTY_PROVIDER_RESPONSE_PATTERNS) {
        return Some(ProviderErrorClass::Other);
    }
    if any_of(msg, MEMORY_CEILING_PATTERNS) {
        return Some(ProviderErrorClass::Overloaded);
    }
    if any_of(msg, CONTEXT_OVERFLOW_PATTERNS) {
        return Some(ProviderErrorClass::ContextOverflow);
    }
    None
}

/// hermes `_classify_400`, restricted to the rules that protect the overflow
/// decision. Returns `Other` for the request-shape rejections it recognises
/// (image/tool, codex entitlement, malformed tool args, role alternation) since
/// none of them are compression's business.
fn classify_400(msg: &str, code: &str) -> ProviderErrorClass {
    // Invalid encrypted reasoning replay — before overflow, whose wording it trips.
    if code == "invalid_encrypted_content"
        || any_of(msg, INVALID_ENCRYPTED_MARKERS)
        || (msg.contains("encrypted content for item") && msg.contains("could not be verified"))
        || (msg.contains("encrypted_content") && msg.contains("was not issued to this caller"))
    {
        return ProviderErrorClass::Other;
    }
    // 🔴 Request validation — before overflow, because GPT-5's
    // "Unsupported parameter: 'max_tokens'" contains an overflow pattern.
    if any_of(msg, REQUEST_VALIDATION_PATTERNS) || VALIDATION_ERROR_CODES.contains(&code) {
        return ProviderErrorClass::Other;
    }
    // Memory ceiling by code (a proxy may have stripped the wording).
    if MEMORY_CEILING_ERROR_CODES.contains(&code) {
        return ProviderErrorClass::Overloaded;
    }
    // `_400_TAIL_RULES` = `_OVERFLOW_AS_5XX_RULES` + (policy, model-not-found,
    // rate-limit, billing). On this chain overflow precedes rate-limit/billing.
    if let Some(class) = overflow_as_5xx(msg) {
        return class;
    }
    if any_of(msg, RATE_LIMIT_PATTERNS) {
        return ProviderErrorClass::RateLimitOrBilling;
    }
    if any_of(msg, BILLING_PATTERNS) {
        return ProviderErrorClass::RateLimitOrBilling;
    }
    // Upstream's last resort here is the large-session heuristic ("generic 400 +
    // large session → probable overflow"), which needs session-size context this
    // pure classifier does not have → NOT YET PORTED. Falls through to Other,
    // i.e. today's failover behaviour, which is the safe direction.
    ProviderErrorClass::Other
}

/// hermes `_by_message` for a status-less error: HEAD rules, then the usage-limit
/// disambiguation, then `_MESSAGE_TAIL_RULES`.
fn classify_statusless(msg: &str) -> ProviderErrorClass {
    // HEAD: memory ceiling → payload too large. (role-alternation and
    // stream-render rules are `Other` for our purposes, so they are not listed.)
    if any_of(msg, MEMORY_CEILING_PATTERNS) {
        return ProviderErrorClass::Overloaded;
    }
    if any_of(msg, PAYLOAD_TOO_LARGE_PATTERNS) {
        return ProviderErrorClass::PayloadTooLarge;
    }
    // A status-less usage limit gets the same disambiguation as a 402.
    if any_of(msg, USAGE_LIMIT_PATTERNS) {
        return classify_usage_limit(msg);
    }
    // TAIL: overloaded → billing → rate-limit → empty-response → OVERFLOW.
    // 🔴 Here rate-limit and billing precede overflow, the opposite of the 400
    // chain — upstream's comment: "Overload before rate_limit/billing so
    // 'overloaded' backs off instead of rotating".
    if any_of(msg, OVERLOADED_PATTERNS) {
        return ProviderErrorClass::Overloaded;
    }
    if any_of(msg, BILLING_PATTERNS) || any_of(msg, RATE_LIMIT_PATTERNS) {
        return ProviderErrorClass::RateLimitOrBilling;
    }
    if any_of(msg, EMPTY_PROVIDER_RESPONSE_PATTERNS) {
        return ProviderErrorClass::Other;
    }
    if any_of(msg, CONTEXT_OVERFLOW_PATTERNS) {
        return ProviderErrorClass::ContextOverflow;
    }
    ProviderErrorClass::Other
}

/// hermes `_classify_402`: "usage limit, try again in 5 minutes" is a periodic
/// quota (throttle), not exhausted credit (billing).
///
/// Upstream returns `_V_RATE_LIMIT` for the transient case and `_V_BILLING`
/// otherwise — two different recovery strategies (back off on the same key vs
/// rotate the credential). Both collapse to
/// [`ProviderErrorClass::RateLimitOrBilling`] here because this port has no
/// credential rotation, and the only decision this classifier serves is
/// *compress vs fail over*: neither variant may ever reach the compression
/// branch. Splitting them is part of porting the full `FailoverReason` enum
/// (NOT YET PORTED).
///
/// The two pattern lists stay defined and tested so the disambiguation is
/// visible and the split is a one-line change when rotation lands.
fn classify_usage_limit(msg: &str) -> ProviderErrorClass {
    let transient = any_of(msg, USAGE_LIMIT_PATTERNS) && any_of(msg, USAGE_LIMIT_TRANSIENT_SIGNALS);
    tracing::debug!(
        transient,
        "usage-limit rejection classified as throttle/billing (never compression)"
    );
    ProviderErrorClass::RateLimitOrBilling
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(status: Option<u16>, msg: &str) -> ProviderErrorClass {
        classify_provider_error(status, msg, None)
    }

    // ── genuine overflow is detected on every chain ──────────────────────────

    #[test]
    fn overflow_400_openai_wording() {
        assert_eq!(
            c(
                Some(400),
                "This model's maximum context length is 8192 tokens"
            ),
            ProviderErrorClass::ContextOverflow
        );
    }

    #[test]
    fn overflow_500_llama_cpp_reports_it_as_a_server_error() {
        // llama.cpp reports overflow as 500 (error_classifier.py:596).
        assert_eq!(
            c(Some(500), "prompt is too long: 9000 tokens > 8192 maximum"),
            ProviderErrorClass::ContextOverflow
        );
    }

    #[test]
    fn overflow_503_and_529_are_real_overflow_not_overload() {
        assert_eq!(
            c(Some(503), "context length exceeded"),
            ProviderErrorClass::ContextOverflow
        );
        assert_eq!(
            c(Some(529), "reduce the length of your input"),
            ProviderErrorClass::ContextOverflow
        );
    }

    #[test]
    fn overflow_statusless_transport_error() {
        assert_eq!(
            c(None, "input is too long for this model"),
            ProviderErrorClass::ContextOverflow
        );
    }

    #[test]
    fn overflow_chinese_wording() {
        assert_eq!(
            c(Some(400), "输入超过最大长度限制"),
            ProviderErrorClass::ContextOverflow
        );
        assert_eq!(
            c(Some(400), "上下文长度不足"),
            ProviderErrorClass::ContextOverflow
        );
    }

    #[test]
    fn overflow_together_fireworks_wording() {
        // No other pattern matches this phrasing (port of anomalyco/opencode#37848).
        assert_eq!(
            c(
                Some(400),
                "Input length 131393 exceeds the maximum allowed input length of 131040 tokens."
            ),
            ProviderErrorClass::ContextOverflow
        );
    }

    #[test]
    fn overflow_structured_code_wins_over_missing_wording() {
        // A proxy flattened the body; the code still says overflow.
        assert_eq!(
            classify_provider_error(Some(400), "Error", Some("context_length_exceeded")),
            ProviderErrorClass::ContextOverflow
        );
        assert_eq!(
            classify_provider_error(Some(400), "Error", Some("max_tokens_exceeded")),
            ProviderErrorClass::ContextOverflow
        );
    }

    // ── 🔴 the guards: things that mention tokens but are NOT overflow ───────

    #[test]
    fn unsupported_max_tokens_parameter_is_validation_not_overflow() {
        // 🔴 "max_tokens" is an overflow pattern; this 400 is a request-shape
        // rejection and compression cannot fix it (error_classifier.py:1186).
        assert_eq!(
            c(
                Some(400),
                "Unsupported parameter: 'max_tokens' is not supported with this model"
            ),
            ProviderErrorClass::Other
        );
        assert_eq!(
            c(Some(400), "unknown parameter: temperature"),
            ProviderErrorClass::Other
        );
    }

    #[test]
    fn bedrock_throttle_mentioning_tokens_is_a_rate_limit() {
        // 🔴 "Throttling error: Too many tokens" contains an overflow phrase;
        // rate limit is matched first so the throttle wins (:144).
        assert_eq!(
            c(Some(429), "Throttling error: Too many tokens, please retry"),
            ProviderErrorClass::RateLimitOrBilling
        );
        // And on the status-less chain, where rate-limit also precedes overflow.
        assert_eq!(
            c(None, "Throttling error: Too many tokens"),
            ProviderErrorClass::RateLimitOrBilling
        );
    }

    #[test]
    fn memory_ceiling_is_overloaded_not_overflow() {
        // 🔴 oMLX prefill memory measures bytes, and its remediation tail says
        // "reduce context length" — which contains the overflow pattern
        // "context length". Compression cannot lower a prefill peak: it burns
        // the budget, re-hits the wedged server, and ends in a session reset
        // (#52261). Upstream's wordings are `predicted peak would require /
        // exceed` (oMLX 0.5.7) and the 0.5.6 `prefill would require`, still in
        // the field — both are in the ceiling list, so both must win here.
        assert_eq!(
            c(
                Some(400),
                "predicted peak would require 12.0GB: reduce context length"
            ),
            ProviderErrorClass::Overloaded
        );
        assert_eq!(
            c(
                Some(400),
                "prefill would require more memory than available: reduce context length"
            ),
            ProviderErrorClass::Overloaded
        );
        assert_eq!(
            c(Some(500), "out of memory while allocating tensor"),
            ProviderErrorClass::Overloaded
        );
        assert_eq!(
            classify_provider_error(Some(400), "Error", Some("prefill_memory_exceeded")),
            ProviderErrorClass::Overloaded
        );
    }

    #[test]
    fn memory_limit_exceeded_is_not_billing() {
        // "memory limit exceeded" contains "limit exceeded" (a usage-limit
        // pattern) — the ceiling list must be checked first (:233-234).
        assert_eq!(
            c(None, "memory limit exceeded"),
            ProviderErrorClass::Overloaded
        );
    }

    #[test]
    fn empty_response_advisory_never_enters_compression() {
        assert_eq!(
            c(
                Some(500),
                "provider returned an empty response after 3 attempts; token limit"
            ),
            ProviderErrorClass::Other
        );
        assert_eq!(c(None, "empty response stream"), ProviderErrorClass::Other);
    }

    #[test]
    fn encrypted_reasoning_replay_is_not_overflow() {
        // 🔴 "could not be verified" trips the overflow wording upstream; the fix
        // is stripping replay state, not compacting (:1157).
        assert_eq!(
            c(
                Some(400),
                "encrypted content for item xyz could not be verified"
            ),
            ProviderErrorClass::Other
        );
        assert_eq!(
            classify_provider_error(Some(400), "bad request", Some("invalid_encrypted_content")),
            ProviderErrorClass::Other
        );
    }

    #[test]
    fn billing_and_overloaded_never_compress() {
        assert_eq!(
            c(Some(402), "insufficient credits, top up your credits"),
            ProviderErrorClass::RateLimitOrBilling
        );
        assert_eq!(
            c(None, "server is overloaded"),
            ProviderErrorClass::Overloaded
        );
        for class in [c(Some(402), "insufficient credits"), c(None, "at capacity")] {
            assert!(!class.should_compress());
        }
    }

    #[test]
    fn periodic_quota_is_a_throttle_not_billing() {
        // "usage limit, try again in 5 minutes" — either way, never compression.
        let class = c(Some(402), "usage limit reached, try again in 5 minutes");
        assert_eq!(class, ProviderErrorClass::RateLimitOrBilling);
        assert!(!class.should_compress());
    }

    // ── payload size stays distinct from context overflow ────────────────────

    #[test]
    fn payload_too_large_is_distinct_from_context_overflow() {
        // 🔴 Both compress upstream, but payload_too_large has its own
        // byte-scored recovery path and must not be merged with overflow.
        let payload = c(Some(413), "request entity too large");
        assert_eq!(payload, ProviderErrorClass::PayloadTooLarge);
        assert_ne!(payload, ProviderErrorClass::ContextOverflow);
        assert!(payload.should_compress());
        assert_eq!(
            c(None, "request_too_large"),
            ProviderErrorClass::PayloadTooLarge
        );
    }

    // ── everything else keeps today's behaviour ─────────────────────────────

    #[test]
    fn unrelated_errors_stay_other() {
        assert_eq!(c(Some(401), "invalid api key"), ProviderErrorClass::Other);
        assert_eq!(c(Some(404), "model not found"), ProviderErrorClass::Other);
        assert_eq!(
            c(Some(500), "internal server error"),
            ProviderErrorClass::Other
        );
        assert_eq!(
            c(None, "connection reset by peer"),
            ProviderErrorClass::Other
        );
        assert!(!ProviderErrorClass::Other.should_compress());
    }

    #[test]
    fn empty_message_is_other() {
        assert_eq!(c(Some(400), ""), ProviderErrorClass::Other);
        assert_eq!(c(None, ""), ProviderErrorClass::Other);
    }

    #[test]
    fn matching_is_case_insensitive() {
        // Upstream lowercases once (`msg = (error_msg or "").lower()`).
        assert_eq!(
            c(Some(400), "MAXIMUM CONTEXT LENGTH EXCEEDED"),
            ProviderErrorClass::ContextOverflow
        );
    }
}

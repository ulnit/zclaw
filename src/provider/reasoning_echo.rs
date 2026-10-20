//! Per-provider reasoning-replay policy (hermes `agent/message_sanitization.py` parity).
//!
//! # Why this module exists
//!
//! A stored assistant turn's reasoning text cannot simply be replayed to every provider:
//!
//! * **must-echo tier** (DeepSeek, Kimi, Xiaomi MiMo, one Nous Portal stealth model) returns
//!   **HTTP 400** on a tool-call turn that lacks a non-empty `reasoning_content`. The value must
//!   be padded with `" "` — never `""`, which DeepSeek V4 rejects (upstream #17341).
//! * **strict tier** (Mistral, Groq, Cerebras, SambaNova, Tencent Hunyuan) has
//!   `additionalProperties: false` message schemas and returns **400/422** ("Extra inputs are not
//!   permitted") if a reasoning key is present *at all* (upstream #45655, #70233).
//! * everything else reads `reasoning_content`, the de-facto standard carrier —
//!   including *unknown* routes, which is upstream's
//!   `_DEFAULT_CARRIERS = frozenset({RC})`.
//!
//! # Fidelity to upstream
//!
//! This module reproduces hermes' semantics rather than approximating them:
//!
//! * **strict wins over must-echo.** `reasoning_replay_route` returns
//!   `pad and RC in carriers`, and a strict route's carrier set is empty, so the
//!   must-echo pad is withheld there too ("A strict route never gets a reasoning
//!   key, not even the must-echo pad").
//! * **unknown routes send `reasoning_content`.** That default is only safe
//!   because the runtime rejection learner (`record_reasoning_field_rejection`,
//!   ported as [`ReasoningRejectionLog`]) drops the field for a route after one
//!   4xx. Both halves are upstream behaviour and both are implemented here.
//! * **carrier widening is not representable.** hermes routes local endpoints onto
//!   `_LOCAL_CARRIERS = {RC, R}` (llama.cpp/SGLang read `reasoning_content`,
//!   vLLM/Ollama read `reasoning`) and widens some routes with `reasoning_details`.
//!   ulnclaw's `ApiMessage` has exactly one reasoning field, so those extra
//!   carriers have no wire representation and both cases collapse to
//!   [`ReasoningReplay::Content`] — same observable outcome for the carriers this
//!   port can actually send.

use std::fmt;

/// What a chat-completions route accepts on replayed assistant turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningReplay {
    /// Send **no** reasoning key. Covers the strict tier *and* unknown routes (see module docs).
    Off,
    /// Route reads `reasoning_content`: replay it when the turn actually has text, omit otherwise.
    Content,
    /// must-echo tier: every assistant turn carries `reasoning_content`, padded with `" "` when
    /// there is no text (a tool-call turn without it is a 400 there).
    MustEcho,
}

impl ReasoningReplay {
    /// The value to put in `reasoning_content` for one assistant turn, or `None` to omit the key.
    ///
    /// `has_tool_calls` matters only for the must-echo tier's legacy pad semantics; ulnclaw pads
    /// every assistant turn there, which is the stricter (safer) reading of the upstream contract.
    pub fn wire_value(self, reasoning: Option<&str>) -> Option<String> {
        let text = replayable_text(reasoning);
        match self {
            ReasoningReplay::Off => None,
            ReasoningReplay::Content => text.map(str::to_string),
            // Never "" — DeepSeek V4 rejects an empty string (#17341); " " is the pad.
            ReasoningReplay::MustEcho => {
                Some(text.map(str::to_string).unwrap_or_else(|| " ".into()))
            }
        }
    }
}

impl fmt::Display for ReasoningReplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReasoningReplay::Off => write!(f, "off"),
            ReasoningReplay::Content => write!(f, "content"),
            ReasoningReplay::MustEcho => write!(f, "must-echo"),
        }
    }
}

/// A non-blank reasoning string is replayable; blank/whitespace-only is not.
/// Mirrors hermes `_replayable_reasoning_text` (which prefers `reasoning_content` over
/// `reasoning`; ulnclaw stores a single merged field, so only the blank check applies).
fn replayable_text(reasoning: Option<&str>) -> Option<&str> {
    reasoning
        .map(str::trim)
        .filter(|t| !t.is_empty())
        // Return the *original* (untrimmed) slice: the text is the model's own output and
        // trimming it would silently edit replayed reasoning.
        .map(|_| reasoning.unwrap())
}

// ---------------------------------------------------------------------------
// Route tables — transcribed from hermes `agent/message_sanitization.py`
// (`_REASONING_ECHO_RULES`, `_REASONING_CARRIER_ROUTES`). Keep in sync.
// ---------------------------------------------------------------------------

/// must-echo families: `(exact providers lowercase, model substrings lowercase, host suffixes)`.
///
/// Kimi is host-driven on purpose — aggregators re-exporting Kimi models do **not** need the pad.
const MUST_ECHO: &[(&[&str], &[&str], &[&str])] = &[
    // kimi: exact provider names + kimi/moonshot hosts
    (
        &["kimi-coding", "kimi-coding-cn"],
        &[],
        &["api.kimi.com", "moonshot.ai", "moonshot.cn"],
    ),
    // deepseek: any provider named deepseek, any deepseek model, or the deepseek host
    (&["deepseek"], &["deepseek"], &["api.deepseek.com"]),
    // Xiaomi MiMo
    (
        &["xiaomi"],
        &["mimo"],
        &["api.xiaomimimo.com", "xiaomimimo.com"],
    ),
    // Nous Portal stealth model that 400s on a tool-call turn without reasoning_content
    (&[], &["stealth/missingno"], &[]),
];

/// Routes that read **no** reasoning carrier (strict schemas).
const STRICT_PROVIDERS: &[&str] = &["mistral", "groq", "cerebras", "sambanova"];
const STRICT_HOSTS: &[&str] = &[
    "mistral.ai",
    "groq.com",
    "cerebras.ai",
    "sambanova.ai",
    "hunyuan.cloud.tencent.com",
];

/// Routes that read `reasoning_content` explicitly (not the ulnclaw default — see module docs).
/// OpenRouter-format gateways read the unified array + string + alias; ulnclaw stores only the
/// merged string, so all of these collapse to [`ReasoningReplay::Content`].
const CONTENT_PROVIDERS: &[&str] = &[
    "openrouter",
    "kilocode",
    "ai-gateway",
    "nous",
    "fireworks",
    "novita",
    "tencent-tokenhub",
    "minimax",
    "minimax-cn",
    "ollama-cloud",
    "custom",
    "lmstudio",
    "upstage",
];
const CONTENT_HOSTS: &[&str] = &[
    "openrouter.ai",
    "kilo.ai",
    "ai-gateway.vercel.sh",
    "nousresearch.com",
    "fireworks.ai",
    "novita.ai",
    "tokenhub.tencentmaas.com",
    "tokenhub-intl.tencentcloudmaas.com",
    "minimax.io",
    "minimaxi.com",
    "ollama.com",
    "upstage.ai",
];

/// Extract the host from a base URL / endpoint for suffix matching.
///
/// Accepts a bare host, a host:port, or a full URL. Returns the lowercased host without port.
///
/// Public because the P3-1e persistence layer keys routes by `(provider, host,
/// model)` exactly as hermes does — the agent must derive the same host string
/// the echo rules match on, or a stored rejection would never be found again.
pub fn host_of(endpoint: &str) -> String {
    let s = endpoint.trim().to_ascii_lowercase();
    // strip scheme
    let after_scheme = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s.as_str(),
    };
    // strip path
    let host_port = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // strip userinfo
    let host_port = host_port
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(host_port);
    // strip port (and handle IPv6 literals in brackets)
    if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        host_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_port)
            .to_string()
    }
}

/// Host matches when it equals the pattern or is a subdomain of it
/// (hermes `utils.base_url_host_matches` semantics).
fn host_matches(host: &str, pattern: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    host == pattern || host.ends_with(&format!(".{pattern}"))
}

/// Resolve the replay route for one endpoint. Deterministic in its inputs, so the wire prefix is
/// byte-stable across turns and changes only on a route switch (hermes `reasoning_replay_route`).
pub fn reasoning_replay(provider: &str, model: &str, endpoint: &str) -> ReasoningReplay {
    let provider_l = provider.trim().to_ascii_lowercase();
    let model_l = model.trim().to_ascii_lowercase();
    let host = host_of(endpoint);

    // 1) strict tier wins over everything, including must-echo.
    // hermes `reasoning_replay_route`: `ReasoningReplayRoute(pad and RC in carriers, carriers)`
    // — a strict route's carrier set is empty, so `RC in carriers` is false and even the
    // must-echo pad is withheld ("A strict route never gets a reasoning key, not even the
    // must-echo pad"). Their schemas are `additionalProperties: false` and 400/422 on any
    // reasoning key (#45655, #70233).
    if STRICT_PROVIDERS.contains(&provider_l.as_str())
        || STRICT_HOSTS.iter().any(|h| host_matches(&host, h))
    {
        return ReasoningReplay::Off;
    }

    // 2) must-echo tier: there the *omission* is the proven 400.
    for (providers, model_subs, hosts) in MUST_ECHO {
        let hit = providers.contains(&provider_l.as_str())
            || model_subs.iter().any(|s| model_l.contains(s))
            || hosts.iter().any(|h| host_matches(&host, h));
        if hit {
            return ReasoningReplay::MustEcho;
        }
    }

    // 3) routes known to read reasoning_content.
    if CONTENT_PROVIDERS.contains(&provider_l.as_str())
        || CONTENT_HOSTS.iter().any(|h| host_matches(&host, h))
    {
        return ReasoningReplay::Content;
    }

    // 4) unknown → Content. This is upstream's `_DEFAULT_CARRIERS = frozenset({RC})`:
    // "Every other route defaults to `reasoning_content`, which is the de-facto
    // standard". It is safe here *because* the runtime rejection learner (P3-1d)
    // exists — a route whose schema actually refuses the field is learned after one
    // 400 and downgraded to Off for the rest of the process.
    //
    // hermes additionally splits local endpoints onto `_LOCAL_CARRIERS = {RC, R}`
    // (llama.cpp/SGLang read reasoning_content, vLLM/Ollama read `reasoning`) and
    // widens some routes with `reasoning_details`. ulnclaw's `ApiMessage` carries
    // exactly one reasoning field, so those extra carriers have no wire
    // representation here and both cases collapse to Content.
    ReasoningReplay::Content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_handles_urls_ports_and_bare_hosts() {
        assert_eq!(host_of("https://api.deepseek.com/v1"), "api.deepseek.com");
        assert_eq!(host_of("http://localhost:11434"), "localhost");
        assert_eq!(host_of("HTTP://OpenRouter.AI/v1"), "openrouter.ai");
        assert_eq!(host_of("api.kimi.com"), "api.kimi.com");
        assert_eq!(host_of("https://user@host.example:8443/x"), "host.example");
        assert_eq!(host_of("https://[::1]:8080/v1"), "::1");
        assert_eq!(host_of(""), "");
    }

    #[test]
    fn host_matches_is_suffix_scoped() {
        assert!(host_matches("api.deepseek.com", "deepseek.com"));
        assert!(host_matches("deepseek.com", "deepseek.com"));
        assert!(host_matches("sub.api.kimi.com", "api.kimi.com"));
        // must NOT match a lookalike that merely ends with the string
        assert!(!host_matches("notdeepseek.com", "deepseek.com"));
        assert!(!host_matches("evil-groq.com.attacker.io", "groq.com"));
        assert!(!host_matches("", "groq.com"));
    }

    #[test]
    fn must_echo_tier_pads_with_single_space_never_empty() {
        let r = ReasoningReplay::MustEcho;
        assert_eq!(r.wire_value(Some("thought")).as_deref(), Some("thought"));
        // 🔴 #17341: "" is rejected by DeepSeek V4 — the pad must be " ".
        assert_eq!(r.wire_value(None).as_deref(), Some(" "));
        assert_eq!(r.wire_value(Some("")).as_deref(), Some(" "));
        assert_eq!(r.wire_value(Some("   \n ")).as_deref(), Some(" "));
    }

    #[test]
    fn content_tier_omits_when_blank() {
        let r = ReasoningReplay::Content;
        assert_eq!(r.wire_value(Some("thought")).as_deref(), Some("thought"));
        assert_eq!(r.wire_value(None), None);
        assert_eq!(
            r.wire_value(Some("  ")),
            None,
            "blank must not become a pad"
        );
    }

    #[test]
    fn off_tier_never_sends_anything() {
        let r = ReasoningReplay::Off;
        assert_eq!(r.wire_value(Some("thought")), None);
        assert_eq!(r.wire_value(None), None);
    }

    #[test]
    fn replayable_text_preserves_the_original_untrimmed_string() {
        // Trimming would silently edit the model's own output.
        assert_eq!(
            ReasoningReplay::Content
                .wire_value(Some("  kept  "))
                .as_deref(),
            Some("  kept  ")
        );
    }

    // ---- route resolution ----

    #[test]
    fn deepseek_is_must_echo_by_provider_model_and_host() {
        assert_eq!(
            reasoning_replay("deepseek", "deepseek-chat", "https://api.deepseek.com/v1"),
            ReasoningReplay::MustEcho
        );
        assert_eq!(
            reasoning_replay("DeepSeek", "", ""),
            ReasoningReplay::MustEcho,
            "provider name alone is enough"
        );
        assert_eq!(
            reasoning_replay("", "deepseek-reasoner", ""),
            ReasoningReplay::MustEcho,
            "model substring alone is enough"
        );
        assert_eq!(
            reasoning_replay("", "", "https://api.deepseek.com"),
            ReasoningReplay::MustEcho,
            "host alone is enough"
        );
    }

    #[test]
    fn kimi_is_host_driven_not_model_driven() {
        // upstream: aggregators re-exporting kimi models must NOT get the *pad*.
        // They still default to `reasoning_content` like every other unknown route
        // (`_DEFAULT_CARRIERS = {RC}`) — the distinction is pad vs no-pad, not
        // send vs don't-send.
        assert_eq!(
            reasoning_replay("some-aggregator", "kimi-k2", "https://api.example.com/v1"),
            ReasoningReplay::Content,
            "a kimi *model* via an unrelated host must not be padded"
        );
        assert_eq!(
            reasoning_replay("", "", "https://api.kimi.com/v1"),
            ReasoningReplay::MustEcho,
            "the kimi host itself must pad"
        );
        assert_eq!(
            reasoning_replay("kimi-coding", "", "https://anything.test"),
            ReasoningReplay::MustEcho
        );
        assert_eq!(
            reasoning_replay("", "", "https://moonshot.cn"),
            ReasoningReplay::MustEcho
        );
    }

    #[test]
    fn mimo_and_missingno_are_must_echo() {
        assert_eq!(
            reasoning_replay("xiaomi", "", ""),
            ReasoningReplay::MustEcho
        );
        assert_eq!(
            reasoning_replay("", "MiMo-7B-RL", "https://api.xiaomimimo.com"),
            ReasoningReplay::MustEcho
        );
        assert_eq!(
            reasoning_replay("nous", "stealth/missingno", ""),
            ReasoningReplay::MustEcho,
            "must-echo beats the nous Content route (tier order matters)"
        );
    }

    #[test]
    fn strict_tier_gets_nothing_even_with_a_must_echo_shaped_model() {
        for (p, m, e) in [
            ("mistral", "mistral-large", "https://api.mistral.ai/v1"),
            ("groq", "llama-3.3", "https://api.groq.com/openai/v1"),
            ("cerebras", "llama", "https://api.cerebras.ai/v1"),
            ("sambanova", "x", "https://api.sambanova.ai/v1"),
            ("", "", "https://hunyuan.cloud.tencent.com/v1"),
        ] {
            assert_eq!(reasoning_replay(p, m, e), ReasoningReplay::Off, "{p}/{e}");
        }
    }

    #[test]
    fn content_routes_replay_when_text_exists() {
        assert_eq!(
            reasoning_replay("openrouter", "x", "https://openrouter.ai/api/v1"),
            ReasoningReplay::Content
        );
        assert_eq!(
            reasoning_replay("fireworks", "", ""),
            ReasoningReplay::Content
        );
        assert_eq!(
            reasoning_replay("minimax", "", ""),
            ReasoningReplay::Content
        );
        assert_eq!(
            reasoning_replay("", "", "https://api.novita.ai/v3"),
            ReasoningReplay::Content
        );
        assert_eq!(
            reasoning_replay("lmstudio", "", ""),
            ReasoningReplay::Content
        );
    }

    #[test]
    fn unknown_routes_send_reasoning_content_like_upstream() {
        // Upstream `_DEFAULT_CARRIERS = frozenset({RC})`: "Every other route
        // defaults to reasoning_content, which is the de-facto standard."
        // Safety comes from the runtime rejection learner (P3-1d), not from
        // withholding the field.
        assert_eq!(
            reasoning_replay(
                "brand-new-provider",
                "some-model",
                "https://api.unknown.test/v1"
            ),
            ReasoningReplay::Content
        );
        // No route facts at all → still the de-facto standard carrier.
        assert_eq!(reasoning_replay("", "", ""), ReasoningReplay::Content);
    }

    /// 🔴 strict beats must-echo: `reasoning_replay_route` returns
    /// `pad and RC in carriers`, so a strict route withholds even the pad.
    #[test]
    fn strict_tier_wins_over_must_echo() {
        // A deepseek *model* served through a strict host must not get the pad:
        // the host's schema rejects any reasoning key outright.
        assert_eq!(
            reasoning_replay("", "deepseek-chat", "https://api.groq.com/openai/v1"),
            ReasoningReplay::Off,
            "strict host must override the must-echo model match"
        );
        assert_eq!(
            reasoning_replay("mistral", "kimi-k2", ""),
            ReasoningReplay::Off,
            "strict provider must override the must-echo model match"
        );
        // Conversely the same model on its own host still must-echo.
        assert_eq!(
            reasoning_replay("", "deepseek-chat", "https://api.deepseek.com/v1"),
            ReasoningReplay::MustEcho
        );
    }

    /// Local endpoints in hermes read two carriers (`{RC, R}`); ulnclaw can only
    /// send one, so the observable outcome is Content, not Off.
    #[test]
    fn local_endpoints_are_content_not_off() {
        assert_eq!(
            reasoning_replay("custom", "", "http://localhost:11434/v1"),
            ReasoningReplay::Content
        );
        assert_eq!(
            reasoning_replay("lmstudio", "", "http://127.0.0.1:1234/v1"),
            ReasoningReplay::Content
        );
        // An unknown local endpoint likewise defaults to the de-facto carrier.
        assert_eq!(
            reasoning_replay("", "", "http://192.168.1.50:8000/v1"),
            ReasoningReplay::Content
        );
    }

    #[test]
    fn provider_name_matching_is_case_and_space_insensitive() {
        assert_eq!(
            reasoning_replay("  DeepSeek  ", "", ""),
            ReasoningReplay::MustEcho
        );
        assert_eq!(reasoning_replay("MISTRAL", "", ""), ReasoningReplay::Off);
        assert_eq!(
            reasoning_replay("OpenRouter", "", ""),
            ReasoningReplay::Content
        );
    }
}
// ---------------------------------------------------------------------------
// Runtime rejection learning (P3-1d) — hermes `record_reasoning_field_rejection`
// ---------------------------------------------------------------------------

/// Provider error bodies (lowercased) meaning "this request carries a key my
/// schema does not know". Transcribed from hermes `_UNKNOWN_FIELD_PHRASES`.
const UNKNOWN_FIELD_PHRASES: &[&str] = &[
    "extra inputs are not permitted",
    "is unsupported",
    "no such field",
    "unknown field",
    "unrecognized request argument",
    "unrecognized field",
    "unknown parameter",
    "unexpected field",
    "additional propert", // hermes truncates "additional properties" this way on purpose
    "unknown key",
];

/// The carrier ulnclaw actually puts on the wire. hermes tracks three
/// (`reasoning_content`, `reasoning`, `reasoning_details`) and widens the drop
/// when a non-standard one is named; ulnclaw's `ApiMessage` has exactly one, so
/// only this name is ever learnable.
const REASONING_CONTENT: &str = "reasoning_content";

/// ASCII word-boundary substring search.
///
/// hermes uses `(?<![\w])reasoning_content(?![\w])`. The Rust `regex` crate has
/// no lookbehind, and pulling in `regex` for one literal is not worth it — this
/// reproduces the same semantics for an ASCII needle.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    h.windows(n.len()).enumerate().any(|(i, w)| {
        w == n
            && !i.checked_sub(1).map(|p| is_word(h[p])).unwrap_or(false)
            && !h.get(i + n.len()).map(|b| is_word(*b)).unwrap_or(false)
    })
}

/// Decide whether an error body is an unknown-field rejection of
/// `reasoning_content` **that this request actually sent**.
///
/// `sent_reasoning_content` is the caller's own check that at least one assistant
/// message carried the key — hermes' `rejected_reasoning_fields` does the same
/// (`sent & ...`), because learning a field we never sent would be a lie that
/// silently disables reasoning for the rest of the session.
///
/// Returns false for an unnamed/ unrelated error, so recovery can never fire on
/// e.g. a 500 or an auth failure.
pub fn body_rejects_reasoning_content(error_body: &str, sent_reasoning_content: bool) -> bool {
    if !sent_reasoning_content {
        return false;
    }
    let body = error_body.to_ascii_lowercase();
    if !UNKNOWN_FIELD_PHRASES.iter().any(|p| body.contains(p)) {
        return false;
    }
    // Only a *named* mention counts: an unnamed "extra inputs are not permitted"
    // could be about any field, and guessing would disable reasoning for nothing.
    contains_word(&body, REASONING_CONTENT)
}

/// Per-route memory of "this endpoint rejected `reasoning_content`".
///
/// # Why a flag and not hermes' set
///
/// hermes stores a `set` of carriers per `(provider, host, model)` because it can
/// send three of them and widens the drop accordingly. ulnclaw sends exactly one,
/// so the whole structure collapses to a boolean — and because an
/// [`crate::provider::openai::OpenAiProvider`] instance *is* an immutable
/// `(name, model, endpoint)` triple (the same tuple hermes uses as its route key),
/// per-instance state **is** per-route state. No keying, no map.
///
/// # Scope: process-lifetime, seeded and persisted by the agent (P3-1e)
///
/// hermes writes rejections into the session's `model_config` so a resumed
/// process never re-learns with another 400. ulnclaw's provider layer sits
/// *below* the session layer and has no store handle, so persistence is NOT
/// done here — threading a store in would invert that dependency. Instead the
/// log exposes `is_rejected()`/`record()` and the provider exposes
/// `learned_reasoning_rejection()`/`seed_reasoning_rejection()`; the **agent**
/// (which already holds both the store and the session id) reads the session's
/// stored routes at run start to seed, and writes back when a route newly
/// learns. See `agent::persist_reasoning_rejections`.
#[derive(Debug, Default)]
pub struct ReasoningRejectionLog {
    rejected: std::sync::atomic::AtomicBool,
}

impl ReasoningRejectionLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this route has already been learned to reject `reasoning_content`.
    pub fn is_rejected(&self) -> bool {
        self.rejected.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record a rejection. Returns `true` **only if this is new** — mirroring
    /// hermes' "empty frozenset = no retry" contract, which is what makes the
    /// recovery unable to loop: a repeat rejection of an already-recorded key
    /// returns false, so the caller surfaces the error instead of retrying.
    pub fn record(&self) -> bool {
        !self
            .rejected
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// P3-1e: seed a rejection persisted from an earlier process. Distinct
    /// from `record()` because seeding must never look like "a new rejection
    /// was learned this call" — `record()` returns true-on-first-write and the
    /// retry loop keys off that; seeding returns nothing and simply pre-arms
    /// the flag so the next `build_api_request` skips the carrier.
    pub fn seed(&self) {
        self.rejected
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod rejection_tests {
    use super::*;

    #[test]
    fn word_boundary_matching_is_not_fooled_by_lookalikes() {
        assert!(contains_word(
            "field reasoning_content is bad",
            "reasoning_content"
        ));
        assert!(contains_word("\"reasoning_content\"", "reasoning_content"));
        // must not match a longer identifier that merely contains the needle
        assert!(!contains_word(
            "my_reasoning_content_v2",
            "reasoning_content"
        ));
        assert!(!contains_word("reasoning_contents", "reasoning_content"));
        assert!(!contains_word("no such thing", "reasoning_content"));
        assert!(!contains_word("", "reasoning_content"));
    }

    #[test]
    fn learns_from_a_named_unknown_field_rejection() {
        // The real Mistral/Groq shape (upstream #45655: "Extra inputs are not permitted")
        assert!(body_rejects_reasoning_content(
            r#"{"error":{"message":"messages.2.reasoning_content: Extra inputs are not permitted"}}"#,
            true
        ));
        assert!(body_rejects_reasoning_content(
            "Unrecognized request argument supplied: reasoning_content",
            true
        ));
        assert!(body_rejects_reasoning_content(
            "unknown field `reasoning_content`",
            true
        ));
        // hermes' truncated "additional propert" phrasing
        assert!(body_rejects_reasoning_content(
            "Additional Properties are not allowed: reasoning_content",
            true
        ));
    }

    #[test]
    fn does_not_learn_when_we_never_sent_it() {
        // 🔴 learning a field we did not send would silently disable reasoning
        // for the rest of the session — the `sent` intersection in hermes exists
        // for exactly this reason.
        assert!(!body_rejects_reasoning_content(
            "messages.2.reasoning_content: Extra inputs are not permitted",
            false
        ));
    }

    #[test]
    fn does_not_learn_from_unrelated_or_unnamed_errors() {
        // Unnamed: could be any field, guessing would be a lie.
        assert!(!body_rejects_reasoning_content(
            "Extra inputs are not permitted",
            true
        ));
        // Named but not an unknown-field complaint (e.g. a value complaint).
        assert!(!body_rejects_reasoning_content(
            "reasoning_content must not be empty",
            true
        ));
        // Plain transport/server/auth failures.
        assert!(!body_rejects_reasoning_content(
            "upstream connect error",
            true
        ));
        assert!(!body_rejects_reasoning_content(
            "429 Too Many Requests",
            true
        ));
        assert!(!body_rejects_reasoning_content("invalid api key", true));
        assert!(!body_rejects_reasoning_content("", true));
    }

    #[test]
    fn log_records_once_so_recovery_cannot_loop() {
        let log = ReasoningRejectionLog::new();
        assert!(!log.is_rejected());
        assert!(log.record(), "first rejection is new -> caller may retry");
        assert!(log.is_rejected());
        assert!(
            !log.record(),
            "repeat rejection is not new -> no retry, no loop"
        );
        assert!(log.is_rejected());
    }

    #[test]
    fn must_echo_routes_are_never_learned_away() {
        // hermes: `fields -= {RC}` when needs_reasoning_echo(...). A must-echo
        // route 400s on a tool turn *without* reasoning_content, so a body naming
        // it there is a VALUE complaint ("must not be empty"), never a schema that
        // lacks the field. Downgrading such a route would guarantee a 400 forever.
        let body = "messages.2.reasoning_content: must not be empty";
        assert!(
            !body_rejects_reasoning_content(body, true),
            "a value complaint is not an unknown-field rejection"
        );
        // And the tier itself must be immune even if some body did name it:
        assert_eq!(
            reasoning_replay("deepseek", "", "https://api.deepseek.com/v1"),
            ReasoningReplay::MustEcho
        );
    }
}

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
//! * everything else reads `reasoning_content`, the de-facto standard carrier.
//!
//! # 🔴 Deliberate deviation from upstream
//!
//! hermes routes *unknown* providers to `_DEFAULT_CARRIERS = {reasoning_content}` and stays safe
//! because it **learns** rejections at runtime (`record_reasoning_field_rejection` drops the field
//! for that provider/host/model after a 4xx). ulnclaw has no runtime rejection learning, so an
//! unknown route here resolves to [`ReasoningReplay::Off`] — i.e. it keeps today's behaviour
//! (send nothing) rather than risk breaking a provider that currently works. The must-echo tier is
//! still honoured, because there the omission is the *proven* failure.
//!
//! Porting the runtime-rejection learner would let `Off` for unknown routes be relaxed to
//! `Content`; until then, this is the safe default. See `references/ulnclaw-hermes-parity.md`.

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
            ReasoningReplay::MustEcho => Some(text.map(str::to_string).unwrap_or_else(|| " ".into())),
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
    (&["xiaomi"], &["mimo"], &["api.xiaomimimo.com", "xiaomimimo.com"]),
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
fn host_of(endpoint: &str) -> String {
    let s = endpoint.trim().to_ascii_lowercase();
    // strip scheme
    let after_scheme = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s.as_str(),
    };
    // strip path
    let host_port = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    // strip userinfo
    let host_port = host_port.rsplit_once('@').map(|(_, h)| h).unwrap_or(host_port);
    // strip port (and handle IPv6 literals in brackets)
    if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        host_port.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_port).to_string()
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

    // 1) must-echo tier wins: there the *omission* is the proven 400.
    for (providers, model_subs, hosts) in MUST_ECHO {
        let hit = providers.contains(&provider_l.as_str())
            || model_subs.iter().any(|s| model_l.contains(s))
            || hosts.iter().any(|h| host_matches(&host, h));
        if hit {
            return ReasoningReplay::MustEcho;
        }
    }

    // 2) strict tier: never gets a reasoning key, not even a must-echo pad.
    if STRICT_PROVIDERS.contains(&provider_l.as_str())
        || STRICT_HOSTS.iter().any(|h| host_matches(&host, h))
    {
        return ReasoningReplay::Off;
    }

    // 3) routes known to read reasoning_content.
    if CONTENT_PROVIDERS.contains(&provider_l.as_str())
        || CONTENT_HOSTS.iter().any(|h| host_matches(&host, h))
    {
        return ReasoningReplay::Content;
    }

    // 4) unknown → Off (deliberate deviation; see module docs).
    ReasoningReplay::Off
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
        assert_eq!(r.wire_value(Some("  ")), None, "blank must not become a pad");
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
            ReasoningReplay::Content.wire_value(Some("  kept  ")).as_deref(),
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
        // upstream: aggregators re-exporting kimi models must NOT get the pad
        assert_eq!(
            reasoning_replay("some-aggregator", "kimi-k2", "https://api.example.com/v1"),
            ReasoningReplay::Off,
            "a kimi *model* via an unrelated host must not be padded"
        );
        assert_eq!(
            reasoning_replay("", "", "https://api.kimi.com/v1"),
            ReasoningReplay::MustEcho
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
        assert_eq!(reasoning_replay("xiaomi", "", ""), ReasoningReplay::MustEcho);
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
        assert_eq!(reasoning_replay("fireworks", "", ""), ReasoningReplay::Content);
        assert_eq!(reasoning_replay("minimax", "", ""), ReasoningReplay::Content);
        assert_eq!(
            reasoning_replay("", "", "https://api.novita.ai/v3"),
            ReasoningReplay::Content
        );
        assert_eq!(reasoning_replay("lmstudio", "", ""), ReasoningReplay::Content);
    }

    #[test]
    fn unknown_routes_stay_off_by_design() {
        // 🔴 deliberate deviation: hermes would send reasoning_content here and rely on
        // runtime rejection learning. ulnclaw has none, so unknown stays silent.
        assert_eq!(
            reasoning_replay("brand-new-provider", "some-model", "https://api.unknown.test/v1"),
            ReasoningReplay::Off
        );
        assert_eq!(reasoning_replay("", "", ""), ReasoningReplay::Off);
    }

    #[test]
    fn provider_name_matching_is_case_and_space_insensitive() {
        assert_eq!(
            reasoning_replay("  DeepSeek  ", "", ""),
            ReasoningReplay::MustEcho
        );
        assert_eq!(reasoning_replay("MISTRAL", "", ""), ReasoningReplay::Off);
        assert_eq!(reasoning_replay("OpenRouter", "", ""), ReasoningReplay::Content);
    }
}

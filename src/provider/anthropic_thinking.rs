//! Anthropic prior-turn thinking retention policy.
//!
//! Port of hermes `agent/anthropic_thinking_policy.py` (102 lines). Anthropic
//! preserves and bills prior assistant thinking on Opus 4.5+, Sonnet 4.6+ and
//! Haiku 5.5+ (plus the newer Fable/Mythos families). Older models accept
//! replayed blocks but strip them server-side. Keeping the model capability and
//! the per-endpoint replay contract in one place is what stops message
//! conversion and context accounting from diverging.
//!
//! 🔴 **Unknown or future Claude ids default to KEEP.** Replaying a block the API
//! strips costs nothing, while stripping on a model that keeps it rewrites the
//! cached prefix on every call. Only the known last-turn-only generations
//! (Claude 3, Haiku < 5.5, Opus < 4.5, Sonnet < 4.6) strip. Non-Claude ids never
//! keep.
//!
//! **Scope of this first slice (P3-1c):** the pure *model-capability* half plus the
//! route vocabulary and the route→replay predicate. The route *resolution*
//! ([`anthropic_thinking_route`]) needs seven endpoint predicates whose bodies were
//! not yet transcribed (`_is_bedrock_runtime_endpoint`, `_is_vertex_ai_endpoint`,
//! `_is_azure_anthropic_endpoint`, `_is_kimi_coding_endpoint`,
//! `base_url_host_matches`, `_MINIMAX_ANTHROPIC_PREFIXES`,
//! `_model_name_is_kimi_family`, `_model_name_is_deepseek_thinking`, plus the
//! `NOUS_INFERENCE_BASE_URL` env override) — it is the next slice, recorded rather
//! than guessed.

use regex::Regex;
use std::sync::OnceLock;

/// Claude families whose ids carry the version *before* the family
/// (`claude-opus-4-1`). Semantic minors are short version components.
///
/// 🔴 The minor is bounded to 1-2 digits AND must be followed by end-of-string or a
/// separator, so snapshot dates stay out: `claude-opus-4-20250514` must parse as
/// **4.0**, never 4.20250514 (which would make every snapshot id "newer" than every
/// real release).
///
/// 🔴 Upstream spells that boundary as a lookahead (`(?=$|[-_.])`). **Rust's `regex`
/// crate has no look-around at all** — it is a finite automaton, not a backtracking
/// engine — so it is written as a consuming `(?:$|[-_.])` instead, which is
/// equivalent here because the trailing separator is not captured or reused.
fn claude_version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)claude[-_.](opus|sonnet|haiku|fable|mythos)[-_.](\d+)(?:[-_.](\d{1,2})(?:$|[-_.]))?",
        )
        .expect("static regex")
    })
}

/// Claude 3.x ids put the family *after* the version (`claude-3-5-haiku`), so they
/// need their own pattern rather than the version gate above.
///
/// 🔴 Upstream's `(?!\d)` (keeping `claude-30`-style future majors out of the
/// last-turn-only generation) becomes a consuming `(?:\D|$)` for the same
/// no-look-around reason as [`claude_version_re`].
fn last_turn_only_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)claude[-_.]?3(?:\D|$)").expect("static regex"))
}

/// The first version of each family that keeps *all* prior thinking. Families
/// absent from this table (Fable, Mythos, and anything newer) default to `(5, 0)`
/// upstream, so they keep from 5.0 on.
const KEEP_ALL_FROM: &[(&str, (u32, u32))] =
    &[("opus", (4, 5)), ("sonnet", (4, 6)), ("haiku", (5, 5))];

/// A Claude family name with its parsed `(major, minor)`.
pub type ClaudeFamilyVersion = (&'static str, (u32, u32));

/// Parse `claude-<family>-<major>[-<minor>]` out of a model id.
///
/// `None` for anything that is not a versioned Claude id — including Claude 3.x
/// (family-after-version) and non-Claude models.
pub fn claude_family_version(model: &str) -> Option<ClaudeFamilyVersion> {
    let caps = claude_version_re().captures(model.trim())?;
    let family = match caps[1].to_ascii_lowercase().as_str() {
        "opus" => "opus",
        "sonnet" => "sonnet",
        "haiku" => "haiku",
        "fable" => "fable",
        "mythos" => "mythos",
        _ => return None,
    };
    let major: u32 = caps[2].parse().ok()?;
    // 🔴 A missing minor is 0, and a snapshot date must NOT become the minor —
    // the regex's `(?=$|[-_.])` lookahead already refuses 8-digit dates, so
    // `claude-opus-4-20250514` lands here with `minor == 0`.
    let minor: u32 = caps
        .get(3)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    Some((family, (major, minor)))
}

/// Whether Anthropic keeps prior assistant thinking in model-visible context.
pub fn model_preserves_prior_thinking(model: &str) -> bool {
    let lower = model.to_lowercase();
    if !lower.contains("claude") || last_turn_only_re().is_match(model) {
        return false;
    }
    match claude_family_version(model) {
        // 🔴 Unknown/future ids default to KEEP: replaying a stripped block is
        // free, stripping a kept one rewrites the cached prefix every call.
        None => true,
        Some((family, version)) => {
            version
                >= KEEP_ALL_FROM
                    .iter()
                    .find(|(f, _)| *f == family)
                    .map(|(_, v)| *v)
                    .unwrap_or((5, 0))
        }
    }
}

/// Which thinking-replay contract an Anthropic Messages request follows.
///
/// Verbatim from upstream's `anthropic_thinking_route` docstring:
/// - [`Kimi`](ThinkingRoute::Kimi) — replay as-is
/// - [`DeepSeek`](ThinkingRoute::DeepSeek) — unsigned only
/// - [`Native`](ThinkingRoute::Native) — Anthropic-signed blocks: the direct API,
///   Nous Portal, Bedrock, Vertex AI, Azure AI Foundry
/// - [`MiniMax`](ThinkingRoute::MiniMax) — its own signed blocks, returned
///   unchanged on every turn as its docs require
/// - [`ThirdParty`](ThinkingRoute::ThirdParty) — an unknown relay that cannot
///   verify Anthropic signatures: strip all
///
/// The converter, accounting and signature-rejection state share this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingRoute {
    Kimi,
    DeepSeek,
    Native,
    MiniMax,
    ThirdParty,
}

impl ThinkingRoute {
    /// The wire name upstream uses for this route.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kimi => "kimi",
            Self::DeepSeek => "deepseek",
            Self::Native => "native",
            Self::MiniMax => "minimax",
            Self::ThirdParty => "third_party",
        }
    }

    /// Whether this route carries thinking signatures the upstream verifies, so it
    /// replays signed blocks and a signature 400 is healed by durable suppression.
    ///
    /// hermes `SIGNED_REPLAY_ROUTES = frozenset({"native", "minimax"})`.
    pub fn is_signed_replay(self) -> bool {
        matches!(self, Self::Native | Self::MiniMax)
    }

    /// Whether this route replays signed thinking on assistant turns *before* the
    /// in-flight tool loop.
    ///
    /// 🔴 MiniMax replays regardless of model (its docs require the blocks back
    /// unchanged on every turn); Native replays only when the model keeps prior
    /// thinking. Kimi and DeepSeek replay *unsigned* blocks, which is a different
    /// contract, so they are false here.
    pub fn replays_prior_thinking(self, model: &str) -> bool {
        match self {
            Self::MiniMax => true,
            Self::Native => model_preserves_prior_thinking(model),
            Self::Kimi | Self::DeepSeek | Self::ThirdParty => false,
        }
    }
}

/// True when the route replays (and the model keeps) signed thinking from earlier
/// turns, so stale thinking reaches the wire and accounting must charge the signed
/// carriers.
///
/// hermes `native_anthropic_preserves_prior_thinking`. Takes the route directly
/// because [`anthropic_thinking_route`] is not ported yet — see the module docs.
pub fn preserves_prior_thinking_on_route(route: ThinkingRoute, model: &str) -> bool {
    route.replays_prior_thinking(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- version parsing ---------------------------------------------------

    #[test]
    fn parses_family_and_version() {
        assert_eq!(
            claude_family_version("claude-opus-4-1"),
            Some(("opus", (4, 1)))
        );
        assert_eq!(
            claude_family_version("claude-sonnet-4-6"),
            Some(("sonnet", (4, 6)))
        );
        assert_eq!(
            claude_family_version("claude-haiku-5-5"),
            Some(("haiku", (5, 5)))
        );
        assert_eq!(
            claude_family_version("claude-fable-1-0"),
            Some(("fable", (1, 0)))
        );
        assert_eq!(
            claude_family_version("claude-mythos-2"),
            Some(("mythos", (2, 0)))
        );
    }

    /// Accepts the `.`/`_` separators the upstream regex allows, and surrounding
    /// whitespace.
    #[test]
    fn parses_alternate_separators_and_whitespace() {
        assert_eq!(
            claude_family_version("claude.opus.4.1"),
            Some(("opus", (4, 1)))
        );
        assert_eq!(
            claude_family_version("claude_opus_4_1"),
            Some(("opus", (4, 1)))
        );
        assert_eq!(
            claude_family_version("  claude-opus-4-1  "),
            Some(("opus", (4, 1)))
        );
        assert_eq!(
            claude_family_version("CLAUDE-OPUS-4-1"),
            Some(("opus", (4, 1)))
        );
    }

    /// 🔴 The snapshot-date trap: a date suffix must NOT become the minor version.
    /// `claude-opus-4-20250514` is 4.0 — parsing it as 4.20250514 would make every
    /// snapshot id "newer" than every real release and keep thinking on models
    /// that strip it.
    #[test]
    fn snapshot_dates_do_not_become_the_minor_version() {
        assert_eq!(
            claude_family_version("claude-opus-4-20250514"),
            Some(("opus", (4, 0))),
            "the 8-digit date must not parse as a minor"
        );
        assert!(!model_preserves_prior_thinking("claude-opus-4-20250514"));
        // A real 2-digit minor still parses.
        assert_eq!(
            claude_family_version("claude-opus-4-15"),
            Some(("opus", (4, 15)))
        );
    }

    /// Claude 3.x puts the family after the version, so the version regex does not
    /// match it — and the last-turn-only pattern does.
    #[test]
    fn claude3_is_not_version_parsed_but_is_last_turn_only() {
        assert_eq!(claude_family_version("claude-3-5-haiku"), None);
        assert!(last_turn_only_re().is_match("claude-3-5-haiku"));
        assert!(last_turn_only_re().is_match("claude-3-opus"));
        assert!(last_turn_only_re().is_match("claude-3.5-sonnet"));
        // 🔴 `(?!\\d)` keeps a hypothetical claude-30 out of the 3.x generation.
        assert!(!last_turn_only_re().is_match("claude-30-opus"));
    }

    #[test]
    fn non_claude_ids_do_not_parse() {
        assert_eq!(claude_family_version("gpt-4o"), None);
        assert_eq!(claude_family_version("deepseek-v3"), None);
        assert_eq!(claude_family_version(""), None);
    }

    // ---- retention policy --------------------------------------------------

    /// The KEEP_ALL_FROM thresholds, on both sides of each boundary.
    #[test]
    fn keeps_from_the_documented_version_onward() {
        // Opus 4.5+
        assert!(model_preserves_prior_thinking("claude-opus-4-5"));
        assert!(model_preserves_prior_thinking("claude-opus-4-6"));
        assert!(!model_preserves_prior_thinking("claude-opus-4-4"));
        assert!(!model_preserves_prior_thinking("claude-opus-4-1"));
        // Sonnet 4.6+
        assert!(model_preserves_prior_thinking("claude-sonnet-4-6"));
        assert!(model_preserves_prior_thinking("claude-sonnet-4-7"));
        assert!(!model_preserves_prior_thinking("claude-sonnet-4-5"));
        // Haiku 5.5+
        assert!(model_preserves_prior_thinking("claude-haiku-5-5"));
        assert!(!model_preserves_prior_thinking("claude-haiku-5-4"));
    }

    /// 🔴 Unknown or future Claude ids default to KEEP — the asymmetry is
    /// deliberate: replaying a stripped block is free, stripping a kept one
    /// rewrites the cached prefix on every call.
    #[test]
    fn unknown_and_future_claude_ids_default_to_keep() {
        // A family with no KEEP_ALL_FROM entry defaults to (5,0).
        assert!(model_preserves_prior_thinking("claude-fable-5-0"));
        assert!(model_preserves_prior_thinking("claude-mythos-9-9"));
        assert!(!model_preserves_prior_thinking("claude-fable-4-9"));
        // A Claude id the version regex cannot parse at all still keeps.
        assert!(model_preserves_prior_thinking("claude-next"));
        assert!(model_preserves_prior_thinking(
            "anthropic/claude-experimental"
        ));
    }

    /// Non-Claude ids never keep, whatever the relay.
    #[test]
    fn non_claude_ids_never_keep() {
        assert!(!model_preserves_prior_thinking("gpt-5"));
        assert!(!model_preserves_prior_thinking("deepseek-reasoner"));
        assert!(!model_preserves_prior_thinking("kimi-k2-turbo"));
        assert!(!model_preserves_prior_thinking("MiniMax-M2"));
        assert!(!model_preserves_prior_thinking(""));
    }

    /// Claude 3.x is last-turn-only: prior thinking is stripped server-side, so
    /// replaying it is pointless (and it is not signed).
    #[test]
    fn claude3_generation_never_keeps() {
        assert!(!model_preserves_prior_thinking("claude-3-5-haiku"));
        assert!(!model_preserves_prior_thinking("claude-3-5-sonnet"));
        assert!(!model_preserves_prior_thinking("claude-3-opus"));
        assert!(!model_preserves_prior_thinking("claude-3.7-sonnet"));
    }

    // ---- route vocabulary --------------------------------------------------

    #[test]
    fn route_wire_names_match_upstream() {
        assert_eq!(ThinkingRoute::Kimi.as_str(), "kimi");
        assert_eq!(ThinkingRoute::DeepSeek.as_str(), "deepseek");
        assert_eq!(ThinkingRoute::Native.as_str(), "native");
        assert_eq!(ThinkingRoute::MiniMax.as_str(), "minimax");
        assert_eq!(ThinkingRoute::ThirdParty.as_str(), "third_party");
    }

    /// hermes `SIGNED_REPLAY_ROUTES = {"native", "minimax"}` — exactly these two
    /// carry signatures the upstream verifies.
    #[test]
    fn only_native_and_minimax_are_signed_replay_routes() {
        assert!(ThinkingRoute::Native.is_signed_replay());
        assert!(ThinkingRoute::MiniMax.is_signed_replay());
        assert!(!ThinkingRoute::Kimi.is_signed_replay());
        assert!(!ThinkingRoute::DeepSeek.is_signed_replay());
        assert!(!ThinkingRoute::ThirdParty.is_signed_replay());
    }

    /// 🔴 MiniMax replays regardless of model — its docs require its own signed
    /// blocks back unchanged on every turn, so the model gate does not apply.
    #[test]
    fn minimax_replays_regardless_of_model() {
        assert!(ThinkingRoute::MiniMax.replays_prior_thinking("anything-at-all"));
        assert!(ThinkingRoute::MiniMax.replays_prior_thinking("claude-3-opus"));
        assert!(ThinkingRoute::MiniMax.replays_prior_thinking(""));
    }

    /// Native replays only when the model keeps prior thinking.
    #[test]
    fn native_replays_only_for_retaining_models() {
        assert!(ThinkingRoute::Native.replays_prior_thinking("claude-opus-4-5"));
        assert!(!ThinkingRoute::Native.replays_prior_thinking("claude-3-5-sonnet"));
        assert!(!ThinkingRoute::Native.replays_prior_thinking("claude-opus-4-1"));
    }

    /// Kimi/DeepSeek replay *unsigned* blocks (a different contract), and an
    /// unknown relay strips everything — so none of them count as signed replay.
    #[test]
    fn unsigned_and_strip_all_routes_never_replay_signed_thinking() {
        for route in [
            ThinkingRoute::Kimi,
            ThinkingRoute::DeepSeek,
            ThinkingRoute::ThirdParty,
        ] {
            assert!(
                !route.replays_prior_thinking("claude-opus-4-5"),
                "{} must not replay signed thinking",
                route.as_str()
            );
        }
    }

    #[test]
    fn preserves_prior_thinking_on_route_delegates_to_the_route() {
        assert!(preserves_prior_thinking_on_route(
            ThinkingRoute::Native,
            "claude-sonnet-4-6"
        ));
        assert!(!preserves_prior_thinking_on_route(
            ThinkingRoute::Native,
            "claude-3-5-haiku"
        ));
        assert!(preserves_prior_thinking_on_route(
            ThinkingRoute::MiniMax,
            "minimax-m2"
        ));
    }
}

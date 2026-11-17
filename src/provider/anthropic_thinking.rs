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

// ===========================================================================
// P3-1c slice2a — request-side thinking policy: model-family predicates and
// the effort→parameter tables.
//
// Verbatim port of `agent/anthropic_adapter.py:73-217` plus
// `agent/anthropic_endpoints.py:48-60`. These are the predicates
// `_thinking_kwargs` needs to decide WHICH thinking parameter shape a model
// accepts. Until now the port never sent a `thinking` parameter at all, so the
// API never returned thinking blocks and the response-side parser above was
// unreachable — hence this request-side layer is the load-bearing prerequisite
// for signed-thinking replay.
//
// 🔴 Numeric constants and family substring lists are copied EXACTLY (external
// contract: they encode which HTTP 400s each model family produces).
//
// Additive only: nothing consumes these yet, so runtime behaviour is unchanged.
// ===========================================================================

/// Effort → `thinking.budget_tokens` for the LEGACY manual-thinking path
/// (Claude ≤4.5 and non-Claude Anthropic-Messages models).
/// Upstream `THINKING_BUDGET` (`anthropic_adapter.py:73`).
pub const THINKING_BUDGET: &[(&str, u32)] = &[
    ("xhigh", 32000),
    ("high", 16000),
    ("medium", 8000),
    ("low", 4000),
];

/// Upstream `THINKING_BUDGET.get(effort, 8000)` — unknown effort falls back to
/// the `medium` budget rather than erroring.
pub fn thinking_budget(effort: &str) -> u32 {
    THINKING_BUDGET
        .iter()
        .find(|(k, _)| *k == effort)
        .map(|(_, v)| *v)
        .unwrap_or(8000)
}

/// Hermes effort → Anthropic adaptive-thinking effort (`output_config.effort`).
/// 4.7+ exposes low/medium/high/xhigh/max; Opus/Sonnet 4.6 have no `xhigh`, so
/// callers downgrade xhigh→max there (see [`supports_xhigh_effort`]). `minimal`
/// is a legacy alias for `low` on every model.
/// Upstream `ADAPTIVE_EFFORT_MAP` (`anthropic_adapter.py:77-80`).
pub const ADAPTIVE_EFFORT_MAP: &[(&str, &str)] = &[
    ("ultra", "max"),
    ("max", "max"),
    ("xhigh", "xhigh"),
    ("high", "high"),
    ("medium", "medium"),
    ("low", "low"),
    ("minimal", "low"),
];

/// Upstream `ADAPTIVE_EFFORT_MAP.get(effort, "medium")` — unknown effort
/// degrades to `medium`, never to an unmapped value the API would 400 on.
pub fn adaptive_effort(effort: &str) -> &str {
    ADAPTIVE_EFFORT_MAP
        .iter()
        .find(|(k, _)| *k == effort)
        .map(|(_, v)| *v)
        .unwrap_or("medium")
}

/// Older Claude families that need MANUAL thinking (`budget_tokens` only).
/// `claude-3` covers 3/3.5/3.7; the `-2025` entries are date-stamped 4.0 ids.
/// Upstream `_LEGACY_MANUAL_THINKING_CLAUDE_SUBSTRINGS` (`:91-96`).
const LEGACY_MANUAL_THINKING_CLAUDE_SUBSTRINGS: &[&str] = &[
    "claude-3",
    "claude-opus-4-0",
    "claude-opus-4.0",
    "claude-opus-4-1",
    "claude-opus-4.1",
    "claude-sonnet-4-0",
    "claude-sonnet-4.0",
    "claude-opus-4-2025",
    "claude-sonnet-4-2025",
    "claude-opus-4-5",
    "claude-opus-4.5",
    "claude-sonnet-4-5",
    "claude-sonnet-4.5",
    "claude-haiku-4-5",
    "claude-haiku-4.5",
];

/// Adaptive families that REJECT the `xhigh` effort (arrived with Opus 4.7) and
/// still accept sampling params. Upstream `_NO_XHIGH_CLAUDE_SUBSTRINGS` (`:99`).
const NO_XHIGH_CLAUDE_SUBSTRINGS: &[&str] = &[
    "claude-opus-4-6",
    "claude-opus-4.6",
    "claude-sonnet-4-6",
    "claude-sonnet-4.6",
];

/// Adaptive families where thinking is MANDATORY: `thinking: {"type":
/// "disabled"}` answers HTTP 400 (Portal flags them `reasoning.mandatory`).
/// The failure is asymmetric — a missing entry 400s the turn, a spurious one
/// only leaves thinking on — so when in doubt, add the family.
/// Upstream `_MANDATORY_THINKING_CLAUDE_SUBSTRINGS` (`:103`).
const MANDATORY_THINKING_CLAUDE_SUBSTRINGS: &[&str] =
    &["claude-fable", "claude-opus-5-5", "claude-opus-5.5"];

/// Families whose documented "off" is `thinking: {"type": "between_tools"}` (no
/// up-front thinking, short notes between tool calls): `disabled` 400s there
/// with a message pointing at it.
/// Upstream `_BETWEEN_TOOLS_OFF_CLAUDE_SUBSTRINGS` (`:106`).
const BETWEEN_TOOLS_OFF_CLAUDE_SUBSTRINGS: &[&str] = &["claude-sonnet-5-5", "claude-sonnet-5.5"];

/// Kimi/Moonshot model-id prefixes whose Anthropic-compatible endpoints
/// implement the ADAPTIVE contract (so they are not Claude yet still take the
/// modern path). Upstream `_KIMI_FAMILY_MODEL_PREFIXES`
/// (`anthropic_endpoints.py:48-50`).
const KIMI_FAMILY_MODEL_PREFIXES: &[&str] = &[
    "kimi-",
    "kimi_",
    "moonshot-",
    "moonshot_",
    "k1.",
    "k1-",
    "k2.",
    "k2-",
    "k25",
    "k2.5",
    "k3.",
    "k3-",
];

/// Bare release slugs with no separator suffix (Kimi Coding Plan serves K3 as
/// exactly `k3`); exact-match so unrelated names sharing the prefix don't
/// match. Upstream `_KIMI_FAMILY_EXACT_SLUGS` (`:53`).
const KIMI_FAMILY_EXACT_SLUGS: &[&str] = &["k3"];

/// Upstream `_is_claude_model` (`:109-110`).
pub fn is_claude_model(model: &str) -> bool {
    model.to_lowercase().contains("claude")
}

/// Case-insensitive substring match of `model` against a family list.
/// Upstream `_model_matches` (`:113-116`).
fn model_matches(model: &str, substrings: &[&str]) -> bool {
    let m = model.to_lowercase();
    substrings.iter().any(|v| m.contains(v))
}

/// Kimi/Moonshot family membership by MODEL NAME. Strips a `org/` prefix first
/// (`moonshotai/kimi-k2.5` → `kimi-k2.5`), then matches exact slugs or prefixes.
/// Upstream `_model_name_is_kimi_family` (`anthropic_endpoints.py:56-60`).
pub fn model_name_is_kimi_family(model: &str) -> bool {
    let m = model.trim().to_lowercase();
    let m = m.rsplit('/').next().unwrap_or("");
    !m.is_empty()
        && (KIMI_FAMILY_EXACT_SLUGS.contains(&m)
            || KIMI_FAMILY_MODEL_PREFIXES.iter().any(|p| m.starts_with(p)))
}

/// True for models using ADAPTIVE thinking (Claude 4.6+). Unknown Claude models
/// DEFAULT to adaptive (an allowlist of "modern" versions would go stale and
/// silently route a new model down the legacy path); the explicit legacy list
/// stays manual; non-Claude models are False — EXCEPT Kimi/Moonshot, whose
/// Anthropic-compatible endpoints implement the adaptive contract.
/// Upstream `_supports_adaptive_thinking` (`:172-178`).
pub fn supports_adaptive_thinking(model: &str) -> bool {
    model_name_is_kimi_family(model)
        || (is_claude_model(model)
            && !model_matches(model, LEGACY_MANUAL_THINKING_CLAUDE_SUBSTRINGS))
}

/// True for models accepting the `xhigh` effort (Opus 4.7+). Opus/Sonnet 4.6
/// 400 on it — callers downgrade xhigh→max when this is False.
/// Upstream `_supports_xhigh_effort` (`:181-184`).
pub fn supports_xhigh_effort(model: &str) -> bool {
    supports_adaptive_thinking(model) && !model_matches(model, NO_XHIGH_CLAUDE_SUBSTRINGS)
}

/// True when `model` accepts an explicit `thinking: {"type": "disabled"}`.
/// Adaptive Claude thinks BY DEFAULT, so "off" only works if the disable is
/// sent; mandatory-thinking families 400 on it and keep the omit behaviour.
/// Legacy manual-thinking models are opt-in via `budget_tokens`, so omission is
/// already off. Scoped to Claude: Kimi's documented disable is omission, and
/// sending it a new parameter on the strength of Claude's contract is a guess.
/// Upstream `_accepts_thinking_disable` (`:187-197`).
pub fn accepts_thinking_disable(model: &str) -> bool {
    // Upstream writes `not _model_matches(model, MANDATORY + BETWEEN_TOOLS)`.
    // Rust slices cannot be concatenated in a const context, and building the
    // union on the heap just to match against it is pointless: by De Morgan,
    // `!matches(A ∪ B)` is exactly `!matches(A) && !matches(B)`.
    is_claude_model(model)
        && supports_adaptive_thinking(model)
        && !model_matches(model, MANDATORY_THINKING_CLAUDE_SUBSTRINGS)
        && !model_matches(model, BETWEEN_TOOLS_OFF_CLAUDE_SUBSTRINGS)
}

/// Haiku generations that never get a thinking request (through 4.5).
/// Haiku 5.5+ uses adaptive thinking. Reuses slice1's `claude_family_version`.
/// Upstream `_is_pre_thinking_haiku` (`:200-207`): `parsed is None or
/// parsed[0] != "haiku" or parsed[1] < (5, 5)`.
pub fn is_pre_thinking_haiku(model: &str) -> bool {
    if !model.to_lowercase().contains("haiku") {
        return false;
    }
    match claude_family_version(model) {
        None => true,
        Some((family, ver)) => family != "haiku" || ver < (5, 5),
    }
}

/// True for models that 400 on any non-default temperature/top_p/top_k (Opus
/// 4.7 and later; unknown Claude defaults to FORBIDDING). The 4.6 family and the
/// legacy manual-thinking families still accept them. Callers omit the fields
/// entirely — the API rejects anything non-null.
/// Upstream `_forbids_sampling_params` (`:210-217`).
pub fn forbids_sampling_params(model: &str) -> bool {
    // Upstream: `not _model_matches(model, NO_XHIGH + LEGACY_MANUAL)` — same
    // De Morgan expansion as `accepts_thinking_disable` above.
    is_claude_model(model)
        && !model_matches(model, NO_XHIGH_CLAUDE_SUBSTRINGS)
        && !model_matches(model, LEGACY_MANUAL_THINKING_CLAUDE_SUBSTRINGS)
}

// ===========================================================================
// P3-1c slice2b — the effort→request-parameter mapping.
//
// Verbatim port of `anthropic_adapter.py:616-642` (`_thinking_kwargs`) and
// `hermes_constants.py:1000-1038` (`VALID_REASONING_EFFORTS` +
// `parse_reasoning_effort`).
//
// Still additive: `build_body` does not consult these yet (slice2c wires them),
// so no request changes shape and runtime behaviour is unchanged.
//
// 🔴 ORDERING CONTRACT for slice2c — do not reorder at the call site:
// `_thinking_kwargs` runs FIRST, then `_forbids_sampling_params` strips
// temperature/top_p/top_k (`anthropic_adapter.py:700-706`). The legacy branch
// below deliberately sets `temperature = 1` (Anthropic requires it when manual
// thinking is enabled); on 4.7+ models that forbid sampling params the safety
// net must remove it again. Upstream calls this a "safety net so 4.6 -> 4.7
// migrations don't need coordinated edits everywhere callers set sampling
// params". Emitting temperature without the strip produces HTTP 400.
// ===========================================================================

/// Accepted `agent.reasoning_effort` values.
/// Upstream `VALID_REASONING_EFFORTS` (`hermes_constants.py:1000`).
pub const VALID_REASONING_EFFORTS: &[&str] =
    &["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

/// Resolved reasoning configuration — the three states
/// `parse_reasoning_effort` can produce.
///
/// 🔴 `Unresolved` and `Disabled` are DIFFERENT and must not be collapsed:
/// upstream keeps the raw value on purpose, because "`or ""` would turn a YAML
/// False into "" and silently re-enable thinking".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReasoningConfig {
    /// Empty or unrecognized input → caller falls back to its default.
    ///
    /// 🔴 Also the `Default` variant, which is what makes adding
    /// [`crate::provider::ProviderRequest::reasoning_config`] non-breaking: a
    /// construction site that says nothing means "no opinion", and
    /// `thinking_request` emits NO `thinking` parameter for it — byte-identical
    /// wire output to before the field existed.
    #[default]
    Unresolved,
    /// `reasoning_effort: none|false|disabled` → an explicit opt-out.
    Disabled,
    /// A recognized level. `effort` is passed through VERBATIM so providers with
    /// bespoke thinking tiers can be asked for their real level; the wire layer
    /// tolerates unknown names.
    Enabled { effort: String },
}

/// Parse a reasoning-effort setting into a [`ReasoningConfig`].
/// Upstream `parse_reasoning_effort` (`hermes_constants.py:1003-1028`).
///
/// 🔴 Structural difference (class (a), not an omission): upstream takes `Any`
/// and also handles `None`/`True` and a dict form
/// (`{"enabled": …, "effort": …}`). This port's config field is a typed
/// `String` (`config/mod.rs:305 agent.reasoning_effort`), so only the string
/// branch is reachable; the bool/dict forms cannot be constructed. Upstream's
/// `False -> "false" -> disabled` path is preserved here because a YAML `false`
/// reaching a String field arrives as the literal text "false".
pub fn parse_reasoning_effort(effort: &str) -> ReasoningConfig {
    let e = effort.trim().to_lowercase();
    if e.is_empty() {
        return ReasoningConfig::Unresolved;
    }
    if e == "none" || e == "false" || e == "disabled" {
        return ReasoningConfig::Disabled;
    }
    if VALID_REASONING_EFFORTS.contains(&e.as_str()) {
        return ReasoningConfig::Enabled { effort: e };
    }
    // Upstream warns and returns None (the caller then uses the default) so a
    // typo like `hgih` never reaches the wire.
    tracing::warn!(
        effort = %effort,
        "Unknown reasoning_effort, using default (medium)"
    );
    ReasoningConfig::Unresolved
}

/// Anthropic's `thinking.type` discriminator. Wire values are the external
/// contract — spelled exactly as upstream sends them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingType {
    /// Claude 4.6+ / Kimi: adaptive thinking, effort via `output_config`.
    Adaptive,
    /// Legacy manual thinking with `budget_tokens`.
    Enabled,
    /// Explicit opt-out — only legal where [`accepts_thinking_disable`].
    Disabled,
    /// Families whose documented "off" is between-tools notes; `disabled` 400s.
    BetweenTools,
}

impl ThinkingType {
    /// Wire spelling of `thinking.type`.
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingType::Adaptive => "adaptive",
            ThinkingType::Enabled => "enabled",
            ThinkingType::Disabled => "disabled",
            ThinkingType::BetweenTools => "between_tools",
        }
    }
}

/// The request parameters `_thinking_kwargs` produces.
///
/// `None` from [`thinking_request`] means "omit the `thinking` parameter
/// entirely" — which is NOT the same as sending `disabled` (adaptive models
/// think by default, so omission is not a disable; and mandatory-thinking
/// families 400 on an explicit disable).
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkingRequest {
    /// `thinking.type`
    pub thinking_type: ThinkingType,
    /// `thinking.display` — "summarized" on the adaptive path only. On 4.7+ the
    /// field defaults to "omitted", which silently hides reasoning text the CLI
    /// surfaces, so "summarized" is requested to keep the activity feed
    /// populated during long tool runs.
    pub display: Option<&'static str>,
    /// `thinking.budget_tokens` — legacy manual path only.
    pub budget_tokens: Option<u32>,
    /// `output_config.effort` — adaptive path only.
    pub output_effort: Option<String>,
    /// Top-level `temperature`. Anthropic REQUIRES `temperature: 1` when manual
    /// thinking is enabled on older models. See the ordering contract above:
    /// the caller must strip this when [`forbids_sampling_params`].
    pub temperature: Option<f32>,
    /// Top-level `max_tokens` floor: `max(effective, budget + 4096)`.
    pub max_tokens: Option<u32>,
}

/// Map a resolved reasoning config to Anthropic thinking parameters.
/// Upstream `_thinking_kwargs` (`anthropic_adapter.py:616-642`).
///
/// Decision order is load-bearing and copied exactly:
/// 1. explicit disable → `between_tools` for the families that document it,
///    else `disabled` only where accepted, else **omit** (a silently-ignored
///    disable beats a dead turn)
/// 2. Haiku through 4.5 → omit (never gets a thinking request)
/// 3. adaptive models → `adaptive` + `display: summarized` + `output_config`
///    (xhigh downgraded to max where the family rejects xhigh)
/// 4. everything else → legacy manual `enabled` + `budget_tokens` + `temperature: 1`
///    + a raised `max_tokens` floor
pub fn thinking_request(
    config: &ReasoningConfig,
    model: &str,
    effective_max_tokens: u32,
) -> Option<ThinkingRequest> {
    // An unresolved config means "caller has no opinion"; upstream only calls
    // `_thinking_kwargs` when `reasoning_config` is a truthy dict, so nothing is
    // emitted here. `Disabled` is a real opt-out and must go through step 1.
    if matches!(config, ReasoningConfig::Unresolved) {
        return None;
    }

    if matches!(config, ReasoningConfig::Disabled) {
        // Adaptive models think by DEFAULT, so omitting the parameter is not a
        // disable — the user silently keeps paying.
        if model_matches(model, BETWEEN_TOOLS_OFF_CLAUDE_SUBSTRINGS) {
            return Some(ThinkingRequest {
                thinking_type: ThinkingType::BetweenTools,
                display: None,
                budget_tokens: None,
                output_effort: None,
                temperature: None,
                max_tokens: None,
            });
        }
        // Mandatory-thinking models 400 on the disable, so they keep the
        // omission: a silently-ignored disable beats a dead turn.
        if accepts_thinking_disable(model) {
            return Some(ThinkingRequest {
                thinking_type: ThinkingType::Disabled,
                display: None,
                budget_tokens: None,
                output_effort: None,
                temperature: None,
                max_tokens: None,
            });
        }
        return None;
    }

    // Step 2: Haiku through 4.5 never gets a thinking request.
    if is_pre_thinking_haiku(model) {
        return None;
    }

    // Upstream: `str(reasoning_config.get("effort", "medium")).lower()`. The
    // lookup helpers below already apply upstream's own fallbacks
    // (adaptive → "medium", budget → 8000) for an unmapped level, so passing an
    // empty string through behaves exactly like "medium".
    let effort = match config {
        ReasoningConfig::Enabled { effort } => effort.to_lowercase(),
        _ => "medium".to_string(),
    };

    // Step 3: adaptive contract (Claude 4.6+, Kimi/Moonshot).
    if supports_adaptive_thinking(model) {
        let mut out = adaptive_effort(&effort).to_string();
        // Opus/Sonnet 4.6 have no xhigh and 400 on it → downgrade to max.
        if out == "xhigh" && !supports_xhigh_effort(model) {
            out = "max".to_string();
        }
        return Some(ThinkingRequest {
            thinking_type: ThinkingType::Adaptive,
            display: Some("summarized"),
            budget_tokens: None,
            output_effort: Some(out),
            temperature: None,
            max_tokens: None,
        });
    }

    // Step 4: legacy manual thinking (Claude ≤4.5, MiniMax, qwen3, GLM, …).
    let budget = thinking_budget(&effort);
    Some(ThinkingRequest {
        thinking_type: ThinkingType::Enabled,
        display: None,
        budget_tokens: Some(budget),
        output_effort: None,
        // Required when thinking is enabled on older models.
        temperature: Some(1.0),
        // Upstream: max(effective_max_tokens, budget + 4096).
        max_tokens: Some(effective_max_tokens.max(budget.saturating_add(4096))),
    })
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

    // -----------------------------------------------------------------------
    // P3-1c slice2a — request-side thinking policy predicates.
    //
    // Every expectation below is taken from upstream `anthropic_adapter.py`
    // (`THINKING_BUDGET`:73, `ADAPTIVE_EFFORT_MAP`:77-80, the four family
    // substring lists:91-106, `_supports_adaptive_thinking`:172-178,
    // `_supports_xhigh_effort`:181-184, `_accepts_thinking_disable`:187-197,
    // `_is_pre_thinking_haiku`:200-207, `_forbids_sampling_params`:210-217)
    // and `anthropic_endpoints.py:48-60` (Kimi family). If one of these fails,
    // the PORT is wrong — do not relax the assertion to match.
    // -----------------------------------------------------------------------

    #[test]
    fn thinking_budget_matches_upstream_table_with_medium_fallback() {
        assert_eq!(thinking_budget("xhigh"), 32000);
        assert_eq!(thinking_budget("high"), 16000);
        assert_eq!(thinking_budget("medium"), 8000);
        assert_eq!(thinking_budget("low"), 4000);
        // Upstream: THINKING_BUDGET.get(effort, 8000) — unknown falls back.
        assert_eq!(thinking_budget("ultra"), 8000);
        assert_eq!(thinking_budget(""), 8000);
    }

    #[test]
    fn adaptive_effort_map_includes_legacy_aliases() {
        assert_eq!(adaptive_effort("ultra"), "max");
        assert_eq!(adaptive_effort("max"), "max");
        assert_eq!(adaptive_effort("xhigh"), "xhigh");
        assert_eq!(adaptive_effort("high"), "high");
        assert_eq!(adaptive_effort("medium"), "medium");
        assert_eq!(adaptive_effort("low"), "low");
        // "minimal" is a legacy alias for low on EVERY model.
        assert_eq!(adaptive_effort("minimal"), "low");
        // Upstream: .get(effort, "medium").
        assert_eq!(adaptive_effort("bogus"), "medium");
    }

    #[test]
    fn adaptive_thinking_defaults_to_modern_for_unknown_claude() {
        // 🔴 The load-bearing default: an allowlist of "modern" versions would
        // go stale and silently route a NEW model down the legacy path.
        assert!(supports_adaptive_thinking("claude-opus-4-6"));
        assert!(supports_adaptive_thinking("claude-opus-5-5"));
        assert!(supports_adaptive_thinking("claude-sonnet-9-9-20300101"));
        // Explicit legacy list stays manual.
        assert!(!supports_adaptive_thinking("claude-sonnet-4-5"));
        assert!(!supports_adaptive_thinking("claude-haiku-4.5"));
        assert!(!supports_adaptive_thinking("claude-3-7-sonnet"));
        // Non-Claude: false — EXCEPT the Kimi family.
        assert!(!supports_adaptive_thinking("gpt-4o"));
        assert!(supports_adaptive_thinking("kimi-k2.5"));
        assert!(supports_adaptive_thinking("moonshotai/kimi-k2.5"));
    }

    #[test]
    fn kimi_family_strips_org_prefix_and_honours_exact_slugs() {
        assert!(model_name_is_kimi_family("kimi-k2.5"));
        assert!(model_name_is_kimi_family("moonshotai/kimi-k2.5"));
        assert!(model_name_is_kimi_family("moonshot-v1"));
        assert!(model_name_is_kimi_family("k1.5"));
        assert!(model_name_is_kimi_family("k25"));
        // Bare release slug, exact-match only (Kimi Coding Plan serves K3 as "k3").
        assert!(model_name_is_kimi_family("k3"));
        assert!(model_name_is_kimi_family("k3-turbo"));
        assert!(!model_name_is_kimi_family("gpt-4o"));
        assert!(!model_name_is_kimi_family(""));
        assert!(!model_name_is_kimi_family("k3000xl"));
    }

    #[test]
    fn xhigh_effort_rejected_only_by_the_4_6_family() {
        assert!(supports_xhigh_effort("claude-opus-5"));
        assert!(supports_xhigh_effort("claude-opus-4-7"));
        // Opus/Sonnet 4.6 400 on xhigh → callers downgrade to max.
        assert!(!supports_xhigh_effort("claude-opus-4-6"));
        assert!(!supports_xhigh_effort("claude-sonnet-4.6"));
        // Legacy manual models are not adaptive at all.
        assert!(!supports_xhigh_effort("claude-sonnet-4-5"));
        assert!(!supports_xhigh_effort("gpt-4o"));
    }

    #[test]
    fn thinking_disable_accepted_except_mandatory_and_between_tools_families() {
        // Adaptive Claude thinks BY DEFAULT, so "off" only works if sent.
        assert!(accepts_thinking_disable("claude-opus-4-6"));
        assert!(accepts_thinking_disable("claude-sonnet-4-7"));
        // Mandatory-thinking families 400 on a disable → keep the omit behaviour.
        assert!(!accepts_thinking_disable("claude-opus-5-5"));
        assert!(!accepts_thinking_disable("claude-fable"));
        // These document `between_tools` as their "off"; `disabled` 400s there.
        assert!(!accepts_thinking_disable("claude-sonnet-5-5"));
        // Legacy manual models: omission is already off, and they are not adaptive.
        assert!(!accepts_thinking_disable("claude-sonnet-4-5"));
        // Scoped to Claude: Kimi's documented disable is omission.
        assert!(!accepts_thinking_disable("kimi-k2.5"));
        assert!(!accepts_thinking_disable("gpt-4o"));
    }

    #[test]
    fn pre_thinking_haiku_covers_generations_through_4_5() {
        assert!(is_pre_thinking_haiku("claude-haiku-4-5"));
        assert!(is_pre_thinking_haiku("claude-haiku-4.5"));
        assert!(is_pre_thinking_haiku("claude-haiku-3-5"));
        // Haiku 5.5+ uses adaptive thinking.
        assert!(!is_pre_thinking_haiku("claude-haiku-5-5"));
        assert!(!is_pre_thinking_haiku("claude-haiku-6"));
        // Non-haiku never matches, even when unparseable.
        assert!(!is_pre_thinking_haiku("claude-opus-5"));
        assert!(!is_pre_thinking_haiku("gpt-4o"));
        // 🔴 Upstream short-circuits on the literal "haiku" substring BEFORE
        // parsing, so an unparseable haiku id is still pre-thinking (fail-safe:
        // omitting the thinking param is the conservative choice there).
        assert!(is_pre_thinking_haiku("haiku-experimental"));
    }

    #[test]
    fn sampling_params_forbidden_for_modern_claude_only() {
        // Opus 4.7 and later; unknown Claude DEFAULTS to forbidding.
        assert!(forbids_sampling_params("claude-opus-5"));
        assert!(forbids_sampling_params("claude-sonnet-9-9"));
        // The 4.6 family still accepts them.
        assert!(!forbids_sampling_params("claude-opus-4-6"));
        assert!(!forbids_sampling_params("claude-sonnet-4.6"));
        // Legacy manual-thinking families still accept them.
        assert!(!forbids_sampling_params("claude-sonnet-4-5"));
        assert!(!forbids_sampling_params("claude-3-5-sonnet"));
        // Non-Claude unaffected.
        assert!(!forbids_sampling_params("gpt-4o"));
        assert!(!forbids_sampling_params("kimi-k2.5"));
    }

    #[test]
    fn is_claude_model_is_case_insensitive_substring() {
        assert!(is_claude_model("claude-opus-5"));
        assert!(is_claude_model("anthropic/CLAUDE-3-5-sonnet"));
        assert!(!is_claude_model("gpt-4o"));
        assert!(!is_claude_model(""));
    }

    // -----------------------------------------------------------------------
    // P3-1c slice2b — `_thinking_kwargs` decision order + effort parsing.
    // Expectations come from `anthropic_adapter.py:616-642` and
    // `hermes_constants.py:1000-1028`. If one fails, the PORT is wrong.
    // -----------------------------------------------------------------------

    fn enabled(e: &str) -> ReasoningConfig {
        ReasoningConfig::Enabled {
            effort: e.to_string(),
        }
    }

    #[test]
    fn parse_effort_maps_the_three_upstream_states() {
        assert_eq!(parse_reasoning_effort(""), ReasoningConfig::Unresolved);
        assert_eq!(parse_reasoning_effort("   "), ReasoningConfig::Unresolved);
        // Explicit opt-outs, incl. YAML `false` arriving as text.
        assert_eq!(parse_reasoning_effort("none"), ReasoningConfig::Disabled);
        assert_eq!(parse_reasoning_effort("false"), ReasoningConfig::Disabled);
        assert_eq!(
            parse_reasoning_effort("disabled"),
            ReasoningConfig::Disabled
        );
        assert_eq!(parse_reasoning_effort("False"), ReasoningConfig::Disabled);
        // Valid levels pass through verbatim (lowercased + trimmed).
        assert_eq!(parse_reasoning_effort("medium"), enabled("medium"));
        assert_eq!(parse_reasoning_effort("  XHIGH "), enabled("xhigh"));
        assert_eq!(parse_reasoning_effort("ultra"), enabled("ultra"));
        assert_eq!(parse_reasoning_effort("minimal"), enabled("minimal"));
        // 🔴 A typo must NOT reach the wire — falls back to Unresolved.
        assert_eq!(parse_reasoning_effort("hgih"), ReasoningConfig::Unresolved);
    }

    #[test]
    fn unresolved_config_emits_nothing() {
        // Upstream only calls _thinking_kwargs when reasoning_config is truthy.
        assert!(thinking_request(&ReasoningConfig::Unresolved, "claude-opus-5", 8000).is_none());
    }

    #[test]
    fn disabled_takes_the_between_tools_exit_for_sonnet_5_5() {
        // `disabled` 400s there with a message pointing at between_tools.
        let r = thinking_request(&ReasoningConfig::Disabled, "claude-sonnet-5-5", 8000).unwrap();
        assert_eq!(r.thinking_type, ThinkingType::BetweenTools);
        assert_eq!(r.thinking_type.as_str(), "between_tools");
        assert!(r.display.is_none() && r.budget_tokens.is_none());
        assert!(r.output_effort.is_none() && r.temperature.is_none());
    }

    #[test]
    fn disabled_is_sent_only_where_accepted() {
        // Adaptive Claude thinks BY DEFAULT → omission is not a disable.
        let r = thinking_request(&ReasoningConfig::Disabled, "claude-opus-4-6", 8000).unwrap();
        assert_eq!(r.thinking_type, ThinkingType::Disabled);
        assert_eq!(r.thinking_type.as_str(), "disabled");
    }

    #[test]
    fn disabled_omits_for_mandatory_thinking_families() {
        // These 400 on an explicit disable: "a silently-ignored disable beats a
        // dead turn", so upstream keeps the omission.
        assert!(thinking_request(&ReasoningConfig::Disabled, "claude-opus-5-5", 8000).is_none());
        assert!(thinking_request(&ReasoningConfig::Disabled, "claude-fable", 8000).is_none());
    }

    #[test]
    fn disabled_omits_for_legacy_and_non_claude_models() {
        // Legacy manual models are opt-in via budget_tokens → omission is off.
        assert!(thinking_request(&ReasoningConfig::Disabled, "claude-sonnet-4-5", 8000).is_none());
        assert!(thinking_request(&ReasoningConfig::Disabled, "claude-3-5-sonnet", 8000).is_none());
        // Scoped to Claude: Kimi's documented disable is omission.
        assert!(thinking_request(&ReasoningConfig::Disabled, "kimi-k2.5", 8000).is_none());
        assert!(thinking_request(&ReasoningConfig::Disabled, "gpt-4o", 8000).is_none());
    }

    #[test]
    fn pre_thinking_haiku_never_gets_a_thinking_request() {
        assert!(thinking_request(&enabled("high"), "claude-haiku-4-5", 8000).is_none());
        assert!(thinking_request(&enabled("high"), "claude-haiku-3-5", 8000).is_none());
        // Haiku 5.5+ IS adaptive → gets a request.
        assert!(thinking_request(&enabled("high"), "claude-haiku-5-5", 8000).is_some());
    }

    #[test]
    fn adaptive_path_requests_summarized_display_and_output_config() {
        let r = thinking_request(&enabled("high"), "claude-opus-5", 8000).unwrap();
        assert_eq!(r.thinking_type, ThinkingType::Adaptive);
        // 4.7+ defaults display to "omitted", which would hide the reasoning the
        // CLI surfaces → "summarized" keeps the activity feed populated.
        assert_eq!(r.display, Some("summarized"));
        assert_eq!(r.output_effort.as_deref(), Some("high"));
        // Adaptive path sends NO budget_tokens / temperature / max_tokens floor.
        assert!(r.budget_tokens.is_none());
        assert!(r.temperature.is_none());
        assert!(r.max_tokens.is_none());
    }

    #[test]
    fn adaptive_effort_downgrades_xhigh_only_on_the_4_6_family() {
        // Opus/Sonnet 4.6 have no xhigh and 400 on it.
        let r = thinking_request(&enabled("xhigh"), "claude-opus-4-6", 8000).unwrap();
        assert_eq!(r.output_effort.as_deref(), Some("max"));
        let r2 = thinking_request(&enabled("xhigh"), "claude-sonnet-4.6", 8000).unwrap();
        assert_eq!(r2.output_effort.as_deref(), Some("max"));
        // 4.7+ accepts xhigh.
        let r3 = thinking_request(&enabled("xhigh"), "claude-opus-4-7", 8000).unwrap();
        assert_eq!(r3.output_effort.as_deref(), Some("xhigh"));
        // ultra/max alias to max.
        let r4 = thinking_request(&enabled("ultra"), "claude-opus-5", 8000).unwrap();
        assert_eq!(r4.output_effort.as_deref(), Some("max"));
        // minimal is a legacy alias for low on every model.
        let r5 = thinking_request(&enabled("minimal"), "claude-opus-5", 8000).unwrap();
        assert_eq!(r5.output_effort.as_deref(), Some("low"));
        // Unknown level → upstream's ADAPTIVE_EFFORT_MAP fallback "medium".
        let r6 = thinking_request(&enabled("bogus"), "claude-opus-5", 8000).unwrap();
        assert_eq!(r6.output_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn kimi_family_uses_the_adaptive_contract() {
        let r = thinking_request(&enabled("medium"), "kimi-k2.5", 8000).unwrap();
        assert_eq!(r.thinking_type, ThinkingType::Adaptive);
        assert_eq!(r.display, Some("summarized"));
        assert_eq!(r.output_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn legacy_path_sets_budget_temperature_one_and_raises_max_tokens() {
        let r = thinking_request(&enabled("medium"), "claude-sonnet-4-5", 8000).unwrap();
        assert_eq!(r.thinking_type, ThinkingType::Enabled);
        assert_eq!(r.thinking_type.as_str(), "enabled");
        assert_eq!(r.budget_tokens, Some(8000));
        // 🔴 Anthropic REQUIRES temperature=1 when manual thinking is enabled.
        // The caller must strip it when forbids_sampling_params(model) — see the
        // ordering contract in the module header.
        assert_eq!(r.temperature, Some(1.0));
        // max(effective, budget + 4096)
        assert_eq!(r.max_tokens, Some(8000 + 4096));
        // Adaptive-only fields are absent on this path.
        assert!(r.display.is_none() && r.output_effort.is_none());
    }

    #[test]
    fn legacy_max_tokens_floor_keeps_a_larger_effective_value() {
        // max(effective=32000, budget 16000 + 4096 = 20096) → 32000
        let r = thinking_request(&enabled("high"), "claude-sonnet-4-5", 32000).unwrap();
        assert_eq!(r.budget_tokens, Some(16000));
        assert_eq!(r.max_tokens, Some(32000));
        // xhigh on legacy: budget 32000 + 4096 = 36096 > effective 8000
        let r2 = thinking_request(&enabled("xhigh"), "claude-3-5-sonnet", 8000).unwrap();
        assert_eq!(r2.budget_tokens, Some(32000));
        assert_eq!(r2.max_tokens, Some(36096));
    }

    #[test]
    fn legacy_path_covers_non_claude_anthropic_messages_models() {
        // MiniMax / qwen3 / GLM need the manual-thinking path.
        for m in ["minimax-m2", "qwen3-max", "glm-4.6"] {
            let r = thinking_request(&enabled("low"), m, 8000)
                .unwrap_or_else(|| panic!("legacy path must apply to {m}"));
            assert_eq!(r.thinking_type, ThinkingType::Enabled, "model={m}");
            assert_eq!(r.budget_tokens, Some(4000), "model={m}");
            assert_eq!(r.temperature, Some(1.0), "model={m}");
        }
        // Non-anthropic-protocol models never reach here in practice; gpt-4o is
        // not Claude/Kimi and is not pre-thinking-haiku, so it falls to legacy —
        // harmless because only the anthropic provider calls this function.
        let r = thinking_request(&enabled("low"), "gpt-4o", 8000).unwrap();
        assert_eq!(r.thinking_type, ThinkingType::Enabled);
    }

    #[test]
    fn empty_effort_behaves_like_medium_on_both_paths() {
        // Upstream: reasoning_config.get("effort", "medium"); a dict form with an
        // empty level resolves to None upstream, so this is the fallback path.
        let a = thinking_request(&enabled(""), "claude-opus-5", 8000).unwrap();
        assert_eq!(a.output_effort.as_deref(), Some("medium"));
        let b = thinking_request(&enabled(""), "claude-sonnet-4-5", 8000).unwrap();
        assert_eq!(b.budget_tokens, Some(8000));
    }
}

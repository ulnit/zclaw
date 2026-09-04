//! Random startup tips shown at CLI session start to help users discover
//! features — port of hermes `hermes_cli/tips.py` (v2026.8.3), with the
//! corpus rewritten for ulnclaw's actual surface.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Tip corpus — one-liners covering slash commands, CLI subcommands, config,
// tools, browser, gateway, skills, goals, and workflow tricks.
// ---------------------------------------------------------------------------

pub const TIPS: &[&str] = &[
    // --- Slash commands ---
    "/goal <text> sets a standing Ralph-loop objective — ulnclaw auto-continues turn after turn until a judge model says done.",
    "/goal draft <objective> expands a plain objective into a structured completion contract (outcome/verification/constraints/boundaries/stop_when).",
    "Inline contract lines work too: /goal Migrate auth to JWT followed by verify: the auth test suite passes.",
    "/goal wait <pid> parks the goal loop on a background process — no re-poking until it exits.",
    "/subgoal <text> adds extra criteria the judge must also see satisfied before the goal counts as done.",
    "/rollback lists filesystem checkpoints — restore files the agent modified to any prior state.",
    "/rollback diff 2 previews what changed since checkpoint 2 without restoring anything.",
    "/rollback 2 src/file.py restores a single file from a specific checkpoint.",
    "/diff shows the cumulative working-tree diff for this session — what has the agent changed here?",
    "/gitdiff staged shows exactly what is staged for the next commit.",
    "/recap summarizes recent activity in this conversation — handy after a long session.",
    "/search <text> full-text searches all past sessions.",
    "/moa <prompt> runs one prompt through the Mixture-of-Agents preset: parallel reference models + an aggregator synthesis.",
    "/browser connect ws://127.0.0.1:9222/... attaches browser tools to a running Chromium-family browser via CDP.",
    "/browser status shows the active browser endpoint and mode (managed vs endpoint vs camofox).",
    "/memory shows the persistent memory the agent carries across sessions.",
    "/usage points at the per-session token usage tracked in state.db.",
    "/new starts a fresh conversation — and shows you one of these tips.",
    // --- Goals & workflows ---
    "A goal with a contract is judged against evidence: the judge wants command output or file contents, not just a claim of 'done'.",
    "While a goal is active, every turn is judged — pause any time with /goal pause, resume with /goal resume.",
    "Set [auxiliary.goal_judge] in config.toml to route the goal judge to a cheap, strict model instead of your main one.",
    // --- CLI flags & subcommands ---
    "ulnclaw run \"one-shot prompt\" runs a single non-interactive query and exits.",
    "ulnclaw sessions list|show|export|recover — manage, export (Markdown/HTML), and rescue damaged session databases.",
    "ulnclaw sessions recap <id> prints a compact recap of any stored session.",
    "ulnclaw skills lists installed skills; ulnclaw skills blueprints shows starter templates.",
    "ulnclaw journey renders your learning timeline — skills and memories the agent accumulated, with --play animation.",
    "ulnclaw journey edit <node> opens a learned skill or memory in $EDITOR.",
    "ulnclaw curator status shows which agent-created skills are idle and would be pruned; --dry-run previews.",
    "ulnclaw curator adopt stamps provenance on skills you want to keep out of pruning forever.",
    "ulnclaw moa run|list|delete manages saved Mixture-of-Agents presets.",
    "ulnclaw models providers|list|info|refresh browses the models.dev catalog with context windows and capabilities.",
    "ulnclaw cron add/list/run schedules recurring prompts — jobs survive restarts in state.db.",
    "ulnclaw checkpoints list|restore|diff|prune manages the shadow-git checkpoint store from outside a session.",
    "ulnclaw gateway starts the OpenAI-compatible HTTP gateway on [gateway] host/port.",
    "ulnclaw init writes a starter config.toml to ~/.ulnclaw.",
    "ulnclaw -p work chat runs under the [profiles.work] override without changing your default model.",
    // --- Configuration ---
    "Set [model] fallbacks = [\"anthropic:claude-sonnet-4\", \"ollama:qwen3\"] to fail over automatically when the primary provider errors.",
    "Set [agent] approval = \"smart\" to let a guardian model pre-screen dangerous commands before escalating to you.",
    "Set [terminal] backend = \"docker\" (or \"ssh\") to run every terminal command in a container or on a remote host.",
    "Set [gateway] multiplex_profiles = true to serve every gateway route under /p/<profile>/... mirrors, each with its own agent and state.",
    "Set ULNCLAW_BROWSER_CDP=auto (the default) to auto-launch a managed headless Chrome for the browser tools.",
    "Point CAMOFOX_URL at a Camoufox REST endpoint and all 12 browser tools route through the anti-detect browser instead of CDP.",
    "[auxiliary.compression] and [auxiliary.title_generation] route side LLM tasks to cheaper models than your main chat model.",
    "[moa.presets.<name>] defines custom Mixture-of-Agents fan-outs — reference models + aggregator, per preset.",
    // --- Tools & capabilities ---
    "The terminal tool supports background processes: start one, then poll or kill it by session id — great for CI watchers.",
    "delegate spawns a sub-agent for a bounded subtask and consolidates its result back into your conversation.",
    "video_generate creates videos from text or images; provider auto-selects from xAI, FAL, DeepInfra, or the Nous gateway.",
    "The project toolset (opt-in) gives the agent first-class multi-folder workspaces with a projects.db registry.",
    "Skills are folders with a SKILL.md — drop one into ~/.ulnclaw/skills and the agent discovers it automatically.",
    "MCP servers configured under [mcp.servers] register their tools alongside the built-ins at startup.",
    // --- Gateway ---
    "The gateway speaks OpenAI chat/completions and Responses formats — point any OpenAI SDK at it with a bearer key.",
    "POST /v1/runs starts a tracked async run; GET /v1/runs/:id/events streams lifecycle + approval events over SSE.",
    "Dangerous commands in gateway runs park in waiting_for_approval until you resolve them via POST /v1/runs/:id/approval.",
    "GET /metrics exposes Prometheus counters: uptime, sessions, messages, active runs, cron jobs.",
    // --- Hidden gems ---
    "Everything the agent learns — skills it wrote, memories it saved — is browsable with ulnclaw journey.",
    "ulnclaw curator prune --days 30 --dry-run shows what idle agent-created skills would be archived before you commit.",
    "Session databases are recoverable: ulnclaw sessions recover works offline and never modifies the original file.",
    "Checkpoints snapshot automatically before destructive file edits when [checkpoints] is enabled — /rollback restores.",
];

// ---------------------------------------------------------------------------
// Picker
// ---------------------------------------------------------------------------

static RNG_STATE: AtomicU64 = AtomicU64::new(0);

/// xorshift64* step — plenty of quality for tip selection without pulling
/// in an RNG dependency.
fn next_u64() -> u64 {
    let mut state = RNG_STATE.load(Ordering::Relaxed);
    if state == 0 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        state = nanos ^ ((std::process::id() as u64) << 32) ^ 0x2545F4914F6CDD1D;
        if state == 0 {
            state = 0x9E3779B97F4A7C15;
        }
    }
    state ^= state >> 12;
    state ^= state << 25;
    state ^= state >> 27;
    RNG_STATE.store(state, Ordering::Relaxed);
    state.wrapping_mul(0x2545F4914F6CDD1D)
}

/// Return a random tip (hermes `get_random_tip`; `exclude_recent` is
/// reserved for future deduplication and unused, same as upstream).
pub fn get_random_tip() -> &'static str {
    let index = (next_u64() % TIPS.len() as u64) as usize;
    TIPS[index]
}

/// Format a tip for display (hermes `✦ Tip: ...` line).
pub fn format_tip(tip: &str) -> String {
    format!("✦ Tip: {}", tip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_nontrivial_and_clean() {
        assert!(TIPS.len() >= 40, "tip corpus should be substantial");
        for tip in TIPS {
            assert!(!tip.trim().is_empty());
            assert!(
                tip.chars().count() < 400,
                "tips stay one-liner-ish: {}",
                tip
            );
        }
        // No exact duplicates.
        let mut seen = std::collections::HashSet::new();
        for tip in TIPS {
            assert!(seen.insert(tip), "duplicate tip: {}", tip);
        }
    }

    #[test]
    fn random_tip_is_from_corpus() {
        for _ in 0..50 {
            let tip = get_random_tip();
            assert!(TIPS.contains(&tip));
        }
    }

    #[test]
    fn picker_varies() {
        let picks: std::collections::HashSet<&str> = (0..200).map(|_| get_random_tip()).collect();
        assert!(picks.len() > 1, "picker should not be constant");
    }

    #[test]
    fn format_tip_prefix() {
        assert_eq!(format_tip("hello"), "✦ Tip: hello");
    }
}

//! Persistent session goals — the Ralph loop (port of hermes
//! `hermes_cli/goals.py`, v2026.8.3).
//!
//! A goal is a free-form user objective that stays active across turns.
//! After each turn completes, a small judge call asks an auxiliary model
//! "is this goal satisfied by the assistant's last response?". If not, a
//! continuation prompt is fed back into the same session and the agent
//! keeps working until the goal is done, the turn budget is exhausted,
//! the user pauses/clears it, or a new user message preempts the loop.
//!
//! State is persisted in the session store's `state_meta` table keyed by
//! `goal:<session_id>` so a resumed session picks it up.
//!
//! Design invariants (hermes):
//! - The continuation prompt is just a normal user message appended to
//!   the session — no system-prompt mutation, no toolset swap.
//! - Judge failures are fail-OPEN: `continue`. A broken judge must not
//!   wedge progress; the turn budget is the backstop. Consecutive parse /
//!   transport failures auto-pause the loop.
//! - WAIT verdicts park the loop on a pid / deadline without burning a
//!   turn; the barrier auto-clears when the condition releases.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::config::UlncLawConfig;
use crate::provider::{Message, Provider, ProviderRequest, Role};
use crate::session::SqliteSessionStore;

// ---------------------------------------------------------------------------
// Constants & defaults
// ---------------------------------------------------------------------------

pub const DEFAULT_MAX_TURNS: u64 = 20;
pub const DEFAULT_JUDGE_TIMEOUT_SECS: u64 = 30;
/// Judge output budget — reasoning models burn tokens on hidden reasoning
/// before the visible JSON verdict; 4096 covers reasoning + verdict on
/// every live-tested model (hermes DEFAULT_JUDGE_MAX_TOKENS).
pub const DEFAULT_JUDGE_MAX_TOKENS: u32 = 4096;
/// Cap how much of the last response we send to the judge.
const JUDGE_RESPONSE_SNIPPET_CHARS: usize = 4000;
/// After this many consecutive judge *parse* failures the loop auto-pauses.
pub const DEFAULT_MAX_CONSECUTIVE_PARSE_FAILURES: u32 = 3;
/// After this many consecutive transport failures the loop auto-pauses.
pub const DEFAULT_MAX_CONSECUTIVE_TRANSPORT_FAILURES: u32 = 5;

pub const CONTINUATION_PROMPT_TEMPLATE: &str =
    "[Continuing toward your standing goal]\nGoal: {goal}\n\n\
     Continue working toward this goal. Take the next concrete step. \
     If you believe the goal is complete, state so explicitly and stop. \
     If you are blocked and need input from the user, say so clearly and stop.";

pub const CONTINUATION_PROMPT_WITH_CONTRACT_TEMPLATE: &str =
    "[Continuing toward your standing goal]\nGoal: {goal}\n\n\
     Completion contract:\n{contract_block}\n\n\
     Continue working toward the outcome above. Take the next concrete step. \
     Stay within the stated boundaries and do not violate the constraints. \
     Before claiming the goal is done, satisfy the Verification criterion and \
     show the concrete evidence (command output, file contents, test result). \
     If you hit the stated stop condition or are otherwise blocked and need \
     user input, say so clearly and stop.";

pub const CONTINUATION_PROMPT_WITH_SUBGOALS_TEMPLATE: &str =
    "[Continuing toward your standing goal]\nGoal: {goal}\n\n\
     Additional criteria the user added mid-loop:\n{subgoals_block}\n\n\
     Continue working toward the goal AND all additional criteria. Take \
     the next concrete step. If you believe the goal and every \
     additional criterion are complete, state so explicitly and stop. \
     If you are blocked and need input from the user, say so clearly \
     and stop.";

pub const JUDGE_SYSTEM_PROMPT: &str =
    "You are a strict judge evaluating whether an autonomous agent has \
     achieved a user's stated goal. You receive the goal text, the agent's \
     most recent response, and — when present — a list of background \
     processes the agent has running. Decide one of three verdicts.\n\n\
     DONE — the goal is fully satisfied:\n\
     - The response explicitly confirms the goal was completed, OR\n\
     - The response clearly shows the final deliverable was produced, OR\n\
     - The response explains the goal is unachievable / blocked / needs \
     user input (treat this as DONE with reason describing the block).\n\n\
     WAIT — the goal is NOT done, but the next step is to wait for async \
     work to finish rather than act again. Choose this ONLY when the agent's \
     progress is genuinely gated on something running on its own:\n\
     - A background process listed below is still running AND the response \
     shows the agent is waiting on its result (e.g. a CI poller, build, \
     test run, deploy). If the process has a session id, return it in \
     ``wait_on_session``. Otherwise return its pid in ``wait_on_pid`` \
     (releases on exit only).\n\
     - The agent says it is rate-limited / backing off / must wait a fixed \
     period — return seconds in ``wait_for_seconds``.\n\
     Picking WAIT parks the loop without burning a turn; it resumes \
     automatically when the pid exits or the time elapses. Do NOT pick WAIT \
     just because work remains — only when re-poking now would be pure \
     busy-work because the agent can't progress until the async thing \
     finishes.\n\n\
     CONTINUE — not done, and there is a concrete next step the agent can \
     take right now. This is the default when in doubt.\n\n\
     Reply ONLY with a single JSON object on one line. Shapes:\n\
     {\"verdict\": \"done\", \"reason\": \"<one sentence>\"}\n\
     {\"verdict\": \"continue\", \"reason\": \"<one sentence>\"}\n\
     {\"verdict\": \"wait\", \"wait_on_session\": \"<id>\", \"reason\": \"<one sentence>\"}\n\
     {\"verdict\": \"wait\", \"wait_on_pid\": <int>, \"reason\": \"<one sentence>\"}\n\
     {\"verdict\": \"wait\", \"wait_for_seconds\": <int>, \"reason\": \"<one sentence>\"}\n\
     The legacy shape {\"done\": <true|false>, \"reason\": \"...\"} is still \
     accepted (true=done, false=continue).";

pub const JUDGE_USER_PROMPT_TEMPLATE: &str =
    "Goal:\n{goal}\n\nAgent's most recent response:\n{response}\n\n\
     {background_block}Current time: {current_time}\n\n\
     Is the goal satisfied — done, continue, or wait?";

pub const JUDGE_USER_PROMPT_WITH_SUBGOALS_TEMPLATE: &str =
    "Goal:\n{goal}\n\nAdditional criteria the user added mid-loop:\n\
     {subgoals_block}\n\nAgent's most recent response:\n{response}\n\n\
     {background_block}Current time: {current_time}\n\n\
     The goal is DONE only when the goal text AND every additional \
     criterion above are satisfied. Is the goal satisfied — done, \
     continue, or wait?";

pub const JUDGE_USER_PROMPT_WITH_CONTRACT_TEMPLATE: &str =
    "Goal:\n{goal}\n\nCompletion contract (the authoritative definition of done):\n\
     {contract_block}\n\nAgent's most recent response:\n{response}\n\n\
     {background_block}Current time: {current_time}\n\n\
     Decision rules:\n\
     - The goal is DONE only when the Verification criterion is satisfied AND \
     the response shows concrete evidence of it (a command result, file \
     contents excerpt, test/benchmark output) — not a claim like 'done' or \
     'all tests pass' without evidence.\n\
     - If any stated Constraint was violated, the goal is NOT done — CONTINUE.\n\
     - If the response shows the agent is waiting on a listed background \
     process to satisfy the Verification criterion (e.g. CI is the \
     verification and it's still running), return WAIT on that process \
     instead of re-poking — re-poking now would be pure busy-work.\n\
     - If the response explains the work is blocked / unachievable / needs \
     user input (e.g. the stated Stop condition was hit), treat it as DONE \
     with the reason describing the block.\n\
     - Otherwise the goal is NOT done — CONTINUE.\n\n\
     Is the goal satisfied per its completion contract — done, continue, or wait?";

pub const DRAFT_CONTRACT_SYSTEM_PROMPT: &str =
    "You turn a user's plain-language objective into a structured completion \
     contract for an autonomous coding agent. The contract has five fields:\n\
     - outcome: the single end state that must be true when done\n\
     - verification: the specific test / command / artifact that PROVES the \
     outcome (must be concrete and checkable)\n\
     - constraints: what must NOT change or regress\n\
     - boundaries: which files, dirs, tools, or systems are in scope\n\
     - stop_when: the condition under which the agent should stop and ask \
     for human input instead of pushing on\n\n\
     Infer sensible, specific values from the objective and any project \
     context implied by it. Prefer concrete verification (a named test \
     command, a build, a benchmark) over vague phrases. Keep each field to \
     one or two sentences. If a field genuinely cannot be inferred, use an \
     empty string for it.\n\n\
     Reply ONLY with a single JSON object on one line:\n\
     {\"outcome\": \"...\", \"verification\": \"...\", \"constraints\": \"...\", \
     \"boundaries\": \"...\", \"stop_when\": \"...\"}";

// ---------------------------------------------------------------------------
// Completion contract
// ---------------------------------------------------------------------------

const CONTRACT_LABELS: &[(&str, &str)] = &[
    ("outcome", "Outcome"),
    ("verification", "Verification"),
    ("constraints", "Constraints"),
    ("boundaries", "Boundaries"),
    ("stop_when", "Stop when blocked"),
];

fn contract_alias(prefix: &str) -> Option<&'static str> {
    let key = prefix.trim().to_ascii_lowercase();
    let alias: &'static str = match key.as_str() {
        "outcome" | "goal" | "done" | "done when" => "outcome",
        "verification" | "verify" | "verified by" | "evidence" | "proof" => "verification",
        "constraints" | "constraint" | "preserve" | "must not" | "do not change" => "constraints",
        "boundaries" | "boundary" | "scope" | "allowed" | "files" => "boundaries",
        "stop when" | "stop_when" | "blocked" | "stop if blocked" | "give up when" => "stop_when",
        _ => return None,
    };
    Some(alias)
}

/// Optional structured completion contract for a goal. Empty fields are
/// omitted everywhere — a goal with no contract behaves exactly like the
/// original free-form goal.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GoalContract {
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub verification: String,
    #[serde(default)]
    pub constraints: String,
    #[serde(default)]
    pub boundaries: String,
    #[serde(default)]
    pub stop_when: String,
}

impl GoalContract {
    pub fn is_empty(&self) -> bool {
        [
            &self.outcome,
            &self.verification,
            &self.constraints,
            &self.boundaries,
            &self.stop_when,
        ]
        .iter()
        .all(|f| f.trim().is_empty())
    }

    pub fn to_map(&self) -> HashMap<&'static str, String> {
        let mut map = HashMap::new();
        map.insert("outcome", self.outcome.clone());
        map.insert("verification", self.verification.clone());
        map.insert("constraints", self.constraints.clone());
        map.insert("boundaries", self.boundaries.clone());
        map.insert("stop_when", self.stop_when.clone());
        map
    }

    pub fn from_value(value: Option<&Value>) -> GoalContract {
        let Some(Value::Object(map)) = value else {
            return GoalContract::default();
        };
        let field = |key: &str| {
            map.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        GoalContract {
            outcome: field("outcome"),
            verification: field("verification"),
            constraints: field("constraints"),
            boundaries: field("boundaries"),
            stop_when: field("stop_when"),
        }
    }

    /// Render non-empty contract fields as a labelled block. Empty
    /// contract → empty string.
    pub fn render_block(&self) -> String {
        let values = self.to_map();
        let mut lines = Vec::new();
        for (field, label) in CONTRACT_LABELS {
            let value = values
                .get(*field)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if !value.is_empty() {
                lines.push(format!("- {}: {}", label, value));
            }
        }
        lines.join("\n")
    }

    fn from_fields(fields: &HashMap<&'static str, Vec<String>>) -> GoalContract {
        let join = |key: &'static str| {
            fields
                .get(key)
                .map(|v| v.join(" "))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        GoalContract {
            outcome: join("outcome"),
            verification: join("verification"),
            constraints: join("constraints"),
            boundaries: join("boundaries"),
            stop_when: join("stop_when"),
        }
    }
}

/// Split user-typed goal text into a headline + structured contract
/// (hermes `parse_contract`). Inline `field: value` lines populate the
/// contract; unrecognized prefixes stay in the headline so a plain goal
/// with an incidental colon is not mangled.
pub fn parse_contract(text: &str) -> (String, GoalContract) {
    if text.trim().is_empty() {
        return (String::new(), GoalContract::default());
    }
    let mut headline_parts: Vec<String> = Vec::new();
    let mut fields: HashMap<&'static str, Vec<String>> = HashMap::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let mut matched = false;
        if let Some(colon) = line.find(':') {
            let prefix = &line[..colon];
            let value = line[colon + 1..].trim();
            if let Some(key) = contract_alias(prefix) {
                if !value.is_empty() {
                    fields.entry(key).or_default().push(value.to_string());
                    matched = true;
                }
            }
        }
        if !matched {
            headline_parts.push(line.to_string());
        }
    }
    (
        headline_parts.join(" ").trim().to_string(),
        GoalContract::from_fields(&fields),
    )
}

// ---------------------------------------------------------------------------
// Goal state
// ---------------------------------------------------------------------------

/// Serializable goal state stored per session (hermes `GoalState`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalState {
    pub goal: String,
    /// active | paused | done | cleared
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub turns_used: u64,
    #[serde(default = "default_max_turns")]
    pub max_turns: u64,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub last_turn_at: f64,
    #[serde(default)]
    pub last_verdict: Option<String>,
    #[serde(default)]
    pub last_reason: Option<String>,
    #[serde(default)]
    pub paused_reason: Option<String>,
    #[serde(default)]
    pub consecutive_parse_failures: u32,
    #[serde(default)]
    pub consecutive_transport_failures: u32,
    #[serde(default)]
    pub subgoals: Vec<String>,
    #[serde(default)]
    pub waiting_on_pid: Option<u32>,
    #[serde(default)]
    pub waiting_on_session: Option<String>,
    #[serde(default)]
    pub waiting_until: f64,
    #[serde(default)]
    pub waiting_reason: Option<String>,
    #[serde(default)]
    pub waiting_since: f64,
    #[serde(default)]
    pub contract: GoalContract,
}

fn default_status() -> String {
    "active".to_string()
}

fn default_max_turns() -> u64 {
    DEFAULT_MAX_TURNS
}

impl GoalState {
    pub fn new(goal: impl Into<String>, max_turns: u64) -> Self {
        let now = now_epoch();
        Self {
            goal: goal.into(),
            status: "active".to_string(),
            turns_used: 0,
            max_turns,
            created_at: now,
            last_turn_at: 0.0,
            last_verdict: None,
            last_reason: None,
            paused_reason: None,
            consecutive_parse_failures: 0,
            consecutive_transport_failures: 0,
            subgoals: Vec::new(),
            waiting_on_pid: None,
            waiting_on_session: None,
            waiting_until: 0.0,
            waiting_reason: None,
            waiting_since: 0.0,
            contract: GoalContract::default(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(raw: &str) -> Option<GoalState> {
        let mut state: GoalState = serde_json::from_str(raw).ok()?;
        state.subgoals = state
            .subgoals
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Some(state)
    }

    pub fn is_active(&self) -> bool {
        self.status == "active"
    }

    pub fn has_contract(&self) -> bool {
        !self.contract.is_empty()
    }

    /// Render subgoals as a numbered block (empty when none).
    pub fn render_subgoals_block(&self) -> String {
        self.subgoals
            .iter()
            .enumerate()
            .map(|(i, text)| format!("- {}. {}", i + 1, text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting_on_pid.is_some()
            || self.waiting_on_session.is_some()
            || self.waiting_until > 0.0
    }
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        text.chars().take(limit).collect()
    }
}

// ---------------------------------------------------------------------------
// Judge response parsing
// ---------------------------------------------------------------------------

/// A WAIT directive from the judge.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitDirective {
    Session(String),
    Pid(u32),
    Seconds(u64),
}

/// Parsed judge verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeVerdict {
    /// "done" | "continue" | "wait"
    pub verdict: String,
    pub reason: String,
    /// True when the judge call succeeded but its output was unusable.
    pub parse_failed: bool,
    /// Set only for verdict == "wait".
    pub wait: Option<WaitDirective>,
}

/// Parse the judge's reply. Fail-open on unusable output (hermes
/// `_parse_judge_response`). Accepts both `{"verdict": ...}` and the
/// legacy `{"done": <bool>}` shapes; a wait verdict without a usable
/// target downgrades to continue.
pub fn parse_judge_response(raw: &str) -> JudgeVerdict {
    if raw.is_empty() {
        return JudgeVerdict {
            verdict: "continue".into(),
            reason: "judge returned empty response".into(),
            parse_failed: true,
            wait: None,
        };
    }
    let mut text = raw.trim().to_string();
    // Strip markdown code fences the model may wrap JSON in.
    if text.starts_with("```") {
        text = text.trim_matches('`').to_string();
        if let Some(nl) = text.find('\n') {
            text = text[nl + 1..].to_string();
        }
    }
    let data: Option<Value> = serde_json::from_str(&text)
        .ok()
        .or_else(|| extract_json_object(&text));
    let Some(Value::Object(map)) = data else {
        return JudgeVerdict {
            verdict: "continue".into(),
            reason: format!("judge reply was not JSON: {:?}", truncate_chars(raw, 200)),
            parse_failed: true,
            wait: None,
        };
    };

    let reason = map
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "no reason provided".to_string());

    // Verdict — explicit field first, legacy "done" boolean fallback.
    let mut verdict = match map.get("verdict").and_then(|v| v.as_str()) {
        Some(raw_verdict) => raw_verdict.trim().to_ascii_lowercase(),
        None => {
            let done = match map.get("done") {
                Some(Value::String(s)) => matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "true" | "yes" | "1" | "done"
                ),
                Some(Value::Bool(b)) => *b,
                _ => false,
            };
            if done {
                "done".to_string()
            } else {
                "continue".to_string()
            }
        }
    };
    if !matches!(verdict.as_str(), "done" | "continue" | "wait") {
        verdict = "continue".to_string();
    }
    if verdict != "wait" {
        return JudgeVerdict {
            verdict,
            reason,
            parse_failed: false,
            wait: None,
        };
    }

    let first_int = |keys: &[&str]| -> Option<i64> {
        for key in keys {
            let Some(value) = map.get(*key) else { continue };
            let parsed = match value {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            };
            if let Some(n) = parsed {
                if n > 0 {
                    return Some(n);
                }
            }
        }
        None
    };

    // Prefer session-id directive, then pid, then seconds.
    if let Some(Value::String(sess)) = map
        .get("wait_on_session")
        .or_else(|| map.get("session_id"))
        .or_else(|| map.get("wait_session"))
    {
        let sess = sess.trim();
        if !sess.is_empty() {
            return JudgeVerdict {
                verdict: "wait".into(),
                reason,
                parse_failed: false,
                wait: Some(WaitDirective::Session(sess.to_string())),
            };
        }
    }
    if let Some(pid) = first_int(&["wait_on_pid", "pid", "wait_pid"]) {
        return JudgeVerdict {
            verdict: "wait".into(),
            reason,
            parse_failed: false,
            wait: Some(WaitDirective::Pid(pid as u32)),
        };
    }
    if let Some(seconds) = first_int(&["wait_for_seconds", "seconds", "wait_seconds"]) {
        return JudgeVerdict {
            verdict: "wait".into(),
            reason,
            parse_failed: false,
            wait: Some(WaitDirective::Seconds(seconds as u64)),
        };
    }
    // Wait with no usable target — can't park on nothing; treat as continue.
    JudgeVerdict {
        verdict: "continue".into(),
        reason: format!("{} (wait verdict had no target — continuing)", reason),
        parse_failed: false,
        wait: None,
    }
}

/// Pull the first JSON object out of a blob (hermes `_extract_json_object`).
fn extract_json_object(raw: &str) -> Option<Value> {
    let start = raw.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in raw[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&raw[start..=start + i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Background-process snapshot for the judge prompt
// ---------------------------------------------------------------------------

/// Rendered into the judge prompt when the agent has background processes
/// running (hermes `JUDGE_BACKGROUND_BLOCK_TEMPLATE`).
pub const JUDGE_BACKGROUND_BLOCK_TEMPLATE: &str =
    "Background processes the agent currently has running (it may be waiting \
     on one of these):\n{background_lines}\n\n";

/// Snapshot of one background process — the subset of fields the judge
/// prompt uses from hermes `process_registry.list_sessions()` entries.
#[derive(Debug, Clone, Default)]
pub struct BackgroundProcessInfo {
    pub pid: Option<u32>,
    pub session_id: Option<String>,
    pub command: String,
    /// "running" | "exited"
    pub status: String,
    pub uptime_seconds: Option<u64>,
    pub output_preview: Option<String>,
}

/// Render the live background-process list for the judge prompt (hermes
/// `_render_background_block`). Only RUNNING processes with a pid are worth
/// showing; returns an empty string when nothing qualifies so the prompt is
/// byte-identical to the no-background case.
pub fn render_background_block(processes: &[BackgroundProcessInfo]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for proc in processes {
        if proc.status == "exited" {
            continue;
        }
        let Some(pid) = proc.pid else { continue };
        let cmd = truncate_prompt(&proc.command.replace('\n', " ").trim().to_string(), 120);
        let tail = truncate_prompt(
            &proc
                .output_preview
                .as_deref()
                .unwrap_or("")
                .replace('\n', " ")
                .trim()
                .to_string(),
            120,
        );
        let mut line = format!("- pid {}", pid);
        if let Some(sid) = proc.session_id.as_deref().filter(|s| !s.is_empty()) {
            line.push_str(&format!(" / session {}", sid));
        }
        line.push_str(&format!(": {}", cmd));
        if let Some(uptime) = proc.uptime_seconds {
            line.push_str(&format!(" (running {}s)", uptime));
        }
        if !tail.is_empty() {
            line.push_str(&format!(" | recent output: {}", tail));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        return String::new();
    }
    JUDGE_BACKGROUND_BLOCK_TEMPLATE.replace("{background_lines}", &lines.join("\n"))
}

/// Live background-process snapshot for the goal judge (hermes
/// `gather_background_processes`) — running entries only.
pub fn gather_background_processes() -> Vec<BackgroundProcessInfo> {
    crate::tools::builtin::terminal::list_background_processes()
        .into_iter()
        .filter(|proc| proc.status != "exited")
        .collect()
}

/// Process liveness check for pid wait barriers (hermes `_pid_alive`).
/// `/proc` existence is the portable Linux check; any uncertainty resolves
/// to false (treat unknown as dead) so a stale barrier never wedges the
/// loop — worst case the goal resumes one turn early, which is safe.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// Whether a goal parked on a terminal background-process session should
/// stay parked (hermes `_session_waiting`). ulnclaw's terminal registry has
/// no watch patterns, so "still running" is the release trigger. Fail-safe:
/// any registry error yields false.
pub fn session_waiting(session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    crate::tools::builtin::terminal::background_process_running(session_id)
}

/// Python `_truncate` semantics — suffix marks the cut.
fn truncate_prompt(text: &str, limit: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}… [truncated]", truncate_chars(text, limit))
}

// ---------------------------------------------------------------------------
// Persistence (session store `state_meta`, key `goal:<session_id>`)
// ---------------------------------------------------------------------------

pub fn meta_key(session_id: &str) -> String {
    format!("goal:{}", session_id)
}

/// Load the goal for a session, or `None` if none exists (hermes `load_goal`).
pub fn load_goal(store: &SqliteSessionStore, session_id: &str) -> Option<GoalState> {
    if session_id.is_empty() {
        return None;
    }
    let raw = match store.get_meta(&meta_key(session_id)) {
        Ok(Some(raw)) => raw,
        Ok(None) => return None,
        Err(e) => {
            tracing::debug!("GoalManager: get_meta failed: {}", e);
            return None;
        }
    };
    match GoalState::from_json(&raw) {
        Some(state) => Some(state),
        None => {
            tracing::warn!(
                "GoalManager: could not parse stored goal for {}",
                session_id
            );
            None
        }
    }
}

/// Persist a goal to the session store. No-op if the session id is empty
/// (hermes `save_goal`).
pub fn save_goal(store: &SqliteSessionStore, session_id: &str, state: &GoalState) {
    if session_id.is_empty() {
        return;
    }
    if let Err(e) = store.set_meta(&meta_key(session_id), &state.to_json()) {
        tracing::debug!("GoalManager: set_meta failed: {}", e);
    }
}

/// Mark a goal cleared in the store — preserved for audit, status=cleared
/// (hermes `clear_goal`).
pub fn clear_goal(store: &SqliteSessionStore, session_id: &str) {
    let Some(mut state) = load_goal(store, session_id) else {
        return;
    };
    state.status = "cleared".to_string();
    save_goal(store, session_id, &state);
}

/// Carry a persistent goal from a parent session to its continuation
/// (hermes `migrate_goal_to_session`). Copies the goal onto the new session
/// and archives the old row as `cleared` so exactly one active goal row
/// exists per logical conversation. Returns true when a goal was migrated.
pub fn migrate_goal_to_session(
    store: &SqliteSessionStore,
    old_session_id: &str,
    new_session_id: &str,
    reason: &str,
) -> bool {
    if old_session_id.is_empty() || new_session_id.is_empty() || old_session_id == new_session_id {
        return false;
    }
    let Some(state) = load_goal(store, old_session_id) else {
        return false;
    };
    if state.status == "cleared" {
        return false;
    }
    // Don't clobber a goal already set on the child.
    if load_goal(store, new_session_id).is_some() {
        return false;
    }
    save_goal(store, new_session_id, &state);
    clear_goal(store, old_session_id);
    tracing::debug!(
        "GoalManager: migrated goal {} -> {} ({})",
        old_session_id,
        new_session_id,
        if reason.is_empty() {
            "rotation"
        } else {
            reason
        }
    );
    true
}

// ---------------------------------------------------------------------------
// GoalManager — the orchestration surface CLI + gateway talk to
// ---------------------------------------------------------------------------

/// Per-session goal state + continuation decisions (hermes `GoalManager`).
///
/// The CLI and gateway each hold one `GoalManager` per live session. State
/// is persisted in the session store's `state_meta` table so a resumed
/// session picks its goal back up.
pub struct GoalManager {
    pub session_id: String,
    store: Option<Arc<SqliteSessionStore>>,
    state: Option<GoalState>,
    default_max_turns: u64,
}

impl GoalManager {
    pub fn new(
        session_id: impl Into<String>,
        store: Option<Arc<SqliteSessionStore>>,
        default_max_turns: u64,
    ) -> Self {
        let session_id = session_id.into();
        let state = store.as_ref().and_then(|s| load_goal(s, &session_id));
        Self {
            session_id,
            store,
            state,
            default_max_turns: if default_max_turns == 0 {
                DEFAULT_MAX_TURNS
            } else {
                default_max_turns
            },
        }
    }

    fn save(&self) {
        if let (Some(store), Some(state)) = (&self.store, &self.state) {
            save_goal(store, &self.session_id, state);
        }
    }

    // --- introspection ------------------------------------------------

    pub fn state(&self) -> Option<&GoalState> {
        self.state.as_ref()
    }

    pub fn is_active(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| s.status == "active")
            .unwrap_or(false)
    }

    pub fn has_goal(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| matches!(s.status.as_str(), "active" | "paused"))
            .unwrap_or(false)
    }

    pub fn has_contract(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| s.has_contract())
            .unwrap_or(false)
    }

    /// Printable one-liner (hermes `status_line`).
    pub fn status_line(&self) -> String {
        let Some(s) = self.state.as_ref() else {
            return "No active goal. Set one with /goal <text>.".to_string();
        };
        if s.status == "cleared" {
            return "No active goal. Set one with /goal <text>.".to_string();
        }
        let turns = format!("{}/{} turns", s.turns_used, s.max_turns);
        let sub = if s.subgoals.is_empty() {
            String::new()
        } else {
            format!(
                ", {} subgoal{}",
                s.subgoals.len(),
                if s.subgoals.len() != 1 { "s" } else { "" }
            )
        };
        let con = if self.has_contract() {
            ", contract"
        } else {
            ""
        };
        let meta = format!("{}{}{}", turns, sub, con);
        if s.status == "active" {
            if let Some(sess) = s.waiting_on_session.as_deref() {
                if session_waiting(sess) {
                    let wr = s
                        .waiting_reason
                        .clone()
                        .unwrap_or_else(|| format!("session {}", sess));
                    return format!("⏳ Goal (parked on {}, {}): {}", wr, meta, s.goal);
                }
            }
            if let Some(pid) = s.waiting_on_pid {
                if pid_alive(pid) {
                    let wr = s
                        .waiting_reason
                        .clone()
                        .unwrap_or_else(|| format!("pid {}", pid));
                    return format!("⏳ Goal (parked on {}, {}): {}", wr, meta, s.goal);
                }
            }
            if s.waiting_until > 0.0 && now_epoch() < s.waiting_until {
                let remaining = (s.waiting_until - now_epoch()) as i64;
                let wr = s
                    .waiting_reason
                    .clone()
                    .unwrap_or_else(|| format!("{}s", remaining));
                return format!(
                    "⏳ Goal (parked {}s — {}, {}): {}",
                    remaining, wr, meta, s.goal
                );
            }
            return format!("⊙ Goal (active, {}): {}", meta, s.goal);
        }
        if s.status == "paused" {
            let extra = s
                .paused_reason
                .as_deref()
                .map(|r| format!(" — {}", r))
                .unwrap_or_default();
            return format!("⏸ Goal (paused, {}{}): {}", meta, extra, s.goal);
        }
        if s.status == "done" {
            return format!("✓ Goal done ({}): {}", meta, s.goal);
        }
        format!("Goal ({}, {}): {}", s.status, meta, s.goal)
    }

    // --- mutation -----------------------------------------------------

    /// Start a new standing goal (hermes `set`).
    pub fn set(
        &mut self,
        goal: &str,
        max_turns: Option<u64>,
        contract: Option<GoalContract>,
    ) -> Result<GoalState, String> {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err("goal text is empty".to_string());
        }
        let budget = max_turns
            .filter(|t| *t > 0)
            .unwrap_or(self.default_max_turns);
        let mut state = GoalState::new(goal, budget);
        state.contract = contract.unwrap_or_default();
        self.state = Some(state.clone());
        self.save();
        Ok(state)
    }

    /// Attach or replace the completion contract on the active goal
    /// (hermes `set_contract`). Returns the updated state, or None when
    /// there is no goal to attach to.
    pub fn set_contract(&mut self, contract: GoalContract) -> Option<GoalState> {
        let state = self.state.as_mut()?;
        state.contract = contract;
        self.save();
        self.state.clone()
    }

    pub fn pause(&mut self, reason: &str) -> Option<GoalState> {
        let state = self.state.as_mut()?;
        state.status = "paused".to_string();
        state.paused_reason = Some(reason.to_string());
        // A wait barrier is meaningless once paused — drop it.
        state.waiting_on_pid = None;
        state.waiting_on_session = None;
        state.waiting_until = 0.0;
        state.waiting_reason = None;
        state.waiting_since = 0.0;
        self.save();
        self.state.clone()
    }

    pub fn resume(&mut self, reset_budget: bool) -> Option<GoalState> {
        let state = self.state.as_mut()?;
        state.status = "active".to_string();
        state.paused_reason = None;
        // Resuming starts fresh — clear any stale barrier.
        state.waiting_on_pid = None;
        state.waiting_on_session = None;
        state.waiting_until = 0.0;
        state.waiting_reason = None;
        state.waiting_since = 0.0;
        if reset_budget {
            state.turns_used = 0;
        }
        self.save();
        self.state.clone()
    }

    pub fn clear(&mut self) {
        if self.state.is_none() {
            return;
        }
        if let Some(state) = self.state.as_mut() {
            state.status = "cleared".to_string();
        }
        self.save();
        self.state = None;
    }

    pub fn mark_done(&mut self, reason: &str) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        state.status = "done".to_string();
        state.last_verdict = Some("done".to_string());
        state.last_reason = Some(reason.to_string());
        self.save();
    }

    // --- /subgoal user controls ---------------------------------------

    /// Append a user-added criterion to the active goal (hermes `add_subgoal`).
    pub fn add_subgoal(&mut self, text: &str) -> Result<String, String> {
        if self.state.is_none() || !self.has_goal() {
            return Err("no active goal".to_string());
        }
        let text = text.trim();
        if text.is_empty() {
            return Err("subgoal text is empty".to_string());
        }
        self.state.as_mut().unwrap().subgoals.push(text.to_string());
        self.save();
        Ok(text.to_string())
    }

    /// Remove a subgoal by 1-based index (hermes `remove_subgoal`).
    pub fn remove_subgoal(&mut self, index_1based: usize) -> Result<String, String> {
        if self.state.is_none() || !self.has_goal() {
            return Err("no active goal".to_string());
        }
        let count = self.state.as_ref().unwrap().subgoals.len();
        if index_1based == 0 || index_1based > count {
            return Err(format!("index out of range (1..{})", count));
        }
        let removed = self
            .state
            .as_mut()
            .unwrap()
            .subgoals
            .remove(index_1based - 1);
        self.save();
        Ok(removed)
    }

    /// Wipe all subgoals; returns the previous count (hermes `clear_subgoals`).
    pub fn clear_subgoals(&mut self) -> Result<usize, String> {
        if self.state.is_none() || !self.has_goal() {
            return Err("no active goal".to_string());
        }
        let prev = self.state.as_ref().unwrap().subgoals.len();
        self.state.as_mut().unwrap().subgoals.clear();
        self.save();
        Ok(prev)
    }

    /// Public helper for the /subgoal slash command (hermes `render_subgoals`).
    pub fn render_subgoals(&self) -> String {
        let Some(state) = self.state.as_ref() else {
            return "(no active goal)".to_string();
        };
        if state.subgoals.is_empty() {
            return "(no subgoals — use /subgoal <text> to add criteria)".to_string();
        }
        state.render_subgoals_block()
    }

    // --- /goal wait barrier -------------------------------------------

    /// Park the goal loop on a background process PID (hermes `wait_on`).
    /// The barrier auto-clears when the PID exits.
    pub fn wait_on(&mut self, pid: u32, reason: &str) -> Result<GoalState, String> {
        if self.state.is_none() || self.state.as_ref().unwrap().status != "active" {
            return Err("no active goal to park".to_string());
        }
        if pid == 0 {
            return Err("pid must be a positive integer".to_string());
        }
        let state = self.state.as_mut().unwrap();
        state.waiting_on_pid = Some(pid);
        state.waiting_on_session = None;
        state.waiting_until = 0.0;
        state.waiting_reason = Some(reason.trim().to_string()).filter(|r| !r.is_empty());
        state.waiting_since = now_epoch();
        self.save();
        self.state
            .clone()
            .ok_or_else(|| "no active goal to park".to_string())
    }

    /// Park the goal loop on a terminal background-process session
    /// (hermes `wait_on_session`). Releases when the process exits.
    pub fn wait_on_session(&mut self, session_id: &str, reason: &str) -> Result<GoalState, String> {
        if self.state.is_none() || self.state.as_ref().unwrap().status != "active" {
            return Err("no active goal to park".to_string());
        }
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err("session_id must be a non-empty string".to_string());
        }
        let state = self.state.as_mut().unwrap();
        state.waiting_on_session = Some(session_id.to_string());
        state.waiting_on_pid = None;
        state.waiting_until = 0.0;
        state.waiting_reason = Some(reason.trim().to_string()).filter(|r| !r.is_empty());
        state.waiting_since = now_epoch();
        self.save();
        self.state
            .clone()
            .ok_or_else(|| "no active goal to park".to_string())
    }

    /// Park the goal loop until `seconds` from now (hermes `wait_for_seconds`).
    pub fn wait_for_seconds(&mut self, seconds: u64, reason: &str) -> Result<GoalState, String> {
        if self.state.is_none() || self.state.as_ref().unwrap().status != "active" {
            return Err("no active goal to park".to_string());
        }
        if seconds == 0 {
            return Err("seconds must be a positive integer".to_string());
        }
        let state = self.state.as_mut().unwrap();
        state.waiting_on_pid = None;
        state.waiting_on_session = None;
        state.waiting_until = now_epoch() + seconds as f64;
        state.waiting_reason = Some(reason.trim().to_string()).filter(|r| !r.is_empty());
        state.waiting_since = now_epoch();
        self.save();
        self.state
            .clone()
            .ok_or_else(|| "no active goal to park".to_string())
    }

    /// Clear any active wait barrier; true if one was cleared (hermes
    /// `stop_waiting`).
    pub fn stop_waiting(&mut self) -> bool {
        let Some(state) = self.state.as_mut() else {
            return false;
        };
        if state.waiting_on_pid.is_none()
            && state.waiting_on_session.is_none()
            && state.waiting_until == 0.0
        {
            return false;
        }
        state.waiting_on_pid = None;
        state.waiting_on_session = None;
        state.waiting_until = 0.0;
        state.waiting_reason = None;
        state.waiting_since = 0.0;
        self.save();
        true
    }

    /// True iff a barrier is set AND not yet satisfied (hermes `is_waiting`).
    /// Side effect: a satisfied barrier is cleared here (lazy auto-clear)
    /// so the next evaluation resumes normal judging.
    pub fn is_waiting(&mut self) -> bool {
        let Some(snapshot) = self.state.clone() else {
            return false;
        };
        if let Some(sess) = snapshot.waiting_on_session.as_deref() {
            if session_waiting(sess) {
                return true;
            }
            self.stop_waiting(); // session exited
            return false;
        }
        if let Some(pid) = snapshot.waiting_on_pid {
            if pid_alive(pid) {
                return true;
            }
            self.stop_waiting(); // process gone
            return false;
        }
        if snapshot.waiting_until > 0.0 {
            if now_epoch() < snapshot.waiting_until {
                return true;
            }
            self.stop_waiting(); // deadline passed
            return false;
        }
        false
    }

    // --- the main entry point called after every turn -----------------

    /// Short-circuit when a wait barrier is still active: quiesce without
    /// burning a turn or calling the judge (hermes barrier branch of
    /// `evaluate_after_turn`).
    pub fn check_wait_barrier(&mut self) -> Option<GoalDecision> {
        {
            let state = self.state.as_ref()?;
            if state.status != "active" {
                return None;
            }
        }
        if !self.is_waiting() {
            return None;
        }
        let state = self.state.as_ref()?;
        let tgt = if let Some(sess) = state.waiting_on_session.as_deref() {
            format!("session {}", sess)
        } else if let Some(pid) = state.waiting_on_pid {
            format!("pid {}", pid)
        } else {
            let remaining = (state.waiting_until - now_epoch()).max(0.0) as i64;
            format!("{}s remaining", remaining)
        };
        let reason = state.waiting_reason.clone().unwrap_or_else(|| tgt.clone());
        Some(GoalDecision {
            status: Some("active".to_string()),
            should_continue: false,
            continuation_prompt: None,
            verdict: "waiting".to_string(),
            reason,
            message: format!(
                "⏳ Goal parked — waiting on {}: {}",
                tgt,
                state.waiting_reason.clone().unwrap_or_else(|| tgt.clone())
            ),
        })
    }

    /// Run the judge and update state (hermes `evaluate_after_turn`).
    /// Both user-initiated turns and continuation turns consume budget.
    pub async fn evaluate_after_turn(
        &mut self,
        config: &UlncLawConfig,
        main_provider: Arc<dyn Provider>,
        last_response: &str,
        background_processes: &[BackgroundProcessInfo],
    ) -> GoalDecision {
        {
            let Some(state) = self.state.as_ref() else {
                return GoalDecision::inactive(None);
            };
            if state.status != "active" {
                return GoalDecision::inactive(Some(state.status.clone()));
            }
        }
        if let Some(decision) = self.check_wait_barrier() {
            return decision;
        }
        let snapshot = {
            let state = self.state.as_mut().unwrap();
            // Count the turn that just finished.
            state.turns_used += 1;
            state.last_turn_at = now_epoch();
            state.clone()
        };
        let contract = if snapshot.has_contract() {
            Some(snapshot.contract.clone())
        } else {
            None
        };
        let (verdict, transport_failed) = judge_goal(
            config,
            main_provider,
            &snapshot.goal,
            last_response,
            &snapshot.subgoals,
            background_processes,
            contract.as_ref(),
        )
        .await;
        self.apply_verdict(verdict, transport_failed)
    }

    /// Pure state-machine half of `evaluate_after_turn`: apply an
    /// already-obtained verdict (hermes decision tree). Assumes the turn
    /// was already counted.
    pub fn apply_verdict(&mut self, verdict: JudgeVerdict, transport_failed: bool) -> GoalDecision {
        let Some(_) = self.state.as_ref() else {
            return GoalDecision::inactive(None);
        };
        {
            let state = self.state.as_mut().unwrap();
            state.last_verdict = Some(verdict.verdict.clone());
            state.last_reason = Some(verdict.reason.clone());
            // Track consecutive judge parse failures. Reset on any usable
            // reply, including transport errors (parse_failed=false) so a
            // flaky network doesn't trip the auto-pause meant for bad judge
            // models.
            if verdict.parse_failed {
                state.consecutive_parse_failures += 1;
            } else {
                state.consecutive_parse_failures = 0;
            }
            // Track consecutive transport failures separately — persistent
            // API errors signal a broken config, not transient flakiness.
            if transport_failed {
                state.consecutive_transport_failures += 1;
            } else {
                state.consecutive_transport_failures = 0;
            }
        }

        // WAIT verdict: set the barrier and park. The turn we just counted
        // stands, but no continuation fires; the loop resumes automatically
        // once the barrier clears.
        if verdict.verdict == "wait" {
            if let Some(directive) = verdict.wait.clone() {
                let tgt = self.park_on(&directive, &verdict.reason);
                self.save();
                return GoalDecision {
                    status: Some("active".to_string()),
                    should_continue: false,
                    continuation_prompt: None,
                    verdict: "wait".to_string(),
                    reason: verdict.reason.clone(),
                    message: format!(
                        "⏳ Goal parked (judge) — waiting on {}: {}",
                        tgt, verdict.reason
                    ),
                };
            }
        }

        if verdict.verdict == "done" {
            self.state.as_mut().unwrap().status = "done".to_string();
            self.save();
            return GoalDecision {
                status: Some("done".to_string()),
                should_continue: false,
                continuation_prompt: None,
                verdict: "done".to_string(),
                reason: verdict.reason.clone(),
                message: format!("✓ Goal achieved: {}", verdict.reason),
            };
        }

        // Auto-pause when the judge cannot reach the API at all N turns in
        // a row (401 auth, DNS failure, timeout).
        let transport_failures = self.state.as_ref().unwrap().consecutive_transport_failures;
        if transport_failures >= DEFAULT_MAX_CONSECUTIVE_TRANSPORT_FAILURES {
            let state = self.state.as_mut().unwrap();
            state.status = "paused".to_string();
            state.paused_reason = Some(format!(
                "judge API unreachable {} turns in a row (check auxiliary.goal_judge provider/key in config.toml)",
                transport_failures
            ));
            self.save();
            return GoalDecision {
                status: Some("paused".to_string()),
                should_continue: false,
                continuation_prompt: None,
                verdict: "continue".to_string(),
                reason: verdict.reason.clone(),
                message: format!(
                    "⏸ Goal paused — judge API returned errors ({} turns). \
                     Check the goal_judge provider/key in ~/.ulnclaw/config.toml:\n\
                     \x20 [auxiliary.goal_judge]\n\
                     \x20 provider = \"...\"\n\
                     \x20 model = \"...\"\n\
                     Then /goal resume to continue.",
                    transport_failures
                ),
            };
        }

        // Auto-pause when the judge model can't produce the expected JSON
        // verdict N turns in a row.
        let parse_failures = self.state.as_ref().unwrap().consecutive_parse_failures;
        if parse_failures >= DEFAULT_MAX_CONSECUTIVE_PARSE_FAILURES {
            let state = self.state.as_mut().unwrap();
            state.status = "paused".to_string();
            state.paused_reason = Some(format!(
                "judge model returned unparseable output {} turns in a row",
                parse_failures
            ));
            self.save();
            return GoalDecision {
                status: Some("paused".to_string()),
                should_continue: false,
                continuation_prompt: None,
                verdict: "continue".to_string(),
                reason: verdict.reason.clone(),
                message: format!(
                    "⏸ Goal paused — the judge model ({} turns) isn't returning the \
                     required JSON verdict. Route the judge to a stricter model in \
                     ~/.ulnclaw/config.toml:\n\
                     \x20 [auxiliary.goal_judge]\n\
                     \x20 provider = \"openrouter\"\n\
                     \x20 model = \"google/gemini-3-flash-preview\"\n\
                     Then /goal resume to continue.",
                    parse_failures
                ),
            };
        }

        let (turns_used, max_turns) = {
            let state = self.state.as_ref().unwrap();
            (state.turns_used, state.max_turns)
        };
        if turns_used >= max_turns {
            let state = self.state.as_mut().unwrap();
            state.status = "paused".to_string();
            state.paused_reason = Some(format!(
                "turn budget exhausted ({}/{})",
                turns_used, max_turns
            ));
            self.save();
            return GoalDecision {
                status: Some("paused".to_string()),
                should_continue: false,
                continuation_prompt: None,
                verdict: "continue".to_string(),
                reason: verdict.reason.clone(),
                message: format!(
                    "⏸ Goal paused — {}/{} turns used. Use /goal resume to keep going, or /goal clear to stop.",
                    turns_used, max_turns
                ),
            };
        }

        self.save();
        GoalDecision {
            status: Some("active".to_string()),
            should_continue: true,
            continuation_prompt: self.next_continuation_prompt(),
            verdict: "continue".to_string(),
            reason: verdict.reason.clone(),
            message: format!(
                "↻ Continuing toward goal ({}/{}): {}",
                turns_used, max_turns, verdict.reason
            ),
        }
    }

    /// Set a wait barrier from a judge directive; returns the display target.
    fn park_on(&mut self, directive: &WaitDirective, reason: &str) -> String {
        let reason = Some(reason.trim().to_string()).filter(|r| !r.is_empty());
        let tgt = match directive {
            WaitDirective::Session(id) => {
                let state = self.state.as_mut().unwrap();
                state.waiting_on_session = Some(id.clone());
                state.waiting_on_pid = None;
                state.waiting_until = 0.0;
                format!("session {}", id)
            }
            WaitDirective::Pid(pid) => {
                let state = self.state.as_mut().unwrap();
                state.waiting_on_pid = Some(*pid);
                state.waiting_on_session = None;
                state.waiting_until = 0.0;
                format!("pid {}", pid)
            }
            WaitDirective::Seconds(seconds) => {
                let state = self.state.as_mut().unwrap();
                state.waiting_on_pid = None;
                state.waiting_on_session = None;
                state.waiting_until = now_epoch() + *seconds as f64;
                format!("{}s", seconds)
            }
        };
        let state = self.state.as_mut().unwrap();
        state.waiting_reason = reason;
        state.waiting_since = now_epoch();
        tgt
    }

    /// The canonical user-role message to feed back into the conversation
    /// (hermes `next_continuation_prompt`). Contract takes priority;
    /// subgoals fold in as extra criteria appended to the contract block.
    pub fn next_continuation_prompt(&self) -> Option<String> {
        let state = self.state.as_ref()?;
        if state.status != "active" {
            return None;
        }
        if state.has_contract() {
            let mut contract_block = state.contract.render_block();
            if !state.subgoals.is_empty() {
                let extra = state
                    .subgoals
                    .iter()
                    .enumerate()
                    .map(|(i, text)| format!("- Extra criterion {}: {}", i + 1, text))
                    .collect::<Vec<_>>()
                    .join("\n");
                contract_block = format!("{}\n{}", contract_block, extra);
            }
            return Some(
                CONTINUATION_PROMPT_WITH_CONTRACT_TEMPLATE
                    .replace("{goal}", &state.goal)
                    .replace("{contract_block}", &contract_block),
            );
        }
        if !state.subgoals.is_empty() {
            return Some(
                CONTINUATION_PROMPT_WITH_SUBGOALS_TEMPLATE
                    .replace("{goal}", &state.goal)
                    .replace("{subgoals_block}", &state.render_subgoals_block()),
            );
        }
        Some(CONTINUATION_PROMPT_TEMPLATE.replace("{goal}", &state.goal))
    }

    /// Public helper for /goal show + /goal draft (hermes `render_contract`).
    pub fn render_contract(&self) -> String {
        let Some(state) = self.state.as_ref() else {
            return "(no active goal)".to_string();
        };
        if !state.has_contract() {
            return "(no completion contract — set one with /goal draft <objective> or inline field: value lines)"
                .to_string();
        }
        state.contract.render_block()
    }
}

/// Decision returned by `evaluate_after_turn` (hermes decision dict).
#[derive(Debug, Clone)]
pub struct GoalDecision {
    /// Current goal status after the update (`None` when no goal exists).
    pub status: Option<String>,
    /// Caller should fire another turn with `continuation_prompt`.
    pub should_continue: bool,
    pub continuation_prompt: Option<String>,
    /// "done" | "continue" | "wait" | "waiting" | "skipped" | "inactive"
    pub verdict: String,
    pub reason: String,
    /// User-visible one-liner to print.
    pub message: String,
}

impl GoalDecision {
    pub fn inactive(status: Option<String>) -> Self {
        Self {
            status,
            should_continue: false,
            continuation_prompt: None,
            verdict: "inactive".to_string(),
            reason: "no active goal".to_string(),
            message: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Judge
// ---------------------------------------------------------------------------

/// Auxiliary task name for the goal judge + contract drafter (hermes
/// `task="goal_judge"`).
pub const TASK_GOAL_JUDGE: &str = "goal_judge";

/// Resolve `auxiliary.goal_judge.max_tokens`, falling back to the default
/// (hermes `_goal_judge_max_tokens`).
pub fn goal_judge_max_tokens(config: &UlncLawConfig) -> u32 {
    config
        .auxiliary
        .get(TASK_GOAL_JUDGE)
        .and_then(|task| task.max_tokens())
        .unwrap_or(DEFAULT_JUDGE_MAX_TOKENS)
}

fn system_message(content: &str) -> Message {
    Message {
        role: Role::System,
        content: Some(content.to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }
}

fn user_message(content: String) -> Message {
    Message {
        role: Role::User,
        content: Some(content),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }
}

/// Ask the auxiliary model whether the goal is satisfied (hermes
/// `judge_goal`). Returns `(verdict, transport_failed)`.
///
/// Deliberately fail-open: transport errors yield a `continue` verdict with
/// `transport_failed=true` so callers can auto-pause after N consecutive
/// transport failures instead of burning the whole turn budget on an
/// unreachable API. `subgoals`, `background_processes`, and `contract` are
/// additive — when none are set the prompt is identical to the original
/// free-form judge.
pub async fn judge_goal(
    config: &UlncLawConfig,
    main_provider: Arc<dyn Provider>,
    goal: &str,
    last_response: &str,
    subgoals: &[String],
    background_processes: &[BackgroundProcessInfo],
    contract: Option<&GoalContract>,
) -> (JudgeVerdict, bool) {
    if goal.trim().is_empty() {
        return (
            JudgeVerdict {
                verdict: "skipped".into(),
                reason: "empty goal".into(),
                parse_failed: false,
                wait: None,
            },
            false,
        );
    }
    if last_response.trim().is_empty() {
        // No substantive reply this turn — almost certainly not done yet.
        return (
            JudgeVerdict {
                verdict: "continue".into(),
                reason: "empty response (nothing to evaluate)".into(),
                parse_failed: false,
                wait: None,
            },
            false,
        );
    }

    let resolution = match crate::provider::auxiliary::resolve_aux_task(
        config,
        TASK_GOAL_JUDGE,
        main_provider,
    ) {
        Ok(resolution) => resolution,
        Err(e) => {
            tracing::debug!("goal judge: auxiliary routing failed: {}", e);
            return (
                JudgeVerdict {
                    verdict: "continue".into(),
                    reason: "auxiliary client unavailable".into(),
                    parse_failed: false,
                    wait: None,
                },
                false,
            );
        }
    };

    // Build the prompt. Priority: contract > subgoals > plain. When both a
    // contract and subgoals exist, the subgoals are appended into the
    // contract block as extra criteria so the judge sees a single source of
    // truth.
    let clean_subgoals: Vec<String> = subgoals
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let background_block = render_background_block(background_processes);
    let current_time = chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string();

    let prompt = if let Some(contract) = contract.filter(|c| !c.is_empty()) {
        let mut contract_block = contract.render_block();
        if !clean_subgoals.is_empty() {
            let extra = clean_subgoals
                .iter()
                .enumerate()
                .map(|(i, text)| format!("- Extra criterion {}: {}", i + 1, text))
                .collect::<Vec<_>>()
                .join("\n");
            contract_block = format!("{}\n{}", contract_block, extra);
        }
        JUDGE_USER_PROMPT_WITH_CONTRACT_TEMPLATE
            .replace("{goal}", &truncate_prompt(goal, 2000))
            .replace("{contract_block}", &truncate_prompt(&contract_block, 2500))
            .replace(
                "{response}",
                &truncate_prompt(last_response, JUDGE_RESPONSE_SNIPPET_CHARS),
            )
            .replace("{background_block}", &background_block)
            .replace("{current_time}", &current_time)
    } else if !clean_subgoals.is_empty() {
        let subgoals_block = clean_subgoals
            .iter()
            .enumerate()
            .map(|(i, text)| format!("- {}. {}", i + 1, text))
            .collect::<Vec<_>>()
            .join("\n");
        JUDGE_USER_PROMPT_WITH_SUBGOALS_TEMPLATE
            .replace("{goal}", &truncate_prompt(goal, 2000))
            .replace("{subgoals_block}", &truncate_prompt(&subgoals_block, 2000))
            .replace(
                "{response}",
                &truncate_prompt(last_response, JUDGE_RESPONSE_SNIPPET_CHARS),
            )
            .replace("{background_block}", &background_block)
            .replace("{current_time}", &current_time)
    } else {
        JUDGE_USER_PROMPT_TEMPLATE
            .replace("{goal}", &truncate_prompt(goal, 2000))
            .replace(
                "{response}",
                &truncate_prompt(last_response, JUDGE_RESPONSE_SNIPPET_CHARS),
            )
            .replace("{background_block}", &background_block)
            .replace("{current_time}", &current_time)
    };

    let request = ProviderRequest {
        messages: vec![system_message(JUDGE_SYSTEM_PROMPT), user_message(prompt)],
        tools: Vec::new(),
        model: resolution.model,
        max_tokens: Some(goal_judge_max_tokens(config)),
        temperature: Some(0.0),
        stream: false,
        stop: None,

        images: None,
    };

    match resolution.provider.chat_completion(request).await {
        Ok(response) => (
            parse_judge_response(&response.content.unwrap_or_default()),
            false,
        ),
        Err(e) => {
            tracing::info!(
                "goal judge: API call failed ({}) — falling through to continue",
                e
            );
            (
                JudgeVerdict {
                    verdict: "continue".into(),
                    reason: format!("judge error: {}", error_kind(&e)),
                    parse_failed: false,
                    wait: None,
                },
                true,
            )
        }
    }
}

/// Short variant name for error reporting (Python `type(exc).__name__`).
fn error_kind(error: &crate::error::AgentError) -> &'static str {
    match error {
        crate::error::AgentError::Provider(_) => "ProviderError",
        crate::error::AgentError::Http(_) => "HttpError",
        crate::error::AgentError::Config(_) => "ConfigError",
        _ => "Error",
    }
}

/// Expand a plain-language objective into a structured completion contract
/// (hermes `draft_contract`). Uses the `goal_judge` auxiliary task
/// (main-model-first, cache-safe — a side LLM call, not a conversation
/// turn). Returns `None` when the auxiliary client is unavailable or the
/// reply can't be parsed; callers fall back to a bare free-form goal.
pub async fn draft_contract(
    config: &UlncLawConfig,
    main_provider: Arc<dyn Provider>,
    objective: &str,
) -> Option<GoalContract> {
    let objective = objective.trim();
    if objective.is_empty() {
        return None;
    }
    let resolution = match crate::provider::auxiliary::resolve_aux_task(
        config,
        TASK_GOAL_JUDGE,
        main_provider,
    ) {
        Ok(resolution) => resolution,
        Err(e) => {
            tracing::debug!("goal draft: auxiliary routing failed: {}", e);
            return None;
        }
    };
    let request = ProviderRequest {
        messages: vec![
            system_message(DRAFT_CONTRACT_SYSTEM_PROMPT),
            user_message(format!("Objective:\n{}", truncate_prompt(objective, 4000))),
        ],
        tools: Vec::new(),
        model: resolution.model,
        max_tokens: Some(goal_judge_max_tokens(config)),
        temperature: Some(0.0),
        stream: false,
        stop: None,

        images: None,
    };
    let response = match resolution.provider.chat_completion(request).await {
        Ok(response) => response,
        Err(e) => {
            tracing::info!("goal draft: API call failed ({})", e);
            return None;
        }
    };
    let raw = response.content.unwrap_or_default();
    let Some(value) = extract_json_object(&raw) else {
        tracing::debug!(
            "goal draft: reply was not JSON: {:?}",
            truncate_chars(&raw, 200)
        );
        return None;
    };
    let contract = GoalContract::from_value(Some(&value));
    if contract.is_empty() {
        None
    } else {
        Some(contract)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Kanban goal-mode worker loop (hermes `run_kanban_goal_loop`)
// ---------------------------------------------------------------------------

/// Fed to a goal-mode worker when the judge says the card is not done
/// yet (hermes `KANBAN_GOAL_CONTINUATION_TEMPLATE`).
pub const KANBAN_GOAL_CONTINUATION_TEMPLATE: &str =
    "[Continuing toward this kanban task — judge says it is not done yet]\n\
     Reason: {reason}\n\n\
     Take the next concrete step toward completing the task. When the work \
     is genuinely finished, call kanban_complete with a summary. If you are \
     blocked and need human input, call kanban_block with a reason. Do not \
     stop without calling one of them.";

/// Fed when the judge believes the work is done but the worker never
/// called kanban_complete / kanban_block (hermes
/// `KANBAN_GOAL_FINALIZE_TEMPLATE`).
pub const KANBAN_GOAL_FINALIZE_TEMPLATE: &str =
    "[The work looks complete, but the task is still open]\n\
     Reason: {reason}\n\n\
     If the task is genuinely done, call kanban_complete now with a short \
     summary of what you did. If something still blocks completion, call \
     kanban_block with the reason instead.";

fn fill_reason(template: &str, reason: &str) -> String {
    template.replace("{reason}", reason)
}

/// Terminal outcome of a kanban goal loop (hermes
/// `run_kanban_goal_loop` decision dict).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KanbanGoalOutcome {
    /// The worker called kanban_complete itself.
    CompletedByWorker,
    /// The worker called kanban_block itself.
    BlockedByWorker,
    /// The loop blocked the task: turn budget exhausted, or judged done
    /// but the worker never finalized.
    BlockedBudget,
    /// The loop gave up without mutating the task (status check failed,
    /// task reclaimed/archived, run_turn error) — the dispatcher owns
    /// whatever happens next.
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KanbanGoalLoopResult {
    pub outcome: KanbanGoalOutcome,
    pub turns_used: u32,
    pub reason: String,
}

/// Boxed future alias for the injected callbacks.
pub type GoalLoopFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Drive a kanban goal-mode worker through the Ralph-style goal loop
/// (hermes `run_kanban_goal_loop`).
///
/// The dispatcher spawns a goal-mode worker like any other worker; its
/// first turn has already run by the time this is called and
/// `first_response` is that turn's reply. From here:
///
/// 1. If the worker already terminated the task (kanban_complete /
///    kanban_block), stop — nothing to do.
/// 2. Otherwise judge the latest response against `goal_text` (the
///    card's title + body). `continue` → feed a continuation prompt and
///    run another turn IN THE SAME SESSION. `done` but the task still
///    open → one explicit "call kanban_complete" nudge; a second
///    `done` verdict blocks the card for human review.
/// 3. When the turn budget is exhausted with the task still open,
///    `block` is invoked so the card lands in a sticky blocked state
///    for human review (NOT a silent exit).
///
/// No session persistence here — a worker process is ephemeral, the
/// turn budget lives in a local counter. `wait` verdicts are treated as
/// `continue`: workers finish via kanban_complete / kanban_block, not
/// by parking.
pub async fn run_kanban_goal_loop<'e, R, S, B, J, L>(
    task_id: &str,
    mut run_turn: R,
    mut task_status: S,
    mut block: B,
    mut judge: J,
    mut log: L,
    max_turns: u32,
    first_response: &str,
) -> KanbanGoalLoopResult
where
    R: FnMut(String) -> GoalLoopFuture<'e, Result<String, String>>,
    S: FnMut() -> GoalLoopFuture<'e, Option<String>>,
    B: FnMut(String) -> GoalLoopFuture<'e, ()>,
    J: FnMut(String) -> GoalLoopFuture<'e, JudgeVerdict>,
    L: FnMut(String),
{
    let max_turns = if max_turns < 1 {
        DEFAULT_MAX_TURNS as u32
    } else {
        max_turns
    };
    let mut last_response = first_response.to_string();
    // The first turn already consumed one unit of budget.
    let mut turns_used: u32 = 1;
    let mut nudged_to_finalize = false;

    loop {
        // Did the worker terminate the task itself this turn?
        let status = match task_status().await {
            Some(status) => status,
            None => {
                return KanbanGoalLoopResult {
                    outcome: KanbanGoalOutcome::Stopped,
                    turns_used,
                    reason: "status check failed".into(),
                }
            }
        };
        match status.as_str() {
            "done" => {
                log(format!(
                    "kanban goal loop: task {task_id} completed by worker after {turns_used} turn(s)"
                ));
                return KanbanGoalLoopResult {
                    outcome: KanbanGoalOutcome::CompletedByWorker,
                    turns_used,
                    reason: "worker completed the task".into(),
                };
            }
            "blocked" => {
                log(format!(
                    "kanban goal loop: task {task_id} blocked by worker after {turns_used} turn(s)"
                ));
                return KanbanGoalLoopResult {
                    outcome: KanbanGoalOutcome::BlockedByWorker,
                    turns_used,
                    reason: "worker blocked the task".into(),
                };
            }
            "running" | "ready" => {}
            other => {
                // Reclaimed / archived / unexpected — let the
                // dispatcher own it.
                log(format!(
                    "kanban goal loop: task {task_id} status={other:?}; stopping"
                ));
                return KanbanGoalLoopResult {
                    outcome: KanbanGoalOutcome::Stopped,
                    turns_used,
                    reason: format!("status={other}"),
                };
            }
        }

        // Still open — judge whether the latest response satisfies the
        // card.
        let mut verdict = judge(last_response.clone()).await;
        if verdict.verdict == "wait" {
            verdict.verdict = "continue".into();
        }
        log(format!(
            "kanban goal loop: turn {turns_used}/{max_turns} verdict={} reason={}",
            verdict.verdict,
            truncate_chars(&verdict.reason, 120)
        ));

        let prompt = if verdict.verdict == "done" {
            if nudged_to_finalize {
                // Already asked once to call kanban_complete and it
                // still didn't — block for review rather than spin.
                block(format!(
                    "Goal-mode worker's output looked complete but it never called \
                     kanban_complete after a finalize nudge ({}).",
                    truncate_chars(&verdict.reason, 300)
                ))
                .await;
                return KanbanGoalLoopResult {
                    outcome: KanbanGoalOutcome::BlockedBudget,
                    turns_used,
                    reason: "judged done, never finalized".into(),
                };
            }
            nudged_to_finalize = true;
            fill_reason(
                KANBAN_GOAL_FINALIZE_TEMPLATE,
                &truncate_chars(&verdict.reason, 400),
            )
        } else {
            fill_reason(
                KANBAN_GOAL_CONTINUATION_TEMPLATE,
                &truncate_chars(&verdict.reason, 400),
            )
        };

        // Budget check BEFORE spending another turn.
        if turns_used >= max_turns {
            block(format!(
                "Goal-mode worker exhausted its turn budget ({turns_used}/{max_turns}) \
                 without completing the task. Last judge verdict: {}",
                truncate_chars(&verdict.reason, 300)
            ))
            .await;
            return KanbanGoalLoopResult {
                outcome: KanbanGoalOutcome::BlockedBudget,
                turns_used,
                reason: "turn budget exhausted".into(),
            };
        }

        // Run another turn in the same session.
        match run_turn(prompt).await {
            Ok(response) => last_response = response,
            Err(e) => {
                return KanbanGoalLoopResult {
                    outcome: KanbanGoalOutcome::Stopped,
                    turns_used,
                    reason: format!("run_turn error: {e}"),
                }
            }
        }
        turns_used += 1;
    }
}

/// Completion-gate decision for goal-mode cards (hermes #38367 judge
/// gate). Pure helper: `verdict` is the judge's verdict string for the
/// proposed completion text, or None when no judge could run at all.
///
/// Non-goal tasks always pass. Goal tasks pass on `done`, on `skipped`
/// (judge unreachable — fail-open like the hermes availability
/// precheck), and when no verdict exists; any other verdict rejects.
pub fn goal_gate_allows(goal_mode: bool, verdict: Option<&str>) -> bool {
    if !goal_mode {
        return true;
    }
    matches!(verdict, None | Some("done") | Some("skipped"))
}

/// Completion gate for goal-mode cards (hermes #38367 judge gate):
/// check the proposed completion text against the card's acceptance
/// criteria with the auxiliary judge.
///
/// Fail-open: no provider, or auxiliary routing that cannot resolve a
/// judge, allows the completion (the hermes availability precheck). A
/// judge that runs allows only `done` / `skipped`; anything else
/// rejects with the judge's reason.
pub async fn goal_completion_gate(
    config: &UlncLawConfig,
    provider: Option<Arc<dyn Provider>>,
    goal: &str,
    text: &str,
) -> Result<(), String> {
    let Some(provider) = provider else {
        return Ok(());
    };
    if crate::provider::auxiliary::resolve_aux_task(config, TASK_GOAL_JUDGE, provider.clone())
        .is_err()
    {
        return Ok(());
    }
    let (verdict, _transport_failed) =
        judge_goal(config, provider, goal, text, &[], &[], None).await;
    if goal_gate_allows(true, Some(verdict.verdict.as_str())) {
        Ok(())
    } else {
        Err(verdict.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SqliteSessionStore;

    fn temp_store(name: &str) -> (tempfile::TempDir, Arc<SqliteSessionStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            SqliteSessionStore::open(dir.path().join(name).join("state.db")).expect("store opens"),
        );
        (dir, store)
    }

    fn judge_verdict(verdict: &str, reason: &str) -> JudgeVerdict {
        JudgeVerdict {
            verdict: verdict.to_string(),
            reason: reason.to_string(),
            parse_failed: false,
            wait: None,
        }
    }

    // --- parse_contract -------------------------------------------------

    #[test]
    fn parse_contract_plain_goal_unchanged() {
        let (headline, contract) = parse_contract("Fix the flaky login test");
        assert_eq!(headline, "Fix the flaky login test");
        assert!(contract.is_empty());
    }

    #[test]
    fn parse_contract_field_lines() {
        let text = "Ship the parser\n\
                    verify: cargo test parser\n\
                    constraints: do not touch the lexer\n\
                    scope: src/parser/\n\
                    stop when: blocked on API keys";
        let (headline, contract) = parse_contract(text);
        assert_eq!(headline, "Ship the parser");
        assert_eq!(contract.verification, "cargo test parser");
        assert_eq!(contract.constraints, "do not touch the lexer");
        assert_eq!(contract.boundaries, "src/parser/");
        assert_eq!(contract.stop_when, "blocked on API keys");
    }

    #[test]
    fn parse_contract_incidental_colon_stays_in_headline() {
        let (headline, contract) = parse_contract("Note: this goal has a colon");
        assert_eq!(headline, "Note: this goal has a colon");
        assert!(contract.is_empty());
    }

    #[test]
    fn parse_contract_empty_value_stays_in_headline() {
        // Hermes semantics: a recognized prefix with an EMPTY value does not
        // match, so the line stays in the headline (only populated fields
        // are pulled into the contract).
        let (headline, contract) = parse_contract("Do the thing\nverify:");
        assert_eq!(headline, "Do the thing verify:");
        assert!(contract.is_empty());
    }

    // --- GoalState roundtrip ---------------------------------------------

    #[test]
    fn goal_state_json_roundtrip() {
        let mut state = GoalState::new("make it work", 7);
        state.subgoals = vec!["a".into(), "  ".into(), "b".into()];
        state.waiting_on_pid = Some(4242);
        state.contract.outcome = "works".into();
        let restored = GoalState::from_json(&state.to_json()).expect("roundtrip");
        assert_eq!(restored.goal, "make it work");
        assert_eq!(restored.max_turns, 7);
        assert_eq!(restored.subgoals, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(restored.waiting_on_pid, Some(4242));
        assert_eq!(restored.contract.outcome, "works");
        assert_eq!(restored.status, "active");
    }

    // --- parse_judge_response ---------------------------------------------

    #[test]
    fn judge_verdict_shapes() {
        assert_eq!(
            parse_judge_response(r#"{"verdict": "done", "reason": "finished"}"#).verdict,
            "done"
        );
        assert_eq!(
            parse_judge_response(r#"{"verdict": "CONTINUE", "reason": "more work"}"#).verdict,
            "continue"
        );
        let legacy_done = parse_judge_response(r#"{"done": true, "reason": "ok"}"#);
        assert_eq!(legacy_done.verdict, "done");
        let legacy_not_done = parse_judge_response(r#"{"done": false}"#);
        assert_eq!(legacy_not_done.verdict, "continue");
        assert_eq!(legacy_not_done.reason, "no reason provided");
    }

    #[test]
    fn judge_verdict_fenced_json() {
        let raw = "```json\n{\"verdict\": \"done\", \"reason\": \"complete\"}\n```";
        let verdict = parse_judge_response(raw);
        assert_eq!(verdict.verdict, "done");
        assert!(!verdict.parse_failed);
    }

    #[test]
    fn judge_verdict_not_json_fails_open() {
        let verdict = parse_judge_response("I think it is probably done honestly");
        assert_eq!(verdict.verdict, "continue");
        assert!(verdict.parse_failed);
        let empty = parse_judge_response("");
        assert_eq!(empty.verdict, "continue");
        assert!(empty.parse_failed);
    }

    #[test]
    fn judge_verdict_unknown_verdict_becomes_continue() {
        let verdict = parse_judge_response(r#"{"verdict": "maybe", "reason": "unsure"}"#);
        assert_eq!(verdict.verdict, "continue");
        assert!(!verdict.parse_failed);
    }

    #[test]
    fn judge_wait_directives() {
        let by_session = parse_judge_response(
            r#"{"verdict": "wait", "wait_on_session": "bg-1", "reason": "ci"}"#,
        );
        assert_eq!(by_session.wait, Some(WaitDirective::Session("bg-1".into())));
        let by_pid =
            parse_judge_response(r#"{"verdict": "wait", "wait_on_pid": 1234, "reason": "build"}"#);
        assert_eq!(by_pid.wait, Some(WaitDirective::Pid(1234)));
        let by_seconds = parse_judge_response(
            r#"{"verdict": "wait", "wait_for_seconds": 60, "reason": "rate limit"}"#,
        );
        assert_eq!(by_seconds.wait, Some(WaitDirective::Seconds(60)));
        // Aliases + string numbers.
        let alias = parse_judge_response(r#"{"verdict": "wait", "session_id": "bg-9"}"#);
        assert_eq!(alias.wait, Some(WaitDirective::Session("bg-9".into())));
        let string_pid = parse_judge_response(r#"{"verdict": "wait", "pid": "777"}"#);
        assert_eq!(string_pid.wait, Some(WaitDirective::Pid(777)));
    }

    #[test]
    fn judge_wait_without_target_downgrades() {
        let verdict = parse_judge_response(r#"{"verdict": "wait", "reason": "waiting"}"#);
        assert_eq!(verdict.verdict, "continue");
        assert!(verdict.wait.is_none());
        assert!(verdict.reason.contains("no target"));
        let zero_seconds = parse_judge_response(r#"{"verdict": "wait", "wait_for_seconds": 0}"#);
        assert_eq!(zero_seconds.verdict, "continue");
    }

    #[test]
    fn judge_verdict_embedded_json_extracted() {
        let raw = "Here is my decision:\n{\"verdict\": \"done\", \"reason\": \"built\"}\nThanks!";
        let verdict = parse_judge_response(raw);
        assert_eq!(verdict.verdict, "done");
        assert!(!verdict.parse_failed);
    }

    // --- background block --------------------------------------------------

    #[test]
    fn background_block_skips_exited_and_pidless() {
        let processes = vec![
            BackgroundProcessInfo {
                pid: None,
                session_id: Some("bg-0".into()),
                command: "no pid".into(),
                status: "running".into(),
                uptime_seconds: Some(3),
                output_preview: None,
            },
            BackgroundProcessInfo {
                pid: Some(99),
                session_id: None,
                command: "done".into(),
                status: "exited".into(),
                uptime_seconds: None,
                output_preview: None,
            },
            BackgroundProcessInfo {
                pid: Some(100),
                session_id: Some("bg-1".into()),
                command: "cargo test".into(),
                status: "running".into(),
                uptime_seconds: Some(12),
                output_preview: Some("ok so far".into()),
            },
        ];
        let block = render_background_block(&processes);
        assert!(block.contains("pid 100 / session bg-1: cargo test (running 12s)"));
        assert!(block.contains("recent output: ok so far"));
        assert!(!block.contains("pid 99"));
        assert!(!block.contains("no pid"));
        assert!(render_background_block(&[]).is_empty());
    }

    // --- continuation prompts ------------------------------------------------

    fn manager_with_state(store: Option<Arc<SqliteSessionStore>>) -> GoalManager {
        let mut manager = GoalManager::new("sess-1", store, DEFAULT_MAX_TURNS);
        manager.set("ship the feature", Some(5), None).expect("set");
        manager
    }

    #[test]
    fn continuation_prompt_plain() {
        let manager = manager_with_state(None);
        let prompt = manager.next_continuation_prompt().expect("prompt");
        assert!(prompt.contains("Goal: ship the feature"));
        assert!(prompt.contains("next concrete step"));
        assert!(!prompt.contains("Completion contract"));
    }

    #[test]
    fn continuation_prompt_subgoals() {
        let mut manager = manager_with_state(None);
        manager.add_subgoal("docs updated").unwrap();
        let prompt = manager.next_continuation_prompt().expect("prompt");
        assert!(prompt.contains("Additional criteria"));
        assert!(prompt.contains("- 1. docs updated"));
    }

    #[test]
    fn continuation_prompt_contract_with_subgoals_folded() {
        let mut manager = manager_with_state(None);
        manager
            .set_contract(GoalContract {
                outcome: "feature shipped".into(),
                verification: "cargo test passes".into(),
                ..Default::default()
            })
            .expect("contract");
        manager.add_subgoal("changelog entry").unwrap();
        let prompt = manager.next_continuation_prompt().expect("prompt");
        assert!(prompt.contains("Completion contract"));
        assert!(prompt.contains("- Outcome: feature shipped"));
        assert!(prompt.contains("- Verification: cargo test passes"));
        assert!(prompt.contains("- Extra criterion 1: changelog entry"));
        assert!(!prompt.contains("constraints:"));
    }

    // --- persistence ---------------------------------------------------------

    #[test]
    fn goal_persists_across_managers() {
        let (_dir, store) = temp_store("persist");
        {
            let mut manager = GoalManager::new("sess-p", Some(store.clone()), DEFAULT_MAX_TURNS);
            manager.set("persist me", None, None).expect("set");
            manager.add_subgoal("sub one").unwrap();
        }
        let manager = GoalManager::new("sess-p", Some(store.clone()), DEFAULT_MAX_TURNS);
        let state = manager.state().expect("loaded");
        assert_eq!(state.goal, "persist me");
        assert_eq!(state.subgoals, vec!["sub one".to_string()]);
        assert!(manager.is_active());
    }

    #[test]
    fn clear_archives_row_as_cleared() {
        let (_dir, store) = temp_store("clear");
        let mut manager = GoalManager::new("sess-c", Some(store.clone()), DEFAULT_MAX_TURNS);
        manager.set("to clear", None, None).expect("set");
        manager.clear();
        assert!(manager.state().is_none());
        let archived = load_goal(&store, "sess-c").expect("row preserved");
        assert_eq!(archived.status, "cleared");
        // A fresh manager sees no active goal.
        let fresh = GoalManager::new("sess-c", Some(store.clone()), DEFAULT_MAX_TURNS);
        assert!(!fresh.has_goal());
        assert_eq!(
            fresh.status_line(),
            "No active goal. Set one with /goal <text>."
        );
    }

    #[test]
    fn migrate_goal_moves_active_row() {
        let (_dir, store) = temp_store("migrate");
        let mut manager = GoalManager::new("old-sess", Some(store.clone()), DEFAULT_MAX_TURNS);
        manager.set("carry me over", None, None).expect("set");
        assert!(migrate_goal_to_session(
            &store,
            "old-sess",
            "new-sess",
            "compaction"
        ));
        let migrated = load_goal(&store, "new-sess").expect("migrated");
        assert_eq!(migrated.goal, "carry me over");
        assert_eq!(migrated.status, "active");
        assert_eq!(load_goal(&store, "old-sess").unwrap().status, "cleared");
        // Second migration is a no-op (old row cleared).
        assert!(!migrate_goal_to_session(
            &store,
            "old-sess",
            "newer-sess",
            ""
        ));
    }

    // --- state machine ---------------------------------------------------------

    #[test]
    fn apply_verdict_continue_keeps_loop_alive() {
        let mut manager = manager_with_state(None);
        // evaluate_after_turn counts the turn BEFORE judging, so the state
        // already reflects the turn that just finished.
        manager.state.as_mut().unwrap().turns_used = 2;
        let decision = manager.apply_verdict(judge_verdict("continue", "more work"), false);
        assert!(decision.should_continue);
        assert_eq!(decision.verdict, "continue");
        assert!(decision.continuation_prompt.is_some());
        assert!(decision.message.contains("2/5"));
    }

    #[test]
    fn apply_verdict_done_marks_done() {
        let mut manager = manager_with_state(None);
        let decision = manager.apply_verdict(judge_verdict("done", "all green"), false);
        assert!(!decision.should_continue);
        assert_eq!(decision.status.as_deref(), Some("done"));
        assert!(decision.message.contains("Goal achieved"));
        assert_eq!(manager.state().unwrap().status, "done");
        // A done goal evaluates inactive afterwards.
        assert!(!manager.is_active());
    }

    #[test]
    fn budget_exhaustion_pauses() {
        let mut manager = manager_with_state(None);
        // Turn already counted by evaluate_after_turn → at the budget cap.
        manager.state.as_mut().unwrap().turns_used = 5;
        let decision = manager.apply_verdict(judge_verdict("continue", "still going"), false);
        assert!(!decision.should_continue);
        assert_eq!(decision.status.as_deref(), Some("paused"));
        assert!(decision.message.contains("5/5 turns used"));
        assert_eq!(
            manager.state().unwrap().paused_reason.as_deref(),
            Some("turn budget exhausted (5/5)")
        );
    }

    #[test]
    fn parse_failures_auto_pause() {
        let mut manager = manager_with_state(None);
        for _ in 0..DEFAULT_MAX_CONSECUTIVE_PARSE_FAILURES - 1 {
            let mut bad = judge_verdict("continue", "not json");
            bad.parse_failed = true;
            let decision = manager.apply_verdict(bad, false);
            assert!(decision.should_continue);
        }
        let mut bad = judge_verdict("continue", "not json");
        bad.parse_failed = true;
        let decision = manager.apply_verdict(bad, false);
        assert!(!decision.should_continue);
        assert_eq!(decision.status.as_deref(), Some("paused"));
        assert!(decision
            .message
            .contains("isn't returning the required JSON verdict"));
    }

    #[test]
    fn transport_failures_auto_pause() {
        let mut manager = manager_with_state(None);
        manager.state.as_mut().unwrap().max_turns = 100;
        for _ in 0..DEFAULT_MAX_CONSECUTIVE_TRANSPORT_FAILURES {
            let decision =
                manager.apply_verdict(judge_verdict("continue", "judge error: HttpError"), true);
            if decision.status.as_deref() == Some("paused") {
                assert!(decision.message.contains("judge API returned errors"));
                return;
            }
            assert!(decision.should_continue);
        }
        panic!(
            "expected auto-pause after {} transport failures",
            DEFAULT_MAX_CONSECUTIVE_TRANSPORT_FAILURES
        );
    }

    #[test]
    fn parse_counter_resets_on_transport_error() {
        let mut manager = manager_with_state(None);
        manager.state.as_mut().unwrap().max_turns = 100;
        for _ in 0..2 {
            let mut bad = judge_verdict("continue", "not json");
            bad.parse_failed = true;
            manager.apply_verdict(bad, false);
        }
        assert_eq!(manager.state().unwrap().consecutive_parse_failures, 2);
        // Transport errors are usable replies for the parse counter.
        manager.apply_verdict(judge_verdict("continue", "judge error: HttpError"), true);
        assert_eq!(manager.state().unwrap().consecutive_parse_failures, 0);
        assert_eq!(manager.state().unwrap().consecutive_transport_failures, 1);
    }

    #[test]
    fn wait_verdict_parks_on_seconds() {
        let mut manager = manager_with_state(None);
        let verdict = JudgeVerdict {
            verdict: "wait".into(),
            reason: "rate limited".into(),
            parse_failed: false,
            wait: Some(WaitDirective::Seconds(30)),
        };
        let decision = manager.apply_verdict(verdict, false);
        assert!(!decision.should_continue);
        assert_eq!(decision.verdict, "wait");
        assert!(decision.message.contains("30s"));
        assert!(manager.state().unwrap().waiting_until > now_epoch());
        // Barrier short-circuits the next evaluation without a judge call.
        let barrier = manager.check_wait_barrier().expect("parked");
        assert_eq!(barrier.verdict, "waiting");
        assert!(!barrier.should_continue);
        // Drop the barrier → loop resumes.
        assert!(manager.stop_waiting());
        assert!(manager.check_wait_barrier().is_none());
    }

    #[test]
    fn wait_barrier_on_dead_pid_autoclears() {
        let mut manager = manager_with_state(None);
        // Pid 1 is init — alive. Use a pid that cannot exist: 0 is rejected
        // by wait_on, so park on a huge pid instead.
        manager.wait_on(u32::MAX, "never exists").expect("park");
        assert!(!manager.is_waiting()); // dead pid → barrier auto-cleared
        assert!(manager.state().unwrap().waiting_on_pid.is_none());
    }

    #[test]
    fn inactive_goal_decision() {
        let manager_decision = GoalManager::new("sess-none", None, DEFAULT_MAX_TURNS);
        assert!(!manager_decision.is_active());
        let decision = GoalDecision::inactive(None);
        assert_eq!(decision.verdict, "inactive");
        assert!(!decision.should_continue);
    }

    // --- status line -----------------------------------------------------------

    #[test]
    fn status_line_variants() {
        let mut manager = manager_with_state(None);
        assert!(manager
            .status_line()
            .contains("⊙ Goal (active, 0/5 turns): ship the feature"));
        manager.add_subgoal("extra").unwrap();
        assert!(manager.status_line().contains("1 subgoal"));
        manager.pause("user-paused").unwrap();
        let line = manager.status_line();
        assert!(line.contains("⏸ Goal (paused"));
        assert!(line.contains("user-paused"));
        manager.resume(true).unwrap();
        assert!(manager.status_line().contains("0/5"));
        manager.mark_done("wrapped up");
        assert!(manager.status_line().contains("✓ Goal done"));
    }

    #[test]
    fn pause_drops_wait_barrier() {
        let mut manager = manager_with_state(None);
        manager.wait_for_seconds(60, "backoff").expect("park");
        assert!(manager.state().unwrap().waiting_until > 0.0);
        manager.pause("user-paused").unwrap();
        assert_eq!(manager.state().unwrap().waiting_until, 0.0);
        assert!(manager.state().unwrap().waiting_reason.is_none());
    }

    #[test]
    fn subgoal_management() {
        let mut manager = manager_with_state(None);
        assert_eq!(manager.add_subgoal("first").unwrap(), "first");
        manager.add_subgoal("second").unwrap();
        assert_eq!(manager.remove_subgoal(1).unwrap(), "first");
        assert!(manager.remove_subgoal(5).is_err());
        assert_eq!(manager.clear_subgoals().unwrap(), 1);
        assert!(manager.render_subgoals().contains("no subgoals"));
        // No goal → errors.
        let mut empty = GoalManager::new("sess-e", None, DEFAULT_MAX_TURNS);
        assert!(empty.add_subgoal("x").is_err());
        assert!(empty.clear_subgoals().is_err());
    }

    fn verdict(v: &str) -> JudgeVerdict {
        JudgeVerdict {
            verdict: v.into(),
            reason: format!("{v} reason"),
            parse_failed: false,
            wait: None,
        }
    }

    type LoopBox<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

    #[tokio::test]
    async fn goal_loop_worker_completed_itself() {
        let turns = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let turns_clone = turns.clone();
        let result = run_kanban_goal_loop(
            "t_done",
            move |_prompt| {
                turns_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { Ok("more work".to_string()) }) as LoopBox<_>
            },
            || Box::pin(async { Some("done".to_string()) }) as LoopBox<_>,
            |_reason| Box::pin(async {}) as LoopBox<()>,
            |_response| Box::pin(async { verdict("continue") }) as LoopBox<_>,
            |_msg| {},
            5,
            "first response",
        )
        .await;
        assert_eq!(result.outcome, KanbanGoalOutcome::CompletedByWorker);
        assert_eq!(result.turns_used, 1);
        assert_eq!(turns.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn goal_loop_worker_blocked_itself() {
        let result = run_kanban_goal_loop(
            "t_blk",
            |_prompt| Box::pin(async { Ok(String::new()) }) as LoopBox<_>,
            || Box::pin(async { Some("blocked".to_string()) }) as LoopBox<_>,
            |_reason| Box::pin(async {}) as LoopBox<()>,
            |_response| Box::pin(async { verdict("continue") }) as LoopBox<_>,
            |_msg| {},
            5,
            "first",
        )
        .await;
        assert_eq!(result.outcome, KanbanGoalOutcome::BlockedByWorker);
        assert_eq!(result.turns_used, 1);
    }

    #[tokio::test]
    async fn goal_loop_blocks_when_budget_exhausted() {
        let block_reason = std::sync::Arc::new(std::sync::Mutex::new(None));
        let block_clone = block_reason.clone();
        let result = run_kanban_goal_loop(
            "t_budget",
            |_prompt| Box::pin(async { Ok("still working".to_string()) }) as LoopBox<_>,
            || Box::pin(async { Some("running".to_string()) }) as LoopBox<_>,
            move |reason| {
                *block_clone.lock().unwrap() = Some(reason);
                Box::pin(async {}) as LoopBox<()>
            },
            |_response| Box::pin(async { verdict("continue") }) as LoopBox<_>,
            |_msg| {},
            2,
            "first",
        )
        .await;
        assert_eq!(result.outcome, KanbanGoalOutcome::BlockedBudget);
        assert_eq!(result.turns_used, 2);
        assert_eq!(result.reason, "turn budget exhausted");
        let reason = block_reason.lock().unwrap().clone().unwrap();
        assert!(
            reason.contains("exhausted its turn budget (2/2)"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn goal_loop_finalize_nudge_then_block() {
        let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let prompts_clone = prompts.clone();
        let block_reason = std::sync::Arc::new(std::sync::Mutex::new(None));
        let block_clone = block_reason.clone();
        let result = run_kanban_goal_loop(
            "t_nudge",
            move |prompt| {
                prompts_clone.lock().unwrap().push(prompt);
                Box::pin(async { Ok("ok".to_string()) }) as LoopBox<_>
            },
            || Box::pin(async { Some("running".to_string()) }) as LoopBox<_>,
            move |reason| {
                *block_clone.lock().unwrap() = Some(reason);
                Box::pin(async {}) as LoopBox<()>
            },
            |_response| Box::pin(async { verdict("done") }) as LoopBox<_>,
            |_msg| {},
            8,
            "first",
        )
        .await;
        assert_eq!(result.outcome, KanbanGoalOutcome::BlockedBudget);
        assert_eq!(result.reason, "judged done, never finalized");
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("task is still open"), "{}", prompts[0]);
        let reason = block_reason.lock().unwrap().clone().unwrap();
        assert!(reason.contains("never called kanban_complete"), "{reason}");
    }

    #[tokio::test]
    async fn goal_loop_wait_verdict_treated_as_continue() {
        let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let prompts_clone = prompts.clone();
        let result = run_kanban_goal_loop(
            "t_wait",
            move |prompt| {
                prompts_clone.lock().unwrap().push(prompt);
                Box::pin(async { Ok("ok".to_string()) }) as LoopBox<_>
            },
            || Box::pin(async { Some("running".to_string()) }) as LoopBox<_>,
            |_reason| Box::pin(async {}) as LoopBox<()>,
            |_response| Box::pin(async { verdict("wait") }) as LoopBox<_>,
            |_msg| {},
            2,
            "first",
        )
        .await;
        assert_eq!(result.outcome, KanbanGoalOutcome::BlockedBudget);
        assert_eq!(result.reason, "turn budget exhausted");
        // wait downgrades to continue → continuation template, not
        // finalize.
        let prompts = prompts.lock().unwrap();
        assert!(prompts[0].contains("not done yet"), "{}", prompts[0]);
    }

    #[tokio::test]
    async fn goal_loop_stops_on_unexpected_status() {
        let result = run_kanban_goal_loop(
            "t_gone",
            |_prompt| Box::pin(async { Ok(String::new()) }) as LoopBox<_>,
            || Box::pin(async { Some("archived".to_string()) }) as LoopBox<_>,
            |_reason| Box::pin(async {}) as LoopBox<()>,
            |_response| Box::pin(async { verdict("continue") }) as LoopBox<_>,
            |_msg| {},
            5,
            "first",
        )
        .await;
        assert_eq!(result.outcome, KanbanGoalOutcome::Stopped);
        assert_eq!(result.reason, "status=archived");
    }

    #[test]
    fn goal_gate_allows_semantics() {
        // Non-goal tasks always pass.
        assert!(goal_gate_allows(false, Some("continue")));
        assert!(goal_gate_allows(false, None));
        // Goal tasks: done / skipped / no-judge pass; anything else
        // rejects.
        assert!(goal_gate_allows(true, Some("done")));
        assert!(goal_gate_allows(true, Some("skipped")));
        assert!(goal_gate_allows(true, None));
        assert!(!goal_gate_allows(true, Some("continue")));
        assert!(!goal_gate_allows(true, Some("wait")));
    }

    #[test]
    fn set_rejects_empty_goal() {
        let mut manager = GoalManager::new("sess-x", None, DEFAULT_MAX_TURNS);
        assert!(manager.set("   ", None, None).is_err());
    }

    #[test]
    fn wait_requires_active_goal() {
        let mut manager = GoalManager::new("sess-w", None, DEFAULT_MAX_TURNS);
        assert!(manager.wait_on(123, "").is_err());
        manager.set("goal", None, None).unwrap();
        assert!(manager.wait_on(0, "").is_err());
        assert!(manager.wait_for_seconds(0, "").is_err());
        assert!(manager.wait_on_session("  ", "").is_err());
        manager.pause("p").unwrap();
        assert!(manager.wait_on(123, "").is_err()); // paused → no parking
    }
}

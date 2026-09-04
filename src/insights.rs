//! Session insights — usage analytics over the session store.
//!
//! Port of hermes `agent/insights.py` (v2026.8.3), adapted for ulnclaw's
//! SQLite schema: token consumption, models.dev-backed cost estimates,
//! tool usage, activity patterns (hour/weekday), source and model
//! breakdowns, top sessions. Inspired by Claude Code's `/insights`.

use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, TimeZone, Timelike};
use rusqlite::{params, Connection};

use crate::error::{AgentError, Result};

/// One session row as read by the engine.
#[derive(Debug, Clone)]
struct SessionData {
    id: String,
    source: String,
    model: String,
    started_at: f64,
    ended_at: Option<f64>,
    title: Option<String>,
    message_count: i64,
    tool_call_count: i64,
    input_tokens: i64,
    output_tokens: i64,
}

impl SessionData {
    fn total_tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens
    }

    fn duration_seconds(&self) -> Option<f64> {
        self.ended_at
            .filter(|end| *end >= self.started_at)
            .map(|end| end - self.started_at)
    }
}

/// Overview counters (hermes `_compute_overview`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Overview {
    pub total_sessions: usize,
    pub total_messages: i64,
    pub total_tool_calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub estimated_cost_usd: f64,
    /// True when at least one session had models.dev pricing data.
    pub cost_known: bool,
    pub avg_session_seconds: f64,
    pub active_days: usize,
}

/// Per-model aggregate (hermes model breakdown).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelUsage {
    pub model: String,
    pub sessions: usize,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub estimated_cost_usd: f64,
    pub cost_known: bool,
}

/// Per-source aggregate (hermes platform breakdown).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceUsage {
    pub source: String,
    pub sessions: usize,
    pub total_tokens: i64,
    pub tool_calls: i64,
}

/// Per-tool call counts (hermes tool breakdown).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolUsage {
    pub tool: String,
    pub calls: i64,
}

/// Per-skill aggregate scanned from assistant `tool_calls` (hermes
/// `_get_skill_usage` row).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SkillUsage {
    pub skill: String,
    pub view_count: i64,
    pub manage_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<f64>,
}

/// Skill usage summary (hermes `skills.summary`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SkillSummary {
    pub total_skill_loads: i64,
    pub total_skill_edits: i64,
    pub total_skill_actions: i64,
    pub distinct_skills_used: usize,
}

/// Ranked skill entry (hermes `skills.top_skills[]`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopSkill {
    pub skill: String,
    pub view_count: i64,
    pub manage_count: i64,
    pub total_count: i64,
    pub percentage: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<f64>,
}

/// Skill usage breakdown (hermes `_compute_skill_breakdown`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SkillBreakdown {
    pub summary: SkillSummary,
    pub top_skills: Vec<TopSkill>,
}

/// Lightweight tools+skills payload for gateway/dashboard embedding
/// (hermes `get_usage_breakdown` return shape).
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageBreakdown {
    pub tools: Vec<ToolUsage>,
    pub skills: SkillBreakdown,
}

/// Hour-of-day / weekday session starts (hermes activity patterns).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActivityPatterns {
    /// Session starts per hour of day (0-23).
    pub by_hour: Vec<u64>,
    /// Session starts per weekday (Mon=0 … Sun=6).
    pub by_weekday: Vec<u64>,
    pub peak_hour: Option<usize>,
    pub peak_weekday: Option<usize>,
}

/// Heavy-usage session (hermes top sessions).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopSession {
    pub id: String,
    pub title: Option<String>,
    pub model: String,
    pub started_at: f64,
    pub messages: i64,
    pub tool_calls: i64,
    pub total_tokens: i64,
    pub estimated_cost_usd: f64,
}

/// Full insights report (hermes `generate()` return shape).
#[derive(Debug, Clone, serde::Serialize)]
pub struct InsightsReport {
    pub days: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_filter: Option<String>,
    pub empty: bool,
    pub generated_at: f64,
    pub overview: Overview,
    pub models: Vec<ModelUsage>,
    pub sources: Vec<SourceUsage>,
    pub tools: Vec<ToolUsage>,
    pub skills: SkillBreakdown,
    pub activity: ActivityPatterns,
    pub top_sessions: Vec<TopSession>,
}

fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<SessionData> {
    Ok(SessionData {
        id: row.get(0)?,
        source: row.get(1)?,
        model: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        title: row.get(5)?,
        message_count: row.get(6)?,
        tool_call_count: row.get(7)?,
        input_tokens: row.get(8)?,
        output_tokens: row.get(9)?,
    })
}

fn row_to_tool(row: &rusqlite::Row) -> rusqlite::Result<ToolUsage> {
    Ok(ToolUsage {
        tool: row.get(0)?,
        calls: row.get(1)?,
    })
}

/// Analyzes session history and produces usage insights
/// (hermes `InsightsEngine`).
pub struct InsightsEngine {
    conn: Connection,
}

impl InsightsEngine {
    /// Open the store at `path` (a second WAL reader alongside the store).
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| AgentError::Tool(format!("insights: open {}: {}", path.display(), e)))?;
        Ok(Self { conn })
    }

    /// Open `$ULNCLAW_HOME/state.db`.
    pub fn open_default() -> Result<Self> {
        Self::open(&crate::config::ulnclaw_home().join("state.db"))
    }

    fn sessions_since(&self, cutoff: f64, source: Option<&str>) -> Result<Vec<SessionData>> {
        let mut rows = Vec::new();
        let (sql, source_filter): (String, bool) = match source {
            Some(_) => (
                "SELECT id, source, model, started_at, ended_at, title, message_count, \
                 tool_call_count, input_tokens, output_tokens FROM sessions \
                 WHERE started_at >= ?1 AND source = ?2 AND archived = 0 \
                 ORDER BY started_at DESC"
                    .to_string(),
                true,
            ),
            None => (
                "SELECT id, source, model, started_at, ended_at, title, message_count, \
                 tool_call_count, input_tokens, output_tokens FROM sessions \
                 WHERE started_at >= ?1 AND archived = 0 \
                 ORDER BY started_at DESC"
                    .to_string(),
                false,
            ),
        };
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| AgentError::Tool(format!("insights: prepare: {e}")))?;
        if source_filter {
            let mapped = stmt
                .query_map(params![cutoff, source.unwrap_or("")], row_to_session)
                .map_err(|e| AgentError::Tool(format!("insights: query: {e}")))?;
            for row in mapped {
                rows.push(row.map_err(|e| AgentError::Tool(format!("insights: row: {e}")))?);
            }
        } else {
            let mapped = stmt
                .query_map(params![cutoff], row_to_session)
                .map_err(|e| AgentError::Tool(format!("insights: query: {e}")))?;
            for row in mapped {
                rows.push(row.map_err(|e| AgentError::Tool(format!("insights: row: {e}")))?);
            }
        }
        Ok(rows)
    }

    fn tool_usage_since(&self, cutoff: f64, source: Option<&str>) -> Result<Vec<ToolUsage>> {
        // Tool-result messages carry the tool name; join to sessions for the
        // source filter.
        let (sql, filtered) = match source {
            Some(_) => (
                "SELECT m.tool_name, COUNT(*) FROM messages m \
                 JOIN sessions s ON s.id = m.session_id \
                 WHERE m.role = 'tool' AND m.tool_name IS NOT NULL AND m.tool_name != '' \
                 AND m.timestamp >= ?1 AND s.source = ?2 \
                 GROUP BY m.tool_name ORDER BY COUNT(*) DESC LIMIT 30"
                    .to_string(),
                true,
            ),
            None => (
                "SELECT tool_name, COUNT(*) FROM messages \
                 WHERE role = 'tool' AND tool_name IS NOT NULL AND tool_name != '' \
                 AND timestamp >= ?1 \
                 GROUP BY tool_name ORDER BY COUNT(*) DESC LIMIT 30"
                    .to_string(),
                false,
            ),
        };
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| AgentError::Tool(format!("insights: prepare: {e}")))?;
        let mut out = Vec::new();
        if filtered {
            let mapped = stmt
                .query_map(params![cutoff, source.unwrap_or("")], row_to_tool)
                .map_err(|e| AgentError::Tool(format!("insights: query: {e}")))?;
            for row in mapped {
                out.push(row.map_err(|e| AgentError::Tool(format!("insights: row: {e}")))?);
            }
        } else {
            let mapped = stmt
                .query_map(params![cutoff], row_to_tool)
                .map_err(|e| AgentError::Tool(format!("insights: query: {e}")))?;
            for row in mapped {
                out.push(row.map_err(|e| AgentError::Tool(format!("insights: row: {e}")))?);
            }
        }
        Ok(out)
    }

    fn skill_usage_since(&self, cutoff: f64, source: Option<&str>) -> Result<Vec<SkillUsage>> {
        // instr() prefilter so only assistant rows mentioning the skill tools
        // are loaded (hermes `_GET_SKILL_CALLS_*`).
        let (sql, filtered) = match source {
            Some(_) => (
                "SELECT m.tool_calls, m.timestamp FROM messages m \
                 JOIN sessions s ON s.id = m.session_id \
                 WHERE m.role = 'assistant' AND m.tool_calls IS NOT NULL \
                 AND m.timestamp >= ?1 AND s.source = ?2 \
                 AND (instr(m.tool_calls, 'skill_view') > 0 \
                      OR instr(m.tool_calls, 'skill_manage') > 0)"
                    .to_string(),
                true,
            ),
            None => (
                "SELECT tool_calls, timestamp FROM messages \
                 WHERE role = 'assistant' AND tool_calls IS NOT NULL \
                 AND timestamp >= ?1 \
                 AND (instr(tool_calls, 'skill_view') > 0 \
                      OR instr(tool_calls, 'skill_manage') > 0)"
                    .to_string(),
                false,
            ),
        };
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| AgentError::Tool(format!("insights: prepare: {e}")))?;
        let mut acc = std::collections::BTreeMap::<String, SkillUsage>::new();
        let mut rows: Vec<(String, f64)> = Vec::new();
        if filtered {
            let mapped = stmt
                .query_map(params![cutoff, source.unwrap_or("")], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                })
                .map_err(|e| AgentError::Tool(format!("insights: query: {e}")))?;
            for row in mapped {
                rows.push(row.map_err(|e| AgentError::Tool(format!("insights: row: {e}")))?);
            }
        } else {
            let mapped = stmt
                .query_map(params![cutoff], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                })
                .map_err(|e| AgentError::Tool(format!("insights: query: {e}")))?;
            for row in mapped {
                rows.push(row.map_err(|e| AgentError::Tool(format!("insights: row: {e}")))?);
            }
        }
        for (tool_calls, timestamp) in rows {
            accumulate_skill_calls(&tool_calls, timestamp, &mut acc);
        }
        Ok(acc.into_values().collect())
    }

    /// Tools + skills usage payload without a full report (hermes
    /// `get_usage_breakdown`).
    pub fn get_usage_breakdown(&self, days: u32, source: Option<&str>) -> Result<UsageBreakdown> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let cutoff = now - (days as f64) * 86400.0;
        let tools = self.tool_usage_since(cutoff, source)?;
        let skill_usage = self.skill_usage_since(cutoff, source)?;
        Ok(UsageBreakdown {
            tools,
            skills: compute_skill_breakdown(&skill_usage),
        })
    }

    /// Generate a complete insights report (hermes `generate`).
    ///
    /// `provider_hint` feeds models.dev cost lookup (session rows carry the
    /// model but not the provider).
    pub fn generate(
        &self,
        days: u32,
        source: Option<&str>,
        provider_hint: Option<&str>,
    ) -> Result<InsightsReport> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let cutoff = now - (days as f64) * 86400.0;

        let sessions = self.sessions_since(cutoff, source)?;
        if sessions.is_empty() {
            return Ok(InsightsReport {
                days,
                source_filter: source.map(String::from),
                empty: true,
                generated_at: now,
                overview: Overview::default(),
                models: Vec::new(),
                sources: Vec::new(),
                tools: Vec::new(),
                skills: SkillBreakdown::default(),
                activity: ActivityPatterns {
                    by_hour: vec![0; 24],
                    by_weekday: vec![0; 7],
                    peak_hour: None,
                    peak_weekday: None,
                },
                top_sessions: Vec::new(),
            });
        }

        let tools = self.tool_usage_since(cutoff, source)?;
        let skill_usage = self.skill_usage_since(cutoff, source)?;
        let overview = compute_overview(&sessions, provider_hint);
        let models = compute_model_breakdown(&sessions, provider_hint);
        let sources = compute_source_breakdown(&sessions);
        let activity = compute_activity_patterns(&sessions);
        let top_sessions = compute_top_sessions(&sessions, provider_hint);

        Ok(InsightsReport {
            days,
            source_filter: source.map(String::from),
            empty: false,
            generated_at: now,
            overview,
            models,
            sources,
            tools,
            skills: compute_skill_breakdown(&skill_usage),
            activity,
            top_sessions,
        })
    }
}

// =========================================================================
// Cost estimation (models.dev pricing)
// =========================================================================

/// USD cost for a model/token pair via models.dev pricing; `None` when the
/// model has no known pricing (hermes `_estimate_cost` semantics).
pub fn estimate_cost(
    provider_hint: Option<&str>,
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
) -> Option<f64> {
    // models.dev is keyed by provider — without one the lookup can never
    // match, so skip the registry fetch entirely (keeps tests off the
    // shared models.dev cache).
    let provider = provider_hint.unwrap_or("");
    if provider.trim().is_empty() || model.trim().is_empty() {
        return None;
    }
    let info = crate::models_dev::get_model_info(provider, model)?;
    if !info.has_cost_data() {
        return None;
    }
    Some(
        (input_tokens as f64) * info.cost_input / 1_000_000.0
            + (output_tokens as f64) * info.cost_output / 1_000_000.0,
    )
}

// =========================================================================
// Aggregations
// =========================================================================

fn compute_overview(sessions: &[SessionData], provider_hint: Option<&str>) -> Overview {
    let mut overview = Overview {
        total_sessions: sessions.len(),
        ..Default::default()
    };
    let mut durations: Vec<f64> = Vec::new();
    let mut days: std::collections::HashSet<String> = std::collections::HashSet::new();

    for session in sessions {
        overview.total_messages += session.message_count;
        overview.total_tool_calls += session.tool_call_count;
        overview.input_tokens += session.input_tokens;
        overview.output_tokens += session.output_tokens;
        if let Some(duration) = session.duration_seconds() {
            durations.push(duration);
        }
        if let Some(date) = timestamp_date(session.started_at) {
            days.insert(date);
        }
        if let Some(cost) = estimate_cost(
            provider_hint,
            &session.model,
            session.input_tokens,
            session.output_tokens,
        ) {
            overview.estimated_cost_usd += cost;
            overview.cost_known = true;
        }
    }
    overview.total_tokens = overview.input_tokens + overview.output_tokens;
    overview.active_days = days.len();
    if !durations.is_empty() {
        overview.avg_session_seconds = durations.iter().sum::<f64>() / durations.len() as f64;
    }
    overview
}

fn compute_model_breakdown(
    sessions: &[SessionData],
    provider_hint: Option<&str>,
) -> Vec<ModelUsage> {
    let mut by_model: std::collections::BTreeMap<String, ModelUsage> =
        std::collections::BTreeMap::new();
    for session in sessions {
        let key = if session.model.is_empty() {
            "(unknown)".to_string()
        } else {
            session.model.clone()
        };
        let entry = by_model.entry(key.clone()).or_insert_with(|| ModelUsage {
            model: key,
            sessions: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            estimated_cost_usd: 0.0,
            cost_known: false,
        });
        entry.sessions += 1;
        entry.input_tokens += session.input_tokens;
        entry.output_tokens += session.output_tokens;
        entry.total_tokens += session.total_tokens();
        if let Some(cost) = estimate_cost(
            provider_hint,
            &session.model,
            session.input_tokens,
            session.output_tokens,
        ) {
            entry.estimated_cost_usd += cost;
            entry.cost_known = true;
        }
    }
    let mut out: Vec<ModelUsage> = by_model.into_values().collect();
    out.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));
    out
}

fn compute_source_breakdown(sessions: &[SessionData]) -> Vec<SourceUsage> {
    let mut by_source: std::collections::BTreeMap<String, SourceUsage> =
        std::collections::BTreeMap::new();
    for session in sessions {
        let entry = by_source
            .entry(session.source.clone())
            .or_insert_with(|| SourceUsage {
                source: session.source.clone(),
                sessions: 0,
                total_tokens: 0,
                tool_calls: 0,
            });
        entry.sessions += 1;
        entry.total_tokens += session.total_tokens();
        entry.tool_calls += session.tool_call_count;
    }
    let mut out: Vec<SourceUsage> = by_source.into_values().collect();
    out.sort_by(|a, b| b.sessions.cmp(&a.sessions));
    out
}

fn timestamp_date(ts: f64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

fn compute_activity_patterns(sessions: &[SessionData]) -> ActivityPatterns {
    let mut by_hour = vec![0u64; 24];
    let mut by_weekday = vec![0u64; 7];
    for session in sessions {
        if let Some(dt) = Local.timestamp_opt(session.started_at as i64, 0).single() {
            by_hour[dt.hour() as usize] += 1;
            // chrono Weekday: Monday=0 … Sunday=6 via num_days_from_monday().
            by_weekday[dt.weekday().num_days_from_monday() as usize] += 1;
        }
    }
    let peak_hour = argmax(&by_hour);
    let peak_weekday = argmax(&by_weekday);
    ActivityPatterns {
        by_hour,
        by_weekday,
        peak_hour,
        peak_weekday,
    }
}

fn argmax(values: &[u64]) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for (i, value) in values.iter().enumerate() {
        if *value > 0 && best.map(|(_, b)| *value > b).unwrap_or(true) {
            best = Some((i, *value));
        }
    }
    best.map(|(i, _)| i)
}

fn compute_top_sessions(sessions: &[SessionData], provider_hint: Option<&str>) -> Vec<TopSession> {
    let mut ranked: Vec<&SessionData> = sessions.iter().collect();
    ranked.sort_by(|a, b| b.total_tokens().cmp(&a.total_tokens()));
    ranked
        .into_iter()
        .take(5)
        .map(|session| TopSession {
            id: session.id.clone(),
            title: session.title.clone(),
            model: session.model.clone(),
            started_at: session.started_at,
            messages: session.message_count,
            tool_calls: session.tool_call_count,
            total_tokens: session.total_tokens(),
            estimated_cost_usd: estimate_cost(
                provider_hint,
                &session.model,
                session.input_tokens,
                session.output_tokens,
            )
            .unwrap_or(0.0),
        })
        .collect()
}

// =========================================================================
// Terminal rendering (hermes format_terminal)
// =========================================================================

/// Simple horizontal bar chart strings (hermes `_bar_chart`).
pub fn bar_chart(values: &[u64], max_width: usize) -> Vec<String> {
    let peak = values.iter().copied().max().unwrap_or(1).max(1);
    values
        .iter()
        .map(|v| {
            if *v == 0 {
                String::new()
            } else {
                "█"
                    .repeat(((*v as f64 / peak as f64) * max_width as f64) as usize)
                    .max("█".to_string())
            }
        })
        .collect()
}

/// Compact duration: `3d 4h`, `2h 5m`, `45s` (hermes
/// `format_duration_compact`).
pub fn format_duration_compact(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Human token count (1234567 → "1.2M", 12345 → "12.3K").
pub fn format_tokens(tokens: i64) -> String {
    let value = tokens.max(0) as f64;
    if value >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        format!("{}", tokens)
    }
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Render the report for the terminal (hermes `format_terminal`).
/// Accumulate `skill_view`/`skill_manage` calls from one assistant
/// `tool_calls` JSON blob into `acc` (hermes `_get_skill_usage` per-row
/// loop).
pub fn accumulate_skill_calls(
    tool_calls_json: &str,
    timestamp: f64,
    acc: &mut std::collections::BTreeMap<String, SkillUsage>,
) {
    let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tool_calls_json) else {
        return;
    };
    for call in &calls {
        let Some(func) = call.get("function") else {
            continue;
        };
        let tool_name = func.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if tool_name != "skill_view" && tool_name != "skill_manage" {
            continue;
        }
        let args = match func.get("arguments") {
            Some(serde_json::Value::String(raw)) => {
                match serde_json::from_str::<serde_json::Value>(raw) {
                    Ok(value) => value,
                    Err(_) => continue,
                }
            }
            Some(value) if value.is_object() => value.clone(),
            _ => continue,
        };
        let skill_name = args
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::trim)
            .unwrap_or("");
        if skill_name.is_empty() {
            continue;
        }
        let entry = acc
            .entry(skill_name.to_string())
            .or_insert_with(|| SkillUsage {
                skill: skill_name.to_string(),
                ..Default::default()
            });
        if tool_name == "skill_view" {
            entry.view_count += 1;
        } else {
            entry.manage_count += 1;
        }
        if entry.last_used_at.map_or(true, |seen| timestamp > seen) {
            entry.last_used_at = Some(timestamp);
        }
    }
}

/// Process per-skill usage into summary + ranked list (hermes
/// `_compute_skill_breakdown`).
pub fn compute_skill_breakdown(usage: &[SkillUsage]) -> SkillBreakdown {
    let total_skill_loads: i64 = usage.iter().map(|s| s.view_count).sum();
    let total_skill_edits: i64 = usage.iter().map(|s| s.manage_count).sum();
    let total_skill_actions = total_skill_loads + total_skill_edits;

    let mut top_skills: Vec<TopSkill> = usage
        .iter()
        .map(|s| {
            let total_count = s.view_count + s.manage_count;
            let percentage = if total_skill_actions > 0 {
                total_count as f64 / total_skill_actions as f64 * 100.0
            } else {
                0.0
            };
            TopSkill {
                skill: s.skill.clone(),
                view_count: s.view_count,
                manage_count: s.manage_count,
                total_count,
                percentage,
                last_used_at: s.last_used_at,
            }
        })
        .collect();

    top_skills.sort_by(|a, b| {
        b.total_count
            .cmp(&a.total_count)
            .then(b.view_count.cmp(&a.view_count))
            .then(b.manage_count.cmp(&a.manage_count))
            .then(
                b.last_used_at
                    .unwrap_or(0.0)
                    .partial_cmp(&a.last_used_at.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(b.skill.cmp(&a.skill))
    });

    SkillBreakdown {
        summary: SkillSummary {
            total_skill_loads,
            total_skill_edits,
            total_skill_actions,
            distinct_skills_used: usage.len(),
        },
        top_skills,
    }
}

/// Group an integer with thousands separators (`1,234,567`) for the
/// gateway/terminal renderings.
pub fn format_thousands(value: i64) -> String {
    let text = value.to_string();
    let negative = text.starts_with('-');
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut grouped_rev = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            grouped_rev.push(',');
        }
        grouped_rev.push(ch);
    }
    let grouped: String = grouped_rev.chars().rev().collect();
    if negative {
        format!("-{grouped}")
    } else {
        grouped
    }
}

fn format_last_used(timestamp: f64) -> Option<String> {
    chrono::DateTime::from_timestamp(timestamp as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%b %d").to_string())
}

/// Compact markdown rendering for gateway/messaging replies (hermes
/// `format_gateway`).
pub fn format_gateway(report: &InsightsReport) -> String {
    if report.empty {
        return format!("No sessions found in the last {} days.", report.days);
    }
    let o = &report.overview;
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "📊 **ulnclaw Insights** — Last {} days",
        report.days
    ));
    lines.push(String::new());
    lines.push(format!(
        "**Sessions:** {} | **Messages:** {} | **Tool calls:** {}",
        o.total_sessions,
        format_thousands(o.total_messages),
        format_thousands(o.total_tool_calls)
    ));
    lines.push(format!(
        "**Tokens:** {} (in: {} / out: {})",
        format_thousands(o.total_tokens),
        format_thousands(o.input_tokens),
        format_thousands(o.output_tokens)
    ));
    if o.avg_session_seconds > 0.0 {
        lines.push(format!(
            "**Avg session:** ~{}",
            format_duration_compact(o.avg_session_seconds)
        ));
    }
    lines.push(String::new());

    if !report.models.is_empty() {
        lines.push("**🤖 Models:**".to_string());
        for m in report.models.iter().take(5) {
            lines.push(format!(
                "  {} — {} sessions, {} tokens",
                truncate_str(&m.model, 25),
                m.sessions,
                format_thousands(m.total_tokens)
            ));
        }
        lines.push(String::new());
    }

    if report.sources.len() > 1 {
        lines.push("**📱 Platforms:**".to_string());
        for s in &report.sources {
            lines.push(format!(
                "  {} — {} sessions, {} tokens",
                s.source,
                s.sessions,
                format_thousands(s.total_tokens)
            ));
        }
        lines.push(String::new());
    }

    if !report.tools.is_empty() {
        let total_calls: i64 = report.tools.iter().map(|t| t.calls).sum();
        lines.push("**🔧 Top Tools:**".to_string());
        for t in report.tools.iter().take(8) {
            let percentage = if total_calls > 0 {
                t.calls as f64 / total_calls as f64 * 100.0
            } else {
                0.0
            };
            lines.push(format!(
                "  {} — {} calls ({:.1}%)",
                t.tool,
                format_thousands(t.calls),
                percentage
            ));
        }
        lines.push(String::new());
    }

    if !report.skills.top_skills.is_empty() {
        lines.push("**🧠 Top Skills:**".to_string());
        for skill in report.skills.top_skills.iter().take(5) {
            let suffix = skill
                .last_used_at
                .and_then(format_last_used)
                .map(|date| format!(", last used {date}"))
                .unwrap_or_default();
            lines.push(format!(
                "  {} — {} loads, {} edits{}",
                skill.skill,
                format_thousands(skill.view_count),
                format_thousands(skill.manage_count),
                suffix
            ));
        }
        lines.push(String::new());
    }

    const WEEKDAYS: &[&str] = &[
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    let activity = &report.activity;
    if let (Some(day_idx), Some(hour_idx)) = (activity.peak_weekday, activity.peak_hour) {
        if let (Some(day), Some(&hour_count), Some(&day_count)) = (
            WEEKDAYS.get(day_idx),
            activity.by_hour.get(hour_idx),
            activity.by_weekday.get(day_idx),
        ) {
            let (ampm, mut display_hr) = if hour_idx < 12 {
                ("AM", hour_idx % 12)
            } else {
                ("PM", hour_idx % 12)
            };
            if display_hr == 0 {
                display_hr = 12;
            }
            lines.push(format!(
                "**📅 Busiest:** {}s ({} sessions), {}{} ({} sessions)",
                day, day_count, display_hr, ampm, hour_count
            ));
        }
    }
    if o.active_days > 0 {
        lines.push(format!("**Active days:** {}", o.active_days));
    }

    let mut text = lines.join("\n");
    while text.ends_with('\n') {
        text.pop();
    }
    text
}

pub fn format_terminal(report: &InsightsReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n📊 ulnclaw Insights — last {} day{}\n",
        report.days,
        if report.days == 1 { "" } else { "s" }
    ));
    if let Some(source) = &report.source_filter {
        out.push_str(&format!("   source filter: {source}\n"));
    }
    out.push_str(&format!("{}\n", "─".repeat(50)));

    if report.empty {
        out.push_str("\nNo sessions recorded in this period.\n\n");
        return out;
    }

    let overview = &report.overview;
    out.push_str("\nOverview\n");
    out.push_str(&format!(
        "  Sessions: {} · Messages: {} · Tool calls: {}\n",
        overview.total_sessions, overview.total_messages, overview.total_tool_calls
    ));
    out.push_str(&format!(
        "  Tokens: {} in · {} out · {} total\n",
        format_tokens(overview.input_tokens),
        format_tokens(overview.output_tokens),
        format_tokens(overview.total_tokens)
    ));
    if overview.cost_known {
        out.push_str(&format!(
            "  Est. cost: ${:.4}\n",
            overview.estimated_cost_usd
        ));
    } else {
        out.push_str("  Est. cost: unknown (no models.dev pricing matched)\n");
    }
    out.push_str(&format!(
        "  Avg session: {} · Active days: {}\n",
        format_duration_compact(overview.avg_session_seconds),
        overview.active_days
    ));

    if !report.models.is_empty() {
        out.push_str("\nModels\n");
        let totals: Vec<u64> = report
            .models
            .iter()
            .map(|m| m.total_tokens as u64)
            .collect();
        let bars = bar_chart(&totals, 20);
        for (model, bar) in report.models.iter().zip(bars.iter()).take(8) {
            let cost = if model.cost_known {
                format!("  ${:.4}", model.estimated_cost_usd)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "  {:<28} {:>4} sessions  {:>7} tokens  {}{}\n",
                truncate_str(&model.model, 28),
                model.sessions,
                format_tokens(model.total_tokens),
                bar,
                cost
            ));
        }
    }

    if !report.sources.is_empty() {
        out.push_str("\nSources\n");
        for source in &report.sources {
            out.push_str(&format!(
                "  {:<12} {:>4} sessions  {:>7} tokens  {:>5} tool calls\n",
                truncate_str(&source.source, 12),
                source.sessions,
                format_tokens(source.total_tokens),
                source.tool_calls
            ));
        }
    }

    if !report.tools.is_empty() {
        out.push_str("\nTools\n");
        let totals: Vec<u64> = report.tools.iter().map(|t| t.calls as u64).collect();
        let bars = bar_chart(&totals, 20);
        for (tool, bar) in report.tools.iter().zip(bars.iter()).take(10) {
            out.push_str(&format!(
                "  {:<28} {:>5}  {}\n",
                truncate_str(&tool.tool, 28),
                tool.calls,
                bar
            ));
        }
    }

    if !report.skills.top_skills.is_empty() {
        out.push_str("\nSkills\n");
        out.push_str(&format!(
            "  {:<28} {:>7} {:>7} {:>11}\n",
            "Skill", "Loads", "Edits", "Last used"
        ));
        for skill in report.skills.top_skills.iter().take(10) {
            let last_used = skill
                .last_used_at
                .and_then(format_last_used)
                .unwrap_or_else(|| "—".to_string());
            out.push_str(&format!(
                "  {:<28} {:>7} {:>7} {:>11}\n",
                truncate_str(&skill.skill, 28),
                skill.view_count,
                skill.manage_count,
                last_used
            ));
        }
        let summary = &report.skills.summary;
        out.push_str(&format!(
            "  Distinct skills: {} · Loads: {} · Edits: {}\n",
            summary.distinct_skills_used, summary.total_skill_loads, summary.total_skill_edits
        ));
    }

    let activity = &report.activity;
    let any_activity = activity.by_hour.iter().any(|v| *v > 0);
    if any_activity {
        out.push_str("\nActivity\n");
        if let Some(peak) = activity.peak_hour {
            out.push_str(&format!("  Peak hour: {:02}:00\n", peak));
        }
        if let Some(peak) = activity.peak_weekday {
            out.push_str(&format!(
                "  Peak weekday: {}\n",
                WEEKDAYS.get(peak).copied().unwrap_or("?")
            ));
        }
        let hour_bars = bar_chart(&activity.by_hour, 16);
        for (hour, count) in activity.by_hour.iter().enumerate() {
            if *count > 0 {
                out.push_str(&format!(
                    "  {:02}:00  {:>3}  {}\n",
                    hour, count, hour_bars[hour]
                ));
            }
        }
        let weekday_bars = bar_chart(&activity.by_weekday, 10);
        for (i, day) in WEEKDAYS.iter().enumerate() {
            if activity.by_weekday[i] > 0 {
                out.push_str(&format!(
                    "  {} {:>3} {}\n",
                    day, activity.by_weekday[i], weekday_bars[i]
                ));
            }
        }
    }

    if !report.top_sessions.is_empty() {
        out.push_str("\nTop sessions (by tokens)\n");
        for session in &report.top_sessions {
            let label = session
                .title
                .clone()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| session.id.chars().take(8).collect());
            let date = chrono::DateTime::from_timestamp(session.started_at as i64, 0)
                .map(|dt| dt.with_timezone(&Local).format("%m-%d %H:%M").to_string())
                .unwrap_or_default();
            out.push_str(&format!(
                "  {}  {:<20}  {:>4} msgs  {:>7} tokens  {}\n",
                date,
                truncate_str(&label, 20),
                session.messages,
                format_tokens(session.total_tokens),
                truncate_str(&session.model, 18)
            ));
        }
    }
    out.push('\n');
    out
}

fn truncate_str(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", head)
    }
}

/// Default state.db path helper for CLI wiring.
pub fn default_store_path() -> PathBuf {
    crate::config::ulnclaw_home().join("state.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionStore, SqliteSessionStore};

    fn seeded_store(dir: &Path) -> SqliteSessionStore {
        let store = SqliteSessionStore::open(dir.join("state.db")).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        for i in 0..3 {
            let id = store
                .create_session("cli", Some("test-model"), None)
                .unwrap();
            store
                .update_usage(&id, 100 * (i + 1), 50 * (i + 1), i as u32)
                .unwrap();
            store.end_session(&id, "completed").unwrap();
        }
        // An archived session that must be excluded.
        let archived = store
            .create_session("cli", Some("test-model"), None)
            .unwrap();
        store.set_session_archived(&archived, true).unwrap();
        let _ = now;
        store
    }

    #[test]
    fn generate_aggregates_sessions_and_excludes_archived() {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded_store(dir.path());
        drop(store);
        let engine = InsightsEngine::open(&dir.path().join("state.db")).unwrap();
        let report = engine.generate(30, None, None).unwrap();
        assert!(!report.empty);
        assert_eq!(report.overview.total_sessions, 3);
        assert_eq!(report.overview.input_tokens, 600);
        assert_eq!(report.overview.output_tokens, 300);
        assert_eq!(report.overview.total_tokens, 900);
        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].model, "test-model");
        assert_eq!(report.sources[0].source, "cli");
    }

    #[test]
    fn empty_window_reports_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("state.db")).unwrap();
        drop(store);
        let engine = InsightsEngine::open(&dir.path().join("state.db")).unwrap();
        let report = engine.generate(30, None, None).unwrap();
        assert!(report.empty);
        let text = format_terminal(&report);
        assert!(text.contains("No sessions recorded"));
    }

    #[test]
    fn source_filter_restricts() {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded_store(dir.path());
        let gateway_session = store
            .create_session("gateway", Some("test-model"), None)
            .unwrap();
        store.update_usage(&gateway_session, 10, 10, 0).unwrap();
        drop(store);
        let engine = InsightsEngine::open(&dir.path().join("state.db")).unwrap();
        let report = engine.generate(30, Some("gateway"), None).unwrap();
        assert_eq!(report.overview.total_sessions, 1);
        assert_eq!(report.source_filter.as_deref(), Some("gateway"));
    }

    #[test]
    fn bar_chart_scales_to_peak() {
        let bars = bar_chart(&[0, 5, 10], 10);
        assert_eq!(bars[0], "");
        assert_eq!(bars[2].chars().count(), 10);
        assert!(bars[1].chars().count() >= 1 && bars[1].chars().count() <= 10);
    }

    #[test]
    fn duration_and_token_formats() {
        assert_eq!(format_duration_compact(45.0), "45s");
        assert_eq!(format_duration_compact(125.0), "2m 5s");
        assert_eq!(format_duration_compact(3725.0), "1h 2m");
        assert_eq!(format_duration_compact(90061.0), "1d 1h");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(1_234_567), "1.2M");
    }

    #[test]
    fn truncate_keeps_width() {
        assert_eq!(truncate_str("short", 10), "short");
        let long = truncate_str("a very long session title here", 10);
        assert_eq!(long.chars().count(), 10);
        assert!(long.ends_with('…'));
    }

    fn skill_call_json(entries: &[(&str, &str, &str)]) -> String {
        let calls: Vec<serde_json::Value> = entries
            .iter()
            .map(|(id, tool, args)| {
                serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": tool, "arguments": args}
                })
            })
            .collect();
        serde_json::to_string(&calls).unwrap()
    }

    #[test]
    fn accumulate_skill_calls_counts_views_and_edits() {
        let mut acc = std::collections::BTreeMap::new();
        let json = skill_call_json(&[
            ("c1", "skill_view", r#"{"name": "alpha"}"#),
            (
                "c2",
                "skill_manage",
                r#"{"action": "patch", "name": "alpha"}"#,
            ),
            ("c3", "skill_view", r#"{"name": "beta"}"#),
            ("c4", "read_file", r#"{"path": "x"}"#),
        ]);
        accumulate_skill_calls(&json, 100.0, &mut acc);
        accumulate_skill_calls(&json, 200.0, &mut acc);
        let alpha = &acc["alpha"];
        assert_eq!(alpha.view_count, 2);
        assert_eq!(alpha.manage_count, 2);
        assert_eq!(alpha.last_used_at, Some(200.0));
        assert_eq!(acc["beta"].view_count, 2);
        assert_eq!(acc.len(), 2);
    }

    #[test]
    fn accumulate_skill_calls_rejects_bad_input() {
        let mut acc = std::collections::BTreeMap::new();
        accumulate_skill_calls("not json", 1.0, &mut acc);
        let blank = skill_call_json(&[
            ("c1", "skill_view", r#"{"name": "   "}"#),
            ("c2", "skill_view", "not-json-args"),
            ("c3", "skill_manage", r#"{"action": "create"}"#),
        ]);
        accumulate_skill_calls(&blank, 1.0, &mut acc);
        assert!(acc.is_empty());
    }

    #[test]
    fn skill_breakdown_ranks_and_percentages() {
        let usage = vec![
            SkillUsage {
                skill: "alpha".into(),
                view_count: 3,
                manage_count: 1,
                last_used_at: Some(10.0),
            },
            SkillUsage {
                skill: "beta".into(),
                view_count: 2,
                manage_count: 2,
                last_used_at: Some(20.0),
            },
            SkillUsage {
                skill: "gamma".into(),
                view_count: 0,
                manage_count: 1,
                last_used_at: None,
            },
        ];
        let breakdown = compute_skill_breakdown(&usage);
        assert_eq!(breakdown.summary.total_skill_loads, 5);
        assert_eq!(breakdown.summary.total_skill_edits, 4);
        assert_eq!(breakdown.summary.total_skill_actions, 9);
        assert_eq!(breakdown.summary.distinct_skills_used, 3);
        let order: Vec<&str> = breakdown
            .top_skills
            .iter()
            .map(|s| s.skill.as_str())
            .collect();
        // Ties on total break toward higher view_count (alpha before beta).
        assert_eq!(order, vec!["alpha", "beta", "gamma"]);
        let alpha = &breakdown.top_skills[0];
        assert_eq!(alpha.total_count, 4);
        assert!((alpha.percentage - 44.4444).abs() < 0.01);
        assert!(breakdown.top_skills[2].last_used_at.is_none());
    }

    #[test]
    fn thousands_grouping() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(12_345), "12,345");
        assert_eq!(format_thousands(1_234_567), "1,234,567");
        assert_eq!(format_thousands(-1234), "-1,234");
    }

    #[test]
    fn generate_includes_skill_breakdown_from_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded_store(dir.path());
        let session = store.list_sessions(10).unwrap()[0].id.clone();
        let message = crate::provider::Message {
            role: crate::provider::Role::Assistant,
            content: None,
            tool_calls: Some(vec![
                crate::provider::ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: crate::provider::FunctionCall {
                        name: "skill_view".into(),
                        arguments: r#"{"name": "deploy"}"#.into(),
                    },
                },
                crate::provider::ToolCall {
                    id: "c2".into(),
                    call_type: "function".into(),
                    function: crate::provider::FunctionCall {
                        name: "skill_manage".into(),
                        arguments: r#"{"action": "patch", "name": "deploy"}"#.into(),
                    },
                },
            ]),
            tool_call_id: None,
            name: None,
        };
        store.append_message(&session, &message).unwrap();
        drop(store);
        let engine = InsightsEngine::open(&dir.path().join("state.db")).unwrap();
        let report = engine.generate(30, None, None).unwrap();
        assert_eq!(report.skills.summary.total_skill_loads, 1);
        assert_eq!(report.skills.summary.total_skill_edits, 1);
        assert_eq!(report.skills.summary.distinct_skills_used, 1);
        assert_eq!(report.skills.top_skills[0].skill, "deploy");
        assert!(report.skills.top_skills[0].last_used_at.is_some());
        let terminal = format_terminal(&report);
        assert!(terminal.contains("Skills"));
        assert!(terminal.contains("deploy"));
        let gateway = format_gateway(&report);
        assert!(gateway.contains("**🧠 Top Skills:**"));
        assert!(gateway.contains("deploy — 1 loads, 1 edits"));
        let breakdown = engine.get_usage_breakdown(30, None).unwrap();
        assert_eq!(breakdown.skills.summary.total_skill_actions, 2);
    }

    #[test]
    fn format_gateway_empty_window() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("state.db")).unwrap();
        drop(store);
        let engine = InsightsEngine::open(&dir.path().join("state.db")).unwrap();
        let report = engine.generate(7, None, None).unwrap();
        assert_eq!(
            format_gateway(&report),
            "No sessions found in the last 7 days."
        );
    }

    #[test]
    fn activity_patterns_bucket_starts() {
        let dir = tempfile::tempdir().unwrap();
        let store = seeded_store(dir.path());
        drop(store);
        let engine = InsightsEngine::open(&dir.path().join("state.db")).unwrap();
        let report = engine.generate(30, None, None).unwrap();
        assert_eq!(report.activity.by_hour.len(), 24);
        assert_eq!(report.activity.by_weekday.len(), 7);
        let total: u64 = report.activity.by_hour.iter().sum();
        assert_eq!(total, 3);
    }
}

//! Gateway monitoring — port of hermes `agent/monitoring/*`.
//!
//! Service health monitoring plus redacted operational diagnostics for the
//! gateway daemon, exported over OTLP/HTTP to an operator-configured
//! endpoint. Content-free by construction: no prompts, messages, tool
//! args/results, session history, or usage analytics ever leave the process
//! — rendered log messages are never exported, and bounded structured
//! strings are scrubbed anyway (defense in depth).
//!
//! Architecture mirrors hermes:
//!   - [`emit`] hands typed events to a fire-and-forget bounded queue (the
//!     emitter never blocks or raises into gateway code — the hot-path
//!     invariant); nothing is persisted locally.
//!   - the OTLP streamer consumes events off the hot path and maps them to
//!     OTel spans over OTLP/HTTP JSON (ulnclaw's exporter is built in —
//!     hermes needs the optional `hermes-agent[otlp]` SDK extra).
//!   - [`redact_for_export`] is the one unconditional egress scrub: secrets
//!     first (fail-closed), then PII. There is deliberately no setting to
//!     weaken it.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Config (`[monitoring]` in config.toml — hermes `monitoring.*`)
// ---------------------------------------------------------------------------

/// `[monitoring.export.otlp]` — OTLP/HTTP destination.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OtlpExportConfig {
    /// Master switch for OTLP export.
    pub enabled: Option<crate::config::Truthiness>,
    /// Collector endpoint, e.g. `http://localhost:4318` (the `/v1/traces`
    /// signal path is appended when absent).
    pub endpoint: Option<String>,
    /// Header name -> environment variable NAME holding the value. Values
    /// are read from the environment at export time, never logged/stored.
    pub headers_env: std::collections::HashMap<String, String>,
}

/// `[monitoring.export]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExportConfig {
    pub otlp: OtlpExportConfig,
}

/// `[monitoring.gateway_health_export]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GatewayHealthExportConfig {
    /// Master switch for the gateway-health export plane.
    pub enabled: Option<crate::config::Truthiness>,
    /// Periodic health metrics (default on).
    pub metrics_enabled: Option<crate::config::Truthiness>,
    /// Structured diagnostic events (default on).
    pub diagnostic_events_enabled: Option<crate::config::Truthiness>,
    /// Warning/error log-derived events (default on).
    pub warning_error_events_enabled: Option<crate::config::Truthiness>,
    /// Metrics export cadence in seconds (default 60, floor 5).
    pub export_interval_seconds: Option<u64>,
    /// Diagnostic/log export cadence in seconds (default 5, floor 1).
    pub logs_export_interval_seconds: Option<u64>,
}

/// `[monitoring]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MonitoringConfig {
    /// Stable, resettable pseudonymous install id (`service.instance.id`).
    /// Minted + persisted on first use; clear to rotate.
    pub install_id: Option<String>,
    pub gateway_health_export: GatewayHealthExportConfig,
    pub export: ExportConfig,
}

impl MonitoringConfig {
    pub fn enabled(&self) -> bool {
        self.gateway_health_export
            .enabled
            .as_ref()
            .map(|t| t.resolve(false))
            .unwrap_or(false)
    }

    pub fn metrics_enabled(&self) -> bool {
        self.gateway_health_export
            .metrics_enabled
            .as_ref()
            .map(|t| t.resolve(true))
            .unwrap_or(true)
    }

    pub fn diagnostic_events_enabled(&self) -> bool {
        self.gateway_health_export
            .diagnostic_events_enabled
            .as_ref()
            .map(|t| t.resolve(true))
            .unwrap_or(true)
    }

    pub fn warning_error_events_enabled(&self) -> bool {
        self.gateway_health_export
            .warning_error_events_enabled
            .as_ref()
            .map(|t| t.resolve(true))
            .unwrap_or(true)
    }

    pub fn export_interval_seconds(&self) -> u64 {
        self.gateway_health_export
            .export_interval_seconds
            .unwrap_or(60)
            .max(5)
    }

    pub fn logs_export_interval_seconds(&self) -> u64 {
        self.gateway_health_export
            .logs_export_interval_seconds
            .unwrap_or(5)
            .max(1)
    }

    pub fn otlp_enabled(&self) -> bool {
        self.export
            .otlp
            .enabled
            .as_ref()
            .map(|t| t.resolve(false))
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Install identity (hermes agent/monitoring/policy.py)
// ---------------------------------------------------------------------------

/// Return a stable install id, minting and persisting one when empty
/// (hermes `ensure_install_id`). The id survives gateway restarts (it
/// becomes `service.instance.id` on exported signals). Persisting is
/// fail-open: if the write fails, the ephemeral id is still returned.
/// Clearing `monitoring.install_id` rotates the id on the next start.
pub fn ensure_install_id(current: Option<&str>) -> String {
    if let Some(existing) = current.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return existing.to_string();
    }
    let minted = uuid::Uuid::new_v4().to_string();
    let path = crate::config_cmd::config_path();
    let persisted = (|| -> Result<(), String> {
        let mut value = crate::config_cmd::load_toml(&path)?;
        if crate::config_cmd::get_nested(&value, "monitoring.install_id")
            .and_then(toml::Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok(());
        }
        crate::config_cmd::set_nested(
            &mut value,
            "monitoring.install_id",
            toml::Value::String(minted.clone()),
        )?;
        crate::config_cmd::save_toml(&path, &value)
    })()
    .is_ok();
    if !persisted {
        tracing::debug!("install_id persist failed; using ephemeral id");
    }
    minted
}

// ---------------------------------------------------------------------------
// Redaction (hermes agent/monitoring/redaction.py)
// ---------------------------------------------------------------------------

fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bBearer\s+[A-Za-z0-9._~+\-/]+=*").expect("regex"))
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b(xox[baprs]-[A-Za-z0-9-]+|sk-[A-Za-z0-9_-]{8,}|gh[pousr]_[A-Za-z0-9_]{8,})\b",
        )
        .expect("regex")
    })
}

fn secret_literal_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*{3,}").expect("regex"))
}

fn bearer_residue_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bBearer\s+\[[^\]]+\]").expect("regex"))
}

fn email_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").expect("regex")
    })
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        )
        .expect("regex")
    })
}

/// E.164-ish phone core without boundaries. Hermes uses `(?<!\w)…(?!\w)`
/// lookarounds, which the `regex` crate cannot express; the surrounding
/// word-boundary checks are applied manually in [`scrub_phones`].
fn phone_core_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:\+?\d{1,3}[\s.\-]?)?(?:\(\d{2,4}\)[\s.\-]?)?\d{3}[\s.\-]?\d{3,4}(?:[\s.\-]?\d{2,4})?")
            .expect("regex")
    })
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Replace phone-shaped spans with `[phone]`, honoring hermes'
/// `(?<!\w)…(?!\w)` guards via explicit neighbor checks. On a rejected
/// candidate the scan retries one char later (Python `re.sub` semantics).
fn scrub_phones(text: &str) -> String {
    let re = phone_core_re();
    let mut out = String::with_capacity(text.len());
    let mut last_emit = 0usize;
    let mut search_start = 0usize;
    while let Some(m) = re.find_at(text, search_start) {
        let (start, end) = (m.start(), m.end());
        if start < last_emit {
            search_start = next_char_boundary(text, start + 1);
            continue;
        }
        let prev_ok = text[..start]
            .chars()
            .next_back()
            .map_or(true, |c| !is_word_char(c));
        let next_ok = text[end..]
            .chars()
            .next()
            .map_or(true, |c| !is_word_char(c));
        if prev_ok && next_ok {
            out.push_str(&text[last_emit..start]);
            out.push_str("[phone]");
            last_emit = end;
            search_start = end;
        } else {
            search_start = next_char_boundary(text, start + 1);
        }
    }
    out.push_str(&text[last_emit..]);
    out
}

fn next_char_boundary(text: &str, mut pos: usize) -> usize {
    while pos < text.len() && !text.is_char_boundary(pos) {
        pos += 1;
    }
    pos
}

/// Scrub a string for egress: secrets, then PII. Unconditional — one scrub,
/// no modes, no knobs (hermes `redact_for_export`).
pub fn redact_for_export(text: Option<&str>) -> Option<String> {
    let text = text?;
    // Secrets first — the shared redactor plus belt-and-suspenders shapes.
    let mut out = crate::redact::redact_sensitive_text(text, crate::redact::RedactOpts::default());
    out = bearer_re().replace_all(&out, "[redacted]").into_owned();
    out = token_re().replace_all(&out, "[redacted]").into_owned();
    out = secret_literal_re()
        .replace_all(&out, "[redacted]")
        .into_owned();
    out = bearer_residue_re()
        .replace_all(&out, "[redacted]")
        .into_owned();
    // PII second.
    out = email_re().replace_all(&out, "[email]").into_owned();
    out = uuid_re().replace_all(&out, "[id]").into_owned();
    out = scrub_phones(&out);
    Some(out)
}

// ---------------------------------------------------------------------------
// Events (hermes agent/monitoring/events.py)
// ---------------------------------------------------------------------------

/// Current wall clock as nanoseconds since the Unix epoch.
pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Content-free gateway health snapshot / lifecycle event (hermes
/// `GatewayHealthEvent`).
#[derive(Debug, Clone, Serialize)]
pub struct GatewayHealthEvent {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_reason: Option<String>,
    pub active_agents: u64,
    pub gateway_busy: bool,
    pub platform_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub ts_ns: u64,
}

impl GatewayHealthEvent {
    pub fn to_value(&self) -> Value {
        let mut map = serde_json::to_value(self).expect("serializes");
        map.as_object_mut()
            .unwrap()
            .insert("event".to_string(), json!("gateway_health"));
        map
    }
}

/// Redacted gateway diagnostic event (hermes `GatewayDiagnosticEvent`).
#[derive(Debug, Clone, Serialize)]
pub struct GatewayDiagnosticEvent {
    pub name: String,
    pub subsystem: String,
    #[serde(default)]
    pub error_class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub ts_ns: u64,
}

impl GatewayDiagnosticEvent {
    pub fn to_value(&self) -> Value {
        let mut map = serde_json::to_value(self).expect("serializes");
        map.as_object_mut()
            .unwrap()
            .insert("event".to_string(), json!("gateway_diagnostic"));
        map
    }
}

/// Content-free cron execution lifecycle projection (hermes
/// `CronExecutionEvent`).
#[derive(Debug, Clone, Serialize)]
pub struct CronExecutionEvent {
    pub status: String,
    pub job_key: String,
    #[serde(default)]
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    pub ts_ns: u64,
}

impl CronExecutionEvent {
    pub fn to_value(&self) -> Value {
        let mut map = serde_json::to_value(self).expect("serializes");
        map.as_object_mut()
            .unwrap()
            .insert("event".to_string(), json!("cron_execution"));
        map
    }
}

// ---------------------------------------------------------------------------
// Emitter (hermes agent/monitoring/emitter.py)
// ---------------------------------------------------------------------------

const EMITTER_CAPACITY: usize = 1024;

fn emitter_channel() -> &'static (
    tokio::sync::mpsc::Sender<Value>,
    std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<Value>>>,
) {
    static SLOT: OnceLock<(
        tokio::sync::mpsc::Sender<Value>,
        std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<Value>>>,
    )> = OnceLock::new();
    SLOT.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(EMITTER_CAPACITY);
        (tx, std::sync::Mutex::new(Some(rx)))
    })
}

/// Fire-and-forget event submission (hermes `emit`). Never blocks, never
/// raises into caller code: a full queue drops the event (monitoring is an
/// egress path, not a store).
pub fn emit(event: Value) {
    let (tx, _) = emitter_channel();
    let _ = tx.try_send(event);
}

/// Take the emitter's receiver (single-consumer). Returns `None` when a
/// streamer already owns it.
pub fn take_receiver() -> Option<tokio::sync::mpsc::Receiver<Value>> {
    let (_, rx_slot) = emitter_channel();
    rx_slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Queue depth (test/status introspection).
pub fn queue_len_estimate() -> usize {
    let (tx, _) = emitter_channel();
    EMITTER_CAPACITY - tx.capacity()
}

// ---------------------------------------------------------------------------
// OTLP/HTTP export (hermes agent/monitoring/otlp_exporter.py — ulnclaw
// speaks OTLP/HTTP JSON natively; hermes needs the optional OTel SDK extra)
// ---------------------------------------------------------------------------

/// Attribute whitelist per event kind (hermes `_span_attrs` keep_by_kind).
const GATEWAY_HEALTH_COLS: &[&str] = &[
    "name",
    "gateway_state",
    "old_state",
    "new_state",
    "exit_reason",
    "active_agents",
    "gateway_busy",
    "platform_count",
    "version",
    "pid",
];
const GATEWAY_DIAGNOSTIC_COLS: &[&str] = &[
    "name",
    "subsystem",
    "error_class",
    "error_code",
    "platform",
    "old_state",
    "new_state",
    "version",
    "severity",
];
const CRON_EXECUTION_COLS: &[&str] = &[
    "status",
    "job_key",
    "source",
    "duration_ms",
    "delivery_outcome",
    "error_class",
];

fn otlp_value(value: &Value) -> Value {
    match value {
        Value::Bool(b) => json!({"boolValue": b}),
        Value::Number(n) if n.is_i64() || n.is_u64() => {
            json!({"intValue": n.as_i64().unwrap_or_default().to_string()})
        }
        Value::Number(n) => json!({"doubleValue": n.as_f64().unwrap_or(0.0)}),
        other => {
            let text = match other {
                Value::String(s) => s.clone(),
                _ => other.to_string(),
            };
            let redacted = redact_for_export(Some(&text))
                .unwrap_or_else(|| "[redaction-unavailable]".to_string());
            let clamped: String = redacted.chars().take(500).collect();
            json!({"stringValue": clamped})
        }
    }
}

/// Span attributes for one monitoring event (content-free by construction;
/// strings pass the egress redactor + 500-char clamp — hermes `_span_attrs`).
pub fn span_attrs(event: &Value) -> Vec<Value> {
    let kind = event
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut attrs = vec![json!({
        "key": "ulnclaw.event",
        "value": {"stringValue": kind},
    })];
    let cols: &[&str] = match kind {
        "gateway_health" => GATEWAY_HEALTH_COLS,
        "gateway_diagnostic" => GATEWAY_DIAGNOSTIC_COLS,
        "cron_execution" => CRON_EXECUTION_COLS,
        _ => &[],
    };
    for col in cols {
        if let Some(value) = event.get(col) {
            if value.is_null() {
                continue;
            }
            attrs.push(json!({"key": format!("ulnclaw.{col}"), "value": otlp_value(value)}));
        }
    }
    attrs
}

fn random_hex(bytes: usize) -> String {
    let mut hex = String::with_capacity(bytes * 2);
    while hex.len() < bytes * 2 {
        hex.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    hex.truncate(bytes * 2);
    hex
}

/// Map a batch of events onto the OTLP/HTTP JSON traces envelope (hermes
/// `export_batch`: one span per event under the monitoring scope).
pub fn build_otlp_traces_payload(
    events: &[Value],
    resource_attrs: &BTreeMap<String, String>,
) -> Value {
    let resource_attributes: Vec<Value> = resource_attrs
        .iter()
        .map(|(k, v)| json!({"key": k, "value": {"stringValue": v}}))
        .collect();
    let spans: Vec<Value> = events
        .iter()
        .map(|event| {
            let kind = event
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or("event");
            let ts = event
                .get("ts_ns")
                .and_then(Value::as_u64)
                .unwrap_or_else(now_ns)
                .to_string();
            json!({
                "traceId": random_hex(16),
                "spanId": random_hex(8),
                "name": format!("ulnclaw.{kind}"),
                "kind": 1,
                "startTimeUnixNano": ts,
                "endTimeUnixNano": ts,
                "attributes": span_attrs(event),
                "status": {},
            })
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": {"attributes": resource_attributes},
            "scopeSpans": [{
                "scope": {"name": "ulnclaw.monitoring"},
                "spans": spans,
            }],
        }],
    })
}

/// Resource attributes attached to every export (hermes
/// `_resource_attributes`).
pub fn resource_attributes(
    install_id: &str,
    version: &str,
    profile: Option<&str>,
) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    attrs.insert("service.name".to_string(), "ulnclaw".to_string());
    attrs.insert("service.version".to_string(), version.to_string());
    attrs.insert("service.instance.id".to_string(), install_id.to_string());
    if let Some(profile) = profile.filter(|p| !p.trim().is_empty()) {
        attrs.insert("ulnclaw.profile".to_string(), profile.to_string());
    }
    attrs
}

/// Resolve `{header_name: ENV_VAR_NAME}` -> header values from the
/// environment at export time (hermes `_resolve_headers`). Missing vars are
/// skipped; values are never logged.
pub fn resolve_headers(
    headers_env: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut resolved = Vec::new();
    for (header, env_name) in headers_env {
        if let Some(value) = crate::config::get_env_value(env_name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                resolved.push((header.clone(), value));
            }
        } else {
            tracing::debug!("OTLP header {header}: env var {env_name} not set; skipping");
        }
    }
    resolved
}

/// OTLP signal URL: append `/v1/traces` when the endpoint does not already
/// carry the signal path.
pub fn traces_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1/traces") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/traces")
    }
}

/// POST one batch of events to the collector. Returns spans exported.
/// Fail-isolated by callers: an error here never affects the gateway.
pub async fn export_batch(
    endpoint: &str,
    headers_env: &std::collections::HashMap<String, String>,
    events: &[Value],
    resource: &BTreeMap<String, String>,
) -> Result<usize, String> {
    if events.is_empty() {
        return Ok(0);
    }
    let payload = build_otlp_traces_payload(events, resource);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("OTLP client init failed: {e}"))?;
    let mut request = client
        .post(traces_url(endpoint))
        .header("Content-Type", "application/json");
    for (header, value) in resolve_headers(headers_env) {
        request = request.header(header, value);
    }
    let resp = request
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("OTLP export failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "OTLP endpoint returned {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    Ok(events.len())
}

/// Continuous streamer: drain the emitter and push each batch to OTLP
/// (hermes `OTLPStreamer` + `start_streaming`). Runs until the receiver
/// closes; export errors are logged and swallowed (fail-isolated).
pub async fn run_streamer(
    endpoint: String,
    headers_env: std::collections::HashMap<String, String>,
    resource: BTreeMap<String, String>,
    flush_interval: std::time::Duration,
) {
    let Some(mut rx) = take_receiver() else {
        tracing::debug!("monitoring streamer: receiver already taken");
        return;
    };
    let mut buffer: Vec<Value> = Vec::new();
    loop {
        match tokio::time::timeout(flush_interval, rx.recv()).await {
            Ok(Some(event)) => {
                buffer.push(event);
                // Drain whatever else is already queued.
                while buffer.len() < EMITTER_CAPACITY {
                    match rx.try_recv() {
                        Ok(more) => buffer.push(more),
                        Err(_) => break,
                    }
                }
            }
            Ok(None) => break, // channel closed
            Err(_) => {}       // timeout — flush below
        }
        if buffer.is_empty() {
            continue;
        }
        let batch = std::mem::take(&mut buffer);
        if let Err(e) = export_batch(&endpoint, &headers_env, &batch, &resource).await {
            tracing::warn!("monitoring OTLP export dropped a batch: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// CLI status (hermes cmd_monitoring)
// ---------------------------------------------------------------------------

/// Render `ulnclaw monitoring status` (hermes `cmd_monitoring` output).
pub fn render_status(config: &MonitoringConfig) -> String {
    let mut out = String::new();
    out.push_str("Gateway monitoring\n");
    out.push_str(&format!(
        "  Health export:  {} (monitoring.gateway_health_export.enabled)\n",
        if config.enabled() {
            "enabled"
        } else {
            "disabled"
        }
    ));
    if config.enabled() {
        out.push_str(&format!(
            "    Metrics:            {} (interval {}s)\n",
            if config.metrics_enabled() {
                "on"
            } else {
                "off"
            },
            config.export_interval_seconds()
        ));
        out.push_str(&format!(
            "    Diagnostic events:  {}\n",
            if config.diagnostic_events_enabled() {
                "on"
            } else {
                "off"
            }
        ));
        out.push_str(&format!(
            "    Warning/error logs: {} (interval {}s)\n",
            if config.warning_error_events_enabled() {
                "on"
            } else {
                "off"
            },
            config.logs_export_interval_seconds()
        ));
        out.push_str(
            "    Content safety:     always on (rendered messages are never exported; not configurable)\n",
        );
    }
    match (
        config.otlp_enabled(),
        config.export.otlp.endpoint.as_deref(),
    ) {
        (true, Some(endpoint)) if !endpoint.trim().is_empty() => {
            out.push_str(&format!("  OTLP endpoint:  {}\n", endpoint.trim()));
        }
        _ => {
            out.push_str("  OTLP endpoint:  not configured (monitoring.export.otlp)\n");
        }
    }
    out.push_str("  OTLP transport: built-in OTLP/HTTP JSON exporter (always available)\n");
    out.push_str("\n  Scope: gateway service health + redacted diagnostics only.\n");
    out.push_str("  No prompts, messages, tool args/results, usage analytics, or traces.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_for_export_scrubs_secrets_and_pii() {
        let input = "contact bob@example.com or +1 555 123 4567, \
                     token sk-abcdefghij1234, Bearer abc123.xyz, \
                     id 550e8400-e29b-41d4-a716-446655440000, xoxb-123-abc";
        let out = redact_for_export(Some(input)).expect("redacted");
        assert!(out.contains("[email]"), "{out}");
        assert!(out.contains("[phone]"), "{out}");
        assert!(out.contains("[id]"), "{out}");
        assert!(!out.contains("sk-abcdefghij1234"), "{out}");
        assert!(!out.contains("abc123.xyz"), "{out}");
        assert!(!out.contains("xoxb-123-abc"), "{out}");
        assert!(out.contains("[redacted]"), "{out}");
    }

    #[test]
    fn redact_for_export_none_passthrough() {
        assert!(redact_for_export(None).is_none());
        assert_eq!(redact_for_export(Some("clean text")).unwrap(), "clean text");
    }

    #[test]
    fn config_defaults_match_hermes() {
        let config = MonitoringConfig::default();
        assert!(!config.enabled());
        assert!(config.metrics_enabled());
        assert!(config.diagnostic_events_enabled());
        assert!(config.warning_error_events_enabled());
        assert_eq!(config.export_interval_seconds(), 60);
        assert_eq!(config.logs_export_interval_seconds(), 5);
        assert!(!config.otlp_enabled());
    }

    #[test]
    fn config_interval_floors() {
        let config = MonitoringConfig {
            gateway_health_export: GatewayHealthExportConfig {
                export_interval_seconds: Some(1),
                logs_export_interval_seconds: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(config.export_interval_seconds(), 5);
        assert_eq!(config.logs_export_interval_seconds(), 1);
    }

    #[test]
    fn install_id_reuses_existing() {
        assert_eq!(ensure_install_id(Some("kept-id")), "kept-id");
        let minted = ensure_install_id(Some("   "));
        assert!(!minted.trim().is_empty());
        assert_ne!(minted, "kept-id");
    }

    #[test]
    fn health_event_serializes_with_event_key() {
        let event = GatewayHealthEvent {
            name: "heartbeat".into(),
            gateway_state: Some("running".into()),
            old_state: None,
            new_state: None,
            exit_reason: None,
            active_agents: 2,
            gateway_busy: true,
            platform_count: 3,
            profile: None,
            install_id: Some("inst-1".into()),
            version: Some("1.2.3".into()),
            pid: Some(42),
            ts_ns: 123,
        };
        let value = event.to_value();
        assert_eq!(value["event"], "gateway_health");
        assert_eq!(value["name"], "heartbeat");
        assert_eq!(value["active_agents"], 2);
        // None fields are skipped.
        assert!(value.get("old_state").is_none());
    }

    #[test]
    fn span_attrs_whitelist_and_redaction() {
        let event = json!({
            "event": "gateway_diagnostic",
            "name": "platform_error bob@example.com",
            "subsystem": "slack",
            "error_class": "timeout",
            "ts_ns": 1,
            "rogue_field": "must not leak",
        });
        let attrs = span_attrs(&event);
        let keys: Vec<String> = attrs
            .iter()
            .map(|a| a["key"].as_str().unwrap().to_string())
            .collect();
        assert!(keys.contains(&"ulnclaw.event".to_string()));
        assert!(keys.contains(&"ulnclaw.name".to_string()));
        assert!(keys.contains(&"ulnclaw.subsystem".to_string()));
        assert!(!keys.iter().any(|k| k.contains("rogue")));
        let name_attr = attrs.iter().find(|a| a["key"] == "ulnclaw.name").unwrap();
        let text = name_attr["value"]["stringValue"].as_str().unwrap();
        assert!(text.contains("[email]"), "{text}");
    }

    #[test]
    fn span_attrs_typed_values() {
        let event = json!({
            "event": "gateway_health",
            "name": "heartbeat",
            "active_agents": 4,
            "gateway_busy": false,
        });
        let attrs = span_attrs(&event);
        let agents = attrs
            .iter()
            .find(|a| a["key"] == "ulnclaw.active_agents")
            .unwrap();
        assert_eq!(agents["value"]["intValue"], "4");
        let busy = attrs
            .iter()
            .find(|a| a["key"] == "ulnclaw.gateway_busy")
            .unwrap();
        assert_eq!(busy["value"]["boolValue"], false);
    }

    #[test]
    fn otlp_payload_shape() {
        let mut resource = BTreeMap::new();
        resource.insert("service.name".to_string(), "ulnclaw".to_string());
        resource.insert("service.instance.id".to_string(), "inst-1".to_string());
        let events = vec![
            json!({"event": "gateway_health", "name": "heartbeat", "ts_ns": 100}),
            json!({"event": "cron_execution", "status": "ok", "job_key": "j1", "ts_ns": 200}),
        ];
        let payload = build_otlp_traces_payload(&events, &resource);
        let rs = &payload["resourceSpans"][0];
        let res_keys: Vec<&str> = rs["resource"]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["key"].as_str().unwrap())
            .collect();
        assert!(res_keys.contains(&"service.name"));
        assert!(res_keys.contains(&"service.instance.id"));
        let spans = rs["scopeSpans"][0]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["name"], "ulnclaw.gateway_health");
        assert_eq!(spans[1]["name"], "ulnclaw.cron_execution");
        assert_eq!(spans[0]["startTimeUnixNano"], "100");
        assert_eq!(spans[0]["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(spans[0]["spanId"].as_str().unwrap().len(), 16);
    }

    #[test]
    fn traces_url_appends_signal_path_once() {
        assert_eq!(
            traces_url("http://localhost:4318"),
            "http://localhost:4318/v1/traces"
        );
        assert_eq!(
            traces_url("http://localhost:4318/"),
            "http://localhost:4318/v1/traces"
        );
        assert_eq!(
            traces_url("http://localhost:4318/v1/traces"),
            "http://localhost:4318/v1/traces"
        );
    }

    #[test]
    fn resolve_headers_reads_env_and_skips_missing() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("ULNCLAW_OTLP_TEST_TOKEN", "secret-value");
        std::env::remove_var("ULNCLAW_OTLP_TEST_MISSING");
        let mut headers_env = std::collections::HashMap::new();
        headers_env.insert(
            "Authorization".to_string(),
            "ULNCLAW_OTLP_TEST_TOKEN".to_string(),
        );
        headers_env.insert(
            "X-Missing".to_string(),
            "ULNCLAW_OTLP_TEST_MISSING".to_string(),
        );
        let resolved = resolve_headers(&headers_env);
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0],
            ("Authorization".to_string(), "secret-value".to_string())
        );
        std::env::remove_var("ULNCLAW_OTLP_TEST_TOKEN");
    }

    #[tokio::test]
    async fn export_batch_posts_to_mock_collector() {
        let received = std::sync::Arc::new(std::sync::Mutex::new(serde_json::Value::Null));
        let received_for_route = received.clone();
        let app = axum::Router::new().route(
            "/v1/traces",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let received = received_for_route.clone();
                async move {
                    *received.lock().unwrap() = body;
                    axum::Json(json!({}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let mut resource = BTreeMap::new();
        resource.insert("service.name".to_string(), "ulnclaw".to_string());
        let events = vec![json!({"event": "gateway_health", "name": "heartbeat", "ts_ns": 5})];
        let endpoint = format!("http://{addr}");
        let exported = export_batch(&endpoint, &Default::default(), &events, &resource)
            .await
            .expect("export ok");
        assert_eq!(exported, 1);
        let body = received.lock().unwrap().clone();
        assert_eq!(
            body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["name"],
            "ulnclaw.gateway_health"
        );
    }

    #[tokio::test]
    async fn export_batch_empty_is_noop() {
        let resource = BTreeMap::new();
        let exported = export_batch("http://127.0.0.1:1", &Default::default(), &[], &resource)
            .await
            .expect("noop ok");
        assert_eq!(exported, 0);
    }

    #[test]
    fn render_status_disabled_and_enabled() {
        let disabled = MonitoringConfig::default();
        let text = render_status(&disabled);
        assert!(text.contains("Health export:  disabled"));
        assert!(text.contains("OTLP endpoint:  not configured"));
        assert!(text.contains("No prompts, messages, tool args/results"));

        let enabled = MonitoringConfig {
            gateway_health_export: GatewayHealthExportConfig {
                enabled: Some(crate::config::Truthiness::Flag(true)),
                ..Default::default()
            },
            export: ExportConfig {
                otlp: OtlpExportConfig {
                    enabled: Some(crate::config::Truthiness::Flag(true)),
                    endpoint: Some("http://collector:4318".into()),
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        let text = render_status(&enabled);
        assert!(text.contains("Health export:  enabled"));
        assert!(text.contains("Metrics:            on (interval 60s)"));
        assert!(text.contains("OTLP endpoint:  http://collector:4318"));
        assert!(text.contains("Content safety:     always on"));
    }

    #[test]
    fn emitter_is_fire_and_forget() {
        // Emitting never panics even without a consumer.
        for i in 0..10 {
            emit(json!({"event": "gateway_health", "name": format!("e{i}"), "ts_ns": i}));
        }
        assert!(queue_len_estimate() > 0 || take_receiver().is_some());
    }
}

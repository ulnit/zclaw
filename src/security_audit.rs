//! `ulnclaw security audit` — on-demand supply-chain audit (hermes
//! `hermes_cli/security_audit.py` parity).
//!
//! Hermes scans three surfaces (venv, plugins, MCP servers); ulnclaw is a
//! static Rust binary with no venv or plugin surface, so the audit covers
//! the one surface users actually wire up: MCP servers in config.toml whose
//! `command/args` pin a package version (`npx -y <pkg>@<ver>`,
//! `uvx <pkg>==<ver>`). Vulnerabilities are looked up against OSV.dev
//! (`querybatch` + `/v1/vulns/{id}`). Single-shot, on-demand, never daily.

use std::time::Duration;

use crate::config::UlncLawConfig;

pub const OSV_BATCH_URL: &str = "https://api.osv.dev/v1/querybatch";
pub const OSV_VULN_URL: &str = "https://api.osv.dev/v1/vulns/";
/// OSV documented hard cap per batch request.
pub const OSV_BATCH_MAX: usize = 1000;
const HTTP_TIMEOUT_SECS: u64 = 20;

/// A single (name, version, ecosystem) tuple discovered on disk (hermes
/// `Component`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct Component {
    pub name: String,
    pub version: String,
    pub ecosystem: String,
    pub source: String,
}

/// OSV vulnerability detail (hermes `Vulnerability`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Vulnerability {
    pub osv_id: String,
    pub severity: String,
    pub summary: String,
    pub fixed_versions: Vec<String>,
}

/// One audit finding: component + vulnerability (hermes `Finding`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub component: Component,
    pub vuln: Vulnerability,
}

/// Severity ordering for sorting (hermes `SEVERITY_ORDER`).
pub fn severity_rank(severity: &str) -> u8 {
    match severity {
        "CRITICAL" => 4,
        "HIGH" => 3,
        "MODERATE" | "MEDIUM" => 2,
        "LOW" => 1,
        _ => 0,
    }
}

fn is_pkg_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Parse `pkg@version` (npm; scoped `@org/pkg@version` supported).
fn parse_npx_ref(token: &str) -> Option<(String, String)> {
    let (name, version) = if let Some(stripped) = token.strip_prefix('@') {
        // Scoped: @org/name@version — split on the LAST '@'.
        let rest = stripped;
        let at = rest.rfind('@')?;
        let (org_name, ver) = rest.split_at(at);
        if org_name.is_empty() {
            return None;
        }
        (format!("@{org_name}"), ver[1..].to_string())
    } else {
        let at = token.rfind('@')?;
        let (name, ver) = token.split_at(at);
        (name.to_string(), ver[1..].to_string())
    };
    if name.is_empty() || version.is_empty() {
        return None;
    }
    if !name
        .chars()
        .skip(if name.starts_with('@') { 1 } else { 0 })
        .all(|c| is_pkg_name_char(c) || c == '/')
    {
        return None;
    }
    if !version.chars().all(|c| is_pkg_name_char(c) || c == '+') {
        return None;
    }
    Some((name, version))
}

/// Parse `pkg==version` (PyPI).
fn parse_uvx_ref(token: &str) -> Option<(String, String)> {
    let (name, version) = token.split_once("==")?;
    if name.is_empty() || version.is_empty() {
        return None;
    }
    let mut chars = name.chars();
    if !chars
        .next()
        .map(|c| c.is_ascii_alphanumeric())
        .unwrap_or(false)
    {
        return None;
    }
    if !chars.all(is_pkg_name_char) {
        return None;
    }
    if !version
        .chars()
        .all(|c| is_pkg_name_char(c) || c == '+' || c == '!')
    {
        return None;
    }
    Some((name.to_string(), version.to_string()))
}

/// Best-effort: parse an MCP server's `command/args` into an auditable
/// component (hermes `_extract_mcp_component`). Returns `None` when the
/// entry doesn't pin a version we can audit (local paths, Docker images,
/// unversioned npx) — the audit stays silent rather than guess.
pub fn extract_mcp_component(
    server_name: &str,
    command: &str,
    args: &[String],
) -> Option<Component> {
    let cmd = command.trim().to_lowercase();
    if args.is_empty() {
        return None;
    }
    let is_npx = cmd == "npx" || cmd.ends_with("/npx");
    let is_uvx = cmd == "uvx" || cmd.ends_with("/uvx");
    if !is_npx && !is_uvx {
        return None;
    }
    for token in args {
        if token.starts_with('-') {
            continue;
        }
        // First non-flag token decides; unpinned refs are not audited.
        if is_npx {
            let (name, version) = parse_npx_ref(token)?;
            return Some(Component {
                name,
                version,
                ecosystem: "npm".to_string(),
                source: format!("mcp:{server_name}"),
            });
        }
        let (name, version) = parse_uvx_ref(token)?;
        return Some(Component {
            name,
            version,
            ecosystem: "PyPI".to_string(),
            source: format!("mcp:{server_name}"),
        });
    }
    None
}

/// Pinned MCP server packages from config (hermes `_discover_mcp`).
pub fn discover_mcp_components(config: &UlncLawConfig) -> Vec<Component> {
    config
        .mcp
        .servers
        .iter()
        .filter_map(|server| extract_mcp_component(&server.name, &server.command, &server.args))
        .collect()
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("could not build HTTP client: {e}"))
}

/// Batch-query OSV; returns `(component, [osv_id...])` for components with
/// any vulns (hermes `_osv_query_batch`).
pub fn osv_query_batch(components: &[Component]) -> Result<Vec<(Component, Vec<String>)>, String> {
    if components.is_empty() {
        return Ok(Vec::new());
    }
    let client = http_client()?;
    let mut findings: Vec<(Component, Vec<String>)> = Vec::new();
    for chunk in components.chunks(OSV_BATCH_MAX) {
        let queries: Vec<serde_json::Value> = chunk
            .iter()
            .map(|c| {
                serde_json::json!({
                    "package": {"name": c.name, "ecosystem": c.ecosystem},
                    "version": c.version,
                })
            })
            .collect();
        let response = client
            .post(OSV_BATCH_URL)
            .json(&serde_json::json!({ "queries": queries }))
            .send()
            .map_err(|e| format!("OSV batch query failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "OSV batch query failed: HTTP {}",
                response.status()
            ));
        }
        let body: serde_json::Value = response
            .json()
            .map_err(|e| format!("OSV batch query returned invalid JSON: {e}"))?;
        let results = body
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        for (component, result) in chunk.iter().zip(results.iter()) {
            let ids: Vec<String> = result
                .get("vulns")
                .and_then(|v| v.as_array())
                .map(|vulns| {
                    vulns
                        .iter()
                        .filter_map(|v| v.get("id").and_then(|id| id.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if !ids.is_empty() {
                findings.push((component.clone(), ids));
            }
        }
    }
    Ok(findings)
}

/// CVSS-derived severity tier from an OSV record (hermes
/// `_osv_severity_from_record`).
fn osv_severity_from_record(record: &serde_json::Value) -> String {
    if let Some(raw) = record
        .get("database_specific")
        .and_then(|db| db.get("severity"))
        .and_then(|s| s.as_str())
    {
        let upper = raw.trim().to_uppercase();
        if ["UNKNOWN", "LOW", "MODERATE", "MEDIUM", "HIGH", "CRITICAL"].contains(&upper.as_str()) {
            return upper;
        }
    }
    for entry in record
        .get("affected")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if let Some(sev) = entry
            .get("ecosystem_specific")
            .and_then(|e| e.get("severity"))
            .and_then(|s| s.as_str())
        {
            let upper = sev.trim().to_uppercase();
            if ["UNKNOWN", "LOW", "MODERATE", "MEDIUM", "HIGH", "CRITICAL"]
                .contains(&upper.as_str())
            {
                return upper;
            }
        }
    }
    "UNKNOWN".to_string()
}

/// Fixed versions across all affected ranges (hermes `_osv_fixed_versions`).
fn osv_fixed_versions(record: &serde_json::Value) -> Vec<String> {
    let mut fixes = Vec::new();
    for entry in record
        .get("affected")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
    {
        for range in entry
            .get("ranges")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default()
        {
            for event in range
                .get("events")
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if let Some(fixed) = event.get("fixed").and_then(|f| f.as_str()) {
                    if !fixes.contains(&fixed.to_string()) {
                        fixes.push(fixed.to_string());
                    }
                }
            }
        }
    }
    fixes
}

/// Fetch summary/severity for each unique vuln id (hermes
/// `_osv_fetch_details`; sequential — MCP audits surface few ids).
pub fn osv_fetch_details(vuln_ids: &[String]) -> Vec<Vulnerability> {
    let mut unique: Vec<String> = vuln_ids
        .iter()
        .filter(|id| !id.is_empty())
        .cloned()
        .collect();
    unique.sort();
    unique.dedup();
    let Ok(client) = http_client() else {
        return unique
            .into_iter()
            .map(|osv_id| Vulnerability {
                osv_id,
                severity: "UNKNOWN".to_string(),
                summary: String::new(),
                fixed_versions: Vec::new(),
            })
            .collect();
    };
    unique
        .into_iter()
        .map(|osv_id| {
            let record = client
                .get(format!("{OSV_VULN_URL}{osv_id}"))
                .send()
                .ok()
                .filter(|r| r.status().is_success())
                .and_then(|r| r.json::<serde_json::Value>().ok());
            match record {
                Some(record) => Vulnerability {
                    osv_id,
                    severity: osv_severity_from_record(&record),
                    summary: record
                        .get("summary")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string(),
                    fixed_versions: osv_fixed_versions(&record),
                },
                None => Vulnerability {
                    osv_id,
                    severity: "UNKNOWN".to_string(),
                    summary: String::new(),
                    fixed_versions: Vec::new(),
                },
            }
        })
        .collect()
}

/// Query OSV for the discovered components and assemble sorted findings
/// (hermes `run_audit`).
pub fn run_audit(components: Vec<Component>) -> Result<Vec<Finding>, String> {
    if components.is_empty() {
        return Ok(Vec::new());
    }
    let raw = osv_query_batch(&components)?;
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let all_ids: Vec<String> = raw.iter().flat_map(|(_, ids)| ids.clone()).collect();
    let details = osv_fetch_details(&all_ids);
    let mut findings: Vec<Finding> = Vec::new();
    for (component, ids) in raw {
        for id in ids {
            let vuln = details
                .iter()
                .find(|v| v.osv_id == id)
                .cloned()
                .unwrap_or_else(|| Vulnerability {
                    osv_id: id,
                    severity: "UNKNOWN".to_string(),
                    summary: String::new(),
                    fixed_versions: Vec::new(),
                });
            findings.push(Finding {
                component: component.clone(),
                vuln,
            });
        }
    }
    findings.sort_by(|a, b| {
        severity_rank(&b.vuln.severity)
            .cmp(&severity_rank(&a.vuln.severity))
            .then(a.component.source.cmp(&b.component.source))
            .then(
                a.component
                    .name
                    .to_lowercase()
                    .cmp(&b.component.name.to_lowercase()),
            )
            .then(a.vuln.osv_id.cmp(&b.vuln.osv_id))
    });
    Ok(findings)
}

/// Terminal rendering (hermes `_render_human`).
pub fn render_human(findings: &[Finding], total_components: usize) -> String {
    if findings.is_empty() {
        return format!("No known vulnerabilities found across {total_components} component(s).");
    }
    let mut lines = vec![format!(
        "Found {} known vulnerability finding(s) across {} component(s):",
        findings.len(),
        total_components
    )];
    lines.push(String::new());
    let mut last_source: Option<&str> = None;
    for finding in findings {
        if last_source != Some(finding.component.source.as_str()) {
            lines.push(format!("[{}]", finding.component.source));
            last_source = Some(&finding.component.source);
        }
        lines.push(format!(
            "  {:<8}  {}=={}  {}",
            finding.vuln.severity,
            finding.component.name,
            finding.component.version,
            finding.vuln.osv_id
        ));
        if !finding.vuln.summary.is_empty() {
            let mut summary = finding.vuln.summary.clone();
            if summary.chars().count() > 100 {
                summary = summary.chars().take(97).collect::<String>() + "...";
            }
            lines.push(format!("           {summary}"));
        }
        if !finding.vuln.fixed_versions.is_empty() {
            let fixes: Vec<&str> = finding
                .vuln
                .fixed_versions
                .iter()
                .take(3)
                .map(String::as_str)
                .collect();
            lines.push(format!("           fixed in: {}", fixes.join(", ")));
        }
    }
    lines.join("\n")
}

/// JSON rendering (hermes `_render_json`).
pub fn render_json(findings: &[Finding], total_components: usize) -> String {
    let payload = serde_json::json!({
        "total_components_scanned": total_components,
        "finding_count": findings.len(),
        "findings": findings,
    });
    serde_json::to_string_pretty(&payload).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering() {
        assert!(severity_rank("CRITICAL") > severity_rank("HIGH"));
        assert!(severity_rank("HIGH") > severity_rank("MODERATE"));
        assert_eq!(severity_rank("MODERATE"), severity_rank("MEDIUM"));
        assert!(severity_rank("LOW") > severity_rank("UNKNOWN"));
    }

    #[test]
    fn extract_npx_pinned_refs() {
        let args = vec!["-y".to_string(), "some-mcp@1.2.3".to_string()];
        let component = extract_mcp_component("fs", "npx", &args).unwrap();
        assert_eq!(component.name, "some-mcp");
        assert_eq!(component.version, "1.2.3");
        assert_eq!(component.ecosystem, "npm");
        assert_eq!(component.source, "mcp:fs");

        let scoped = vec!["@modelcontextprotocol/server-fs@2.0.1".to_string()];
        let component = extract_mcp_component("x", "/usr/bin/npx", &scoped).unwrap();
        assert_eq!(component.name, "@modelcontextprotocol/server-fs");
        assert_eq!(component.version, "2.0.1");
    }

    #[test]
    fn extract_uvx_pinned_refs() {
        let args = vec![
            "mcp-search==0.4.0".to_string(),
            "--port".to_string(),
            "9".to_string(),
        ];
        let component = extract_mcp_component("search", "uvx", &args).unwrap();
        assert_eq!(component.name, "mcp-search");
        assert_eq!(component.version, "0.4.0");
        assert_eq!(component.ecosystem, "PyPI");
    }

    #[test]
    fn unpinned_and_foreign_commands_skipped() {
        // Unversioned npx -> silent.
        assert_eq!(
            extract_mcp_component("a", "npx", &["-y".into(), "some-mcp".into()]),
            None
        );
        // Local binary -> silent.
        assert_eq!(
            extract_mcp_component("b", "/opt/mcp/server", &["--stdio".into()]),
            None
        );
        // No args -> silent.
        assert_eq!(extract_mcp_component("c", "npx", &[]), None);
        // uvx without pin -> silent.
        assert_eq!(
            extract_mcp_component("d", "uvx", &["mcp-search".into()]),
            None
        );
    }

    #[test]
    fn discover_from_config() {
        let mut config = UlncLawConfig::default();
        config.mcp.servers.push(crate::mcp::McpServerConfig {
            name: "fs".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".into(), "mcp-fs@1.0.0".into()],
            env: Default::default(),
            url: None,
            transport: None,
            headers: Default::default(),
            auth: None,
            oauth: Default::default(),
            lazy: false,
            enabled: true,
        });
        config.mcp.servers.push(crate::mcp::McpServerConfig {
            name: "local".to_string(),
            command: "/opt/bin/server".to_string(),
            args: vec![],
            env: Default::default(),
            url: None,
            transport: None,
            headers: Default::default(),
            auth: None,
            oauth: Default::default(),
            lazy: false,
            enabled: true,
        });
        let components = discover_mcp_components(&config);
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].name, "mcp-fs");
    }

    #[test]
    fn severity_from_record_paths() {
        let record = serde_json::json!({
            "database_specific": {"severity": "HIGH"},
        });
        assert_eq!(osv_severity_from_record(&record), "HIGH");

        let record = serde_json::json!({
            "affected": [{"ecosystem_specific": {"severity": "MODERATE"}}],
        });
        assert_eq!(osv_severity_from_record(&record), "MODERATE");

        let record = serde_json::json!({});
        assert_eq!(osv_severity_from_record(&record), "UNKNOWN");
    }

    #[test]
    fn fixed_versions_deduped() {
        let record = serde_json::json!({
            "affected": [
                {"ranges": [{"events": [{"introduced": "0"}, {"fixed": "1.2.3"}]}]},
                {"ranges": [{"events": [{"fixed": "1.2.3"}, {"fixed": "2.0.0"}]}]},
            ],
        });
        assert_eq!(
            osv_fixed_versions(&record),
            vec!["1.2.3".to_string(), "2.0.0".to_string()]
        );
    }

    #[test]
    fn render_empty_and_findings() {
        assert_eq!(
            render_human(&[], 3),
            "No known vulnerabilities found across 3 component(s)."
        );
        let findings = vec![Finding {
            component: Component {
                name: "pkg".to_string(),
                version: "1.0.0".to_string(),
                ecosystem: "npm".to_string(),
                source: "mcp:fs".to_string(),
            },
            vuln: Vulnerability {
                osv_id: "GHSA-xxxx".to_string(),
                severity: "HIGH".to_string(),
                summary: "Something bad happens in this package".to_string(),
                fixed_versions: vec!["1.0.1".to_string()],
            },
        }];
        let out = render_human(&findings, 1);
        assert!(out.contains("Found 1 known vulnerability"), "{out}");
        assert!(out.contains("[mcp:fs]"), "{out}");
        assert!(out.contains("pkg==1.0.0  GHSA-xxxx"), "{out}");
        assert!(out.contains("fixed in: 1.0.1"), "{out}");

        let json = render_json(&findings, 1);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["finding_count"], 1);
        assert_eq!(parsed["total_components_scanned"], 1);
        assert_eq!(parsed["findings"][0]["component"]["name"], "pkg");
        assert_eq!(parsed["findings"][0]["vuln"]["severity"], "HIGH");
    }

    #[test]
    fn findings_sorted_by_severity() {
        let component = Component {
            name: "a".to_string(),
            version: "1".to_string(),
            ecosystem: "npm".to_string(),
            source: "mcp:x".to_string(),
        };
        let low = Finding {
            component: component.clone(),
            vuln: Vulnerability {
                osv_id: "LOW-1".to_string(),
                severity: "LOW".to_string(),
                summary: String::new(),
                fixed_versions: vec![],
            },
        };
        let critical = Finding {
            component: component.clone(),
            vuln: Vulnerability {
                osv_id: "CRIT-1".to_string(),
                severity: "CRITICAL".to_string(),
                summary: String::new(),
                fixed_versions: vec![],
            },
        };
        let mut findings = vec![low, critical];
        findings
            .sort_by(|a, b| severity_rank(&b.vuln.severity).cmp(&severity_rank(&a.vuln.severity)));
        assert_eq!(findings[0].vuln.osv_id, "CRIT-1");
    }
}

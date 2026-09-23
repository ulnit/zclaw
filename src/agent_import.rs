//! `ulnclaw import-agent` — import Claude Code / Codex CLI setups into
//! ulnclaw (hermes `hermes_cli/agent_import.py` parity).
//!
//! detect → parse → map → apply, with a mandatory preview phase
//! (`--dry-run` writes nothing), per-item imported/skipped/conflict/error
//! records, and secrets NEVER imported: credential files are ignored and
//! MCP env vars with secret-looking names are stripped and reported.
//!
//! Mappings (adapted to ulnclaw storage):
//! - claude-code: `CLAUDE.md` → `memory/MEMORY.md` entries;
//!   `mcpServers` (`.claude.json` + `settings.json`) → config.toml
//!   `[[mcp.servers]]`; `skills/` → `skills/claude-code-imports/`.
//! - codex: `AGENTS.md` + `memories/*.md` → `memory/MEMORY.md` entries;
//!   `config.toml [mcp_servers.*]` → `[[mcp.servers]]`;
//!   `skills/` → `skills/codex-imports/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Agents whose setups we can import (hermes `SUPPORTED_AGENTS`).
pub const SUPPORTED_AGENTS: &[&str] = &["claude-code", "codex"];

/// Character budget for the merged memory file (hermes `MEMORY_CHAR_LIMIT`,
/// the migration budget — larger than the per-turn tool limit on purpose).
pub const MEMORY_CHAR_LIMIT: usize = 20_000;

fn agent_default_dir(agent: &str) -> Option<&'static str> {
    match agent {
        "claude-code" => Some(".claude"),
        "codex" => Some(".codex"),
        _ => None,
    }
}

fn skill_category(agent: &str) -> &'static str {
    match agent {
        "codex" => "codex-imports",
        _ => "claude-code-imports",
    }
}

/// True when an env-var name looks like a credential (hermes
/// `_SECRET_KEY_RE` — never copied into config.toml).
pub fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    const NEEDLES: &[&str] = &[
        "API_KEY",
        "APIKEY",
        "API-KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
        "PRIVATE_KEY",
        "PRIVATE-KEY",
        "ACCESS_KEY",
        "ACCESS-KEY",
    ];
    NEEDLES.iter().any(|n| upper.contains(n)) || upper.ends_with("KEY")
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Split a markdown document into memory entries (hermes
/// `extract_markdown_entries`): headings become context prefixes, bullets
/// and paragraphs become entries; code blocks and tables are skipped.
pub fn extract_markdown_entries(text: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    let mut headings: Vec<(usize, String)> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();

    let context_prefix = |headings: &[(usize, String)]| {
        headings
            .iter()
            .map(|(_, h)| h.as_str())
            .filter(|h| {
                !h.contains("MEMORY.md")
                    && !h.contains("USER.md")
                    && !h.contains("SOUL.md")
                    && !h.contains("AGENTS.md")
                    && !h.contains("TOOLS.md")
                    && !h.contains("IDENTITY.md")
                    && !h.contains("CLAUDE.md")
            })
            .collect::<Vec<_>>()
            .join(" > ")
    };

    let flush =
        |paragraph: &mut Vec<String>, entries: &mut Vec<String>, headings: &[(usize, String)]| {
            if paragraph.is_empty() {
                return;
            }
            let block = paragraph.join(" ");
            paragraph.clear();
            let block = block.trim();
            if block.is_empty() {
                return;
            }
            let prefix = context_prefix(headings);
            if prefix.is_empty() {
                entries.push(block.to_string());
            } else {
                entries.push(format!("{prefix}: {block}"));
            }
        };

    let mut in_code_block = false;
    for raw_line in text.lines() {
        let stripped = raw_line.trim();

        if stripped.starts_with("```") {
            in_code_block = !in_code_block;
            flush(&mut paragraph, &mut entries, &headings);
            continue;
        }
        if in_code_block {
            continue;
        }

        if let Some(rest) = stripped.strip_prefix('#') {
            let level = 1 + rest.chars().take_while(|&c| c == '#').count();
            let value = rest.trim_start_matches('#').trim();
            if !value.is_empty() {
                flush(&mut paragraph, &mut entries, &headings);
                while headings.len() >= level {
                    headings.pop();
                }
                headings.push((level, value.to_string()));
                continue;
            }
        }

        let bullet = stripped
            .strip_prefix("- ")
            .or_else(|| stripped.strip_prefix("* "))
            .or_else(|| {
                stripped
                    .split_once(". ")
                    .filter(|(num, _)| !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()))
                    .map(|(_, rest)| rest)
            });
        if let Some(content) = bullet {
            let content = content.trim();
            if !content.is_empty() {
                flush(&mut paragraph, &mut entries, &headings);
                let prefix = context_prefix(&headings);
                if prefix.is_empty() {
                    entries.push(content.to_string());
                } else {
                    entries.push(format!("{prefix}: {content}"));
                }
            }
            continue;
        }

        if stripped.is_empty() {
            flush(&mut paragraph, &mut entries, &headings);
            continue;
        }
        if stripped.starts_with('|') && stripped.ends_with('|') {
            flush(&mut paragraph, &mut entries, &headings);
            continue;
        }
        paragraph.push(stripped.to_string());
    }
    flush(&mut paragraph, &mut entries, &headings);

    let mut deduped = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let normalized = normalize_text(&entry);
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        deduped.push(entry);
    }
    deduped
}

/// Convert a Claude Code `Bash(...)` permission rule into a command pattern
/// (hermes `claude_rule_to_command_pattern`).
pub fn claude_rule_to_command_pattern(rule: &str) -> Option<String> {
    let rule = rule.trim();
    let inner = rule
        .strip_prefix("Bash(")?
        .strip_suffix(')')
        .map(str::trim)?;
    if inner.is_empty() {
        return None;
    }
    let inner = if let Some(prefix) = inner.strip_suffix(":*") {
        format!("{prefix}*")
    } else {
        inner.to_string()
    };
    Some(inner)
}

/// Supported agents whose default source dir exists under `home_dir`
/// (hermes `detect_agents`).
pub fn detect_agents(home_dir: &Path) -> Vec<String> {
    SUPPORTED_AGENTS
        .iter()
        .filter(|agent| {
            agent_default_dir(agent)
                .map(|dir| home_dir.join(dir).is_dir())
                .unwrap_or(false)
        })
        .map(|agent| agent.to_string())
        .collect()
}

/// Split an MCP server env object into kept values and stripped
/// secret-looking names (hermes `sanitize_mcp_env`).
pub fn sanitize_mcp_env(env: &serde_json::Value) -> (BTreeMap<String, String>, Vec<String>) {
    let mut kept = BTreeMap::new();
    let mut stripped = Vec::new();
    if let Some(map) = env.as_object() {
        for (key, value) in map {
            if is_secret_key(key) {
                stripped.push(key.clone());
            } else {
                kept.insert(
                    key.clone(),
                    value.as_str().unwrap_or(&value.to_string()).to_string(),
                );
            }
        }
    }
    (kept, stripped)
}

/// Merge stats (hermes merge_entries stats dict).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MergeStats {
    pub existing: usize,
    pub added: usize,
    pub duplicates: usize,
    pub overflowed: usize,
}

/// Dedup-merge incoming entries into existing ones under a character budget
/// (hermes `merge_entries`; ulnclaw serializes bullet entries).
pub fn merge_entries(
    existing: &[String],
    incoming: &[String],
    limit: usize,
) -> (Vec<String>, MergeStats) {
    let mut merged = existing.to_vec();
    let mut seen: std::collections::HashSet<String> = existing
        .iter()
        .filter(|e| !e.trim().is_empty())
        .map(|e| normalize_text(e))
        .collect();
    let mut stats = MergeStats {
        existing: existing.len(),
        ..Default::default()
    };
    for entry in incoming {
        let normalized = normalize_text(entry);
        if normalized.is_empty() {
            continue;
        }
        if !seen.insert(normalized.clone()) {
            stats.duplicates += 1;
            continue;
        }
        let mut candidate = merged.clone();
        candidate.push(entry.clone());
        if crate::tools::builtin::memory::entries_to_content(&candidate).len() > limit {
            seen.remove(&normalized);
            stats.overflowed += 1;
            continue;
        }
        merged = candidate;
        stats.added += 1;
    }
    (merged, stats)
}

/// Import item status (hermes report statuses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStatus {
    Imported,
    Skipped,
    Conflict,
    Error,
}

impl ImportStatus {
    fn label(self) -> &'static str {
        match self {
            ImportStatus::Imported => "imported",
            ImportStatus::Skipped => "skipped",
            ImportStatus::Conflict => "conflict",
            ImportStatus::Error => "error",
        }
    }
}

/// One import item record (hermes report item).
#[derive(Debug, Clone)]
pub struct ImportItem {
    pub kind: String,
    pub source: Option<PathBuf>,
    pub destination: Option<PathBuf>,
    pub status: ImportStatus,
    pub reason: String,
    pub details: Vec<(String, String)>,
}

/// Full import report (hermes `build_report`).
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub agent: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub dry_run: bool,
    pub items: Vec<ImportItem>,
    pub stripped_secrets: Vec<String>,
}

impl ImportReport {
    pub fn summary(&self) -> [(&'static str, usize); 4] {
        let count = |s: ImportStatus| self.items.iter().filter(|i| i.status == s).count();
        [
            ("imported", count(ImportStatus::Imported)),
            ("skipped", count(ImportStatus::Skipped)),
            ("conflict", count(ImportStatus::Conflict)),
            ("error", count(ImportStatus::Error)),
        ]
    }
}

/// Detect/parse/map/apply importer for one agent source tree (hermes
/// `AgentImporter`). `execute=false` plans without touching disk.
#[derive(Debug)]
pub struct AgentImporter {
    agent: String,
    source_root: PathBuf,
    target_root: PathBuf,
    execute: bool,
    overwrite: bool,
    items: Vec<ImportItem>,
    stripped_secrets: Vec<String>,
}

impl AgentImporter {
    pub fn new(
        agent: &str,
        source_root: PathBuf,
        target_root: PathBuf,
        execute: bool,
        overwrite: bool,
    ) -> Result<Self, String> {
        if !SUPPORTED_AGENTS.contains(&agent) {
            return Err(format!("Unsupported agent: {agent:?}"));
        }
        Ok(Self {
            agent: agent.to_string(),
            source_root,
            target_root,
            execute,
            overwrite,
            items: Vec::new(),
            stripped_secrets: Vec::new(),
        })
    }

    fn record(
        &mut self,
        kind: &str,
        source: Option<&Path>,
        destination: Option<&Path>,
        status: ImportStatus,
        reason: &str,
    ) -> &mut ImportItem {
        self.items.push(ImportItem {
            kind: kind.to_string(),
            source: source.map(Path::to_path_buf),
            destination: destination.map(Path::to_path_buf),
            status,
            reason: reason.to_string(),
            details: Vec::new(),
        });
        self.items.last_mut().unwrap()
    }

    pub fn run(mut self) -> ImportReport {
        if !self.source_root.is_dir() {
            let source = self.source_root.clone();
            self.record(
                "source",
                Some(&source),
                None,
                ImportStatus::Error,
                "Source directory does not exist",
            );
            return self.build_report();
        }
        if self.agent == "claude-code" {
            self.run_claude_code();
        } else {
            self.run_codex();
        }
        self.build_report()
    }

    fn build_report(self) -> ImportReport {
        let mut stripped = self.stripped_secrets;
        stripped.sort();
        stripped.dedup();
        ImportReport {
            agent: self.agent,
            source: self.source_root,
            target: self.target_root,
            dry_run: !self.execute,
            items: self.items,
            stripped_secrets: stripped,
        }
    }

    // -- claude-code ---------------------------------------------------------

    fn run_claude_code(&mut self) {
        let settings = self.load_claude_settings();
        self.import_context_file(&self.source_root.join("CLAUDE.md"), "claude-md");
        self.import_permissions(&settings);
        let servers = self.claude_mcp_servers(&settings);
        self.import_mcp_servers("mcp-servers", servers);
        let skills = self.source_root.join("skills");
        self.import_skills(&skills);
        let commands = self.source_root.join("commands");
        if commands.is_dir() {
            let has_md = std::fs::read_dir(&commands)
                .map(|entries| {
                    entries
                        .flatten()
                        .any(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
                })
                .unwrap_or(false);
            if has_md {
                self.record(
                    "slash-commands",
                    Some(&commands),
                    None,
                    ImportStatus::Skipped,
                    "Claude slash commands have no direct ulnclaw equivalent — consider \
                     converting them into skills",
                );
            }
        }
    }

    fn load_claude_settings(&mut self) -> serde_json::Value {
        let path = self.source_root.join("settings.json");
        if !path.exists() {
            return serde_json::Value::Null;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(value @ serde_json::Value::Object(_)) => value,
                Ok(_) => {
                    self.record(
                        "settings",
                        Some(&path),
                        None,
                        ImportStatus::Error,
                        "settings.json is not a JSON object",
                    );
                    serde_json::Value::Null
                }
                Err(e) => {
                    self.record(
                        "settings",
                        Some(&path),
                        None,
                        ImportStatus::Error,
                        &format!("Could not parse settings.json: {e}"),
                    );
                    serde_json::Value::Null
                }
            },
            Err(e) => {
                self.record(
                    "settings",
                    Some(&path),
                    None,
                    ImportStatus::Error,
                    &format!("Could not read settings.json: {e}"),
                );
                serde_json::Value::Null
            }
        }
    }

    /// Collect mcpServers from `.claude.json` (next to the source root,
    /// preferred) and `settings.json` (hermes `_claude_mcp_servers`).
    fn claude_mcp_servers(
        &mut self,
        settings: &serde_json::Value,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut servers = serde_json::Map::new();
        let claude_json = self
            .source_root
            .parent()
            .map(|p| p.join(".claude.json"))
            .filter(|p| p.exists());
        if let Some(path) = claude_json {
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            {
                Some(value) => {
                    if let Some(map) = value.get("mcpServers").and_then(|m| m.as_object()) {
                        for (name, server) in map {
                            servers.insert(name.clone(), server.clone());
                        }
                    }
                }
                None => {
                    self.record(
                        "mcp-servers",
                        Some(&path),
                        None,
                        ImportStatus::Error,
                        "Could not parse .claude.json",
                    );
                }
            }
        }
        if let Some(map) = settings.get("mcpServers").and_then(|m| m.as_object()) {
            for (name, server) in map {
                servers
                    .entry(name.clone())
                    .or_insert_with(|| server.clone());
            }
        }
        servers
    }

    /// Claude permission rules have no ulnclaw allowlist surface; report the
    /// converted patterns as skipped so the user can act on them.
    fn import_permissions(&mut self, settings: &serde_json::Value) {
        let Some(permissions) = settings.get("permissions") else {
            return;
        };
        for (key, label) in [
            ("allow", "command allowlist"),
            ("deny", "approval denylist"),
        ] {
            let Some(rules) = permissions.get(key).and_then(|r| r.as_array()) else {
                continue;
            };
            let patterns: Vec<String> = rules
                .iter()
                .filter_map(|rule| rule.as_str())
                .filter_map(claude_rule_to_command_pattern)
                .collect();
            if patterns.is_empty() {
                continue;
            }
            let reason = format!(
                "ulnclaw has no {label} config surface; {} Bash rule(s) not imported: {}",
                patterns.len(),
                patterns.join(", ")
            );
            self.record("permissions", None, None, ImportStatus::Skipped, &reason);
        }
    }

    // -- codex ---------------------------------------------------------------

    fn run_codex(&mut self) {
        let config = self.load_codex_config();
        self.import_context_file(&self.source_root.join("AGENTS.md"), "agents-md");
        let mut servers = serde_json::Map::new();
        if let Some(mcp) = config.get("mcp_servers").and_then(|m| m.as_table()) {
            for (name, server) in mcp {
                if let Some(table) = server.as_table() {
                    let command = table
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let args: Vec<String> = table
                        .get("args")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut env = serde_json::Map::new();
                    if let Some(env_table) = table.get("env").and_then(|v| v.as_table()) {
                        for (key, value) in env_table {
                            if let Some(text) = value.as_str() {
                                env.insert(
                                    key.clone(),
                                    serde_json::Value::String(text.to_string()),
                                );
                            }
                        }
                    }
                    let mut spec = serde_json::Map::new();
                    spec.insert("command".into(), serde_json::Value::String(command));
                    spec.insert(
                        "args".into(),
                        serde_json::Value::Array(
                            args.into_iter().map(serde_json::Value::String).collect(),
                        ),
                    );
                    spec.insert("env".into(), serde_json::Value::Object(env));
                    servers.insert(name.clone(), serde_json::Value::Object(spec));
                }
            }
        }
        self.import_mcp_servers("mcp-servers", servers);
        let memories = self.source_root.join("memories");
        self.import_memories_dir(&memories);
        let skills = self.source_root.join("skills");
        self.import_skills(&skills);
    }

    fn load_codex_config(&mut self) -> toml::Value {
        let path = self.source_root.join("config.toml");
        if !path.exists() {
            self.record(
                "config",
                None,
                None,
                ImportStatus::Skipped,
                "No config.toml found",
            );
            return toml::Value::Table(toml::map::Map::new());
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.parse::<toml::Value>().ok())
        {
            Some(value) => value,
            None => {
                self.record(
                    "config",
                    Some(&path),
                    None,
                    ImportStatus::Error,
                    "Could not parse config.toml",
                );
                toml::Value::Table(toml::map::Map::new())
            }
        }
    }

    // -- shared mappers --------------------------------------------------------

    /// CLAUDE.md / AGENTS.md → memory entries (hermes `import_context_file`).
    fn import_context_file(&mut self, source: &Path, kind: &str) {
        let destination = self.target_root.join("memory").join("MEMORY.md");
        if !source.exists() {
            let file_name = source
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            self.record(
                kind,
                None,
                Some(&destination),
                ImportStatus::Skipped,
                &format!("No {file_name} found"),
            );
            return;
        }
        let Ok(text) = std::fs::read_to_string(source) else {
            self.record(
                kind,
                Some(source),
                Some(&destination),
                ImportStatus::Error,
                "Could not read file",
            );
            return;
        };
        let incoming = extract_markdown_entries(&text);
        if incoming.is_empty() {
            self.record(
                kind,
                Some(source),
                Some(&destination),
                ImportStatus::Skipped,
                "No importable entries found",
            );
            return;
        }
        self.merge_memory_entries(kind, Some(source), &destination, &incoming);
    }

    /// codex `memories/*.md` → memory entries (hermes `import_memories_dir`).
    fn import_memories_dir(&mut self, memories_dir: &Path) {
        let destination = self.target_root.join("memory").join("MEMORY.md");
        if !memories_dir.is_dir() {
            self.record(
                "memories",
                None,
                Some(&destination),
                ImportStatus::Skipped,
                "No memories directory found",
            );
            return;
        }
        let mut incoming = Vec::new();
        let mut files: Vec<PathBuf> = std::fs::read_dir(memories_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        for file in files {
            match std::fs::read_to_string(&file) {
                Ok(text) => incoming.extend(extract_markdown_entries(&text)),
                Err(e) => {
                    self.record(
                        "memories",
                        Some(&file),
                        Some(&destination),
                        ImportStatus::Error,
                        &format!("Could not read file: {e}"),
                    );
                }
            }
        }
        if incoming.is_empty() {
            self.record(
                "memories",
                Some(memories_dir),
                Some(&destination),
                ImportStatus::Skipped,
                "No importable entries found",
            );
            return;
        }
        self.merge_memory_entries("memories", Some(memories_dir), &destination, &incoming);
    }

    fn merge_memory_entries(
        &mut self,
        kind: &str,
        source: Option<&Path>,
        destination: &Path,
        incoming: &[String],
    ) {
        let existing_content = std::fs::read_to_string(destination).unwrap_or_default();
        let existing = crate::tools::builtin::memory::read_entries(&existing_content);
        let (merged, stats) = merge_entries(&existing, incoming, MEMORY_CHAR_LIMIT);
        if stats.added == 0 {
            let reason = format!(
                "No new entries to import ({} existing, {} duplicates, {} overflowed)",
                stats.existing, stats.duplicates, stats.overflowed
            );
            self.record(
                kind,
                source,
                Some(destination),
                ImportStatus::Skipped,
                &reason,
            );
            return;
        }
        if self.execute {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if destination.exists() {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = destination.with_extension(format!("md.bak.{ts}"));
                if std::fs::copy(destination, &backup).is_err() {
                    self.record(
                        kind,
                        source,
                        Some(destination),
                        ImportStatus::Error,
                        "Could not back up existing memory file",
                    );
                    return;
                }
            }
            let content = crate::tools::builtin::memory::entries_to_content(&merged);
            if let Err(e) = std::fs::write(destination, content) {
                self.record(
                    kind,
                    source,
                    Some(destination),
                    ImportStatus::Error,
                    &format!("Could not write memory file: {e}"),
                );
                return;
            }
        }
        let reason = format!(
            "added {} entries ({} existing, {} duplicates, {} overflowed)",
            stats.added, stats.existing, stats.duplicates, stats.overflowed
        );
        self.record(
            kind,
            source,
            Some(destination),
            ImportStatus::Imported,
            &reason,
        );
    }

    /// MCP servers → config.toml `[[mcp.servers]]` with secret env vars
    /// stripped (hermes `import_mcp_servers`).
    fn import_mcp_servers(
        &mut self,
        kind: &str,
        servers: serde_json::Map<String, serde_json::Value>,
    ) {
        let config_path = self.target_root.join("config.toml");
        if servers.is_empty() {
            self.record(
                kind,
                None,
                Some(&config_path),
                ImportStatus::Skipped,
                "No MCP servers found",
            );
            return;
        }

        let existing_text = std::fs::read_to_string(&config_path).unwrap_or_default();
        let mut root = if existing_text.trim().is_empty() {
            toml::Value::Table(toml::map::Map::new())
        } else {
            match existing_text.parse::<toml::Value>() {
                Ok(value) => value,
                Err(e) => {
                    self.record(
                        kind,
                        None,
                        Some(&config_path),
                        ImportStatus::Error,
                        &format!(
                            "Refusing to overwrite {}: existing config.toml is not valid TOML ({e})",
                            config_path.display()
                        ),
                    );
                    return;
                }
            }
        };

        let mcp_table = root.as_table_mut().and_then(|t| {
            t.entry("mcp")
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
                .as_table_mut()
        });
        let Some(mcp_table) = mcp_table else {
            self.record(
                kind,
                None,
                Some(&config_path),
                ImportStatus::Error,
                "config.toml root is not a table",
            );
            return;
        };
        let servers_array = mcp_table
            .entry("servers")
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        let Some(array) = servers_array.as_array_mut() else {
            self.record(
                kind,
                None,
                Some(&config_path),
                ImportStatus::Error,
                "mcp.servers in config.toml is not an array",
            );
            return;
        };

        let existing_names: Vec<String> = array
            .iter()
            .filter_map(|entry| entry.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();

        let mut added = 0usize;
        for (name, spec) in &servers {
            if existing_names.iter().any(|existing| existing == name) {
                self.record(
                    kind,
                    None,
                    Some(&config_path),
                    ImportStatus::Conflict,
                    &format!("MCP server '{name}' already configured"),
                );
                continue;
            }
            let command = spec
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if command.is_empty() {
                self.record(
                    kind,
                    None,
                    Some(&config_path),
                    ImportStatus::Skipped,
                    &format!("MCP server '{name}' has no command"),
                );
                continue;
            }
            let args: Vec<String> = spec
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let (env, stripped) =
                sanitize_mcp_env(spec.get("env").unwrap_or(&serde_json::Value::Null));
            if !stripped.is_empty() {
                self.stripped_secrets
                    .extend(stripped.iter().map(|key| format!("{name}: {key}")));
            }

            let mut entry = toml::map::Map::new();
            entry.insert("name".into(), toml::Value::String(name.clone()));
            entry.insert("command".into(), toml::Value::String(command));
            if !args.is_empty() {
                entry.insert(
                    "args".into(),
                    toml::Value::Array(args.into_iter().map(toml::Value::String).collect()),
                );
            }
            if !env.is_empty() {
                let mut env_table = toml::map::Map::new();
                for (key, value) in env {
                    env_table.insert(key, toml::Value::String(value));
                }
                entry.insert("env".into(), toml::Value::Table(env_table));
            }
            array.push(toml::Value::Table(entry));
            added += 1;
        }

        if added == 0 {
            return;
        }
        if self.execute {
            match toml::to_string_pretty(&root) {
                Ok(rendered) => {
                    if let Err(e) = std::fs::write(&config_path, rendered) {
                        self.record(
                            kind,
                            None,
                            Some(&config_path),
                            ImportStatus::Error,
                            &format!("Could not write config.toml: {e}"),
                        );
                        return;
                    }
                }
                Err(e) => {
                    self.record(
                        kind,
                        None,
                        Some(&config_path),
                        ImportStatus::Error,
                        &format!("Could not serialize merged config.toml: {e}"),
                    );
                    return;
                }
            }
        }
        self.record(
            kind,
            None,
            Some(&config_path),
            ImportStatus::Imported,
            &format!("added {added} MCP server(s)"),
        );
    }

    /// `skills/<name>/` → `<target>/skills/<category>/<name>/` (hermes
    /// `import_skills`).
    fn import_skills(&mut self, skills_dir: &Path) {
        let category = skill_category(&self.agent);
        let destination_root = self.target_root.join("skills").join(category);
        if !skills_dir.is_dir() {
            self.record(
                "skills",
                None,
                Some(&destination_root),
                ImportStatus::Skipped,
                "No skills directory found",
            );
            return;
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(skills_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        if dirs.is_empty() {
            self.record(
                "skills",
                Some(skills_dir),
                Some(&destination_root),
                ImportStatus::Skipped,
                "Skills directory is empty",
            );
            return;
        }
        for dir in dirs {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let destination = destination_root.join(&name);
            if destination.exists() && !self.overwrite {
                self.record(
                    "skills",
                    Some(&dir),
                    Some(&destination),
                    ImportStatus::Conflict,
                    "Skill already exists (use --overwrite to replace)",
                );
                continue;
            }
            if !self.execute {
                self.record(
                    "skills",
                    Some(&dir),
                    Some(&destination),
                    ImportStatus::Imported,
                    "would copy skill",
                );
                continue;
            }
            match copy_dir_recursive(&dir, &destination) {
                Ok(_) => {
                    self.record(
                        "skills",
                        Some(&dir),
                        Some(&destination),
                        ImportStatus::Imported,
                        "copied skill",
                    );
                }
                Err(e) => {
                    self.record(
                        "skills",
                        Some(&dir),
                        Some(&destination),
                        ImportStatus::Error,
                        &format!("Could not copy skill: {e}"),
                    );
                }
            }
        }
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(from)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Terminal rendering of an import report (hermes `print_import_report`).
pub fn format_import_report(report: &ImportReport) -> String {
    let mut out = String::new();
    let mode = if report.dry_run {
        " (dry run — nothing written)"
    } else {
        ""
    };
    out.push_str(&format!(
        "Import from {}: {} -> {}{}\n",
        report.agent,
        report.source.display(),
        report.target.display(),
        mode
    ));
    for item in &report.items {
        let source = item
            .source
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        let destination = item
            .destination
            .as_ref()
            .map(|p| format!(" -> {}", p.display()))
            .unwrap_or_default();
        out.push_str(&format!(
            "  [{:<8}] {:<14} {}{}\n",
            item.status.label(),
            item.kind,
            source,
            destination
        ));
        if !item.reason.is_empty() {
            out.push_str(&format!("             {}\n", item.reason));
        }
    }
    let summary = report.summary();
    out.push_str(&format!(
        "Summary: {} imported, {} skipped, {} conflict, {} error\n",
        summary[0].1, summary[1].1, summary[2].1, summary[3].1
    ));
    if !report.stripped_secrets.is_empty() {
        out.push_str(&format!(
            "Stripped secrets (re-add manually): {}\n",
            report.stripped_secrets.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_detection() {
        assert!(is_secret_key("OPENAI_API_KEY"));
        assert!(is_secret_key("GH_TOKEN"));
        assert!(is_secret_key("DB_PASSWORD"));
        assert!(is_secret_key("SOME_KEY"));
        assert!(!is_secret_key("PATH"));
        assert!(!is_secret_key("LOG_LEVEL"));
    }

    #[test]
    fn markdown_entries_headings_bullets_and_code() {
        let text = "\
# Project Notes
Some intro paragraph
spanning two lines.

## Conventions
- use rustfmt
- tests required

```bash
secret code block
```

| table | row |
|-------|-----|

- final bullet
";
        let entries = extract_markdown_entries(text);
        assert!(
            entries
                .contains(&"Project Notes: Some intro paragraph spanning two lines.".to_string()),
            "{entries:?}"
        );
        assert!(
            entries.contains(&"Project Notes > Conventions: use rustfmt".to_string()),
            "{entries:?}"
        );
        assert!(
            entries.contains(&"Project Notes > Conventions: tests required".to_string()),
            "{entries:?}"
        );
        assert!(
            entries.contains(&"Project Notes > Conventions: final bullet".to_string()),
            "{entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e.contains("secret code block")),
            "{entries:?}"
        );
        assert!(!entries.iter().any(|e| e.contains("table")), "{entries:?}");
    }

    #[test]
    fn markdown_entries_dedup() {
        let text = "- same thing\n- Same   Thing\n- other\n";
        let entries = extract_markdown_entries(text);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn claude_rule_conversion() {
        assert_eq!(
            claude_rule_to_command_pattern("Bash(npm run build)"),
            Some("npm run build".to_string())
        );
        assert_eq!(
            claude_rule_to_command_pattern("Bash(npm run test:*)"),
            Some("npm run test*".to_string())
        );
        assert_eq!(claude_rule_to_command_pattern("Bash()"), None);
        assert_eq!(claude_rule_to_command_pattern("Read(src/**)"), None);
    }

    #[test]
    fn merge_entries_dedups_and_limits() {
        let existing = vec!["alpha".to_string()];
        let incoming = vec![
            "alpha".to_string(),
            "beta".to_string(),
            "gamma is a much longer entry".to_string(),
        ];
        let (merged, stats) = merge_entries(&existing, &incoming, 40);
        assert_eq!(stats.existing, 1);
        assert_eq!(stats.duplicates, 1);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.overflowed, 1);
        assert_eq!(merged, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn sanitize_env_strips_secrets() {
        let env = serde_json::json!({
            "API_URL": "https://example.com",
            "SERVICE_TOKEN": "abc",
            "LOG_LEVEL": "debug",
        });
        let (kept, stripped) = sanitize_mcp_env(&env);
        assert!(kept.contains_key("API_URL"));
        assert!(kept.contains_key("LOG_LEVEL"));
        assert!(!kept.contains_key("SERVICE_TOKEN"));
        assert_eq!(stripped, vec!["SERVICE_TOKEN".to_string()]);
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn seed_claude(source: &Path) {
        write(
            &source.join("CLAUDE.md"),
            "# Project\n- prefers concise output\n\n## Stack\n- rust and musl builds\n",
        );
        write(
            &source.join("settings.json"),
            r#"{
                "permissions": {"allow": ["Bash(cargo build)", "Read(x)"]},
                "mcpServers": {
                    "fs": {"command": "npx", "args": ["-y", "mcp-fs"], "env": {"ROOT": "/tmp", "FS_TOKEN": "abc"}}
                }
            }"#,
        );
        write(
            &source.join("skills/my-skill/SKILL.md"),
            "---\nname: my-skill\n---\nbody\n",
        );
    }

    #[test]
    fn claude_code_import_end_to_end() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join(".claude");
        let target = work.path().join(".ulnclaw");
        seed_claude(&source);

        let importer =
            AgentImporter::new("claude-code", source.clone(), target.clone(), true, false).unwrap();
        let report = importer.run();

        // Memory merged.
        let memory = std::fs::read_to_string(target.join("memory/MEMORY.md")).unwrap();
        assert!(memory.contains("prefers concise output"), "{memory}");
        assert!(
            memory.contains("Project > Stack: rust and musl builds"),
            "{memory}"
        );

        // MCP server merged with secret env stripped.
        let config = std::fs::read_to_string(target.join("config.toml")).unwrap();
        assert!(config.contains("name = \"fs\""), "{config}");
        assert!(config.contains("ROOT"), "{config}");
        assert!(!config.contains("FS_TOKEN"), "{config}");
        assert!(report
            .stripped_secrets
            .iter()
            .any(|s| s.contains("FS_TOKEN")));

        // Skill copied under the category dir.
        assert!(target
            .join("skills/claude-code-imports/my-skill/SKILL.md")
            .exists());

        // Permissions reported as skipped (no ulnclaw surface).
        assert!(report
            .items
            .iter()
            .any(|i| i.kind == "permissions" && i.status == ImportStatus::Skipped));

        let summary = report.summary();
        assert!(summary[0].1 >= 3, "{summary:?}");
        assert_eq!(summary[3].1, 0, "{summary:?}");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join(".claude");
        let target = work.path().join(".ulnclaw");
        seed_claude(&source);

        let importer =
            AgentImporter::new("claude-code", source, target.clone(), false, false).unwrap();
        let report = importer.run();
        assert!(report.dry_run);
        assert!(!target.join("memory/MEMORY.md").exists());
        assert!(!target.join("config.toml").exists());
        assert!(!target.join("skills").exists());
        // Plan still reports what would happen.
        assert!(report
            .items
            .iter()
            .any(|i| i.status == ImportStatus::Imported));
    }

    #[test]
    fn codex_import_end_to_end() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join(".codex");
        let target = work.path().join(".ulnclaw");
        write(
            &source.join("AGENTS.md"),
            "# Rules\n- always run cargo test\n",
        );
        write(&source.join("memories/notes.md"), "- likes dark mode\n");
        write(
            &source.join("config.toml"),
            "[mcp_servers.search]\ncommand = \"uvx\"\nargs = [\"mcp-search\"]\n[mcp_servers.search.env]\nSEARCH_API_KEY = \"x\"\nREGION = \"eu\"\n",
        );

        let importer = AgentImporter::new("codex", source, target.clone(), true, false).unwrap();
        let report = importer.run();

        let memory = std::fs::read_to_string(target.join("memory/MEMORY.md")).unwrap();
        assert!(memory.contains("always run cargo test"), "{memory}");
        assert!(memory.contains("likes dark mode"), "{memory}");

        let config = std::fs::read_to_string(target.join("config.toml")).unwrap();
        assert!(config.contains("name = \"search\""), "{config}");
        assert!(config.contains("REGION"), "{config}");
        assert!(!config.contains("SEARCH_API_KEY"), "{config}");
        assert!(report
            .stripped_secrets
            .iter()
            .any(|s| s.contains("SEARCH_API_KEY")));
    }

    #[test]
    fn mcp_conflict_and_existing_memory_preserved() {
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join(".claude");
        let target = work.path().join(".ulnclaw");
        seed_claude(&source);
        write(
            &target.join("config.toml"),
            "[[mcp.servers]]\nname = \"fs\"\ncommand = \"existing\"\n",
        );
        write(&target.join("memory/MEMORY.md"), "- pre-existing note\n");

        let importer =
            AgentImporter::new("claude-code", source, target.clone(), true, false).unwrap();
        let report = importer.run();

        // Conflict recorded, existing server untouched.
        assert!(report
            .items
            .iter()
            .any(|i| i.kind == "mcp-servers" && i.status == ImportStatus::Conflict));
        let config = std::fs::read_to_string(target.join("config.toml")).unwrap();
        assert!(config.contains("command = \"existing\""), "{config}");

        // Existing memory entry preserved + merged.
        let memory = std::fs::read_to_string(target.join("memory/MEMORY.md")).unwrap();
        assert!(memory.contains("pre-existing note"), "{memory}");
        assert!(memory.contains("prefers concise output"), "{memory}");
        // Backup of the original store was written.
        let backups: Vec<_> = std::fs::read_dir(target.join("memory"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("MEMORY.md.bak."))
            .collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn detect_agents_finds_existing_dirs() {
        let work = tempfile::tempdir().unwrap();
        assert!(detect_agents(work.path()).is_empty());
        std::fs::create_dir_all(work.path().join(".codex")).unwrap();
        assert_eq!(detect_agents(work.path()), vec!["codex".to_string()]);
        std::fs::create_dir_all(work.path().join(".claude")).unwrap();
        let found = detect_agents(work.path());
        assert!(found.contains(&"claude-code".to_string()));
        assert!(found.contains(&"codex".to_string()));
    }

    #[test]
    fn unsupported_agent_rejected() {
        let work = tempfile::tempdir().unwrap();
        let err = AgentImporter::new(
            "cursor",
            work.path().to_path_buf(),
            work.path().to_path_buf(),
            false,
            false,
        )
        .unwrap_err();
        assert!(err.contains("Unsupported agent"), "{err}");
    }
}

//! `ulnclaw prompt-size` — what the system prompt costs (hermes
//! `hermes_cli/prompt_size.py` parity).
//!
//! Measures the fixed per-call payload the agent sends on every turn: the
//! system prompt broken into its four tiers (base identity, persistent
//! memory, environment, volatile timestamp/model block), the tool-schema
//! JSON, per-toolset schema sizes, and installed SKILL.md sizes — answering
//! "what should I disable to cut tokens?".

use std::path::Path;

use crate::config::UlncLawConfig;
use crate::tools::ToolRegistry;

/// One labelled prompt tier with char + UTF-8 byte sizes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SectionSize {
    pub label: String,
    pub chars: usize,
    pub bytes: usize,
}

/// Per-toolset schema footprint (hermes `toolsets_breakdown`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolsetRow {
    pub toolset: String,
    pub tools: usize,
    pub json_bytes: usize,
}

/// Installed skill footprint (hermes `skills_breakdown`; ulnclaw loads
/// skills on demand, so this is the on-disk cost per skill).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillRow {
    pub name: String,
    pub skill_md_bytes: usize,
}

/// Full breakdown (hermes `compute_prompt_breakdown` return value).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PromptBreakdown {
    pub model: String,
    pub provider: String,
    pub system_prompt_chars: usize,
    pub system_prompt_bytes: usize,
    pub sections: Vec<SectionSize>,
    pub memory_file_bytes: usize,
    pub user_profile_file_bytes: usize,
    pub tools_count: usize,
    pub tools_json_bytes: usize,
    pub toolsets: Vec<ToolsetRow>,
    pub skills: Vec<SkillRow>,
    pub skills_total_bytes: usize,
}

fn section(label: &str, text: &str) -> SectionSize {
    SectionSize {
        label: label.to_string(),
        chars: text.chars().count(),
        bytes: text.len(),
    }
}

/// Compose the four system-prompt tiers exactly the way
/// `Agent::effective_system_prompt` does (shared building blocks) and
/// measure each one.
pub fn compute_prompt_breakdown(
    config: &UlncLawConfig,
    home: &Path,
    cwd: &Path,
    registry: &ToolRegistry,
) -> PromptBreakdown {
    // Tier 1: base identity prompt.
    let base = crate::agent::DEFAULT_SYSTEM_PROMPT.to_string();

    // Tier 2: persistent memory block (MEMORY.md + USER.md entries).
    let memory_block = crate::tools::builtin::memory::load_memory_for_prompt(home)
        .map(|memory| format!("## Persistent memory\n{}", memory))
        .unwrap_or_default();

    // Tier 3: environment section.
    let mut env_section = format!(
        "## Environment\n- cwd: {}\n- home: {}",
        cwd.display(),
        home.display()
    );
    if config.agent.environment_probe {
        let line = crate::env_probe::get_environment_probe_line(
            &config
                .terminal
                .backend
                .clone()
                .unwrap_or_else(|| "local".to_string()),
        );
        if !line.is_empty() {
            env_section.push_str(&format!("\n- {}", line));
        }
    }

    // Tier 4: volatile timestamp + model/provider block.
    let mut volatile = crate::hermes_time::conversation_started_line(config.timezone.as_deref());
    volatile.push_str(&format!("\nModel: {}", config.model.model));
    volatile.push_str(&format!("\nProvider: {}", config.model.provider));

    let tiers = [
        ("base (identity)", base.as_str()),
        ("memory (MEMORY.md + USER.md)", memory_block.as_str()),
        ("environment (cwd/home/toolchain)", env_section.as_str()),
        ("volatile (date/model/provider)", volatile.as_str()),
    ];
    let sections: Vec<SectionSize> = tiers
        .iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(label, text)| section(label, text))
        .collect();
    let system_prompt = tiers
        .iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(_, text)| text.to_string())
        .collect::<Vec<_>>()
        .join("\n\n");

    // Tool schema JSON — the other half of the fixed per-call payload.
    let definitions = registry.definitions();
    let tools_json_bytes = serde_json::to_vec(&definitions)
        .map(|v| v.len())
        .unwrap_or(0);

    // Per-toolset schema sizes, largest first (hermes _compute_toolsets_breakdown).
    let mut toolsets: Vec<ToolsetRow> = registry
        .toolset_names()
        .into_iter()
        .map(|name| {
            let tools = registry.toolset_tools(&name);
            let json_bytes: usize = tools
                .iter()
                .map(|tool| {
                    serde_json::to_vec(&tool.definition)
                        .map(|v| v.len())
                        .unwrap_or(0)
                })
                .sum();
            ToolsetRow {
                toolset: name,
                tools: tools.len(),
                json_bytes,
            }
        })
        .collect();
    toolsets.sort_by(|a, b| {
        b.json_bytes
            .cmp(&a.json_bytes)
            .then(a.toolset.cmp(&b.toolset))
    });

    // Installed skills, largest SKILL.md first (hermes skills_breakdown).
    let skills_dir = home.join("skills");
    let mut skills: Vec<SkillRow> = crate::skills::list_skills(&skills_dir)
        .into_iter()
        .map(|skill| {
            let bytes = std::fs::read(skill.path.join("SKILL.md"))
                .map(|v| v.len())
                .unwrap_or(0);
            SkillRow {
                name: skill.name,
                skill_md_bytes: bytes,
            }
        })
        .collect();
    skills.sort_by(|a, b| {
        b.skill_md_bytes
            .cmp(&a.skill_md_bytes)
            .then(a.name.cmp(&b.name))
    });
    let skills_total_bytes: usize = skills.iter().map(|s| s.skill_md_bytes).sum();

    let file_bytes = |name: &str| {
        std::fs::read(home.join("memory").join(name))
            .map(|v| v.len())
            .unwrap_or(0)
    };

    PromptBreakdown {
        model: config.model.model.clone(),
        provider: config.model.provider.clone(),
        system_prompt_chars: system_prompt.chars().count(),
        system_prompt_bytes: system_prompt.len(),
        sections,
        memory_file_bytes: file_bytes("MEMORY.md"),
        user_profile_file_bytes: file_bytes("USER.md"),
        tools_count: definitions.len(),
        tools_json_bytes,
        toolsets,
        skills,
        skills_total_bytes,
    }
}

fn fmt_kb(bytes: usize) -> String {
    format!("{:.1} KB", bytes as f64 / 1024.0)
}

/// Terminal rendering (hermes `render_breakdown`).
pub fn render_breakdown(data: &PromptBreakdown) -> String {
    let mut out = String::new();
    out.push_str("ulnclaw prompt-size — fixed per-call payload\n");
    out.push_str(&format!(
        "model: {}   provider: {}\n\n",
        data.model, data.provider
    ));

    out.push_str(&format!(
        "System prompt: {} ({} chars)\n",
        fmt_kb(data.system_prompt_bytes),
        data.system_prompt_chars
    ));
    for section in &data.sections {
        out.push_str(&format!(
            "  {:<32} {}\n",
            section.label,
            fmt_kb(section.bytes)
        ));
    }

    out.push_str(&format!(
        "\nMemory files: MEMORY.md {}   USER.md {}\n",
        fmt_kb(data.memory_file_bytes),
        fmt_kb(data.user_profile_file_bytes)
    ));

    out.push_str(&format!(
        "Tools: {} tools, {} of JSON schema\n",
        data.tools_count,
        fmt_kb(data.tools_json_bytes)
    ));
    out.push_str("\nToolsets (largest schema first):\n");
    for row in &data.toolsets {
        out.push_str(&format!(
            "  {:<16} {:>3} tools   {:>8}\n",
            row.toolset,
            row.tools,
            fmt_kb(row.json_bytes)
        ));
    }

    out.push_str(&format!(
        "\nSkills: {} installed, {} of SKILL.md on disk (loaded on demand, not in the base prompt)\n",
        data.skills.len(),
        fmt_kb(data.skills_total_bytes)
    ));
    if !data.skills.is_empty() {
        out.push_str("Skills (largest first):\n");
        for row in data.skills.iter().take(15) {
            out.push_str(&format!(
                "  {:<24} {:>8}\n",
                row.name,
                fmt_kb(row.skill_md_bytes)
            ));
        }
        if data.skills.len() > 15 {
            out.push_str(&format!("  … {} more\n", data.skills.len() - 15));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::builtin::register_builtin_tools;

    fn seeded_home(dir: &Path) {
        let memory = dir.join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("MEMORY.md"), "- note one\n- note two\n").unwrap();
        std::fs::write(memory.join("USER.md"), "- concise please\n").unwrap();
        let skill_dir = dir.join("skills/big-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: big-skill\n---\n{}", "x".repeat(4096)),
        )
        .unwrap();
        let small = dir.join("skills/small-skill");
        std::fs::create_dir_all(&small).unwrap();
        std::fs::write(small.join("SKILL.md"), "---\nname: small-skill\n---\ntiny").unwrap();
    }

    #[test]
    fn breakdown_measures_all_sections() {
        let dir = tempfile::tempdir().unwrap();
        seeded_home(dir.path());
        let config = UlncLawConfig::default();
        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let data = compute_prompt_breakdown(&config, dir.path(), dir.path(), &registry);

        assert!(data.system_prompt_bytes > 0);
        let labels: Vec<&str> = data.sections.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.iter().any(|l| l.starts_with("base")), "{labels:?}");
        assert!(labels.iter().any(|l| l.starts_with("memory")), "{labels:?}");
        assert!(
            labels.iter().any(|l| l.starts_with("environment")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.starts_with("volatile")),
            "{labels:?}"
        );

        // Memory tier contains the seeded entries.
        let memory_section = data
            .sections
            .iter()
            .find(|s| s.label.starts_with("memory"))
            .unwrap();
        assert!(memory_section.bytes > 30);

        assert!(data.tools_count > 10);
        assert!(data.tools_json_bytes > 1024);
        assert_eq!(data.memory_file_bytes, "- note one\n- note two\n".len());
        assert_eq!(data.user_profile_file_bytes, "- concise please\n".len());
    }

    #[test]
    fn toolsets_sorted_largest_first() {
        let dir = tempfile::tempdir().unwrap();
        let config = UlncLawConfig::default();
        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let data = compute_prompt_breakdown(&config, dir.path(), dir.path(), &registry);
        assert!(!data.toolsets.is_empty());
        for pair in data.toolsets.windows(2) {
            assert!(pair[0].json_bytes >= pair[1].json_bytes);
        }
    }

    #[test]
    fn skills_sorted_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        seeded_home(dir.path());
        let config = UlncLawConfig::default();
        let registry = ToolRegistry::new();
        let data = compute_prompt_breakdown(&config, dir.path(), dir.path(), &registry);
        assert_eq!(data.skills.len(), 2);
        assert_eq!(data.skills[0].name, "big-skill");
        assert!(data.skills[0].skill_md_bytes > data.skills[1].skill_md_bytes);
        assert_eq!(
            data.skills_total_bytes,
            data.skills[0].skill_md_bytes + data.skills[1].skill_md_bytes
        );
    }

    #[test]
    fn render_covers_headlines() {
        let dir = tempfile::tempdir().unwrap();
        seeded_home(dir.path());
        let config = UlncLawConfig::default();
        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let data = compute_prompt_breakdown(&config, dir.path(), dir.path(), &registry);
        let out = render_breakdown(&data);
        for needle in [
            "ulnclaw prompt-size",
            "System prompt:",
            "Memory files:",
            "Tools:",
            "Toolsets (largest schema first):",
            "Skills (largest first):",
            "big-skill",
        ] {
            assert!(out.contains(needle), "missing {needle}\n{out}");
        }
    }

    #[test]
    fn empty_home_still_renders() {
        let dir = tempfile::tempdir().unwrap();
        let config = UlncLawConfig::default();
        let registry = ToolRegistry::new();
        let data = compute_prompt_breakdown(&config, dir.path(), dir.path(), &registry);
        // No memory files -> no memory tier, but base/environment/volatile remain.
        assert_eq!(data.sections.len(), 3);
        assert_eq!(data.memory_file_bytes, 0);
        let out = render_breakdown(&data);
        assert!(out.contains("System prompt:"), "{out}");
    }
}

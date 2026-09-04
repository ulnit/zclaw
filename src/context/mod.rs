//! Context management - prompt building and compression
//!
//! Inspired by Hermes Agent's prompt_builder.py and context_compressor.py.
//! Handles system prompt assembly and context window optimization.

pub mod breakdown;
pub mod compressor;
pub use compressor::ContextCompressor;

/// Prompt builder - assembles system prompts from multiple layers
///
/// Follows Hermes Agent's tiered approach:
/// - Stable tier: identity, tool guidance, skills
/// - Context tier: context files, environment hints
/// - Volatile tier: memory, profile, timestamp
pub struct PromptBuilder {
    /// Agent identity (who the agent is)
    identity: Option<String>,
    /// Tool usage guidance
    tool_guidance: Option<String>,
    /// Skills/instructions
    skills: Vec<String>,
    /// Context files content
    context_files: Vec<String>,
    /// Environment hints (OS, cwd, timezone, etc.)
    environment_hints: Vec<(String, String)>,
    /// Persistent memory
    memory: Option<String>,
    /// Custom suffix
    suffix: Option<String>,
}

impl Default for PromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptBuilder {
    pub fn new() -> Self {
        Self {
            identity: None,
            tool_guidance: None,
            skills: Vec::new(),
            context_files: Vec::new(),
            environment_hints: Vec::new(),
            memory: None,
            suffix: None,
        }
    }

    /// Set the agent identity
    pub fn identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = Some(identity.into());
        self
    }

    /// Set tool usage guidance
    pub fn tool_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.tool_guidance = Some(guidance.into());
        self
    }

    /// Add a skill/instruction
    pub fn add_skill(mut self, skill: impl Into<String>) -> Self {
        self.skills.push(skill.into());
        self
    }

    /// Add a context file
    pub fn add_context_file(mut self, content: impl Into<String>) -> Self {
        self.context_files.push(content.into());
        self
    }

    /// Add an environment hint
    pub fn add_env_hint(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment_hints.push((key.into(), value.into()));
        self
    }

    /// Set persistent memory
    pub fn memory(mut self, memory: impl Into<String>) -> Self {
        self.memory = Some(memory.into());
        self
    }

    /// Set custom suffix
    pub fn suffix(mut self, suffix: impl Into<String>) -> Self {
        self.suffix = Some(suffix.into());
        self
    }

    /// Build the complete system prompt
    pub fn build(&self) -> String {
        let mut parts = Vec::new();

        // Stable tier: identity
        if let Some(ref identity) = self.identity {
            parts.push(identity.clone());
        }

        // Stable tier: tool guidance
        if let Some(ref guidance) = self.tool_guidance {
            parts.push(guidance.clone());
        }

        // Stable tier: skills
        if !self.skills.is_empty() {
            parts.push("## Skills".to_string());
            for skill in &self.skills {
                parts.push(skill.clone());
            }
        }

        // Context tier: context files
        if !self.context_files.is_empty() {
            parts.push("## Context".to_string());
            for ctx in &self.context_files {
                parts.push(ctx.clone());
            }
        }

        // Context tier: environment hints
        if !self.environment_hints.is_empty() {
            parts.push("## Environment".to_string());
            for (key, value) in &self.environment_hints {
                parts.push(format!("- {}: {}", key, value));
            }
        }

        // Volatile tier: memory
        if let Some(ref memory) = self.memory {
            parts.push("## Memory".to_string());
            parts.push(memory.clone());
        }

        // Volatile tier: timestamp
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        parts.push(format!(
            "Current time: {}",
            format!("unix_timestamp: {}", now)
        ));

        // Custom suffix
        if let Some(ref suffix) = self.suffix {
            parts.push(suffix.clone());
        }

        parts.join("\n\n")
    }
}

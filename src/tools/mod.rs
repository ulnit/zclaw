//! Tool registry — port of hermes' tools/registry.py
//!
//! Tools self-register with a JSON schema, a toolset, an async handler, an
//! optional `check_fn` availability gate, and optional confirmation
//! requirements for dangerous operations.

pub mod approval;
pub mod builtin;
pub mod context;
pub mod fuzzy;
pub mod hints;

pub use context::ToolContext;

use crate::error::{AgentError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::debug;

/// Definition of a tool (schema exposed to the model)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Availability check result — port of hermes `check_fn`.
#[derive(Debug, Clone)]
pub enum ToolAvailability {
    Available,
    /// Unavailable with a reason (tool hidden from the model schema).
    Unavailable(String),
}

impl ToolAvailability {
    pub fn available() -> Self {
        Self::Available
    }
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable(reason.into())
    }
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Type alias for the availability gate.
pub type CheckFn = Arc<dyn Fn() -> ToolAvailability + Send + Sync>;

/// A tool with its definition and async handler
pub struct Tool {
    pub definition: ToolDefinition,
    pub handler: ToolHandler,
    /// Toolset this tool belongs to (for grouping/enable/disable)
    pub toolset: String,
    /// Whether this tool requires user confirmation before execution
    pub dangerous: bool,
    /// Emoji shown in UI logs (hermes `emoji`).
    pub emoji: String,
    /// Max result size in characters (hermes `max_result_size_chars`).
    pub max_result_size_chars: usize,
    /// Availability gate (hermes `check_fn`).
    pub check_fn: Option<CheckFn>,
}

/// Type alias for async tool handler function.
/// Handlers receive the arguments plus the shared tool context.
pub type ToolHandler = Arc<
    dyn Fn(
            serde_json::Value,
            Arc<ToolContext>,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send>>
        + Send
        + Sync,
>;

/// Result of a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl ToolResult {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(msg.into()),
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(
            |_| serde_json::json!({"success": false, "error": "Failed to serialize result"}),
        )
    }
}

/// Central registry for all tools
pub struct ToolRegistry {
    tools: HashMap<String, Tool>,
    /// Toolset name -> list of tool names
    toolsets: HashMap<String, Vec<String>>,
    /// Disabled toolsets
    disabled_toolsets: Vec<String>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            toolsets: HashMap::new(),
            disabled_toolsets: Vec::new(),
        }
    }

    /// Register a tool
    pub fn register(&mut self, tool: Tool) {
        let name = tool.definition.name.clone();
        let toolset = tool.toolset.clone();

        self.toolsets.entry(toolset).or_default().push(name.clone());
        self.tools.insert(name.clone(), tool);
        debug!("Registered tool: {}", name);
    }

    /// Unregister a tool by name
    pub fn unregister(&mut self, name: &str) -> Option<Tool> {
        if let Some(tool) = self.tools.remove(name) {
            if let Some(tools) = self.toolsets.get_mut(&tool.toolset) {
                tools.retain(|n| n != name);
            }
            Some(tool)
        } else {
            None
        }
    }

    /// Get a tool by name
    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    /// Dispatch a tool call — checks toolset enablement and availability.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: serde_json::Value,
        context: Arc<ToolContext>,
    ) -> Result<serde_json::Value> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| AgentError::ToolNotFound(name.to_string()))?;

        if self.disabled_toolsets.contains(&tool.toolset) {
            return Err(AgentError::Tool(format!(
                "Toolset '{}' is disabled",
                tool.toolset
            )));
        }

        if let Some(ref check) = tool.check_fn {
            if let ToolAvailability::Unavailable(reason) = check() {
                return Err(AgentError::Tool(format!(
                    "Tool '{}' is unavailable: {}",
                    name, reason
                )));
            }
        }

        debug!("Dispatching tool: {}", name);
        let mut result = (tool.handler)(arguments, context).await?;
        // Enforce max result size (hermes truncates oversized tool output).
        let limit = tool.max_result_size_chars;
        if let Some(text) = result.as_str() {
            if text.chars().count() > limit {
                let truncated: String = text.chars().take(limit).collect();
                result = serde_json::json!(format!(
                    "{}\n\n[output truncated at {} chars]",
                    truncated, limit
                ));
            }
        }
        Ok(result)
    }

    /// Get all enabled + available tool definitions (for sending to the model)
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = self
            .tools
            .values()
            .filter(|tool| !self.disabled_toolsets.contains(&tool.toolset))
            .filter(|tool| {
                tool.check_fn
                    .as_ref()
                    .map(|check| check().is_available())
                    .unwrap_or(true)
            })
            .map(|tool| tool.definition.clone())
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Get all tool names
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }

    /// Check if a tool exists
    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Get the number of registered tools
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if registry is empty
    pub fn is_empty(&self) -> bool {
        self.tools.len() == 0
    }

    /// Enable a toolset
    pub fn enable_toolset(&mut self, name: &str) {
        self.disabled_toolsets.retain(|n| n != name);
    }

    /// Disable a toolset
    pub fn disable_toolset(&mut self, name: &str) {
        if !self.disabled_toolsets.contains(&name.to_string()) {
            self.disabled_toolsets.push(name.to_string());
        }
    }

    /// Get all toolset names
    pub fn toolset_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.toolsets.keys().cloned().collect();
        names.sort();
        names
    }

    /// Names of toolsets that are not disabled (banner display — hermes
    /// restricts the banner to toolsets actually enabled for the agent).
    pub fn enabled_toolset_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .toolsets
            .keys()
            .filter(|name| !self.disabled_toolsets.contains(name))
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Get tools in a specific toolset
    pub fn toolset_tools(&self, toolset: &str) -> Vec<&Tool> {
        self.toolsets
            .get(toolset)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| self.tools.get(name))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Builder for creating tools with a fluent API
pub struct ToolBuilder {
    name: String,
    description: String,
    parameters: serde_json::Value,
    handler: Option<ToolHandler>,
    toolset: String,
    dangerous: bool,
    emoji: String,
    max_result_size_chars: usize,
    check_fn: Option<CheckFn>,
}

impl ToolBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            handler: None,
            toolset: "default".to_string(),
            dangerous: false,
            emoji: String::new(),
            max_result_size_chars: 100_000,
            check_fn: None,
        }
    }

    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    pub fn parameters(mut self, params: serde_json::Value) -> Self {
        self.parameters = params;
        self
    }

    pub fn handler<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(serde_json::Value, Arc<ToolContext>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<serde_json::Value>> + Send + 'static,
    {
        self.handler = Some(Arc::new(move |args, ctx| Box::pin(handler(args, ctx))));
        self
    }

    pub fn toolset(mut self, toolset: impl Into<String>) -> Self {
        self.toolset = toolset.into();
        self
    }

    pub fn dangerous(mut self, dangerous: bool) -> Self {
        self.dangerous = dangerous;
        self
    }

    pub fn emoji(mut self, emoji: impl Into<String>) -> Self {
        self.emoji = emoji.into();
        self
    }

    pub fn max_result_size_chars(mut self, limit: usize) -> Self {
        self.max_result_size_chars = limit;
        self
    }

    pub fn check_fn<F>(mut self, check: F) -> Self
    where
        F: Fn() -> ToolAvailability + Send + Sync + 'static,
    {
        self.check_fn = Some(Arc::new(check));
        self
    }

    pub fn build(self) -> Result<Tool> {
        let handler = self
            .handler
            .ok_or_else(|| AgentError::config("Tool handler is required"))?;

        Ok(Tool {
            definition: ToolDefinition {
                name: self.name,
                description: self.description,
                parameters: self.parameters,
            },
            handler,
            toolset: self.toolset,
            dangerous: self.dangerous,
            emoji: self.emoji,
            max_result_size_chars: self.max_result_size_chars,
            check_fn: self.check_fn,
        })
    }
}

/// Convenience function to create a tool builder
pub fn tool(name: impl Into<String>) -> ToolBuilder {
    ToolBuilder::new(name)
}

//! Tool registry and dispatch for the native executor.
//!
//! Provides `ToolRegistry` that maps tool names to implementations,
//! generates JSON Schema definitions for the API, and dispatches calls.

pub mod bash;
pub mod file;
pub mod wg;

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use serde_json;

use super::client::ToolDefinition;

/// Output from executing a tool.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(content: String) -> Self {
        Self {
            content,
            is_error: false,
        }
    }

    pub fn error(message: String) -> Self {
        Self {
            content: message,
            is_error: true,
        }
    }
}

/// Trait that all tools must implement.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's name (used for dispatch and API registration).
    fn name(&self) -> &str;

    /// JSON Schema definition for the API.
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool with the given JSON input.
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput;
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
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
        }
    }

    /// Register a tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Get JSON Schema definitions for all registered tools (for API request).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// Execute a tool by name.
    pub async fn execute(&self, name: &str, input: &serde_json::Value) -> ToolOutput {
        match self.tools.get(name) {
            Some(tool) => tool.execute(input).await,
            None => ToolOutput::error(format!("Unknown tool: {}", name)),
        }
    }

    /// Create a filtered registry containing only the named tools.
    pub fn filter(mut self, allowed: &[String]) -> ToolRegistry {
        let wildcard = allowed.iter().any(|s| s == "*");
        if wildcard {
            return self;
        }

        let mut filtered = ToolRegistry::new();
        for name in allowed {
            if let Some(tool) = self.tools.remove(name) {
                filtered.tools.insert(name.clone(), tool);
            }
        }
        filtered
    }

    /// Create the full default registry with all tools.
    pub fn default_all(workgraph_dir: &Path, working_dir: &Path) -> Self {
        let mut registry = Self::new();

        // File tools
        file::register_file_tools(&mut registry);

        // Bash tool
        bash::register_bash_tool(&mut registry, working_dir.to_path_buf());

        // Workgraph tools
        wg::register_wg_tools(&mut registry, workgraph_dir.to_path_buf());

        registry
    }
}

/// Maximum tool output size (100KB) to prevent context overflow.
const MAX_TOOL_OUTPUT_SIZE: usize = 100 * 1024;

/// Truncate tool output if it exceeds the maximum size.
pub fn truncate_output(output: String) -> String {
    if output.len() > MAX_TOOL_OUTPUT_SIZE {
        let truncated = &output[..output.floor_char_boundary(MAX_TOOL_OUTPUT_SIZE)];
        format!(
            "{}\n\n[Output truncated: {} bytes total, showing first {}]",
            truncated,
            output.len(),
            MAX_TOOL_OUTPUT_SIZE
        )
    } else {
        output
    }
}

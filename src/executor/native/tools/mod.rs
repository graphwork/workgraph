//! Tool registry and dispatch for the native executor.
//!
//! Provides `ToolRegistry` that maps tool names to implementations,
//! generates JSON Schema definitions for the API, and dispatches calls.

pub mod bash;
pub mod bg;
pub mod file;
pub mod file_cache;
pub mod wg;
pub mod web_search;

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use futures_util::future::join_all;
use serde_json;
use tokio::sync::Semaphore;

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

/// Callback type for streaming tool output chunks.
pub type ToolStreamCallback =
    Box<dyn Fn(String) + Send + Sync>;

/// Trait that all tools must implement.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's name (used for dispatch and API registration).
    fn name(&self) -> &str;

    /// JSON Schema definition for the API.
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool with the given JSON input.
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput;

    /// Whether this tool is read-only (safe to execute concurrently).
    /// Read-only tools never modify files, state, or external systems.
    /// Default: false (conservative — unknown tools are treated as mutating).
    fn is_read_only(&self) -> bool {
        false
    }

    /// Execute the tool with streaming output support.
    ///
    /// The callback is invoked for each chunk of output as it arrives
    /// (e.g., each line for bash). Default implementation just calls
    /// `execute()` and streams nothing.
    async fn execute_streaming(
        &self,
        input: &serde_json::Value,
        on_chunk: ToolStreamCallback,
    ) -> ToolOutput {
        // Default: fall back to non-streaming
        let _ = on_chunk;
        self.execute(input).await
    }
}

/// Default maximum concurrent read-only tool executions.
pub const DEFAULT_MAX_CONCURRENT_TOOLS: usize = 10;

/// A tool call request (name + input).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub input: serde_json::Value,
}

/// Result of a single tool call within a batch.
#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub name: String,
    pub output: ToolOutput,
    pub duration_ms: u64,
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

    /// Execute a tool by name with streaming output support.
    pub async fn execute_streaming(
        &self,
        name: &str,
        input: &serde_json::Value,
        on_chunk: ToolStreamCallback,
    ) -> ToolOutput {
        match self.tools.get(name) {
            Some(tool) => tool.execute_streaming(input, on_chunk).await,
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

    /// Check whether a tool is read-only by name.
    pub fn is_read_only(&self, name: &str) -> bool {
        self.tools.get(name).map_or(false, |t| t.is_read_only())
    }

    /// Execute a batch of tool calls with parallelism for read-only tools.
    ///
    /// Partitions calls into read-only and mutating. Read-only calls execute
    /// concurrently (up to `max_concurrent`), then mutating calls execute serially.
    /// Results are returned in the original call order.
    pub async fn execute_batch(
        &self,
        calls: &[ToolCall],
        max_concurrent: usize,
    ) -> Vec<ToolCallResult> {
        // Separate into (index, call) pairs by type
        let mut read_only: Vec<(usize, &ToolCall)> = Vec::new();
        let mut mutating: Vec<(usize, &ToolCall)> = Vec::new();

        for (i, call) in calls.iter().enumerate() {
            if self.is_read_only(&call.name) {
                read_only.push((i, call));
            } else {
                mutating.push((i, call));
            }
        }

        let mut results: Vec<(usize, ToolCallResult)> = Vec::with_capacity(calls.len());

        // Execute read-only calls concurrently with semaphore-based cap.
        // Uses join_all (not tokio::spawn) so we borrow &self without 'static.
        if !read_only.is_empty() {
            let semaphore = Semaphore::new(max_concurrent);

            let futures: Vec<_> = read_only
                .iter()
                .map(|(idx, call)| {
                    let sem = &semaphore;
                    async move {
                        let _permit = sem.acquire().await.unwrap();
                        let start = std::time::Instant::now();
                        let output = match self.tools.get(&call.name) {
                            Some(tool) => tool.execute(&call.input).await,
                            None => ToolOutput::error(format!("Unknown tool: {}", call.name)),
                        };
                        let duration_ms = start.elapsed().as_millis() as u64;
                        (
                            *idx,
                            ToolCallResult {
                                name: call.name.clone(),
                                output,
                                duration_ms,
                            },
                        )
                    }
                })
                .collect();

            let read_results = join_all(futures).await;
            results.extend(read_results);
        }

        // Execute mutating calls serially
        for (idx, call) in &mutating {
            let start = std::time::Instant::now();
            let output = self.execute(&call.name, &call.input).await;
            let duration_ms = start.elapsed().as_millis() as u64;
            results.push((
                *idx,
                ToolCallResult {
                    name: call.name.clone(),
                    output,
                    duration_ms,
                },
            ));
        }

        // Sort by original index to maintain call order
        results.sort_by_key(|(idx, _)| *idx);
        results.into_iter().map(|(_, r)| r).collect()
    }

    /// Execute a batch of tool calls with streaming output for each tool.
    ///
    /// Each tool's output is streamed via its own callback. This is used for
    /// bash tools where we want to see incremental output.
    pub async fn execute_batch_streaming(
        &self,
        calls: &[ToolCall],
        max_concurrent: usize,
        make_stream_callback: impl Fn(usize) -> ToolStreamCallback + Clone,
    ) -> Vec<ToolCallResult> {
        use std::sync::Arc;

        // Separate into (index, call) pairs by type
        let mut read_only: Vec<(usize, &ToolCall)> = Vec::new();
        let mut mutating: Vec<(usize, &ToolCall)> = Vec::new();

        for (i, call) in calls.iter().enumerate() {
            if self.is_read_only(&call.name) {
                read_only.push((i, call));
            } else {
                mutating.push((i, call));
            }
        }

        let mut results: Vec<(usize, ToolCallResult)> = Vec::with_capacity(calls.len());

        // Execute read-only calls concurrently with semaphore-based cap.
        if !read_only.is_empty() {
            let semaphore = Arc::new(Semaphore::new(max_concurrent));
            let tools = Arc::new(&self.tools);

            let futures: Vec<_> = read_only
                .iter()
                .map(|(idx, call)| {
                    let sem = Arc::clone(&semaphore);
                    let tools = Arc::clone(&tools);
                    let cb = make_stream_callback(*idx);
                    async move {
                        let _permit = sem.acquire().await.unwrap();
                        let start = std::time::Instant::now();
                        let output = match tools.get(&call.name) {
                            Some(tool) => tool.execute_streaming(&call.input, cb).await,
                            None => ToolOutput::error(format!("Unknown tool: {}", call.name)),
                        };
                        let duration_ms = start.elapsed().as_millis() as u64;
                        (*idx, ToolCallResult {
                            name: call.name.clone(),
                            output,
                            duration_ms,
                        })
                    }
                })
                .collect();

            let read_results = join_all(futures).await;
            results.extend(read_results);
        }

        // Execute mutating calls serially
        for (idx, call) in &mutating {
            let start = std::time::Instant::now();
            let cb = make_stream_callback(*idx);
            let output = self.execute_streaming(&call.name, &call.input, cb).await;
            let duration_ms = start.elapsed().as_millis() as u64;
            results.push((
                *idx,
                ToolCallResult {
                    name: call.name.clone(),
                    output,
                    duration_ms,
                },
            ));
        }

        // Sort by original index to maintain call order
        results.sort_by_key(|(idx, _)| *idx);
        results.into_iter().map(|(_, r)| r).collect()
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

        // Web search tool
        web_search::register_web_search_tool(&mut registry);

        // Background job tool
        bg::register_bg_tool(&mut registry, workgraph_dir.to_path_buf());

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

/// Per-tool output size limits for smart truncation.
pub struct ToolTruncationConfig {
    /// Maximum character count before truncation kicks in.
    pub max_chars: usize,
}

impl ToolTruncationConfig {
    /// Returns the truncation config for a given tool name.
    pub fn for_tool(tool_name: &str) -> Self {
        let max_chars = match tool_name {
            "bash" => 8_000,
            "read_file" => 16_000,
            "grep" => 4_000,
            "glob" => 4_000,
            "wg_show" => 2_000,
            "wg_list" => 4_000,
            "web_search" => 16_000,
            _ => MAX_TOOL_OUTPUT_SIZE,
        };
        Self { max_chars }
    }
}

/// Smart truncation with head+tail preservation.
///
/// When output exceeds `max_chars`, shows the first half and last half
/// with an omission notice in between.
pub fn truncate_tool_output(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }

    let total_chars = output.len();
    let total_lines = output.lines().count();

    let half = max_chars / 2;
    let head_end = output.floor_char_boundary(half);
    let raw_tail_start = total_chars.saturating_sub(half);
    let tail_start = output.floor_char_boundary(raw_tail_start).max(head_end);

    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let omitted_chars = total_chars - head.len() - tail.len();
    let head_lines = head.lines().count();
    let tail_lines = tail.lines().count();
    let omitted_lines = total_lines.saturating_sub(head_lines + tail_lines);

    format!(
        "{}\n\n[... {} chars omitted ({} lines). \
         Showing first/last ~{} chars. \
         Use read_file or grep for specific content. ...]\n\n{}",
        head, omitted_chars, omitted_lines, half, tail
    )
}

/// Apply smart truncation for a specific tool type.
pub fn truncate_for_tool(output: &str, tool_name: &str) -> String {
    let config = ToolTruncationConfig::for_tool(tool_name);
    truncate_tool_output(output, config.max_chars)
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn test_truncation_under_limit_passthrough() {
        let short = "hello world";
        let result = truncate_tool_output(short, 8_000);
        assert_eq!(result, short);
    }

    #[test]
    fn test_truncation_exact_limit_passthrough() {
        let exact = "a".repeat(8_000);
        let result = truncate_tool_output(&exact, 8_000);
        assert_eq!(result, exact);
    }

    #[test]
    fn test_truncation_preserves_head_tail() {
        let head_content = "HEAD_START\n".repeat(100);
        let middle_content = "MIDDLE_FILLER\n".repeat(500);
        let tail_content = "TAIL_END\n".repeat(100);
        let full = format!("{}{}{}", head_content, middle_content, tail_content);

        let result = truncate_tool_output(&full, 4_000);

        assert!(result.starts_with("HEAD_START"));
        assert!(result.ends_with("TAIL_END\n"));
        assert!(result.contains("chars omitted"));
        assert!(result.contains("lines)"));
        assert!(result.contains("Use read_file or grep"));
        assert!(result.len() < full.len());
    }

    #[test]
    fn test_truncation_bash() {
        let config = ToolTruncationConfig::for_tool("bash");
        assert_eq!(config.max_chars, 8_000);

        let big_output = "x".repeat(10_000);
        let result = truncate_tool_output(&big_output, config.max_chars);
        assert!(result.contains("chars omitted"));
        assert!(result.starts_with("xxxx"));
        assert!(result.ends_with("xxxx"));
    }

    #[test]
    fn test_truncation_configs() {
        assert_eq!(ToolTruncationConfig::for_tool("bash").max_chars, 8_000);
        assert_eq!(
            ToolTruncationConfig::for_tool("read_file").max_chars,
            16_000
        );
        assert_eq!(ToolTruncationConfig::for_tool("grep").max_chars, 4_000);
        assert_eq!(ToolTruncationConfig::for_tool("glob").max_chars, 4_000);
        assert_eq!(ToolTruncationConfig::for_tool("wg_show").max_chars, 2_000);
        assert_eq!(ToolTruncationConfig::for_tool("wg_list").max_chars, 4_000);
        assert_eq!(
            ToolTruncationConfig::for_tool("unknown").max_chars,
            MAX_TOOL_OUTPUT_SIZE
        );
    }

    #[test]
    fn test_truncation_omission_notice_has_counts() {
        let lines: Vec<String> = (0..1000)
            .map(|i| format!("line {}: some content here", i))
            .collect();
        let big = lines.join("\n");
        let result = truncate_tool_output(&big, 2_000);

        assert!(result.contains("chars omitted"));
        assert!(result.contains("lines)"));
    }

    #[test]
    fn test_truncation_multibyte_safe() {
        let content = "\u{1f980}".repeat(5000);
        let result = truncate_tool_output(&content, 4_000);
        assert!(result.contains("chars omitted"));
    }
}

#[cfg(test)]
mod parallelism_tests {
    use super::*;

    /// Minimal test tool for unit tests.
    struct TestTool {
        tool_name: String,
        read_only: bool,
    }

    #[async_trait]
    impl Tool for TestTool {
        fn name(&self) -> &str {
            &self.tool_name
        }

        fn definition(&self) -> crate::executor::native::client::ToolDefinition {
            crate::executor::native::client::ToolDefinition {
                name: self.tool_name.clone(),
                description: "test".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        fn is_read_only(&self) -> bool {
            self.read_only
        }

        async fn execute(&self, _input: &serde_json::Value) -> ToolOutput {
            ToolOutput::success(format!("ok-{}", self.tool_name))
        }
    }

    #[test]
    fn test_is_read_only_query() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TestTool {
            tool_name: "reader".to_string(),
            read_only: true,
        }));
        registry.register(Box::new(TestTool {
            tool_name: "writer".to_string(),
            read_only: false,
        }));

        assert!(registry.is_read_only("reader"));
        assert!(!registry.is_read_only("writer"));
        assert!(!registry.is_read_only("missing"));
    }

    #[tokio::test]
    async fn test_execute_batch_preserves_order() {
        let mut registry = ToolRegistry::new();
        for name in &["a", "b", "c"] {
            registry.register(Box::new(TestTool {
                tool_name: name.to_string(),
                read_only: true,
            }));
        }

        let calls = vec![
            ToolCall {
                name: "c".to_string(),
                input: serde_json::json!({}),
            },
            ToolCall {
                name: "a".to_string(),
                input: serde_json::json!({}),
            },
            ToolCall {
                name: "b".to_string(),
                input: serde_json::json!({}),
            },
        ];

        let results = registry.execute_batch(&calls, 10).await;
        assert_eq!(results[0].name, "c");
        assert_eq!(results[1].name, "a");
        assert_eq!(results[2].name, "b");
    }

    #[tokio::test]
    async fn test_execute_batch_empty() {
        let registry = ToolRegistry::new();
        let results = registry.execute_batch(&[], 10).await;
        assert!(results.is_empty());
    }

    #[test]
    fn test_default_max_concurrent() {
        assert_eq!(DEFAULT_MAX_CONCURRENT_TOOLS, 10);
    }
}

//! Delegate tool: in-process lightweight subtask delegation.
//!
//! Spawns a mini agent loop within the current agent's process, runs a short
//! conversation with restricted tools, and returns the result text as tool output.
//! The child's tokens don't count against the parent's context.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolOutput, ToolRegistry, truncate_for_tool, truncate_tool_output};
#[cfg(test)]
use crate::executor::native::client::Usage;
use crate::executor::native::client::{
    ContentBlock, Message, MessagesRequest, MessagesResponse, Role, StopReason, ToolDefinition,
};
use crate::executor::native::provider::Provider;

/// Maximum output chars for delegate results.
const MAX_DELEGATE_OUTPUT_CHARS: usize = 8_000;

/// Default max turns for delegated sub-agents.
const DEFAULT_MAX_TURNS: usize = 5;

/// Maximum allowed max_turns to prevent runaway delegation.
const MAX_ALLOWED_TURNS: usize = 20;

/// Default exec_mode for delegated sub-agents.
const DEFAULT_EXEC_MODE: &str = "light";

/// System prompt for delegated sub-agents.
const DELEGATE_SYSTEM_PROMPT: &str = "\
You are a focused sub-agent handling a delegated task. \
Complete the task concisely and return your findings as plain text. \
Be direct and factual. Do not ask follow-up questions.";

/// The delegate tool for in-process subtask delegation.
pub struct DelegateTool {
    workgraph_dir: PathBuf,
    working_dir: PathBuf,
    /// Configured default max turns (from config or DEFAULT_MAX_TURNS).
    config_max_turns: usize,
    /// Configured delegate model override. Empty = use parent model.
    config_model: String,
}

impl DelegateTool {
    pub fn new(workgraph_dir: PathBuf, working_dir: PathBuf) -> Self {
        Self {
            workgraph_dir,
            working_dir,
            config_max_turns: DEFAULT_MAX_TURNS,
            config_model: String::new(),
        }
    }

    pub fn with_config(
        workgraph_dir: PathBuf,
        working_dir: PathBuf,
        max_turns: usize,
        model: &str,
    ) -> Self {
        Self {
            workgraph_dir,
            working_dir,
            config_max_turns: max_turns.clamp(1, MAX_ALLOWED_TURNS),
            config_model: model.to_string(),
        }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "delegate".to_string(),
            description:
                "Delegate a subtask to a focused sub-agent that runs within your process. \
                The sub-agent has its own conversation context (your context is not affected) and \
                returns its result as text. Use this for focused queries like reading files, \
                searching code, or answering specific questions that would benefit from a \
                separate context window."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The task to delegate. Be specific about what you need."
                    },
                    "exec_mode": {
                        "type": "string",
                        "enum": ["light", "full"],
                        "description": "Tool access level. 'light' (default) = read-only tools only. 'full' = all tools except delegate."
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": "Maximum conversation turns (default: 5, max: 20). Each tool use counts as one turn."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let prompt = match input.get("prompt").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => p,
            Some(_) => {
                return ToolOutput::error("Parameter 'prompt' must not be empty".to_string());
            }
            None => return ToolOutput::error("Missing required parameter: prompt".to_string()),
        };

        let exec_mode = input
            .get("exec_mode")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_EXEC_MODE);

        // Validate exec_mode
        if exec_mode != "light" && exec_mode != "full" {
            return ToolOutput::error(format!(
                "Invalid exec_mode '{}'. Must be 'light' or 'full'.",
                exec_mode
            ));
        }

        let max_turns = input
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_ALLOWED_TURNS))
            .unwrap_or(self.config_max_turns);

        // Resolve model: config delegate_model > WG_MODEL env var > default
        let model = if !self.config_model.is_empty() {
            self.config_model.clone()
        } else {
            std::env::var("WG_MODEL")
                .ok()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "claude-sonnet-4-latest".to_string())
        };

        // Create provider for the child conversation
        let provider =
            match crate::executor::native::provider::create_provider(&self.workgraph_dir, &model) {
                Ok(p) => p,
                Err(e) => {
                    return ToolOutput::error(format!(
                        "Failed to create provider for delegate: {}",
                        e
                    ));
                }
            };

        // Build child registry without delegate (prevents recursion)
        let registry = build_child_registry(&self.workgraph_dir, &self.working_dir, exec_mode);

        eprintln!(
            "[delegate] Starting sub-agent: exec_mode={}, max_turns={}, model={}",
            exec_mode,
            max_turns,
            provider.model()
        );

        // Run the mini agent loop
        match run_mini_loop(provider, registry, prompt, max_turns).await {
            Ok(result) => {
                let truncated = truncate_tool_output(&result, MAX_DELEGATE_OUTPUT_CHARS);
                ToolOutput::success(truncated)
            }
            Err(e) => ToolOutput::error(format!("Delegate failed: {}", e)),
        }
    }
}

/// Build a tool registry for a delegated sub-agent.
///
/// Creates a registry with standard tools (excluding `delegate` to prevent recursion),
/// then filters by the exec_mode bundle.
pub fn build_child_registry(
    workgraph_dir: &Path,
    working_dir: &Path,
    exec_mode: &str,
) -> ToolRegistry {
    use super::{bash, bg, file, web_fetch, web_search, wg};
    use crate::executor::native::bundle::resolve_bundle;

    let mut registry = ToolRegistry::new();

    // Register all standard tools except delegate (prevents recursion)
    file::register_file_tools(&mut registry);
    bash::register_bash_tool(&mut registry, working_dir.to_path_buf());
    wg::register_wg_tools(&mut registry, workgraph_dir.to_path_buf());
    web_search::register_web_search_tool(&mut registry);
    web_search::register_arxiv_search_tool(&mut registry);
    web_fetch::register_web_fetch_tool(&mut registry, workgraph_dir.to_path_buf());
    bg::register_bg_tool(&mut registry, workgraph_dir.to_path_buf());

    // Apply bundle filtering for the exec_mode
    if let Some(bundle) = resolve_bundle(exec_mode, workgraph_dir) {
        bundle.filter_registry(registry)
    } else {
        registry
    }
}

/// Run a minimal agent loop for delegation.
///
/// This is a lightweight version of `AgentLoop::run()` without journal persistence,
/// resume, streaming, or state injection. The child's tokens are independent of
/// the parent's context.
async fn run_mini_loop(
    provider: Box<dyn Provider>,
    tools: ToolRegistry,
    prompt: &str,
    max_turns: usize,
) -> Result<String, String> {
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
    }];

    let tool_defs = tools.definitions();

    for turn in 0..max_turns {
        let request = MessagesRequest {
            model: provider.model().to_string(),
            max_tokens: provider.max_tokens(),
            system: Some(DELEGATE_SYSTEM_PROMPT.to_string()),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            stream: false,
        };

        let response = provider
            .send(&request)
            .await
            .map_err(|e| format!("API error on turn {}: {}", turn + 1, e))?;

        // Add assistant response to conversation
        messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        match response.stop_reason {
            Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                // Done — extract final text
                return Ok(extract_text(&response));
            }
            Some(StopReason::ToolUse) => {
                // Execute tool calls
                let tool_uses: Vec<_> = response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect();

                // Per-turn progress line so the outer user sees work
                // happening inside the delegated sub-agent instead of
                // a silent multi-turn loop that looks hung.
                let tool_names: Vec<&str> = tool_uses.iter().map(|(_, n, _)| n.as_str()).collect();
                eprintln!(
                    "\x1b[2m[delegate turn {}/{}: {}]\x1b[0m",
                    turn + 1,
                    max_turns,
                    tool_names.join("+")
                );

                let mut results = Vec::new();
                for (id, name, input) in &tool_uses {
                    let output = tools.execute(name, input).await;
                    // Truncate tool output to prevent child context blowup
                    let truncated = truncate_for_tool(&output.content, name);
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: truncated,
                        is_error: output.is_error,
                    });
                }

                messages.push(Message {
                    role: Role::User,
                    content: results,
                });
            }
            Some(StopReason::MaxTokens) => {
                // Truncated response — prompt for continuation
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Your response was truncated. Please provide your answer concisely."
                            .to_string(),
                    }],
                });
            }
        }
    }

    // Max turns reached — return whatever text we have from the last assistant message
    let last_text = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| extract_text_from_content(&m.content))
        .unwrap_or_default();

    if last_text.is_empty() {
        Ok("[delegate reached max turns without producing a text response]".to_string())
    } else {
        Ok(format!(
            "{}\n\n[Note: delegate reached max turns ({})]",
            last_text, max_turns
        ))
    }
}

/// Extract text from a MessagesResponse.
fn extract_text(response: &MessagesResponse) -> String {
    extract_text_from_content(&response.content)
}

/// Extract text from content blocks.
fn extract_text_from_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Register the delegate tool with default config.
pub fn register_delegate_tool(
    registry: &mut super::ToolRegistry,
    workgraph_dir: PathBuf,
    working_dir: PathBuf,
) {
    registry.register(Box::new(DelegateTool::new(workgraph_dir, working_dir)));
}

/// Register the delegate tool with custom config values.
pub fn register_delegate_tool_with_config(
    registry: &mut super::ToolRegistry,
    workgraph_dir: PathBuf,
    working_dir: PathBuf,
    max_turns: usize,
    model: &str,
) {
    registry.register(Box::new(DelegateTool::with_config(
        workgraph_dir,
        working_dir,
        max_turns,
        model,
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Mock provider for testing the mini agent loop.
    struct MockProvider {
        model_name: String,
        responses: Mutex<Vec<MessagesResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<MessagesResponse>) -> Self {
            Self {
                model_name: "mock-model".to_string(),
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn model(&self) -> &str {
            &self.model_name
        }

        fn max_tokens(&self) -> u32 {
            1024
        }

        async fn send(&self, _request: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("No more mock responses available")
            }
            Ok(responses.remove(0))
        }
    }

    fn text_response(text: &str) -> MessagesResponse {
        MessagesResponse {
            id: "test".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        }
    }

    fn tool_use_response(
        tool_id: &str,
        tool_name: &str,
        input: serde_json::Value,
    ) -> MessagesResponse {
        MessagesResponse {
            id: "test".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
                input,
            }],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        }
    }

    #[tokio::test]
    async fn test_mini_loop_simple_response() {
        let provider: Box<dyn Provider> =
            Box::new(MockProvider::new(vec![text_response("The answer is 42.")]));
        let registry = ToolRegistry::new();

        let result = run_mini_loop(provider, registry, "What is 6 * 7?", 5).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "The answer is 42.");
    }

    #[tokio::test]
    async fn test_mini_loop_max_turns_enforced() {
        // Provider always returns tool use — should hit max_turns
        let responses = vec![
            tool_use_response("t1", "unknown_tool", json!({})),
            tool_use_response("t2", "unknown_tool", json!({})),
            tool_use_response("t3", "unknown_tool", json!({})),
        ];
        let provider: Box<dyn Provider> = Box::new(MockProvider::new(responses));
        let registry = ToolRegistry::new();

        let result = run_mini_loop(provider, registry, "Do something", 3).await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(
            text.contains("max turns"),
            "Expected max turns message, got: {}",
            text
        );
    }

    #[tokio::test]
    async fn test_mini_loop_with_tool_use_then_text() {
        // First response: tool use, second response: text
        let responses = vec![
            tool_use_response("t1", "some_tool", json!({"query": "test"})),
            text_response("Found: test results"),
        ];
        let provider: Box<dyn Provider> = Box::new(MockProvider::new(responses));
        let registry = ToolRegistry::new();

        let result = run_mini_loop(provider, registry, "Search for test", 5).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Found: test results");
    }

    #[tokio::test]
    async fn test_mini_loop_api_error() {
        // No responses — will trigger API error
        let provider: Box<dyn Provider> = Box::new(MockProvider::new(vec![]));
        let registry = ToolRegistry::new();

        let result = run_mini_loop(provider, registry, "Hello", 5).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("API error on turn 1"));
    }

    #[tokio::test]
    async fn test_child_registry_light_mode() {
        let tmp = TempDir::new().unwrap();
        let working = std::env::current_dir().unwrap();

        let registry = build_child_registry(tmp.path(), &working, "light");
        let tool_names: Vec<String> = registry
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();

        // Light mode should have read-only tools
        assert!(
            tool_names.contains(&"read_file".to_string()),
            "light mode should include read_file"
        );
        assert!(
            tool_names.contains(&"grep".to_string()),
            "light mode should include grep"
        );
        assert!(
            tool_names.contains(&"glob".to_string()),
            "light mode should include glob"
        );
        // Should NOT have delegate (recursion prevention)
        assert!(
            !tool_names.contains(&"delegate".to_string()),
            "child registry must not include delegate"
        );
        // Should NOT have write_file (research bundle excludes it)
        assert!(
            !tool_names.contains(&"write_file".to_string()),
            "light mode should not include write_file"
        );
    }

    #[tokio::test]
    async fn test_child_registry_full_mode() {
        let tmp = TempDir::new().unwrap();
        let working = std::env::current_dir().unwrap();

        let registry = build_child_registry(tmp.path(), &working, "full");
        let tool_names: Vec<String> = registry
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();

        // Full mode should have write tools
        assert!(
            tool_names.contains(&"read_file".to_string()),
            "full mode should include read_file"
        );
        assert!(
            tool_names.contains(&"write_file".to_string()),
            "full mode should include write_file"
        );
        assert!(
            tool_names.contains(&"bash".to_string()),
            "full mode should include bash"
        );
        // Should NOT have delegate (recursion prevention)
        assert!(
            !tool_names.contains(&"delegate".to_string()),
            "child registry must not include delegate"
        );
    }

    #[tokio::test]
    async fn test_delegate_tool_missing_prompt() {
        let tmp = TempDir::new().unwrap();
        let tool = DelegateTool::new(tmp.path().to_path_buf(), tmp.path().to_path_buf());

        let input = json!({});
        let result = tool.execute(&input).await;

        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("Missing required parameter: prompt")
        );
    }

    #[tokio::test]
    async fn test_delegate_tool_empty_prompt() {
        let tmp = TempDir::new().unwrap();
        let tool = DelegateTool::new(tmp.path().to_path_buf(), tmp.path().to_path_buf());

        let input = json!({"prompt": "   "});
        let result = tool.execute(&input).await;

        assert!(result.is_error);
        assert!(result.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn test_delegate_tool_invalid_exec_mode() {
        let tmp = TempDir::new().unwrap();
        let tool = DelegateTool::new(tmp.path().to_path_buf(), tmp.path().to_path_buf());

        let input = json!({"prompt": "test", "exec_mode": "invalid"});
        let result = tool.execute(&input).await;

        assert!(result.is_error);
        assert!(result.content.contains("Invalid exec_mode"));
    }
}

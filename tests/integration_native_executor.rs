//! Integration tests for the native executor bundle system.
//!
//! Non-LLM tests verify bundle loading, tool filtering, and command wiring.
//! LLM tests (gated behind `llm-tests` feature) run an actual agent loop.

use std::fs;
use std::path::Path;
use tempfile::TempDir;

use workgraph::executor::native::bundle::{
    Bundle, ensure_default_bundles, load_all_bundles, resolve_bundle,
};
use workgraph::executor::native::tools::ToolRegistry;
use workgraph::graph::WorkGraph;
use workgraph::parser::save_graph;

fn setup_workgraph(dir: &Path) -> std::path::PathBuf {
    let graph_path = dir.join("graph.jsonl");
    fs::create_dir_all(dir).unwrap();
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    graph_path
}

// ── Bundle loading and resolution tests ─────────────────────────────────

#[test]
fn test_bundle_toml_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let bundles_dir = tmp.path().join("bundles");
    fs::create_dir_all(&bundles_dir).unwrap();

    let bundle = Bundle {
        name: "custom".to_string(),
        description: "A custom test bundle".to_string(),
        tools: vec![
            "read_file".to_string(),
            "wg_show".to_string(),
            "wg_done".to_string(),
        ],
        context_scope: "graph".to_string(),
        system_prompt_suffix: "Be helpful.".to_string(),
    };

    let toml_content = toml::to_string_pretty(&bundle).unwrap();
    let path = bundles_dir.join("custom.toml");
    fs::write(&path, &toml_content).unwrap();

    let loaded = Bundle::load(&path).unwrap();
    assert_eq!(loaded.name, "custom");
    assert_eq!(loaded.tools.len(), 3);
    assert_eq!(loaded.context_scope, "graph");
    assert_eq!(loaded.system_prompt_suffix, "Be helpful.");
}

#[test]
fn test_ensure_default_bundles_creates_three_files() {
    let tmp = TempDir::new().unwrap();
    ensure_default_bundles(tmp.path()).unwrap();

    let bundles_dir = tmp.path().join("bundles");
    assert!(bundles_dir.join("bare.toml").exists());
    assert!(bundles_dir.join("research.toml").exists());
    assert!(bundles_dir.join("implementer.toml").exists());

    // All should parse correctly
    let bare = Bundle::load(&bundles_dir.join("bare.toml")).unwrap();
    let research = Bundle::load(&bundles_dir.join("research.toml")).unwrap();
    let implementer = Bundle::load(&bundles_dir.join("implementer.toml")).unwrap();

    assert_eq!(bare.name, "bare");
    assert!(!bare.allows_all());
    assert!(bare.tools.contains(&"wg_done".to_string()));

    assert_eq!(research.name, "research");
    assert!(!research.allows_all());
    assert!(research.tools.contains(&"read_file".to_string()));

    assert_eq!(implementer.name, "implementer");
    assert!(implementer.allows_all());
}

#[test]
fn test_exec_mode_bundle_mapping() {
    let tmp = TempDir::new().unwrap();

    // shell → None
    assert!(resolve_bundle("shell", tmp.path()).is_none());

    // bare → bare bundle
    let bare = resolve_bundle("bare", tmp.path()).unwrap();
    assert_eq!(bare.name, "bare");
    assert!(!bare.tools.contains(&"read_file".to_string()));
    assert!(bare.tools.contains(&"wg_show".to_string()));

    // light → research bundle
    let research = resolve_bundle("light", tmp.path()).unwrap();
    assert_eq!(research.name, "research");
    assert!(research.tools.contains(&"read_file".to_string()));
    assert!(research.tools.contains(&"grep".to_string()));
    assert!(!research.tools.contains(&"write_file".to_string()));

    // full → implementer bundle
    let implementer = resolve_bundle("full", tmp.path()).unwrap();
    assert_eq!(implementer.name, "implementer");
    assert!(implementer.allows_all());
}

#[test]
fn test_bundle_file_overrides_builtin() {
    let tmp = TempDir::new().unwrap();
    let bundles_dir = tmp.path().join("bundles");
    fs::create_dir_all(&bundles_dir).unwrap();

    // Write a custom research bundle that only allows 2 tools
    let content = r#"
name = "research"
description = "Minimal custom research bundle"
tools = ["read_file", "wg_done"]
context_scope = "task"
system_prompt_suffix = "Custom research agent."
"#;
    fs::write(bundles_dir.join("research.toml"), content).unwrap();

    let bundle = resolve_bundle("light", tmp.path()).unwrap();
    assert_eq!(bundle.name, "research");
    assert_eq!(bundle.tools.len(), 2);
    assert_eq!(bundle.context_scope, "task");
    assert_eq!(bundle.system_prompt_suffix, "Custom research agent.");
}

#[test]
fn test_tool_registry_filtering_bare() {
    let tmp = TempDir::new().unwrap();
    let working_dir = std::env::current_dir().unwrap();

    let registry = ToolRegistry::default_all(tmp.path(), &working_dir);
    let total_tools = registry.definitions().len();
    assert!(total_tools > 7, "Should have more than 7 tools total");

    let bundle = Bundle::bare();
    let filtered = bundle.filter_registry(registry);
    let filtered_defs = filtered.definitions();

    // All filtered tools should be wg_* tools
    for def in &filtered_defs {
        assert!(
            def.name.starts_with("wg_"),
            "Bare bundle should only have wg tools, found: {}",
            def.name
        );
    }
    assert!(filtered_defs.len() < total_tools);
    assert!(filtered_defs.len() >= 5); // At least wg_show, wg_list, wg_add, wg_done, wg_fail
}

#[test]
fn test_tool_registry_filtering_research() {
    let tmp = TempDir::new().unwrap();
    let working_dir = std::env::current_dir().unwrap();

    let registry = ToolRegistry::default_all(tmp.path(), &working_dir);
    let total_tools = registry.definitions().len();

    let bundle = Bundle::research();
    let filtered = bundle.filter_registry(registry);
    let filtered_defs = filtered.definitions();

    // Should have read_file but NOT write_file or edit_file
    let names: Vec<&str> = filtered_defs.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"grep"));
    assert!(names.contains(&"glob"));
    assert!(names.contains(&"wg_show"));
    assert!(!names.contains(&"write_file"));
    assert!(!names.contains(&"edit_file"));
    assert!(filtered_defs.len() < total_tools);
}

#[test]
fn test_tool_registry_filtering_implementer_wildcard() {
    let tmp = TempDir::new().unwrap();
    let working_dir = std::env::current_dir().unwrap();

    let registry = ToolRegistry::default_all(tmp.path(), &working_dir);
    let total_tools = registry.definitions().len();

    let bundle = Bundle::implementer();
    let filtered = bundle.filter_registry(registry);
    assert_eq!(
        filtered.definitions().len(),
        total_tools,
        "Implementer bundle with wildcard should keep all tools"
    );
}

#[test]
fn test_load_all_bundles_from_dir() {
    let tmp = TempDir::new().unwrap();
    ensure_default_bundles(tmp.path()).unwrap();

    let bundles = load_all_bundles(tmp.path());
    assert_eq!(bundles.len(), 3);

    let names: Vec<&str> = bundles.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"bare"));
    assert!(names.contains(&"research"));
    assert!(names.contains(&"implementer"));
}

// ── Native executor registry tests ──────────────────────────────────────

#[test]
fn test_native_executor_in_registry() {
    let tmp = TempDir::new().unwrap();
    setup_workgraph(tmp.path());

    let registry = workgraph::service::executor::ExecutorRegistry::new(tmp.path());
    let config = registry.load_config("native").unwrap();
    assert_eq!(config.executor.executor_type, "native");
    assert_eq!(config.executor.command, "wg");
}

#[test]
fn test_native_executor_default_config() {
    let tmp = TempDir::new().unwrap();
    setup_workgraph(tmp.path());

    let registry = workgraph::service::executor::ExecutorRegistry::new(tmp.path());
    let config = registry.load_config("native").unwrap();

    assert_eq!(config.executor.executor_type, "native");
    assert_eq!(config.executor.command, "wg");
    assert!(config.executor.args.contains(&"native-exec".to_string()));
    assert!(config.executor.working_dir.is_some());
}

#[test]
fn test_native_executor_config_from_toml() {
    let tmp = TempDir::new().unwrap();
    setup_workgraph(tmp.path());

    // Write a custom native executor config
    let executors_dir = tmp.path().join("executors");
    fs::create_dir_all(&executors_dir).unwrap();

    let toml_content = r#"
[executor]
type = "native"
command = "wg"
args = ["native-exec", "--max-turns", "50"]
working_dir = "{{working_dir}}"
model = "claude-haiku-3-5-20241022"
"#;
    fs::write(executors_dir.join("native.toml"), toml_content).unwrap();

    let registry = workgraph::service::executor::ExecutorRegistry::new(tmp.path());
    let config = registry.load_config("native").unwrap();
    assert_eq!(config.executor.executor_type, "native");
    assert_eq!(
        config.executor.model.as_deref(),
        Some("claude-haiku-3-5-20241022")
    );
}

// ── LLM integration test (requires API key) ───────────────────────────

// Run with: cargo test --features llm-tests --test integration_native_executor -- --nocapture
//
// The LLM test requires an ANTHROPIC_API_KEY environment variable and exercises
// the full native executor agent loop: reading a file, calling tools, and
// marking the task done.
#[cfg(feature = "llm-tests")]
mod llm_tests {
    use super::*;
    use workgraph::executor::native::agent::AgentLoop;
    use workgraph::executor::native::client::AnthropicClient;
    use workgraph::graph::{Node, Status, Task};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: Some(format!("Test task: {}", title)),
            ..Task::default()
        }
    }

    #[test]
    fn test_native_executor_runs_simple_task_e2e() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let graph_path = setup_workgraph(dir);

        // Create a task that requires reading a file and creating a subtask
        let test_file_dir = dir.parent().unwrap_or(dir);
        let test_file = test_file_dir.join("test_input.txt");
        fs::write(&test_file, "This file contains the answer: 42.").unwrap();

        let mut graph = workgraph::parser::load_graph(&graph_path).unwrap();
        let mut task = make_task("test-native", "Read file and report answer");
        task.description = Some(format!(
            "Read the file at {} and report what number the answer is. \
             Then mark this task as done using wg_done with task_id 'test-native'.",
            test_file.display()
        ));
        task.status = Status::InProgress;
        graph.add_node(Node::Task(task));
        save_graph(&graph, &graph_path).unwrap();

        // Build the tool registry with full access
        let working_dir = test_file_dir.to_path_buf();
        let registry = ToolRegistry::default_all(dir, &working_dir);

        // Create the API client
        let client = AnthropicClient::from_env("claude-haiku-3-5-20241022")
            .expect("ANTHROPIC_API_KEY must be set for LLM tests");

        let system_prompt = format!(
            "You are a test agent working on task 'test-native'. \
             Read the file at {} and report the answer. \
             When done, use the wg_done tool with task_id 'test-native'.",
            test_file.display()
        );

        let output_log = dir.join("agent.ndjson");
        let agent = AgentLoop::new(client, registry, system_prompt, 10, output_log);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt
            .block_on(agent.run("Complete the task as described in your system prompt."))
            .expect("Agent loop should complete");

        eprintln!(
            "Agent result: {} turns, final: {}",
            result.turns, result.final_text
        );

        // Verify the task was marked done
        let graph = workgraph::parser::load_graph(&graph_path).unwrap();
        let task = graph.get_task("test-native").unwrap();
        assert_eq!(
            task.status,
            Status::Done,
            "Task should be marked as done by the agent"
        );
    }
}

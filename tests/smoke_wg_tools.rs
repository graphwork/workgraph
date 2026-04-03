//! Smoke tests for wg tool integration via OpenRouter.
//!
//! Exercises the in-process wg tools (wg_show, wg_list, wg_add, wg_done,
//! wg_log, wg_artifact) through the native executor agent loop with
//! minimax-m2.7 via OpenRouter.
//!
//! Run with: cargo test smoke_wg_tools -- --ignored
//! Requires: OPENROUTER_API_KEY environment variable

use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use workgraph::executor::native::agent::AgentLoop;
use workgraph::executor::native::openai_client::OpenAiClient;
use workgraph::executor::native::tools::ToolRegistry;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Setup workgraph directory and graph file.
/// Returns the path to the graph.jsonl file.
fn setup_workgraph(dir: &Path) -> PathBuf {
    let wg_dir = dir.join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    graph_path
}

fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        ..Task::default()
    }
}

// ---------------------------------------------------------------------------
// Smoke Tests
// ---------------------------------------------------------------------------

/// End-to-end smoke test: exercise wg_show and wg_done through the native executor.
#[test]
#[ignore = "requires OPENROUTER_API_KEY"]
fn smoke_wg_tools_live() {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .expect("OPENROUTER_API_KEY must be set");

    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();
    // setup_workgraph creates .workgraph/graph.jsonl
    let graph_path = setup_workgraph(wg_dir);

    let mut graph = WorkGraph::new();
    let mut parent = make_task("smoke-parent", "Parent smoke test task", Status::InProgress);
    parent.description = Some("Parent task for smoke testing wg tools".to_string());
    parent.tags = vec!["smoke-test".to_string()];

    let child = make_task("smoke-child", "Child smoke test task", Status::Open);
    parent.after = vec!["smoke-child".to_string()];

    graph.add_node(Node::Task(parent));
    graph.add_node(Node::Task(child));
    save_graph(&graph, &graph_path).unwrap();

    // wg_dir is the workgraph directory, wg_dir is also working_dir
    let registry = ToolRegistry::default_all(wg_dir, wg_dir);

    // Verify wg tools are registered
    let definitions = registry.definitions();
    let tool_names: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
    assert!(tool_names.contains(&"wg_show"));
    assert!(tool_names.contains(&"wg_list"));
    assert!(tool_names.contains(&"wg_add"));
    assert!(tool_names.contains(&"wg_done"));
    assert!(tool_names.contains(&"wg_log"));
    assert!(tool_names.contains(&"wg_artifact"));

    let client = OpenAiClient::new(api_key, "minimax/minimax-m2.7", None)
        .expect("Failed to create OpenRouter client")
        .with_provider_hint("openrouter");

    let system_prompt = "Call wg_show with task_id=\"smoke-parent\". Then call wg_done with task_id=\"smoke-parent\". Report what happened.".to_string();

    let output_log = wg_dir.join("agent.ndjson");
    let agent = AgentLoop::new(Box::new(client), registry, system_prompt, 15, output_log);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt
        .block_on(agent.run("Complete the tasks."))
        .expect("Agent loop should complete");

    eprintln!("[smoke_wg_tools] Agent completed: {} turns", result.turns);

    let graph = load_graph(&graph_path).unwrap();
    let parent_task = graph.get_task("smoke-parent").expect("smoke-parent should exist");
    assert_eq!(parent_task.status, Status::Done, "Task should be Done. Output: {}", result.final_text);
    eprintln!("[smoke_wg_tools] All assertions passed!");
}

/// Test wg_list works.
#[test]
#[ignore = "requires OPENROUTER_API_KEY"]
fn smoke_wg_list_works() {
    let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");

    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();
    let graph_path = setup_workgraph(wg_dir);

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("task-a", "Task A", Status::Open)));
    graph.add_node(Node::Task(make_task("task-b", "Task B", Status::InProgress)));
    save_graph(&graph, &graph_path).unwrap();

    let registry = ToolRegistry::default_all(wg_dir, wg_dir);

    let client = OpenAiClient::new(api_key, "minimax/minimax-m2.7", None)
        .expect("Failed to create client")
        .with_provider_hint("openrouter");

    let output_log = wg_dir.join("agent_list.ndjson");
    let agent = AgentLoop::new(
        Box::new(client),
        registry,
        "Call wg_list. Report what you see.".to_string(),
        10,
        output_log,
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(agent.run("Call wg_list.")).expect("Agent loop should complete");

    eprintln!("[smoke_wg_list] Agent completed: {} turns", result.turns);
    assert!(result.final_text.contains("task-a") || result.final_text.contains("Task A"));
}

/// Test wg_add creates task.
#[test]
#[ignore = "requires OPENROUTER_API_KEY"]
fn smoke_wg_add_creates_task() {
    let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");

    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();
    let graph_path = setup_workgraph(wg_dir);

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("existing", "Existing Task", Status::Open)));
    save_graph(&graph, &graph_path).unwrap();

    let registry = ToolRegistry::default_all(wg_dir, wg_dir);

    let client = OpenAiClient::new(api_key, "minimax/minimax-m2.7", None)
        .expect("Failed to create client")
        .with_provider_hint("openrouter");

    let output_log = wg_dir.join("agent_add.ndjson");
    let agent = AgentLoop::new(
        Box::new(client),
        registry,
        "Call wg_add with title=\"New task from smoke test\". Report the task ID.".to_string(),
        10,
        output_log,
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(agent.run("Create a new task.")).expect("Agent loop should complete");

    let graph = load_graph(&graph_path).unwrap();
    let new_task = graph.tasks().find(|t| t.title.contains("New task from smoke test"));
    assert!(new_task.is_some(), "New task should be created");
    assert_eq!(new_task.unwrap().status, Status::Open);
}

/// Test wg_done marks task as done.
#[test]
#[ignore = "requires OPENROUTER_API_KEY"]
fn smoke_wg_done_simple() {
    let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");

    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();
    let graph_path = setup_workgraph(wg_dir);

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("done-test", "Done Test", Status::InProgress)));
    save_graph(&graph, &graph_path).unwrap();

    let registry = ToolRegistry::default_all(wg_dir, wg_dir);

    let client = OpenAiClient::new(api_key, "minimax/minimax-m2.7", None)
        .expect("Failed to create client")
        .with_provider_hint("openrouter");

    let output_log = wg_dir.join("agent_done.ndjson");
    let agent = AgentLoop::new(
        Box::new(client),
        registry,
        "Call wg_done with task_id=\"done-test\". Report what happens.".to_string(),
        10,
        output_log,
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(agent.run("Mark task as done.")).expect("Agent loop should complete");

    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("done-test").expect("done-test should exist");
    assert_eq!(task.status, Status::Done);
}

/// Skipped test documentation.
#[test]
fn smoke_wg_tools_skip_without_api_key() {
    eprintln!("[smoke_wg_tools] Live tests require: OPENROUTER_API_KEY cargo test smoke_wg_tools -- --ignored");
}

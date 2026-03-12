//! Integration tests for the context scopes feature.
//!
//! Tests cover:
//! 1. `wg add --context-scope <scope>` sets the field correctly on the task
//! 2. `wg edit --context-scope <scope>` updates the field
//! 3. Spawn with clean scope produces minimal prompt (no workflow sections)
//! 4. Spawn with graph scope includes neighborhood summary
//! 5. Scope resolution hierarchy: task > role > config > default
//! 6. Invalid scope values are rejected

use std::fs;
use std::path::Path;
use tempfile::TempDir;

use workgraph::config::Config;
use workgraph::context_scope::{ContextScope, resolve_context_scope};
use workgraph::graph::{Node, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::executor::{
    ScopeContext, TemplateVars, build_prompt, description_has_pattern_keywords,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_task(id: &str, title: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        ..Task::default()
    }
}

/// Set up a workgraph directory with graph.jsonl containing the given tasks.
fn setup_workgraph(dir: &Path, tasks: Vec<Task>) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &path).unwrap();
}

// ---------------------------------------------------------------------------
// Integration: wg add --context-scope
// ---------------------------------------------------------------------------

#[test]
fn test_add_with_context_scope_clean() {
    let temp_dir = TempDir::new().unwrap();
    let wg_dir = temp_dir.path();
    setup_workgraph(wg_dir, vec![]);

    // Use the add command with --context-scope clean
    let result = workgraph::graph::Node::Task(Task {
        id: "scoped-task".to_string(),
        title: "Scoped task".to_string(),
        context_scope: Some("clean".to_string()),
        ..Task::default()
    });

    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    graph.add_node(result);
    save_graph(&graph, &graph_path).unwrap();

    // Reload and verify
    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("scoped-task").unwrap();
    assert_eq!(
        task.context_scope,
        Some("clean".to_string()),
        "context_scope should be 'clean' after add"
    );
}

#[test]
fn test_add_with_context_scope_full() {
    let temp_dir = TempDir::new().unwrap();
    let wg_dir = temp_dir.path();
    setup_workgraph(wg_dir, vec![]);

    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    graph.add_node(Node::Task(Task {
        id: "full-task".to_string(),
        title: "Full scope task".to_string(),
        context_scope: Some("full".to_string()),
        ..Task::default()
    }));
    save_graph(&graph, &graph_path).unwrap();

    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("full-task").unwrap();
    assert_eq!(task.context_scope, Some("full".to_string()));
}

#[test]
fn test_add_without_context_scope_defaults_to_none() {
    let temp_dir = TempDir::new().unwrap();
    let wg_dir = temp_dir.path();
    setup_workgraph(wg_dir, vec![]);

    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    graph.add_node(Node::Task(Task {
        id: "default-task".to_string(),
        title: "Default task".to_string(),
        ..Task::default()
    }));
    save_graph(&graph, &graph_path).unwrap();

    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("default-task").unwrap();
    assert_eq!(
        task.context_scope, None,
        "context_scope should be None by default"
    );
}

// ---------------------------------------------------------------------------
// Integration: context_scope field roundtrip through serialization
// ---------------------------------------------------------------------------

#[test]
fn test_context_scope_survives_serialization_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let wg_dir = temp_dir.path();
    fs::create_dir_all(wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");

    // Create tasks with each scope
    let scopes = ["clean", "task", "graph", "full"];
    let mut graph = WorkGraph::new();
    for scope in &scopes {
        let mut t = make_task(&format!("t-{}", scope), &format!("{} scope task", scope));
        t.context_scope = Some(scope.to_string());
        graph.add_node(Node::Task(t));
    }
    save_graph(&graph, &graph_path).unwrap();

    // Reload and verify each scope survived
    let graph = load_graph(&graph_path).unwrap();
    for scope in &scopes {
        let task = graph.get_task(&format!("t-{}", scope)).unwrap();
        assert_eq!(
            task.context_scope,
            Some(scope.to_string()),
            "Scope '{}' should survive serialization roundtrip",
            scope
        );
    }
}

// ---------------------------------------------------------------------------
// Integration: spawn with clean scope produces minimal prompt
// ---------------------------------------------------------------------------

#[test]
fn test_clean_scope_prompt_is_minimal() {
    let task = Task {
        id: "clean-task".to_string(),
        title: "Clean scoped task".to_string(),
        description: Some("Do something simple".to_string()),
        context_scope: Some("clean".to_string()),
        ..Task::default()
    };

    let vars = TemplateVars::from_task(&task, Some("Context from dep"), None);
    let ctx = ScopeContext::default();
    let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

    // Should contain task info
    assert!(prompt.contains("clean-task"), "Should include task ID");
    assert!(
        prompt.contains("Clean scoped task"),
        "Should include task title"
    );
    assert!(
        prompt.contains("Do something simple"),
        "Should include description"
    );
    assert!(
        prompt.contains("Context from dep"),
        "Should include dependency context"
    );
    assert!(
        prompt.contains("Begin working on the task now."),
        "Should end with begin message"
    );

    // Should NOT contain workflow sections
    assert!(
        !prompt.contains("## Required Workflow"),
        "Clean scope should NOT include Required Workflow"
    );
    assert!(
        !prompt.contains("## Graph Patterns"),
        "Clean scope should NOT include Graph Patterns"
    );
    assert!(
        !prompt.contains("wg log"),
        "Clean scope should NOT reference wg commands"
    );
    assert!(
        !prompt.contains("wg done"),
        "Clean scope should NOT reference wg done"
    );
    assert!(
        !prompt.contains("## CRITICAL"),
        "Clean scope should NOT include CRITICAL section"
    );
    assert!(
        !prompt.contains("## System Awareness"),
        "Clean scope should NOT include system awareness"
    );
}

// ---------------------------------------------------------------------------
// Integration: spawn with graph scope includes neighborhood
// ---------------------------------------------------------------------------

#[test]
fn test_graph_scope_prompt_includes_neighborhood() {
    let task = Task {
        id: "graph-task".to_string(),
        title: "Graph scoped task".to_string(),
        description: Some("Task with graph context".to_string()),
        context_scope: Some("graph".to_string()),
        ..Task::default()
    };

    let vars = TemplateVars::from_task(&task, Some("Dep context"), None);
    let ctx = ScopeContext {
        project_description: "My awesome project".to_string(),
        graph_summary: "## Graph Status\n\n10 tasks — 5 done, 2 in-progress, 3 open, 0 blocked, 0 failed\n\n### Upstream (dependencies)\n<neighbor-context source=\"dep-1\">\n- **dep-1** [done]: Dependency\n</neighbor-context>".to_string(),
        downstream_info: "## Downstream Consumers\n\nTasks that depend on your work:\n- **next-task**: \"Next step\"".to_string(),
        tags_skills_info: "- **Tags:** integration, testing".to_string(),
        ..Default::default()
    };

    let prompt = build_prompt(&vars, ContextScope::Graph, &ctx);

    // Should include all task+ sections
    assert!(
        prompt.contains("## Required Workflow"),
        "Graph scope should include workflow"
    );
    assert!(
        prompt.contains("## Graph Patterns"),
        "Graph scope should include graph patterns"
    );

    // Should include graph context
    assert!(
        prompt.contains("My awesome project"),
        "Graph scope should include project description"
    );
    assert!(
        prompt.contains("## Graph Status"),
        "Graph scope should include graph summary"
    );
    assert!(
        prompt.contains("10 tasks"),
        "Graph scope should include task counts"
    );
    assert!(
        prompt.contains("neighbor-context"),
        "Graph scope should include XML-fenced neighbors"
    );

    // Should include R1 (downstream) and R4 (tags)
    assert!(
        prompt.contains("Downstream Consumers"),
        "Graph scope should include downstream info"
    );
    assert!(
        prompt.contains("integration, testing"),
        "Graph scope should include tags"
    );

    // Should NOT include full-scope sections
    assert!(
        !prompt.contains("## System Awareness"),
        "Graph scope should NOT include system awareness"
    );
    assert!(
        !prompt.contains("## Full Graph Summary"),
        "Graph scope should NOT include full graph summary"
    );
    assert!(
        !prompt.contains("CLAUDE.md"),
        "Graph scope should NOT include CLAUDE.md"
    );
}

// ---------------------------------------------------------------------------
// Integration: full scope includes everything
// ---------------------------------------------------------------------------

#[test]
fn test_full_scope_prompt_includes_everything() {
    let task = Task {
        id: "full-task".to_string(),
        title: "Full scoped task".to_string(),
        description: Some("Task with full context".to_string()),
        ..Task::default()
    };

    let vars = TemplateVars::from_task(&task, Some("Dep context"), None);
    let ctx = ScopeContext {
        downstream_info: "## Downstream\n- consumer".to_string(),
        tags_skills_info: "- **Tags:** meta".to_string(),
        project_description: "Full project".to_string(),
        graph_summary: "## Graph Status\n\n5 tasks".to_string(),
        full_graph_summary: "## Full Graph Summary\n\n- t1 [done]\n- t2 [open]".to_string(),
        claude_md_content: "Always use workgraph.".to_string(),
        queued_messages: String::new(),
        previous_attempt_context: String::new(),
    };

    let prompt = build_prompt(&vars, ContextScope::Full, &ctx);

    // Should include everything
    assert!(prompt.contains("## System Awareness"));
    assert!(prompt.contains("## Required Workflow"));
    assert!(prompt.contains("## Graph Patterns"));
    assert!(prompt.contains("Full project"));
    assert!(prompt.contains("## Graph Status"));
    assert!(prompt.contains("## Full Graph Summary"));
    assert!(prompt.contains("## Project Instructions (CLAUDE.md)\n\nAlways use workgraph."));
    assert!(prompt.contains("## Downstream"));
    assert!(prompt.contains("meta"));
}

// ---------------------------------------------------------------------------
// Scope resolution hierarchy
// ---------------------------------------------------------------------------

#[test]
fn test_resolve_hierarchy_task_wins() {
    let scope = resolve_context_scope(Some("clean"), Some("graph"), Some("full"));
    assert_eq!(scope, ContextScope::Clean);
}

#[test]
fn test_resolve_hierarchy_role_wins_when_no_task() {
    let scope = resolve_context_scope(None, Some("graph"), Some("full"));
    assert_eq!(scope, ContextScope::Graph);
}

#[test]
fn test_resolve_hierarchy_config_wins_when_no_task_or_role() {
    let scope = resolve_context_scope(None, None, Some("full"));
    assert_eq!(scope, ContextScope::Full);
}

#[test]
fn test_resolve_hierarchy_default_is_task() {
    let scope = resolve_context_scope(None, None, None);
    assert_eq!(scope, ContextScope::Task);
}

#[test]
fn test_resolve_skips_invalid_values() {
    // Invalid task scope falls through to role
    let scope = resolve_context_scope(Some("bogus"), Some("graph"), None);
    assert_eq!(scope, ContextScope::Graph);

    // All invalid => default (task)
    let scope = resolve_context_scope(Some("nope"), Some("bad"), Some("invalid"));
    assert_eq!(scope, ContextScope::Task);
}

// ---------------------------------------------------------------------------
// Invalid scope values are rejected by FromStr
// ---------------------------------------------------------------------------

#[test]
fn test_invalid_scope_values_rejected() {
    assert!("bogus".parse::<ContextScope>().is_err());
    assert!("CLEAN ".parse::<ContextScope>().is_err()); // trailing space not trimmed
    assert!("tasks".parse::<ContextScope>().is_err()); // plural
    assert!("".parse::<ContextScope>().is_err());
}

// ---------------------------------------------------------------------------
// Config coordinator.default_context_scope
// ---------------------------------------------------------------------------

#[test]
fn test_config_default_context_scope_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let wg_dir = temp_dir.path();
    fs::create_dir_all(wg_dir).unwrap();

    let mut config = Config::default();
    config.coordinator.default_context_scope = Some("graph".to_string());
    config.save(wg_dir).unwrap();

    let loaded = Config::load(wg_dir).unwrap();
    assert_eq!(
        loaded.coordinator.default_context_scope,
        Some("graph".to_string()),
        "default_context_scope should survive config roundtrip"
    );
}

#[test]
fn test_config_default_context_scope_none_by_default() {
    let config = Config::default();
    assert_eq!(
        config.coordinator.default_context_scope, None,
        "default_context_scope should be None by default"
    );
}

// ---------------------------------------------------------------------------
// Scope ordering
// ---------------------------------------------------------------------------

#[test]
fn test_scope_ordering_is_strict() {
    assert!(ContextScope::Clean < ContextScope::Task);
    assert!(ContextScope::Task < ContextScope::Graph);
    assert!(ContextScope::Graph < ContextScope::Full);

    // Superset checks used in build_prompt
    assert!(ContextScope::Task >= ContextScope::Task);
    assert!(ContextScope::Graph >= ContextScope::Task);
    assert!(ContextScope::Full >= ContextScope::Task);
    assert!(ContextScope::Full >= ContextScope::Graph);
    assert!((ContextScope::Clean < ContextScope::Task));
    assert!((ContextScope::Task < ContextScope::Graph));
}

// ---------------------------------------------------------------------------
// Pattern keyword glossary: conditional inclusion
// ---------------------------------------------------------------------------

#[test]
fn test_pattern_keyword_detection() {
    // Positive cases
    assert!(description_has_pattern_keywords(
        "Use autopoietic decomposition"
    ));
    assert!(description_has_pattern_keywords(
        "This is a self-organizing task"
    ));
    assert!(description_has_pattern_keywords(
        "Create a committee review"
    ));
    assert!(description_has_pattern_keywords("Use swarm intelligence"));
    assert!(description_has_pattern_keywords("Fork-join the workers"));
    assert!(description_has_pattern_keywords(
        "Fan-out to parallel tasks"
    ));
    assert!(description_has_pattern_keywords("Run tasks in parallel"));
    assert!(description_has_pattern_keywords("Loop until converged"));
    assert!(description_has_pattern_keywords("Add a cycle for retries"));
    assert!(description_has_pattern_keywords("Iterate on the design"));
    assert!(description_has_pattern_keywords("Research the codebase"));
    assert!(description_has_pattern_keywords("Investigate the bug"));
    assert!(description_has_pattern_keywords("Audit the security"));
    assert!(description_has_pattern_keywords("Use DELIBERATION process"));
    assert!(description_has_pattern_keywords("Open a discussion"));

    // Negative cases
    assert!(!description_has_pattern_keywords("Build a widget factory"));
    assert!(!description_has_pattern_keywords("Fix the login bug"));
    assert!(!description_has_pattern_keywords("Add unit tests"));
}

#[test]
fn test_pattern_glossary_included_when_keywords_present() {
    let vars = TemplateVars {
        task_id: "pattern-task".into(),
        task_title: "Research patterns".into(),
        task_description: "Research the codebase for security issues".into(),
        task_context: "No dependencies".into(),
        task_identity: String::new(),
        working_dir: String::new(),
        skills_preamble: String::new(),
        model: String::new(),
        task_loop_info: String::new(),
        task_verify: None,
        max_child_tasks: 10,
        max_task_depth: 8,
    };
    let ctx = ScopeContext::default();
    let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

    assert!(
        prompt.contains("## Pattern Keywords"),
        "Prompt should include pattern glossary when description contains keywords"
    );
    assert!(
        prompt.contains("autopoietic / self-organizing"),
        "Glossary should define autopoietic pattern"
    );
    assert!(
        prompt.contains("committee / discussion / deliberation / swarm"),
        "Glossary should define committee pattern"
    );
    assert!(
        prompt.contains("fork-join / fan-out / parallel"),
        "Glossary should define fork-join pattern"
    );
    assert!(
        prompt.contains("loop / cycle / iterate"),
        "Glossary should define loop pattern"
    );
    assert!(
        prompt.contains("research / investigate / audit"),
        "Glossary should define research pattern"
    );
    assert!(
        prompt.contains("docs/research/organizational-patterns.md"),
        "Glossary should reference the patterns doc"
    );
}

#[test]
fn test_pattern_glossary_excluded_when_no_keywords() {
    let vars = TemplateVars {
        task_id: "plain-task".into(),
        task_title: "Build widget".into(),
        task_description: "Build a widget factory that produces widgets from specs.".into(),
        task_context: "No dependencies".into(),
        task_identity: String::new(),
        working_dir: String::new(),
        skills_preamble: String::new(),
        model: String::new(),
        task_loop_info: String::new(),
        task_verify: None,
        max_child_tasks: 10,
        max_task_depth: 8,
    };
    let ctx = ScopeContext::default();
    let prompt = build_prompt(&vars, ContextScope::Clean, &ctx);

    assert!(
        !prompt.contains("## Pattern Keywords"),
        "Prompt should NOT include pattern glossary when description has no keywords"
    );
}

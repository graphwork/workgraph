//! Snapshot tests for all prompt generation functions.
//!
//! Uses `insta` to capture generated prompts as golden files.
//! Any change to prompt construction fails the test until explicitly approved
//! via `cargo insta review`.

use workgraph::agency::{
    self, EvaluatorInput, ResolvedSkill, Role, TradeoffConfig, render_evaluator_prompt,
    render_identity_prompt,
};
use workgraph::config::CLAUDE_SONNET_MODEL_ID;
use workgraph::context_scope::ContextScope;
use workgraph::graph::LogEntry;
use workgraph::service::executor::{ScopeContext, TemplateVars, build_prompt};

// ---------------------------------------------------------------------------
// Test data builders
// ---------------------------------------------------------------------------

fn test_role() -> Role {
    agency::build_role(
        "Builder",
        "Builds features from specifications with clean, tested code.",
        vec![
            "rust".to_string(),
            "inline:Write idiomatic Rust.".to_string(),
        ],
        "Working, tested code merged to main.",
    )
}

fn test_tradeoff() -> TradeoffConfig {
    agency::build_tradeoff(
        "Quality First",
        "Prioritise correctness and maintainability over speed.",
        vec![
            "Slower delivery for higher quality".into(),
            "More verbose code for clarity".into(),
        ],
        vec!["Skipping tests".into(), "Ignoring error handling".into()],
    )
}

fn test_skills() -> Vec<ResolvedSkill> {
    vec![
        ResolvedSkill {
            name: "Rust".into(),
            content: "Write idiomatic Rust code with proper error handling.".into(),
        },
        ResolvedSkill {
            name: "Testing".into(),
            content: "Write comprehensive unit and integration tests.".into(),
        },
    ]
}

fn test_log_entries() -> Vec<LogEntry> {
    vec![
        LogEntry {
            timestamp: "2025-01-15T10:00:00Z".into(),
            actor: Some("agent-abc".into()),
            user: None,
            message: "Starting implementation of feature X".into(),
        },
        LogEntry {
            timestamp: "2025-01-15T10:30:00Z".into(),
            actor: None,
            user: None,
            message: "Completed core logic, writing tests".into(),
        },
    ]
}

fn test_template_vars() -> TemplateVars {
    TemplateVars {
        task_id: "test-task-123".into(),
        task_title: "Implement widget factory".into(),
        task_description: "Build a widget factory that produces widgets from specs.".into(),
        task_context: "From prerequisite-task: Widget spec is defined in docs/spec.md".into(),
        task_identity: "## Agent Identity\n\nYou are a Builder agent.".into(),
        working_dir: "/home/user/project".into(),
        skills_preamble: "".into(),
        model: CLAUDE_SONNET_MODEL_ID.into(),
        task_loop_info: "".into(),
        max_child_tasks: 10,
        max_task_depth: 8,
        has_failed_deps: false,
        failed_deps_info: String::new(),
        in_worktree: false,
    }
}

fn test_scope_context() -> ScopeContext {
    ScopeContext {
        downstream_info: "\n## Downstream Consumers\n\nTasks that depend on your work:\n- **verify-widgets**: \"Verify widget factory output\"".into(),
        tags_skills_info: "\n## Tags & Skills\n- Tags: implementation, rust\n- Skills: rust, testing".into(),
        project_description: "Workgraph: A lightweight work coordination graph for humans and AI agents.".into(),
        graph_summary: "\n## Graph Status\n\n50 tasks — 45 done, 2 in-progress, 3 open".into(),
        full_graph_summary: "\n## Full Graph\n\nDetailed graph with all 50 tasks and their relationships.".into(),
        claude_md_content: "Use workgraph for task management.\nAlways run tests before marking done.".into(),
        queued_messages: String::new(),
        previous_attempt_context: String::new(),
        wg_guide_content: String::new(),
        discovered_tests: String::new(),
        decomp_guidance: true,
        telegram_available: false,
        native_file_tools: false,
    }
}

// ============================================================================
// render_identity_prompt snapshots
// ============================================================================

#[test]
fn snapshot_identity_prompt_full() {
    let role = test_role();
    let tradeoff = test_tradeoff();
    let skills = test_skills();
    let output = render_identity_prompt(&role, &tradeoff, &skills);
    insta::assert_snapshot!("identity_prompt_full", output);
}

#[test]
fn snapshot_identity_prompt_no_skills() {
    let role = agency::build_role(
        "Reviewer",
        "Reviews code for quality and correctness.",
        vec![],
        "All code reviewed and approved.",
    );
    let tradeoff = test_tradeoff();
    let output = render_identity_prompt(&role, &tradeoff, &[]);
    insta::assert_snapshot!("identity_prompt_no_skills", output);
}

#[test]
fn snapshot_identity_prompt_empty_tradeoffs() {
    let role = test_role();
    let tradeoff = agency::build_tradeoff("Minimal", "Minimal constraints.", vec![], vec![]);
    let skills = test_skills();
    let output = render_identity_prompt(&role, &tradeoff, &skills);
    insta::assert_snapshot!("identity_prompt_empty_tradeoffs", output);
}

#[test]
fn snapshot_identity_prompt_name_only_skills() {
    let role = test_role();
    let tradeoff = test_tradeoff();
    let skills = vec![
        ResolvedSkill {
            name: "rust".into(),
            content: "rust".into(),
        },
        ResolvedSkill {
            name: "testing".into(),
            content: "testing".into(),
        },
    ];
    let output = render_identity_prompt(&role, &tradeoff, &skills);
    insta::assert_snapshot!("identity_prompt_name_only_skills", output);
}

// ============================================================================
// render_evaluator_prompt snapshots
// ============================================================================

#[test]
fn snapshot_evaluator_prompt_full() {
    let role = test_role();
    let tradeoff = test_tradeoff();
    let artifacts = vec![
        "src/widget.rs".to_string(),
        "tests/test_widget.rs".to_string(),
    ];
    let log = test_log_entries();
    let skills = vec!["rust".to_string(), "testing".to_string()];

    let input = EvaluatorInput {
        task_title: "Implement widget factory",
        task_description: Some("Build a widget factory with full test coverage."),
        task_skills: &skills,
        agent: None,
        role: Some(&role),
        tradeoff: Some(&tradeoff),
        artifacts: &artifacts,
        log_entries: &log,
        started_at: Some("2025-01-15T10:00:00Z"),
        completed_at: Some("2025-01-15T11:00:00Z"),
        artifact_diff: Some("diff --git a/src/widget.rs\n+pub fn create_widget() {}"),
        evaluator_identity: None,
        downstream_tasks: &[],
        flip_score: None,
        verify_status: None,
        verify_findings: None,
        resolved_outcome_name: None,
        child_tasks: &[],
    };

    let output = render_evaluator_prompt(&input);
    insta::assert_snapshot!("evaluator_prompt_full", output);
}

#[test]
fn snapshot_evaluator_prompt_minimal() {
    let input = EvaluatorInput {
        task_title: "Simple task",
        task_description: None,
        task_skills: &[],
        agent: None,
        role: None,
        tradeoff: None,
        artifacts: &[],
        log_entries: &[],
        started_at: None,
        completed_at: None,
        artifact_diff: None,
        evaluator_identity: None,
        downstream_tasks: &[],
        flip_score: None,
        verify_status: None,
        verify_findings: None,
        resolved_outcome_name: None,
        child_tasks: &[],
    };

    let output = render_evaluator_prompt(&input);
    insta::assert_snapshot!("evaluator_prompt_minimal", output);
}

#[test]
fn snapshot_evaluator_prompt_with_evaluator_identity() {
    let input = EvaluatorInput {
        task_title: "Feature implementation",
        task_description: Some("Implement the feature."),
        task_skills: &[],
        agent: None,
        role: None,
        tradeoff: None,
        artifacts: &["output.txt".to_string()],
        log_entries: &[],
        started_at: None,
        completed_at: None,
        artifact_diff: None,
        evaluator_identity: Some(
            "## Custom Evaluator\n\nYou are a specialized code quality evaluator.",
        ),
        downstream_tasks: &[],
        flip_score: None,
        verify_status: None,
        verify_findings: None,
        resolved_outcome_name: None,
        child_tasks: &[],
    };

    let output = render_evaluator_prompt(&input);
    insta::assert_snapshot!("evaluator_prompt_with_identity", output);
}

#[test]
fn snapshot_evaluator_prompt_with_downstream_tasks() {
    let role = test_role();
    let tradeoff = test_tradeoff();
    let artifacts = vec!["src/api.rs".to_string()];
    let log = test_log_entries();
    let skills = vec!["rust".to_string()];
    let downstream = vec![
        (
            "Integrate API client".to_string(),
            "Open".to_string(),
            Some("Wire the API client into the service layer.".to_string()),
        ),
        ("Write API docs".to_string(), "Open".to_string(), None),
    ];

    let input = EvaluatorInput {
        task_title: "Build API client",
        task_description: Some("Implement the HTTP API client for the external service."),
        task_skills: &skills,
        agent: None,
        role: Some(&role),
        tradeoff: Some(&tradeoff),
        artifacts: &artifacts,
        log_entries: &log,
        started_at: Some("2025-01-15T10:00:00Z"),
        completed_at: Some("2025-01-15T11:30:00Z"),
        artifact_diff: None,
        evaluator_identity: None,
        downstream_tasks: &downstream,
        flip_score: None,
        verify_status: None,
        verify_findings: None,
        resolved_outcome_name: None,
        child_tasks: &[],
    };

    let output = render_evaluator_prompt(&input);
    insta::assert_snapshot!("evaluator_prompt_with_downstream", output);
}

// ============================================================================
// build_prompt snapshots (all context scopes)
// ============================================================================

#[test]
fn snapshot_build_prompt_clean_scope() {
    let vars = test_template_vars();
    let ctx = test_scope_context();
    let output = build_prompt(&vars, ContextScope::Clean, &ctx);
    insta::assert_snapshot!("build_prompt_clean", output);
}

#[test]
fn snapshot_build_prompt_task_scope() {
    let vars = test_template_vars();
    let ctx = test_scope_context();
    let output = build_prompt(&vars, ContextScope::Task, &ctx);
    insta::assert_snapshot!("build_prompt_task", output);
}

#[test]
fn snapshot_build_prompt_graph_scope() {
    let vars = test_template_vars();
    let ctx = test_scope_context();
    let output = build_prompt(&vars, ContextScope::Graph, &ctx);
    insta::assert_snapshot!("build_prompt_graph", output);
}

#[test]
fn snapshot_build_prompt_full_scope() {
    let vars = test_template_vars();
    let ctx = test_scope_context();
    let output = build_prompt(&vars, ContextScope::Full, &ctx);
    insta::assert_snapshot!("build_prompt_full", output);
}

#[test]
fn snapshot_build_prompt_with_loop_info() {
    let mut vars = test_template_vars();
    vars.task_loop_info =
        "## Cycle Information\n\nThis task is a cycle header (iteration 2, max 5).".into();
    let ctx = test_scope_context();
    let output = build_prompt(&vars, ContextScope::Task, &ctx);
    insta::assert_snapshot!("build_prompt_with_loop", output);
}

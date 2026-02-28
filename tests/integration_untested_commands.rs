//! Integration tests for previously-untested command modules.
//!
//! Covers: agency_migrate, agent_crud, func_cmd, agency_merge, agency_pull/push, resources.
//! All tests invoke the real `wg` binary end-to-end using temp directories for isolation.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Estimate, Node, Resource, Status, Task, WorkGraph};
use workgraph::parser::save_graph;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    stdout
}

fn wg_fail(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    assert!(
        !output.status.success(),
        "wg {:?} unexpectedly succeeded.\nstdout: {}",
        args,
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    format!("{}{}", stdout, stderr)
}

fn setup_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

fn setup_workgraph_with_tasks(tmp: &TempDir, tasks: Vec<Task>) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        ..Task::default()
    }
}

fn write_yaml(dir: &Path, name: &str, content: &str) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{}.yaml", name));
    let mut f = fs::File::create(path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
}

// ===========================================================================
// agency_migrate: CLI integration tests
// ===========================================================================

#[test]
fn test_agency_migrate_nothing_to_migrate() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(&wg_dir, &["agency", "migrate"]);
    assert!(
        output.contains("Nothing to migrate"),
        "Expected 'Nothing to migrate', got: {}",
        output
    );
}

#[test]
fn test_agency_migrate_dry_run() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");

    // Create old-format role
    write_yaml(
        &agency_dir.join("roles"),
        "old-role",
        r#"
id: old-role
name: Tester
description: Tests code
skills:
  - !inline "Always verify tests pass"
desired_outcome: Verified tests
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    let output = wg_ok(&wg_dir, &["agency", "migrate", "--dry-run"]);
    assert!(
        output.contains("[dry-run]"),
        "Expected dry-run output, got: {}",
        output
    );
    assert!(output.contains("Tester"));

    // Verify no new directories created
    assert!(
        !agency_dir.join("primitives/components").exists(),
        "Dry run should not create directories"
    );
}

#[test]
fn test_agency_migrate_full_cycle() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let agency_dir = wg_dir.join("agency");

    // Create old-format role
    write_yaml(
        &agency_dir.join("roles"),
        "role-a",
        r#"
id: role-a
name: Developer
description: Writes and tests code
skills:
  - !name rust
  - !inline "Write clean code with tests"
desired_outcome: Working tested code
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    // Create old-format motivation
    write_yaml(
        &agency_dir.join("motivations"),
        "mot-a",
        r#"
id: mot-a
name: Thorough
description: Prioritizes thoroughness
acceptable_tradeoffs:
  - Slow
unacceptable_tradeoffs:
  - Incomplete
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    // Create old-format agent
    write_yaml(
        &agency_dir.join("agents"),
        "agent-a",
        r#"
id: agent-a
role_id: role-a
motivation_id: mot-a
name: Thorough Developer
lineage:
  generation: 0
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    let output = wg_ok(&wg_dir, &["agency", "migrate"]);
    assert!(
        output.contains("Migration complete"),
        "Expected migration complete, got: {}",
        output
    );
    assert!(output.contains("Verification:"));

    // Verify new-format directories were populated
    assert!(agency_dir.join("primitives/components").exists());
    assert!(agency_dir.join("primitives/tradeoffs").exists());
    assert!(agency_dir.join("cache/roles").exists());
    assert!(agency_dir.join("cache/agents").exists());

    // Check components were created
    let comp_count = fs::read_dir(agency_dir.join("primitives/components"))
        .unwrap()
        .count();
    assert!(
        comp_count >= 2,
        "Expected at least 2 components, got {}",
        comp_count
    );

    // Running again should be idempotent
    let output2 = wg_ok(&wg_dir, &["agency", "migrate"]);
    assert!(output2.contains("Migration complete"));
}

// ===========================================================================
// agent_crud: CLI integration tests
// ===========================================================================

/// Helper: initialize agency and create a role + tradeoff, return their IDs.
fn setup_agency_with_role_and_tradeoff(wg_dir: &Path) -> (String, String) {
    // Init agency
    wg_ok(wg_dir, &["agency", "init"]);

    // We need to create a role and tradeoff via the library API since the CLI
    // for creating these involves the `role` and `tradeoff` commands.
    // Instead, let's use the CLI to create primitives indirectly via migrate.
    // Actually, let's write them directly as YAML.
    let agency_dir = wg_dir.join("agency");

    // Create a role by writing YAML
    let role_content = r#"id: aaaa1111bbbb2222cccc3333dddd4444eeee5555ffff6666aaaa1111bbbb2222
name: Test Role
description: A test role for integration tests
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  avg_score: null
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
default_context_scope: null
"#;
    let roles_dir = agency_dir.join("cache/roles");
    fs::create_dir_all(&roles_dir).unwrap();
    fs::write(
        roles_dir.join("aaaa1111bbbb2222cccc3333dddd4444eeee5555ffff6666aaaa1111bbbb2222.yaml"),
        role_content,
    )
    .unwrap();

    // Create a tradeoff by writing YAML
    let tradeoff_content = r#"id: 1111aaaa2222bbbb3333cccc4444dddd5555eeee6666ffff1111aaaa2222bbbb
name: Test Tradeoff
description: A test tradeoff for integration tests
acceptable_tradeoffs:
  - Slow
unacceptable_tradeoffs:
  - Broken
performance:
  task_count: 0
  avg_score: null
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
access_control:
  owner: local
  policy: open
former_agents: []
former_deployments: []
"#;
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    fs::create_dir_all(&tradeoffs_dir).unwrap();
    fs::write(
        tradeoffs_dir.join("1111aaaa2222bbbb3333cccc4444dddd5555eeee6666ffff1111aaaa2222bbbb.yaml"),
        tradeoff_content,
    )
    .unwrap();

    (
        "aaaa1111bbbb2222cccc3333dddd4444eeee5555ffff6666aaaa1111bbbb2222".to_string(),
        "1111aaaa2222bbbb3333cccc4444dddd5555eeee6666ffff1111aaaa2222bbbb".to_string(),
    )
}

#[test]
fn test_agent_create_list_show_rm() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let (role_id, tradeoff_id) = setup_agency_with_role_and_tradeoff(&wg_dir);

    // Create agent
    let output = wg_ok(
        &wg_dir,
        &[
            "agent",
            "create",
            "Integration Test Agent",
            "--role",
            &role_id[..8],
            "--tradeoff",
            &tradeoff_id[..8],
        ],
    );
    assert!(
        output.contains("Created agent"),
        "Expected 'Created agent', got: {}",
        output
    );
    assert!(output.contains("Integration Test Agent"));

    // List agents
    let list_output = wg_ok(&wg_dir, &["agent", "list"]);
    assert!(
        list_output.contains("Integration Test Agent"),
        "Expected agent in list, got: {}",
        list_output
    );

    // List agents (JSON)
    let json_output = wg_ok(&wg_dir, &["--json", "agent", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    let agents = parsed.as_array().unwrap();
    assert!(
        agents.len() >= 1,
        "Expected at least 1 agent in list"
    );
    let our_agent = agents
        .iter()
        .find(|a| a["name"] == "Integration Test Agent")
        .expect("Created agent not found in JSON list");

    // Extract agent ID from JSON
    let agent_id = our_agent["id"].as_str().unwrap().to_string();

    // Show agent
    let show_output = wg_ok(&wg_dir, &["agent", "show", &agent_id[..8]]);
    assert!(show_output.contains("Integration Test Agent"));
    assert!(show_output.contains("Role:"));

    // Show agent (JSON)
    let show_json = wg_ok(&wg_dir, &["--json", "agent", "show", &agent_id[..8]]);
    let show_parsed: serde_json::Value = serde_json::from_str(&show_json).unwrap();
    assert_eq!(show_parsed["name"], "Integration Test Agent");

    // Remove agent
    let rm_output = wg_ok(&wg_dir, &["agent", "rm", &agent_id[..8]]);
    assert!(
        rm_output.contains("Removed agent"),
        "Expected 'Removed agent', got: {}",
        rm_output
    );

    // Verify agent was removed (it should no longer appear in JSON list)
    let json_after = wg_ok(&wg_dir, &["--json", "agent", "list"]);
    let parsed_after: serde_json::Value = serde_json::from_str(&json_after).unwrap();
    let found = parsed_after
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["name"] == "Integration Test Agent");
    assert!(
        !found,
        "Removed agent should not appear in list"
    );
}

#[test]
fn test_agent_create_human_without_role() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(&wg_dir, &["agency", "init"]);

    // Human agent (non-AI executor) should not require role/tradeoff
    let output = wg_ok(
        &wg_dir,
        &[
            "agent",
            "create",
            "Human Operator",
            "--executor",
            "matrix",
            "--contact",
            "@human:matrix.org",
        ],
    );
    assert!(output.contains("Created agent"));
    assert!(output.contains("Human Operator"));
}

#[test]
fn test_agent_create_ai_requires_role() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(&wg_dir, &["agency", "init"]);

    // AI agent (default executor=claude) without role should fail
    let output = wg_fail(&wg_dir, &["agent", "create", "Bad AI"]);
    assert!(
        output.contains("--role is required"),
        "Expected role required error, got: {}",
        output
    );
}

#[test]
fn test_agent_lineage_and_performance() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    let (role_id, tradeoff_id) = setup_agency_with_role_and_tradeoff(&wg_dir);

    // Create agent
    wg_ok(
        &wg_dir,
        &[
            "agent",
            "create",
            "Lineage Agent",
            "--role",
            &role_id[..8],
            "--tradeoff",
            &tradeoff_id[..8],
        ],
    );

    // Get agent ID
    let json_output = wg_ok(&wg_dir, &["--json", "agent", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    let agent_id = parsed[0]["id"].as_str().unwrap();

    // Lineage should work
    let lineage_output = wg_ok(&wg_dir, &["agent", "lineage", &agent_id[..8]]);
    assert!(
        lineage_output.contains("Lineage for agent"),
        "Expected lineage output, got: {}",
        lineage_output
    );

    // Performance should work (empty but not error)
    let perf_output = wg_ok(&wg_dir, &["agent", "performance", &agent_id[..8]]);
    assert!(
        perf_output.contains("Performance for agent"),
        "Expected performance output, got: {}",
        perf_output
    );
}

#[test]
fn test_agent_rm_not_found() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(&wg_dir, &["agency", "init"]);

    let output = wg_fail(&wg_dir, &["agent", "rm", "nonexistent"]);
    assert!(
        output.contains("Failed to find agent") || output.contains("not found") || output.contains("No agent matching"),
        "Expected not-found error, got: {}",
        output
    );
}

// ===========================================================================
// func_cmd: CLI integration tests
// ===========================================================================

fn setup_function(wg_dir: &Path) {
    let func_dir = wg_dir.join("functions");
    fs::create_dir_all(&func_dir).unwrap();

    let func_yaml = r#"kind: trace-function
version: 1
id: test-feature
name: Test Feature Implementation
description: Plan, implement, and test a feature
extracted_from:
  - task_id: sample-task
    timestamp: "2026-02-20T12:00:00Z"
tags:
  - implementation
inputs:
  - name: feature_name
    type: string
    description: Name of the feature
    required: true
  - name: test_command
    type: string
    description: Command to verify
    required: false
    default: cargo test
tasks:
  - template_id: plan
    title: "Plan {{input.feature_name}}"
    description: Plan the implementation
    skills:
      - analysis
    after: []
    loops_to: []
    deliverables: []
    tags: []
  - template_id: implement
    title: "Implement {{input.feature_name}}"
    description: Build the feature
    skills:
      - implementation
    after:
      - plan
    loops_to: []
    deliverables: []
    tags: []
outputs:
  - name: modified_files
    description: Files changed
    from_task: implement
    field: artifacts
visibility: internal
redacted_fields: []
"#;

    fs::write(func_dir.join("test-feature.yaml"), func_yaml).unwrap();
}

#[test]
fn test_func_list_empty() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    fs::create_dir_all(wg_dir.join("functions")).unwrap();

    let output = wg_ok(&wg_dir, &["func", "list"]);
    assert!(
        output.contains("No functions found"),
        "Expected 'No functions found', got: {}",
        output
    );
}

#[test]
fn test_func_list_with_function() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    setup_function(&wg_dir);

    let output = wg_ok(&wg_dir, &["func", "list"]);
    assert!(
        output.contains("test-feature"),
        "Expected function ID in list, got: {}",
        output
    );
    assert!(output.contains("Test Feature Implementation") || output.contains("2 tasks"));
}

#[test]
fn test_func_list_json() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    setup_function(&wg_dir);

    let output = wg_ok(&wg_dir, &["--json", "func", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "test-feature");
}

#[test]
fn test_func_list_verbose() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    setup_function(&wg_dir);

    let output = wg_ok(&wg_dir, &["func", "list", "--verbose"]);
    assert!(
        output.contains("Inputs:") || output.contains("Tasks:"),
        "Expected verbose output with inputs/tasks, got: {}",
        output
    );
}

#[test]
fn test_func_show() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    setup_function(&wg_dir);

    let output = wg_ok(&wg_dir, &["func", "show", "test-feature"]);
    assert!(output.contains("Function: test-feature"));
    assert!(output.contains("Name: Test Feature Implementation"));
    assert!(output.contains("feature_name"));
}

#[test]
fn test_func_show_by_prefix() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    setup_function(&wg_dir);

    let output = wg_ok(&wg_dir, &["func", "show", "test"]);
    assert!(output.contains("Function: test-feature"));
}

#[test]
fn test_func_show_json() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    setup_function(&wg_dir);

    let output = wg_ok(&wg_dir, &["--json", "func", "show", "test-feature"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["id"], "test-feature");
    assert_eq!(parsed["tasks"].as_array().unwrap().len(), 2);
}

#[test]
fn test_func_show_not_found() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    fs::create_dir_all(wg_dir.join("functions")).unwrap();

    let output = wg_fail(&wg_dir, &["func", "show", "nonexistent"]);
    assert!(
        output.contains("No function matching") || output.contains("not found"),
        "Expected not-found error, got: {}",
        output
    );
}

// ===========================================================================
// agency_merge: CLI integration tests
// ===========================================================================

fn setup_agency_store(base: &Path, name: &str) -> PathBuf {
    let store_dir = base.join(name).join("agency");
    workgraph::agency::init(&store_dir).unwrap();
    store_dir
}

#[test]
fn test_agency_merge_two_stores() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create two agency stores with different entities
    let store_a = setup_agency_store(tmp.path(), "store-a");
    let store_b = setup_agency_store(tmp.path(), "store-b");

    // Write a role into store A
    write_yaml(
        &store_a.join("cache/roles"),
        "role-from-a",
        r#"
id: role-from-a
name: Role A
description: test
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    // Write a role into store B
    write_yaml(
        &store_b.join("cache/roles"),
        "role-from-b",
        r#"
id: role-from-b
name: Role B
description: test
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    // Initialize the target agency
    wg_ok(&wg_dir, &["agency", "init"]);

    let output = wg_ok(
        &wg_dir,
        &[
            "agency",
            "merge",
            store_a.to_str().unwrap(),
            store_b.to_str().unwrap(),
        ],
    );
    assert!(
        output.contains("Merged from 2 sources"),
        "Expected merge output, got: {}",
        output
    );

    // Verify both roles exist in the target
    let target_roles_dir = wg_dir.join("agency/cache/roles");
    assert!(target_roles_dir.join("role-from-a.yaml").exists());
    assert!(target_roles_dir.join("role-from-b.yaml").exists());
}

#[test]
fn test_agency_merge_dry_run() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let store_a = setup_agency_store(tmp.path(), "store-a");
    let store_b = setup_agency_store(tmp.path(), "store-b");

    write_yaml(
        &store_a.join("cache/roles"),
        "dry-role",
        r#"
id: dry-role
name: Dry Role
description: test
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    wg_ok(&wg_dir, &["agency", "init"]);

    let output = wg_ok(
        &wg_dir,
        &[
            "agency",
            "merge",
            "--dry-run",
            store_a.to_str().unwrap(),
            store_b.to_str().unwrap(),
        ],
    );
    assert!(
        output.contains("Would merge"),
        "Expected dry-run output, got: {}",
        output
    );

    // Verify nothing was written
    let target_roles_dir = wg_dir.join("agency/cache/roles");
    assert!(!target_roles_dir.join("dry-role.yaml").exists());
}

#[test]
fn test_agency_merge_requires_two_sources() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(&wg_dir, &["agency", "init"]);

    let output = wg_fail(&wg_dir, &["agency", "merge", "/one/source"]);
    assert!(
        output.contains("at least 2 sources") || output.contains("2 sources"),
        "Expected at-least-2 error, got: {}",
        output
    );
}

// ===========================================================================
// agency_pull / agency_push: CLI integration tests
// ===========================================================================

#[test]
fn test_agency_pull_from_path() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create source store with a role
    let source_dir = setup_agency_store(tmp.path(), "source");
    write_yaml(
        &source_dir.join("cache/roles"),
        "pull-role",
        r#"
id: pull-role
name: Pulled Role
description: A role to pull
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    wg_ok(&wg_dir, &["agency", "init"]);

    let output = wg_ok(
        &wg_dir,
        &["agency", "pull", source_dir.to_str().unwrap()],
    );
    assert!(
        output.contains("Pulled") || output.contains("pull"),
        "Expected pull output, got: {}",
        output
    );

    // Verify role was pulled
    assert!(wg_dir
        .join("agency/cache/roles/pull-role.yaml")
        .exists());
}

#[test]
fn test_agency_pull_dry_run() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let source_dir = setup_agency_store(tmp.path(), "source");
    write_yaml(
        &source_dir.join("cache/roles"),
        "dry-pull-role",
        r#"
id: dry-pull-role
name: Dry Pull Role
description: test
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    wg_ok(&wg_dir, &["agency", "init"]);

    let output = wg_ok(
        &wg_dir,
        &[
            "agency",
            "pull",
            "--dry-run",
            source_dir.to_str().unwrap(),
        ],
    );
    assert!(
        output.contains("Would pull"),
        "Expected dry-run output, got: {}",
        output
    );

    // Verify nothing was written
    assert!(!wg_dir
        .join("agency/cache/roles/dry-pull-role.yaml")
        .exists());
}

#[test]
fn test_agency_push_to_path() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(&wg_dir, &["agency", "init"]);

    // Create a role in the local agency
    write_yaml(
        &wg_dir.join("agency/cache/roles"),
        "push-role",
        r#"
id: push-role
name: Pushed Role
description: A role to push
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    // Create target store
    let target_dir = tmp.path().join("target");
    fs::create_dir_all(&target_dir).unwrap();

    let output = wg_ok(
        &wg_dir,
        &["agency", "push", target_dir.to_str().unwrap()],
    );
    assert!(
        output.contains("Pushed") || output.contains("push"),
        "Expected push output, got: {}",
        output
    );

    // Verify role was pushed (stored under target/agency/)
    assert!(
        target_dir.join("agency/cache/roles/push-role.yaml").exists(),
        "Expected pushed role at target"
    );
}

#[test]
fn test_agency_push_dry_run() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    wg_ok(&wg_dir, &["agency", "init"]);

    write_yaml(
        &wg_dir.join("agency/cache/roles"),
        "dry-push-role",
        r#"
id: dry-push-role
name: Dry Push Role
description: test
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    let target_dir = setup_agency_store(tmp.path(), "target");

    let output = wg_ok(
        &wg_dir,
        &[
            "agency",
            "push",
            "--dry-run",
            target_dir.to_str().unwrap(),
        ],
    );
    assert!(
        output.contains("Dry run") || output.contains("dry_run") || output.contains("would push"),
        "Expected dry-run output, got: {}",
        output
    );

    // Verify nothing was written
    assert!(!target_dir
        .join("cache/roles/dry-push-role.yaml")
        .exists());
}

#[test]
fn test_agency_push_no_local_store_errors() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);
    // Don't init agency

    let target_dir = tmp.path().join("target");
    fs::create_dir_all(&target_dir).unwrap();

    let output = wg_fail(
        &wg_dir,
        &["agency", "push", target_dir.to_str().unwrap()],
    );
    assert!(
        output.contains("No local agency store") || output.contains("agency init"),
        "Expected no-store error, got: {}",
        output
    );
}

#[test]
fn test_agency_pull_push_roundtrip() {
    let tmp = TempDir::new().unwrap();

    // Project A: has a role
    let proj_a_dir = tmp.path().join("project-a").join(".workgraph");
    fs::create_dir_all(&proj_a_dir).unwrap();
    save_graph(&WorkGraph::new(), &proj_a_dir.join("graph.jsonl")).unwrap();
    wg_ok(&proj_a_dir, &["agency", "init"]);

    write_yaml(
        &proj_a_dir.join("agency/cache/roles"),
        "shared-role",
        r#"
id: shared-role
name: Shared Role
description: A role shared between projects
component_ids: []
outcome_id: ""
performance:
  task_count: 0
  evaluations: []
lineage:
  generation: 0
  parent_ids: []
  created_by: human
  created_at: 2026-02-25T00:00:00Z
"#,
    );

    // Project B: initially empty
    let proj_b_dir = tmp.path().join("project-b").join(".workgraph");
    fs::create_dir_all(&proj_b_dir).unwrap();
    save_graph(&WorkGraph::new(), &proj_b_dir.join("graph.jsonl")).unwrap();
    wg_ok(&proj_b_dir, &["agency", "init"]);

    // Push from A to a shared store
    let shared = tmp.path().join("shared");
    fs::create_dir_all(&shared).unwrap();

    wg_ok(
        &proj_a_dir,
        &["agency", "push", shared.to_str().unwrap()],
    );

    // Pull from shared store into B
    let shared_agency = shared.join("agency");
    wg_ok(
        &proj_b_dir,
        &["agency", "pull", shared_agency.to_str().unwrap()],
    );

    // Verify B now has the shared role
    assert!(proj_b_dir
        .join("agency/cache/roles/shared-role.yaml")
        .exists());
}

// ===========================================================================
// resources: CLI integration tests
// ===========================================================================

#[test]
fn test_resources_no_resources() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph_with_tasks(
        &tmp,
        vec![make_task("t1", "Some task", Status::Open)],
    );

    let output = wg_ok(&wg_dir, &["resources"]);
    assert!(
        output.contains("No resources with capacity defined"),
        "Expected no-resources message, got: {}",
        output
    );
}

#[test]
fn test_resources_basic_utilization() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    let mut graph = WorkGraph::new();

    // Add resource
    graph.add_node(Node::Resource(Resource {
        id: "eng-budget".to_string(),
        name: Some("Engineering Budget".to_string()),
        resource_type: Some("money".to_string()),
        available: Some(10000.0),
        unit: Some("usd".to_string()),
    }));

    // Add open task requiring the resource
    let task = Task {
        id: "task-1".to_string(),
        title: "Build feature".to_string(),
        requires: vec!["eng-budget".to_string()],
        estimate: Some(Estimate {
            hours: Some(10.0),
            cost: Some(5000.0),
        }),
        ..Task::default()
    };
    graph.add_node(Node::Task(task));

    // Add done task
    let done_task = Task {
        id: "task-2".to_string(),
        title: "Done feature".to_string(),
        status: Status::Done,
        requires: vec!["eng-budget".to_string()],
        estimate: Some(Estimate {
            hours: Some(5.0),
            cost: Some(2000.0),
        }),
        ..Task::default()
    };
    graph.add_node(Node::Task(done_task));

    save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

    let output = wg_ok(&wg_dir, &["resources"]);
    assert!(
        output.contains("Engineering Budget"),
        "Expected resource name, got: {}",
        output
    );
    assert!(output.contains("$10000 available") || output.contains("10000"));
    assert!(output.contains("Committed") || output.contains("committed"));
}

#[test]
fn test_resources_over_budget_alert() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    let mut graph = WorkGraph::new();

    graph.add_node(Node::Resource(Resource {
        id: "small-budget".to_string(),
        name: Some("Small Budget".to_string()),
        resource_type: Some("money".to_string()),
        available: Some(100.0),
        unit: Some("usd".to_string()),
    }));

    let task = Task {
        id: "expensive".to_string(),
        title: "Expensive task".to_string(),
        requires: vec!["small-budget".to_string()],
        estimate: Some(Estimate {
            hours: Some(100.0),
            cost: Some(500.0),
        }),
        ..Task::default()
    };
    graph.add_node(Node::Task(task));

    save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

    let output = wg_ok(&wg_dir, &["resources"]);
    assert!(
        output.contains("ALERT") || output.contains("OVER BUDGET"),
        "Expected over-budget alert, got: {}",
        output
    );
    assert!(output.contains("expensive"));
}

#[test]
fn test_resources_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    let mut graph = WorkGraph::new();

    graph.add_node(Node::Resource(Resource {
        id: "budget".to_string(),
        name: Some("Budget".to_string()),
        resource_type: Some("money".to_string()),
        available: Some(5000.0),
        unit: Some("usd".to_string()),
    }));

    let task = Task {
        id: "t1".to_string(),
        title: "Task".to_string(),
        requires: vec!["budget".to_string()],
        estimate: Some(Estimate {
            hours: Some(10.0),
            cost: Some(2000.0),
        }),
        ..Task::default()
    };
    graph.add_node(Node::Task(task));

    save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

    let output = wg_ok(&wg_dir, &["--json", "resources"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(parsed["resources"].is_array());
    assert_eq!(parsed["resources"][0]["id"], "budget");
    assert_eq!(parsed["resources"][0]["available"], 5000.0);
    assert_eq!(parsed["resources"][0]["committed"], 2000.0);
    assert_eq!(parsed["resources"][0]["over_budget"], false);
    assert!(parsed["alerts"].as_array().unwrap().is_empty());
}

#[test]
fn test_resources_json_empty() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(&wg_dir, &["--json", "resources"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(parsed["resources"].as_array().unwrap().is_empty());
    assert!(parsed["alerts"].as_array().unwrap().is_empty());
}

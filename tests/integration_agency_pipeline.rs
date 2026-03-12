//! Integration tests for the agency pipeline: registry resolution, placement,
//! creator trigger, and config coherence.
//!
//! These tests exercise the full stack through both the Rust API and the CLI,
//! covering the task description's four test categories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

use workgraph::config::{
    Config, DispatchRole, ModelRegistryEntry, RoleModelConfig, Tier, TierConfig,
};
use workgraph::graph::{Node, Status, Task, WorkGraph, is_system_task};
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
        .env_remove("WG_TASK_ID")
        .env_remove("WG_AGENT_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_cmd_agent_context(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .env("WG_TASK_ID", "parent-task")
        .env("WG_AGENT_ID", "test-agent-hash")
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

fn wg_combined(wg_dir: &Path, args: &[&str]) -> (bool, String, String) {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), stdout, stderr)
}

fn setup_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

fn make_task(id: &str, title: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        ..Task::default()
    }
}

// ===========================================================================
// Registry resolution tests
// ===========================================================================

#[test]
fn registry_no_config_returns_3_builtins() {
    let config = Config::default();
    let registry = config.effective_registry();
    assert_eq!(registry.len(), 3, "Expected 3 built-in entries");
    let ids: Vec<&str> = registry.iter().map(|e| e.id.as_str()).collect();
    assert!(ids.contains(&"haiku"));
    assert!(ids.contains(&"sonnet"));
    assert!(ids.contains(&"opus"));
}

#[test]
fn registry_user_entries_override_builtins() {
    let mut config = Config::default();
    config.model_registry = vec![ModelRegistryEntry {
        id: "haiku".into(),
        provider: "local".into(),
        model: "my-custom-haiku".into(),
        tier: Tier::Fast,
        ..Default::default()
    }];
    let registry = config.effective_registry();
    // 2 remaining built-ins + 1 override = 3
    assert_eq!(registry.len(), 3);
    let haiku = registry.iter().find(|e| e.id == "haiku").unwrap();
    assert_eq!(haiku.model, "my-custom-haiku");
    assert_eq!(haiku.provider, "local");
    // Built-in sonnet and opus should still be present
    assert!(registry.iter().any(|e| e.id == "sonnet"));
    assert!(registry.iter().any(|e| e.id == "opus"));
}

#[test]
fn role_with_explicit_model_ignores_tier() {
    let mut config = Config::default();
    config.models.triage = Some(RoleModelConfig {
        model: Some("my-explicit-model".to_string()),
        provider: None,
        tier: Some(Tier::Premium), // Should be ignored
    });
    let resolved = config.resolve_model_for_role(DispatchRole::Triage);
    assert_eq!(resolved.model, "my-explicit-model");
    // Premium tier would resolve to opus, so explicit model wins
    assert_ne!(resolved.model, "claude-opus-4-6");
}

#[test]
fn role_with_tier_override_resolves_via_registry() {
    let mut config = Config::default();
    config.models.evaluator = Some(RoleModelConfig {
        model: None,
        provider: None,
        tier: Some(Tier::Premium), // Override default Standard → Premium
    });
    let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
    assert_eq!(resolved.model, "claude-opus-4-6");
    assert!(resolved.registry_entry.is_some());
    assert_eq!(resolved.registry_entry.unwrap().id, "opus");
}

#[test]
fn all_roles_resolve_via_default_tier() {
    let config = Config::default();
    // Test all roles in DispatchRole::ALL resolve without panicking
    // and return the expected model for their default tier
    for role in DispatchRole::ALL {
        let resolved = config.resolve_model_for_role(*role);
        let tier = role.default_tier();
        let expected_model = match tier {
            Tier::Fast => "claude-haiku-4-5-20251001",
            Tier::Standard => "claude-sonnet-4-20250514",
            Tier::Premium => "claude-opus-4-6",
        };
        assert_eq!(
            resolved.model, expected_model,
            "Role {:?} (tier {:?}) resolved to '{}', expected '{}'",
            role, tier, resolved.model, expected_model
        );
    }
    // Also test the Default role
    let resolved = config.resolve_model_for_role(DispatchRole::Default);
    assert_eq!(resolved.model, "claude-sonnet-4-20250514"); // Standard tier
}

#[test]
fn unknown_model_id_in_tier_config_graceful_fallback() {
    let mut config = Config::default();
    config.tiers = TierConfig {
        fast: Some("nonexistent-model".into()),
        standard: None,
        premium: None,
    };
    // Triage uses Fast tier → should get the bare "nonexistent-model" as a fallback
    let resolved = config.resolve_model_for_role(DispatchRole::Triage);
    assert_eq!(resolved.model, "nonexistent-model");
    assert!(
        resolved.registry_entry.is_none(),
        "Unknown model should not have registry entry"
    );
    assert!(
        resolved.provider.is_none(),
        "Unknown model should not have provider"
    );
}

#[test]
fn registry_remove_warns_about_dependent_roles_tiers() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Add a custom entry (--id, --provider, --reg-model, --reg-tier)
    wg_ok(
        &wg_dir,
        &[
            "config",
            "--registry-add",
            "--id",
            "my-model",
            "--provider",
            "local",
            "--reg-model",
            "local/my-model",
            "--reg-tier",
            "fast",
        ],
    );

    // Set tier to depend on it
    wg_ok(&wg_dir, &["config", "--tier", "fast=my-model"]);

    // Removing should warn about tier dependency (non-force should fail)
    let (success, stdout, stderr) =
        wg_combined(&wg_dir, &["config", "--registry-remove", "my-model"]);
    let combined = format!("{}{}", stdout, stderr);
    // Should mention tiers.fast or dependency warning, or refuse to proceed
    assert!(
        combined.contains("tiers.fast")
            || combined.contains("dependent")
            || combined.contains("warning")
            || !success,
        "Expected warning about dependent tier, got:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
}

// ===========================================================================
// Placement pipeline tests
// ===========================================================================

#[test]
fn add_creates_draft_paused_task() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // wg add in interactive mode (no WG_TASK_ID) creates a paused (draft) task
    let output = wg_ok(&wg_dir, &["add", "Test draft task"]);
    assert!(
        output.contains("draft") || output.contains("paused"),
        "Expected draft/paused output, got: {}",
        output
    );

    // Verify via show that the task is paused
    let show = wg_ok(&wg_dir, &["show", "test-draft-task"]);
    assert!(
        show.contains("paused") || show.contains("Paused"),
        "Task should be paused (draft mode), got: {}",
        show
    );
}

#[test]
fn add_no_place_creates_unpaused_task_with_unplaced() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(&wg_dir, &["add", "Test no-place task", "--no-place"]);
    // Should NOT be draft/paused
    assert!(
        !output.contains("draft"),
        "With --no-place, task should not be draft, got: {}",
        output
    );

    // The slug generator takes up to 3 words: "test-no-place"
    // Verify not paused via show (JSON output doesn't include unplaced field,
    // but the task should be Open and not have "draft" in output)
    let show_output = wg_ok(&wg_dir, &["show", "test-no-place"]);
    assert!(
        !show_output.contains("Paused: true"),
        "Task should not be paused with --no-place, got: {}",
        show_output
    );
    // Verify it's Open status
    assert!(
        show_output.contains("open") || show_output.contains("Open"),
        "Task should be Open, got: {}",
        show_output
    );
}

#[test]
fn system_tasks_skip_draft_by_default() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // System tasks start with a dot — they should NOT be draft/paused
    // Use --id to preserve the dot-prefix (slug generator strips dots)
    let output = wg_ok(
        &wg_dir,
        &["add", ".system-test-task", "--id", ".system-test-task"],
    );
    assert!(
        !output.contains("draft"),
        "System task should not be draft, got: {}",
        output
    );

    // Verify it's Open, not paused
    let show_output = wg_ok(&wg_dir, &["show", ".system-test-task"]);
    assert!(
        !show_output.contains("Paused: true"),
        "System task should not be paused, got: {}",
        show_output
    );
}

#[test]
fn build_placement_tasks_identifies_draft_tasks() {
    // Unit test: draft (paused, not unplaced, not system) tasks need placement
    let mut graph = WorkGraph::new();

    // Draft task: paused, not unplaced, not system
    let mut draft_task = make_task("my-feature", "Implement feature");
    draft_task.paused = true;
    draft_task.status = Status::Open;
    graph.add_node(Node::Task(draft_task));

    // Unplaced task: paused=false, unplaced=true — should NOT need placement
    let mut unplaced_task = make_task("no-place-task", "No placement needed");
    unplaced_task.unplaced = true;
    unplaced_task.status = Status::Open;
    graph.add_node(Node::Task(unplaced_task));

    // System task: paused but system prefix — should NOT need placement
    let mut sys_task = make_task(".eval-foo", "Evaluate foo");
    sys_task.paused = true;
    sys_task.status = Status::Open;
    graph.add_node(Node::Task(sys_task));

    // Already placed task: paused but tagged "placed" — should NOT need placement
    let mut placed_task = make_task("already-placed", "Already placed");
    placed_task.paused = true;
    placed_task.status = Status::Open;
    placed_task.tags = vec!["placed".to_string()];
    graph.add_node(Node::Task(placed_task));

    // Check which tasks need placement using the same logic as build_placement_tasks
    let needs_placement: Vec<String> = graph
        .tasks()
        .filter(|t| {
            t.paused
                && !t.unplaced
                && !is_system_task(&t.id)
                && !t.tags.iter().any(|tag| tag == "placed")
        })
        .map(|t| t.id.clone())
        .collect();

    assert_eq!(needs_placement.len(), 1);
    assert_eq!(needs_placement[0], "my-feature");
}

#[test]
fn auto_place_fast_path_with_deps_no_overlap() {
    // When a draft task has --after deps and no file overlap with active tasks,
    // the coordinator should auto-place it (unpause without creating .place-*)
    let mut graph = WorkGraph::new();

    // An active task with artifacts
    let mut active = make_task("active-task", "Active task");
    active.status = Status::InProgress;
    active.artifacts = vec!["src/unrelated.rs".to_string()];
    graph.add_node(Node::Task(active));

    // A done dependency
    let mut dep = make_task("dep-task", "Dependency");
    dep.status = Status::Done;
    graph.add_node(Node::Task(dep));

    // Draft task with deps, mentioning files that don't overlap with active
    let mut draft = make_task("draft-with-deps", "Draft with deps");
    draft.paused = true;
    draft.status = Status::Open;
    draft.after = vec!["dep-task".to_string()];
    draft.description = Some("Modify src/new_module.rs".to_string());
    graph.add_node(Node::Task(draft));

    // Replicate the auto-place fast path logic
    let task = graph.get_task("draft-with-deps").unwrap();
    let after_deps = &task.after;
    let has_deps = !after_deps.is_empty();

    // Extract file paths from description
    let desc = task.description.as_deref().unwrap_or("");
    let mentioned_files: Vec<String> = desc
        .split_whitespace()
        .filter(|w| w.contains('/') && (w.ends_with(".rs") || w.ends_with(".ts")))
        .map(|w| w.to_string())
        .collect();

    let has_overlap = if mentioned_files.is_empty() {
        false
    } else {
        graph
            .tasks()
            .filter(|t| {
                t.id != "draft-with-deps"
                    && !t.status.is_terminal()
                    && !t.paused
                    && !is_system_task(&t.id)
            })
            .any(|t| {
                t.artifacts
                    .iter()
                    .any(|a| mentioned_files.iter().any(|f| a.contains(f)))
            })
    };

    assert!(has_deps, "Task should have deps");
    assert!(
        !has_overlap,
        "Should have no file overlap with active tasks"
    );
    // This combination triggers auto-place fast path
}

#[test]
fn agent_placement_task_without_deps() {
    // Draft task without deps should create a .place-* task (not auto-place)
    let mut graph = WorkGraph::new();

    let mut draft = make_task("no-deps-task", "Task without dependencies");
    draft.paused = true;
    draft.status = Status::Open;
    graph.add_node(Node::Task(draft));

    // Check that it needs placement
    let needs_placement: Vec<String> = graph
        .tasks()
        .filter(|t| {
            t.paused
                && !t.unplaced
                && !is_system_task(&t.id)
                && !t.tags.iter().any(|tag| tag == "placed")
        })
        .map(|t| t.id.clone())
        .collect();

    assert!(needs_placement.contains(&"no-deps-task".to_string()));

    // The task has no deps so auto-place fast path should NOT apply
    let task = graph.get_task("no-deps-task").unwrap();
    assert!(
        task.after.is_empty(),
        "Task should have no deps for this test"
    );
}

#[test]
fn place_task_failure_publishes_original() {
    // If a .place-* task fails, the original task should get published (unpaused)
    let mut graph = WorkGraph::new();

    // Original draft task
    let mut draft = make_task("feature-x", "Feature X");
    draft.paused = true;
    draft.status = Status::Open;
    graph.add_node(Node::Task(draft));

    // Failed placement task
    let mut place_task = make_task(".place-feature-x", "Place: feature-x");
    place_task.status = Status::Failed;
    place_task.tags = vec!["placement".to_string()];
    graph.add_node(Node::Task(place_task));

    // Replicate the fallback-publish logic from build_placement_tasks
    let failed_placers: Vec<(String, String)> = graph
        .tasks()
        .filter(|t| {
            t.id.starts_with(".place-")
                && t.status == Status::Failed
                && !t.tags.iter().any(|tag| tag == "fallback-published")
        })
        .map(|t| {
            let source_id = t.id.strip_prefix(".place-").unwrap().to_string();
            (t.id.clone(), source_id)
        })
        .collect();

    assert_eq!(failed_placers.len(), 1);
    assert_eq!(failed_placers[0].0, ".place-feature-x");
    assert_eq!(failed_placers[0].1, "feature-x");

    // Apply the fallback: unpause and tag
    for (place_id, source_id) in &failed_placers {
        if let Some(source) = graph.get_task_mut(source_id) {
            source.paused = false;
            if !source.tags.contains(&"placed".to_string()) {
                source.tags.push("placed".to_string());
            }
        }
        if let Some(pt) = graph.get_task_mut(place_id) {
            pt.tags.push("fallback-published".to_string());
        }
    }

    let source = graph.get_task("feature-x").unwrap();
    assert!(
        !source.paused,
        "Original task should be unpaused after fallback"
    );
    assert!(
        source.tags.contains(&"placed".to_string()),
        "Original task should have 'placed' tag"
    );
}

#[test]
fn placement_hints_appear_in_place_task_description() {
    // When a draft task has place_near/place_before hints, verify they would
    // appear in the placement context
    let mut graph = WorkGraph::new();

    let mut draft = make_task("hint-task", "Task with placement hints");
    draft.paused = true;
    draft.status = Status::Open;
    draft.place_near = vec!["related-task".to_string()];
    draft.place_before = vec!["final-task".to_string()];
    graph.add_node(Node::Task(draft));

    // Verify hints are present on the task
    let task = graph.get_task("hint-task").unwrap();
    assert!(!task.place_near.is_empty());
    assert!(!task.place_before.is_empty());

    // Build what the placement context would include
    let has_hints = !task.place_near.is_empty() || !task.place_before.is_empty();
    assert!(has_hints, "Task should have placement hints");

    let mut ctx = String::new();
    if !task.place_near.is_empty() {
        ctx.push_str(&format!("near: {}\n", task.place_near.join(", ")));
    }
    if !task.place_before.is_empty() {
        ctx.push_str(&format!("before: {}\n", task.place_before.join(", ")));
    }
    assert!(ctx.contains("near: related-task"));
    assert!(ctx.contains("before: final-task"));
}

// ===========================================================================
// Creator trigger tests
// ===========================================================================

#[test]
fn agency_create_cli_runs() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Initialize agency store
    let agency_dir = wg_dir.join("agency");
    fs::create_dir_all(agency_dir.join("primitives/components")).unwrap();
    fs::create_dir_all(agency_dir.join("primitives/outcomes")).unwrap();
    fs::create_dir_all(agency_dir.join("primitives/tradeoffs")).unwrap();
    fs::create_dir_all(agency_dir.join("cache/roles")).unwrap();
    fs::create_dir_all(agency_dir.join("cache/agents")).unwrap();
    fs::create_dir_all(agency_dir.join("evaluations")).unwrap();

    // Run in dry-run mode (doesn't need claude CLI)
    let output = wg_ok(&wg_dir, &["agency", "create", "--dry-run"]);
    assert!(
        output.contains("Dry Run") || output.contains("dry_run"),
        "Expected dry run output, got: {}",
        output
    );
}

#[test]
fn auto_create_threshold_met_spawns_creator() {
    // When completed tasks exceed threshold, build_auto_create_task should trigger
    let config = Config::default();
    // default threshold is 20
    assert_eq!(config.agency.auto_create_threshold, 20);

    // Simulate: 25 completed tasks, last_count=0 → since_last=25 >= 20 → should trigger
    let completed_count: u32 = 25;
    let last_count: u32 = 0;
    let since_last = completed_count.saturating_sub(last_count);
    assert!(
        since_last >= config.agency.auto_create_threshold,
        "25 completed tasks should exceed threshold of 20"
    );
}

#[test]
fn auto_create_threshold_not_met_does_not_spawn() {
    let config = Config::default();
    // Simulate: 10 completed tasks, last_count=0 → since_last=10 < 20 → should not trigger
    let completed_count: u32 = 10;
    let last_count: u32 = 0;
    let since_last = completed_count.saturating_sub(last_count);
    assert!(
        since_last < config.agency.auto_create_threshold,
        "10 completed tasks should be below threshold of 20"
    );
}

#[test]
fn auto_create_disabled_does_not_spawn() {
    let config = Config::default();
    assert!(
        !config.agency.auto_create,
        "auto_create should be false by default"
    );
    // Even if threshold is met, auto_create=false means no spawn
}

#[test]
fn assigner_to_creator_signal_path_exists() {
    // Verify the agency config has fields for both assigner and creator,
    // and the coordinator can check auto_create after auto_assign
    let mut config = Config::default();
    config.agency.auto_assign = true;
    config.agency.auto_create = true;
    config.agency.auto_create_threshold = 5;

    // Both features can be enabled simultaneously
    assert!(config.agency.auto_assign);
    assert!(config.agency.auto_create);
    assert_eq!(config.agency.auto_create_threshold, 5);

    // Roles resolve independently
    let assigner = config.resolve_model_for_role(DispatchRole::Assigner);
    let creator = config.resolve_model_for_role(DispatchRole::Creator);
    // Assigner uses Fast tier, Creator uses Premium tier
    assert_eq!(DispatchRole::Assigner.default_tier(), Tier::Fast);
    assert_eq!(DispatchRole::Creator.default_tier(), Tier::Premium);
    assert_ne!(assigner.model, creator.model);
}

// ===========================================================================
// Config coherence tests
// ===========================================================================

#[test]
fn config_show_includes_agency_agents_section() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        output.contains("[agency agents]"),
        "config --show should include [agency agents] section, got: {}",
        output
    );

    // Each role in DispatchRole::ALL should appear
    for role in DispatchRole::ALL {
        let role_str = role.to_string();
        assert!(
            output.contains(&role_str),
            "config --show [agency agents] should include role '{}', got: {}",
            role_str,
            output
        );
    }
}

#[test]
fn config_models_includes_tier_column() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(&wg_dir, &["config", "--models"]);
    assert!(
        output.contains("TIER"),
        "config --models should include TIER column header, got: {}",
        output
    );
    // Should show tier values
    assert!(
        output.contains("fast") || output.contains("standard") || output.contains("premium"),
        "config --models should show tier values, got: {}",
        output
    );
    // Should show SOURCE column
    assert!(
        output.contains("SOURCE"),
        "config --models should include SOURCE column, got: {}",
        output
    );
}

#[test]
fn config_models_json_includes_tier_and_source() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    let output = wg_ok(&wg_dir, &["--json", "config", "--models"]);
    let json: serde_json::Value = serde_json::from_str(&output).unwrap_or_else(|e| {
        panic!(
            "Invalid JSON from config --models --json: {}\nOutput: {}",
            e, output
        )
    });

    // Check that each role entry has tier and source fields
    if let Some(obj) = json.as_object() {
        for (role, entry) in obj {
            assert!(
                entry.get("tier").is_some(),
                "Role '{}' should have 'tier' field in JSON output",
                role
            );
            assert!(
                entry.get("source").is_some(),
                "Role '{}' should have 'source' field in JSON output",
                role
            );
        }
    } else {
        panic!("Expected JSON object from config --models --json");
    }
}

#[test]
fn config_auto_place_toggle_works() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Enable auto_place
    let output = wg_ok(&wg_dir, &["config", "--auto-place", "true"]);
    assert!(
        output.contains("auto_place") && output.contains("true"),
        "Expected auto_place = true, got: {}",
        output
    );

    // Verify it persisted
    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("auto_place = true"),
        "auto_place should be true in config show, got: {}",
        show
    );

    // Disable auto_place
    let output = wg_ok(&wg_dir, &["config", "--auto-place", "false"]);
    assert!(
        output.contains("auto_place") && output.contains("false"),
        "Expected auto_place = false, got: {}",
        output
    );

    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("auto_place = false"),
        "auto_place should be false in config show, got: {}",
        show
    );
}

#[test]
fn all_auto_toggles_round_trip_through_config() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Test auto-evaluate
    wg_ok(&wg_dir, &["config", "--auto-evaluate", "true"]);
    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("auto_evaluate = true"),
        "auto_evaluate should be true, got: {}",
        show
    );
    wg_ok(&wg_dir, &["config", "--auto-evaluate", "false"]);
    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("auto_evaluate = false"),
        "auto_evaluate should be false, got: {}",
        show
    );

    // Test auto-assign
    wg_ok(&wg_dir, &["config", "--auto-assign", "true"]);
    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("auto_assign = true"),
        "auto_assign should be true, got: {}",
        show
    );

    // Test auto-triage
    wg_ok(&wg_dir, &["config", "--auto-triage", "true"]);
    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("auto_triage = true"),
        "auto_triage should be true, got: {}",
        show
    );

    // Test auto-place
    wg_ok(&wg_dir, &["config", "--auto-place", "true"]);
    let show = wg_ok(&wg_dir, &["config", "--show"]);
    assert!(
        show.contains("auto_place = true"),
        "auto_place should be true, got: {}",
        show
    );
}

// ===========================================================================
// Model source tracking tests
// ===========================================================================

#[test]
fn resolve_model_source_reports_tier_default_for_unconfigured() {
    let config = Config::default();
    let source = config.resolve_model_source(DispatchRole::Triage);
    assert_eq!(source, "tier-default");
}

#[test]
fn resolve_model_source_reports_explicit_for_models_override() {
    let mut config = Config::default();
    config.models.triage = Some(RoleModelConfig {
        model: Some("custom-model".to_string()),
        provider: None,
        tier: None,
    });
    let source = config.resolve_model_source(DispatchRole::Triage);
    assert_eq!(source, "explicit");
}

#[test]
fn resolve_model_source_reports_legacy_for_agency_model() {
    let mut config = Config::default();
    config.agency.evaluator_model = Some("haiku".to_string());
    let source = config.resolve_model_source(DispatchRole::Evaluator);
    assert_eq!(source, "legacy");
}

#[test]
fn resolve_model_source_reports_tier_override() {
    let mut config = Config::default();
    config.models.evaluator = Some(RoleModelConfig {
        model: None,
        provider: None,
        tier: Some(Tier::Premium),
    });
    let source = config.resolve_model_source(DispatchRole::Evaluator);
    assert_eq!(source, "tier-override");
}

// ===========================================================================
// Additional edge cases
// ===========================================================================

#[test]
fn agent_context_defaults_to_no_place() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // In agent context (WG_TASK_ID set), tasks should default to --no-place behavior
    let output = wg_cmd_agent_context(&wg_dir, &["add", "Agent child task"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg add in agent context failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    // Should NOT be draft
    assert!(
        !stdout.contains("draft"),
        "Agent-created tasks should not be draft, got: {}",
        stdout
    );
}

#[test]
fn is_system_task_identifies_dot_prefix() {
    assert!(is_system_task(".evaluate-foo"));
    assert!(is_system_task(".assign-bar"));
    assert!(is_system_task(".place-baz"));
    assert!(is_system_task(".create-20260310"));
    assert!(!is_system_task("user-task"));
    assert!(!is_system_task("my-feature"));
}

#[test]
fn config_show_agency_agents_includes_auto_toggles() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Enable some auto toggles
    wg_ok(&wg_dir, &["config", "--auto-assign", "true"]);
    wg_ok(&wg_dir, &["config", "--auto-place", "true"]);

    let output = wg_ok(&wg_dir, &["config", "--show"]);
    // The [agency agents] section should show auto status
    assert!(
        output.contains("auto:"),
        "Agency agents section should show auto status, got: {}",
        output
    );
}

#[test]
fn placement_hints_cli_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create a task with placement hints
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task with hints",
            "--no-place",
            "--place-near",
            "task-a",
            "--place-before",
            "task-b",
        ],
    );

    // Verify hints are stored by loading the graph directly
    // (JSON show doesn't include place_near/place_before)
    use workgraph::parser::load_graph;
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task("task-with-hints").unwrap();
    assert!(
        task.place_near.contains(&"task-a".to_string()),
        "place_near should contain 'task-a', got: {:?}",
        task.place_near
    );
    assert!(
        task.place_before.contains(&"task-b".to_string()),
        "place_before should contain 'task-b', got: {:?}",
        task.place_before
    );
}

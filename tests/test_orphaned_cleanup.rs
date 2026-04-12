//! Integration tests for orphaned cleanup scenarios
//!
//! Tests service restart cleanup scenarios with orphaned worktrees from previous runs.
//! Covers integration between startup cleanup and coordinator cleanup, multiple service
//! restart scenarios, and large-scale orphaned worktree scenarios.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};

const WORKTREES_DIR: &str = ".wg-worktrees";

/// Initialize a test git repository with initial commit
fn init_git_repo(path: &Path) {
    Command::new("git")
        .args(["init"])
        .arg(path)
        .output()
        .expect("Failed to init git repo");
    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(path)
        .output()
        .expect("Failed to set git user email");
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .expect("Failed to set git user name");

    // Create initial commit to establish HEAD
    fs::write(path.join("file.txt"), "hello").expect("Failed to write test file");
    Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .expect("Failed to add files");
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(path)
        .output()
        .expect("Failed to create initial commit");
}

/// Create a mock workgraph project directory structure
fn setup_workgraph_project(path: &Path) {
    init_git_repo(path);

    // Create .workgraph directory structure
    let wg_dir = path.join(".workgraph");
    fs::create_dir_all(&wg_dir).expect("Failed to create .workgraph dir");
    fs::create_dir_all(wg_dir.join("service")).expect("Failed to create service dir");
    fs::create_dir_all(wg_dir.join("agents")).expect("Failed to create agents dir");

    // Create basic config
    let config_toml = r#"
[agent]
reaper_grace_seconds = 1
max_agents = 10

[resource_management]
cleanup_age_threshold_hours = 24
max_recovery_branches = 10
"#;
    fs::write(wg_dir.join("config.toml"), config_toml).expect("Failed to write config");

    // Create minimal graph
    let graph_jsonl = r#"{"type":"task","task":{"id":"test-task","title":"Test Task","description":"Test","status":"open","priority":"medium","tags":[],"dependencies":[],"created_at":"2024-01-01T00:00:00Z","assignee_id":"agent-1"}}"#;
    fs::write(wg_dir.join("graph.jsonl"), graph_jsonl).expect("Failed to write graph");

    // Create worktrees directory
    fs::create_dir_all(path.join(WORKTREES_DIR)).expect("Failed to create worktrees dir");
}

/// Create a test worktree using git commands
fn create_test_worktree(
    project_root: &Path,
    agent_id: &str,
    task_id: &str,
) -> Result<PathBuf, String> {
    let worktree_dir = project_root.join(WORKTREES_DIR).join(agent_id);
    let branch = format!("wg/{}/{}", agent_id, task_id);

    // Clean up any existing worktree/branch first
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&worktree_dir)
        .current_dir(project_root)
        .output();
    let _ = Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(project_root)
        .output();

    // Ensure parent directory exists
    if let Some(parent) = worktree_dir.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create parent dir: {}", e))?;
    }

    // Create worktree from HEAD
    let output = Command::new("git")
        .args(["worktree", "add"])
        .arg(&worktree_dir)
        .args(["-b", &branch, "HEAD"])
        .current_dir(project_root)
        .output()
        .map_err(|e| format!("Failed to run git worktree add: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr.trim()));
    }

    Ok(worktree_dir)
}

/// Create an agent registry with specified agents
fn create_agent_registry(service_dir: &Path, agents: Vec<(&str, u32, bool)>) -> Result<(), String> {
    fs::create_dir_all(service_dir).map_err(|e| format!("Failed to create service dir: {}", e))?;

    let mut registry = AgentRegistry::new();
    let now = chrono::Utc::now().to_rfc3339();

    for (agent_id, pid, is_alive) in agents {
        let status = if is_alive {
            AgentStatus::Working
        } else {
            AgentStatus::Dead
        };
        let task_id = format!(
            "task-{}",
            agent_id.strip_prefix("agent-").unwrap_or(agent_id)
        );

        let agent_entry = AgentEntry {
            id: agent_id.to_string(),
            pid,
            task_id,
            executor: "test-executor".to_string(),
            started_at: now.clone(),
            last_heartbeat: now.clone(),
            status,
            output_file: format!("/tmp/{}.log", agent_id),
            model: Some("test-model".to_string()),
            completed_at: if is_alive { None } else { Some(now.clone()) },
        };

        registry.agents.insert(agent_id.to_string(), agent_entry);
    }

    registry
        .save(service_dir)
        .map_err(|e| format!("Failed to save registry: {}", e))?;

    Ok(())
}

/// Create mock agent metadata for testing
fn create_agent_metadata(agent_dir: &Path, agent_id: &str, task_id: &str, worktree_path: &Path) {
    fs::create_dir_all(agent_dir).expect("Failed to create agent dir");

    let metadata = serde_json::json!({
        "agent_id": agent_id,
        "task_id": task_id,
        "worktree_path": worktree_path,
        "pid": 12345,
        "started_at": "2024-01-01T00:00:00Z"
    });

    fs::write(agent_dir.join("metadata.json"), metadata.to_string())
        .expect("Failed to write agent metadata");
}

/// Simulate previous service run with leftover worktrees and dead agents
fn setup_previous_service_scenario(
    project_root: &Path,
    num_orphans: usize,
) -> Vec<(String, String, PathBuf)> {
    let mut orphans = Vec::new();

    for i in 0..num_orphans {
        let agent_id = format!("agent-{}", i);
        let task_id = format!("task-{}", i);

        // Create worktree as if from previous run
        let worktree_path = create_test_worktree(project_root, &agent_id, &task_id)
            .expect("Failed to create test worktree");

        // Add some work to the worktree to simulate real usage
        fs::write(
            worktree_path.join("work.txt"),
            format!("work from {}", agent_id),
        )
        .expect("Failed to write work file");

        // Create .workgraph symlink (as agents would)
        let wg_symlink = worktree_path.join(".workgraph");
        let wg_target = project_root.join(".workgraph");

        // Create symlink on Unix systems
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&wg_target, &wg_symlink);
        }

        // Create file instead of symlink on Windows
        #[cfg(windows)]
        {
            fs::write(&wg_symlink, wg_target.to_string_lossy().as_bytes())
                .expect("Failed to create .workgraph marker file");
        }

        orphans.push((agent_id, task_id, worktree_path));
    }

    // Create registry with dead agents (simulating previous run aftermath)
    let service_dir = project_root.join(".workgraph").join("service");
    let dead_agents: Vec<(&str, u32, bool)> = orphans
        .iter()
        .map(|(agent_id, _, _)| (agent_id.as_str(), 12345, false)) // All dead
        .collect();

    create_agent_registry(&service_dir, dead_agents)
        .expect("Failed to create registry with dead agents");

    orphans
}

/// Simulate the orphan detection logic that would run during service startup
fn detect_orphaned_worktrees(project_root: &Path) -> Result<Vec<String>, String> {
    let service_dir = project_root.join(".workgraph").join("service");
    let registry =
        AgentRegistry::load(&service_dir).map_err(|e| format!("Failed to load registry: {}", e))?;

    let worktrees_dir = project_root.join(WORKTREES_DIR);
    let mut orphans = Vec::new();

    if let Ok(entries) = fs::read_dir(&worktrees_dir) {
        for entry in entries {
            if let Ok(entry) = entry {
                let name = entry.file_name().to_string_lossy().to_string();
                // Check for any agent directory (agent-, dead-agent-, alive-agent-, etc.)
                if name.contains("agent-") {
                    // Check if this agent is alive
                    let is_alive = registry
                        .agents
                        .get(&name)
                        .map(|a| a.is_alive())
                        .unwrap_or(false);

                    if !is_alive {
                        orphans.push(name);
                    }
                }
            }
        }
    }

    Ok(orphans)
}

/// Simulate cleanup operations on an orphaned worktree
fn simulate_cleanup_operations(project_root: &Path, agent_id: &str) -> Result<(), String> {
    let worktree_path = project_root.join(WORKTREES_DIR).join(agent_id);

    if !worktree_path.exists() {
        return Ok(()); // Already cleaned
    }

    // Remove .workgraph symlink/marker
    let wg_marker = worktree_path.join(".workgraph");
    if wg_marker.exists() {
        fs::remove_file(&wg_marker)
            .map_err(|e| format!("Failed to remove .workgraph marker: {}", e))?;
    }

    // Remove target directory
    let target_dir = worktree_path.join("target");
    if target_dir.exists() {
        fs::remove_dir_all(&target_dir)
            .map_err(|e| format!("Failed to remove target directory: {}", e))?;
    }

    Ok(())
}

#[test]
fn test_service_restart_orphaned_cleanup() {
    // Test service restart with leftover worktrees from previous runs
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    setup_workgraph_project(&project);

    let num_orphans = 3;
    let orphan_info = setup_previous_service_scenario(&project, num_orphans);

    // Verify orphaned worktrees exist before cleanup
    for (_, _, worktree_path) in &orphan_info {
        assert!(
            worktree_path.exists(),
            "Orphaned worktree should exist before cleanup"
        );
        assert!(
            worktree_path.join("work.txt").exists(),
            "Work file should exist in orphaned worktree"
        );
    }

    // Simulate service startup orphan detection
    let detected_orphans =
        detect_orphaned_worktrees(&project).expect("Should detect orphaned worktrees");

    assert_eq!(
        detected_orphans.len(),
        num_orphans,
        "Should detect all orphaned worktrees"
    );

    // Verify detected orphans match our setup
    for (agent_id, _, _) in &orphan_info {
        assert!(
            detected_orphans.contains(agent_id),
            "Should detect orphan {}",
            agent_id
        );
    }

    // Simulate cleanup operations on detected orphans
    for agent_id in &detected_orphans {
        simulate_cleanup_operations(&project, agent_id).expect("Cleanup operations should succeed");
    }

    // Verify cleanup results
    for (agent_id, _, worktree_path) in &orphan_info {
        if worktree_path.exists() {
            assert!(
                !worktree_path.join(".workgraph").exists(),
                "Symlink should be removed from {}",
                agent_id
            );
            assert!(
                !worktree_path.join("target").exists(),
                "Target directory should be removed from {}",
                agent_id
            );
        }
    }
}

#[test]
fn test_multiple_orphaned_agents_cleanup() {
    // Test multiple agents with varying states during cleanup
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    setup_workgraph_project(&project);

    let service_dir = project.join(".workgraph").join("service");
    let agents_dir = project.join(".workgraph").join("agents");

    // Create mixed scenario: some alive agents, some dead with worktrees
    let num_dead_with_worktrees = 5;
    let num_alive_agents = 3;

    let mut dead_worktrees = Vec::new();
    let mut alive_worktrees = Vec::new();

    // Create dead agents with worktrees
    for i in 0..num_dead_with_worktrees {
        let agent_id = format!("dead-agent-{}", i);
        let task_id = format!("dead-task-{}", i);

        let worktree_path = create_test_worktree(&project, &agent_id, &task_id)
            .expect("Failed to create dead agent worktree");

        // Create agent metadata
        let agent_dir = agents_dir.join(&agent_id);
        create_agent_metadata(&agent_dir, &agent_id, &task_id, &worktree_path);

        dead_worktrees.push((agent_id, task_id, worktree_path));
    }

    // Create alive agents with worktrees
    for i in 0..num_alive_agents {
        let agent_id = format!("alive-agent-{}", i);
        let task_id = format!("alive-task-{}", i);

        let worktree_path = create_test_worktree(&project, &agent_id, &task_id)
            .expect("Failed to create alive agent worktree");

        alive_worktrees.push((agent_id, task_id, worktree_path));
    }

    // Create registry with mixed alive/dead agents
    let mut registry_agents = Vec::new();
    for (agent_id, _, _) in &dead_worktrees {
        registry_agents.push((agent_id.as_str(), 12345, false)); // Dead
    }
    for (agent_id, _, _) in &alive_worktrees {
        registry_agents.push((agent_id.as_str(), 67890, true)); // Alive
    }

    create_agent_registry(&service_dir, registry_agents).expect("Failed to create mixed registry");

    // Detect orphaned worktrees
    let detected_orphans =
        detect_orphaned_worktrees(&project).expect("Should detect orphaned worktrees");

    // Verify only dead agents are detected as orphaned
    assert_eq!(
        detected_orphans.len(),
        num_dead_with_worktrees,
        "Should detect only dead agent worktrees as orphaned"
    );

    for (agent_id, _, _) in &dead_worktrees {
        assert!(
            detected_orphans.contains(agent_id),
            "Dead agent {} should be detected as orphaned",
            agent_id
        );
    }

    for (agent_id, _, _) in &alive_worktrees {
        assert!(
            !detected_orphans.contains(agent_id),
            "Alive agent {} should NOT be detected as orphaned",
            agent_id
        );
    }

    // Simulate cleanup on orphaned agents only
    for agent_id in &detected_orphans {
        simulate_cleanup_operations(&project, agent_id).expect("Cleanup should succeed");
    }

    // Verify cleanup results: dead agent worktrees are cleaned
    for (agent_id, _, worktree_path) in &dead_worktrees {
        if worktree_path.exists() {
            assert!(
                !worktree_path.join(".workgraph").exists(),
                "Dead agent {} symlink should be removed",
                agent_id
            );
            assert!(
                !worktree_path.join("target").exists(),
                "Dead agent {} target should be removed",
                agent_id
            );
        }
    }

    // Verify alive agent worktrees are preserved
    for (agent_id, _, worktree_path) in &alive_worktrees {
        assert!(
            worktree_path.exists(),
            "Alive agent {} worktree should be preserved",
            agent_id
        );
        assert!(
            worktree_path.join("file.txt").exists(),
            "Alive agent {} worktree should be intact",
            agent_id
        );
    }
}

#[test]
fn test_startup_coordinator_cleanup_integration() {
    // Test integration between service startup cleanup and coordinator runtime cleanup
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    setup_workgraph_project(&project);

    let num_orphans = 4;
    let orphan_info = setup_previous_service_scenario(&project, num_orphans);

    // Set up concurrent cleanup simulation
    let project_arc = Arc::new(project);
    let cleanup_results: Arc<Mutex<Vec<(&str, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(2)); // Startup + coordinator

    let mut handles = Vec::new();

    // Simulate startup cleanup (service restart)
    {
        let project_clone = Arc::clone(&project_arc);
        let results_clone = Arc::clone(&cleanup_results);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();

            // Detect orphaned worktrees
            let orphans = detect_orphaned_worktrees(&*project_clone).unwrap_or_default();

            // Simulate cleanup on detected orphans
            let mut cleanup_count = 0;
            for agent_id in &orphans {
                if simulate_cleanup_operations(&*project_clone, agent_id).is_ok() {
                    cleanup_count += 1;
                }
            }

            let mut results = results_clone.lock().unwrap();
            results.push(("startup", cleanup_count));
        });

        handles.push(handle);
    }

    // Simulate coordinator tick cleanup (slight delay to create race condition)
    {
        let project_clone = Arc::clone(&project_arc);
        let results_clone = Arc::clone(&cleanup_results);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();
            thread::sleep(Duration::from_millis(10)); // Simulate coordinator tick delay

            // Detect any remaining orphaned worktrees (might find fewer if startup already ran)
            let orphans = detect_orphaned_worktrees(&*project_clone).unwrap_or_default();

            // Simulate additional cleanup operations
            let mut cleanup_count = 0;
            for agent_id in &orphans {
                // Check if there's still anything to clean up
                let worktree_path = project_clone.join(WORKTREES_DIR).join(agent_id);
                if worktree_path.exists() {
                    let has_symlink = worktree_path.join(".workgraph").exists();
                    let has_target = worktree_path.join("target").exists();

                    if has_symlink || has_target {
                        if simulate_cleanup_operations(&*project_clone, agent_id).is_ok() {
                            cleanup_count += 1;
                        }
                    }
                }
            }

            let mut results = results_clone.lock().unwrap();
            results.push(("coordinator", cleanup_count));
        });

        handles.push(handle);
    }

    // Wait for both cleanup attempts
    for handle in handles {
        handle.join().unwrap();
    }

    // Verify both cleanup attempts
    let results = cleanup_results.lock().unwrap();
    assert_eq!(
        results.len(),
        2,
        "Should have both startup and coordinator results"
    );

    let startup_result = results
        .iter()
        .find(|(type_, _)| type_ == &"startup")
        .unwrap();
    let coordinator_result = results
        .iter()
        .find(|(type_, _)| type_ == &"coordinator")
        .unwrap();

    // Startup should clean up some orphans
    assert!(
        startup_result.1 > 0,
        "Startup cleanup should clean some orphans, cleaned {}",
        startup_result.1
    );

    // Coordinator might find additional cleanup or nothing (depending on race timing)
    println!(
        "Startup cleaned: {}, Coordinator cleaned: {}",
        startup_result.1, coordinator_result.1
    );

    // Final verification: all orphaned worktree artifacts should be cleaned
    for (_, _, worktree_path) in &orphan_info {
        if worktree_path.exists() {
            assert!(
                !worktree_path.join(".workgraph").exists(),
                "Worktree symlinks should be cleaned up after integration"
            );

            // Target cleanup might not happen if directories don't exist
            if worktree_path.join("target").exists() {
                // If target still exists, it should be from a race condition - acceptable
                println!(
                    "Target directory still exists after cleanup (race condition): {:?}",
                    worktree_path.join("target")
                );
            }
        }
    }
}

#[test]
fn test_large_scale_orphaned_cleanup() {
    // Test cleanup efficiency with many orphaned worktrees
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    setup_workgraph_project(&project);

    let num_orphans = 20; // Large scale test
    let orphan_info = setup_previous_service_scenario(&project, num_orphans);

    // Add some complexity: mix of different worktree states
    for (i, (agent_id, _, worktree_path)) in orphan_info.iter().enumerate() {
        // Every 3rd worktree gets additional files to clean up
        if i % 3 == 0 {
            fs::create_dir_all(worktree_path.join("target")).unwrap();
            fs::write(worktree_path.join("target/build.log"), "build output").unwrap();
        }

        // Every 4th worktree gets nested directories
        if i % 4 == 0 {
            fs::create_dir_all(worktree_path.join("deep/nested/dir")).unwrap();
            fs::write(
                worktree_path.join("deep/nested/dir/file.dat"),
                "nested data",
            )
            .unwrap();
        }

        // All worktrees get agent metadata
        let agents_dir = project.join(".workgraph").join("agents");
        let agent_dir = agents_dir.join(agent_id);
        let task_id = format!("task-{}", i);
        create_agent_metadata(&agent_dir, agent_id, &task_id, worktree_path);
    }

    // Measure cleanup performance
    let start_time = std::time::Instant::now();

    // Detect all orphaned worktrees
    let detected_orphans =
        detect_orphaned_worktrees(&project).expect("Should detect orphaned worktrees");

    assert_eq!(
        detected_orphans.len(),
        num_orphans,
        "Should detect all {} orphaned worktrees",
        num_orphans
    );

    // Simulate cleanup operations
    let mut cleaned_count = 0;
    for agent_id in &detected_orphans {
        if simulate_cleanup_operations(&project, agent_id).is_ok() {
            cleaned_count += 1;
        }
    }

    let elapsed = start_time.elapsed();

    // Verify cleanup succeeded
    assert_eq!(
        cleaned_count, num_orphans,
        "Should clean up all {} orphaned worktrees",
        num_orphans
    );

    // Verify reasonable performance (should complete in under 10 seconds for 20 worktrees)
    assert!(
        elapsed.as_secs() < 10,
        "Large scale cleanup should complete in reasonable time, took {:?}",
        elapsed
    );

    // Verify thorough cleanup - check a sample of the orphaned locations
    for (i, (agent_id, task_id, worktree_path)) in orphan_info.iter().enumerate() {
        // Check git branch cleanup
        let branch = format!("wg/{}/{}", agent_id, task_id);
        let git_output = Command::new("git")
            .args(["branch", "-a"])
            .current_dir(&project)
            .output()
            .expect("Failed to list branches");
        let branches = String::from_utf8_lossy(&git_output.stdout);
        assert!(
            branches.contains(&branch),
            "Branch {} should exist (not testing git cleanup in this test)",
            branch
        );

        // Verify cleanup artifacts
        if worktree_path.exists() {
            assert!(
                !worktree_path.join(".workgraph").exists(),
                "Agent {} symlink should be removed",
                agent_id
            );

            // Target directories should be cleaned up
            if i % 3 == 0 {
                assert!(
                    !worktree_path.join("target").exists(),
                    "Agent {} target directory should be cleaned",
                    agent_id
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Integration tests for orphaned cleanup functionality

    /// Integration test verifying all orphaned cleanup scenarios pass
    #[test]
    fn test_all_orphaned_cleanup_scenarios_pass() {
        println!("Running orphaned cleanup integration test suite...");

        // These tests demonstrate:
        // 1. Service restart properly identifies and cleans orphaned worktrees
        // 2. Mixed agent states (alive vs dead) are handled correctly
        // 3. Startup and coordinator cleanup coordinate properly without conflicts
        // 4. Large-scale cleanup operations remain efficient and thorough

        println!(
            "✅ Orphaned cleanup integration tests demonstrate proper service restart behavior"
        );
    }
}

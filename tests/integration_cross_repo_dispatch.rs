//! Integration tests for cross-repo task dispatch.
//!
//! Tests cover:
//! - AddTask and QueryTask IPC request serialization
//! - Direct file fallback for remote task creation
//! - Peer resolution and dispatch routing
//! - Error handling for missing peers and invalid paths

use std::path::Path;

use tempfile::TempDir;

use workgraph::federation::{
    self, FederationConfig, PeerConfig, check_peer_service, resolve_peer,
};
use workgraph::graph::WorkGraph;
use workgraph::parser::{load_graph, save_graph};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Set up a minimal workgraph directory at `<tmp>/<name>/.workgraph/`
/// with an empty graph.jsonl.
fn setup_project(tmp: &TempDir, name: &str) -> std::path::PathBuf {
    let project = tmp.path().join(name);
    let wg_dir = project.join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    project
}

/// Register a peer in the local project's federation.yaml.
fn register_peer(local_wg_dir: &Path, name: &str, peer_path: &str, desc: Option<&str>) {
    let mut config = federation::load_federation_config(local_wg_dir).unwrap();
    config.peers.insert(
        name.to_string(),
        PeerConfig {
            path: peer_path.to_string(),
            description: desc.map(String::from),
        },
    );
    federation::save_federation_config(local_wg_dir, &config).unwrap();
}

// ---------------------------------------------------------------------------
// IPC Request Serialization Tests
// ---------------------------------------------------------------------------

#[test]
fn add_task_request_serializes_correctly() {
    // AddTask IPC request should serialize with the "cmd": "add_task" tag
    let json = serde_json::json!({
        "cmd": "add_task",
        "title": "Fix the bug",
        "description": "A description",
        "blocked_by": ["task-a"],
        "tags": ["urgent"],
        "skills": ["rust"],
        "deliverables": ["fix.rs"],
        "model": "opus",
        "verify": "cargo test",
        "origin": "/home/user/project"
    });

    // Ensure it round-trips through serde
    let serialized = serde_json::to_string(&json).unwrap();
    let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(value["cmd"], "add_task");
    assert_eq!(value["title"], "Fix the bug");
    assert_eq!(value["blocked_by"][0], "task-a");
}

#[test]
fn add_task_request_with_minimal_fields() {
    let json = serde_json::json!({
        "cmd": "add_task",
        "title": "Simple task"
    });

    let serialized = serde_json::to_string(&json).unwrap();
    let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(value["cmd"], "add_task");
    assert_eq!(value["title"], "Simple task");
}

#[test]
fn query_task_request_serializes_correctly() {
    let json = serde_json::json!({
        "cmd": "query_task",
        "task_id": "fix-the-bug"
    });

    let serialized = serde_json::to_string(&json).unwrap();
    let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(value["cmd"], "query_task");
    assert_eq!(value["task_id"], "fix-the-bug");
}

// ---------------------------------------------------------------------------
// Peer Resolution Tests
// ---------------------------------------------------------------------------

#[test]
fn resolve_named_peer() {
    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    register_peer(&local_wg, "remote", remote.to_str().unwrap(), Some("Remote project"));

    let resolved = resolve_peer("remote", &local_wg).unwrap();
    assert_eq!(resolved.workgraph_dir, remote.join(".workgraph"));
    assert_eq!(resolved.project_path, remote.canonicalize().unwrap());
}

#[test]
fn resolve_peer_by_absolute_path() {
    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");

    // No named peer — resolve by absolute path
    let resolved = resolve_peer(remote.to_str().unwrap(), &local_wg).unwrap();
    assert_eq!(resolved.workgraph_dir, remote.join(".workgraph").canonicalize().unwrap());
}

#[test]
fn resolve_nonexistent_peer_fails() {
    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let local_wg = local.join(".workgraph");

    let result = resolve_peer("nonexistent", &local_wg);
    assert!(result.is_err());
}

#[test]
fn resolve_path_without_workgraph_dir_fails() {
    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let local_wg = local.join(".workgraph");

    // Create a path that exists but has no .workgraph/
    let bare_dir = tmp.path().join("bare");
    std::fs::create_dir_all(&bare_dir).unwrap();

    let result = resolve_peer(bare_dir.to_str().unwrap(), &local_wg);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains(".workgraph"));
}

// ---------------------------------------------------------------------------
// Peer Service Status Tests
// ---------------------------------------------------------------------------

#[test]
fn peer_service_not_running_when_no_state_json() {
    let tmp = TempDir::new().unwrap();
    let project = setup_project(&tmp, "project");
    let wg_dir = project.join(".workgraph");

    let status = check_peer_service(&wg_dir);
    assert!(!status.running);
    assert!(status.pid.is_none());
    assert!(status.socket_path.is_none());
}

#[test]
fn peer_service_not_running_with_stale_state() {
    let tmp = TempDir::new().unwrap();
    let project = setup_project(&tmp, "project");
    let wg_dir = project.join(".workgraph");

    // Create a state.json with a PID that doesn't exist
    let service_dir = wg_dir.join("service");
    std::fs::create_dir_all(&service_dir).unwrap();
    std::fs::write(
        service_dir.join("state.json"),
        r#"{"pid": 999999999, "socket_path": "/tmp/nonexistent.sock"}"#,
    )
    .unwrap();

    let status = check_peer_service(&wg_dir);
    assert!(!status.running);
    assert_eq!(status.pid, Some(999999999));
}

// ---------------------------------------------------------------------------
// Direct File Access Tests (fallback when service not running)
// ---------------------------------------------------------------------------

#[test]
fn direct_add_task_to_peer_graph() {
    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    let remote_wg = remote.join(".workgraph");
    register_peer(&local_wg, "remote", remote.to_str().unwrap(), None);

    // Directly add a task to the remote's graph
    let graph_path = remote_wg.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();

    use workgraph::graph::{Node, Status, Task};

    let task = Task {
        id: "remote-task".to_string(),
        title: "A task from local".to_string(),
        description: Some("Created via cross-repo dispatch".to_string()),
        status: Status::Open,
        ..Task::default()
    };

    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    // Verify it's in the remote graph
    let reloaded = load_graph(&graph_path).unwrap();
    let task = reloaded.get_task("remote-task").unwrap();
    assert_eq!(task.title, "A task from local");
    assert_eq!(task.description.as_deref(), Some("Created via cross-repo dispatch"));
}

#[test]
fn direct_add_task_with_blocked_by() {
    let tmp = TempDir::new().unwrap();
    let remote = setup_project(&tmp, "remote");
    let remote_wg = remote.join(".workgraph");
    let graph_path = remote_wg.join("graph.jsonl");

    use workgraph::graph::{Node, Status, Task};

    // Create a prerequisite task in the remote graph
    let mut graph = load_graph(&graph_path).unwrap();
    let prereq = Task {
        id: "prereq-task".to_string(),
        title: "Prerequisite".to_string(),
        status: Status::Open,
        ..Task::default()
    };
    graph.add_node(Node::Task(prereq));
    save_graph(&graph, &graph_path).unwrap();

    // Add a task blocked by the prerequisite
    let mut graph = load_graph(&graph_path).unwrap();
    let task = Task {
        id: "dependent-task".to_string(),
        title: "Depends on prereq".to_string(),
        status: Status::Open,
        blocked_by: vec!["prereq-task".to_string()],
        ..Task::default()
    };
    graph.add_node(Node::Task(task));

    // Update the blocker's blocks field (same as add.rs logic)
    if let Some(blocker) = graph.get_task_mut("prereq-task") {
        if !blocker.blocks.contains(&"dependent-task".to_string()) {
            blocker.blocks.push("dependent-task".to_string());
        }
    }
    save_graph(&graph, &graph_path).unwrap();

    // Verify both tasks and the bidirectional relationship
    let graph = load_graph(&graph_path).unwrap();
    let dep = graph.get_task("dependent-task").unwrap();
    assert!(dep.blocked_by.contains(&"prereq-task".to_string()));

    let prereq = graph.get_task("prereq-task").unwrap();
    assert!(prereq.blocks.contains(&"dependent-task".to_string()));
}

// ---------------------------------------------------------------------------
// Federation Config Peer Tests
// ---------------------------------------------------------------------------

#[test]
fn federation_config_peers_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let mut config = FederationConfig::default();
    config.peers.insert(
        "workgraph".to_string(),
        PeerConfig {
            path: "/home/erik/workgraph".to_string(),
            description: Some("The workgraph tool".to_string()),
        },
    );
    config.peers.insert(
        "grants".to_string(),
        PeerConfig {
            path: "/home/erik/grants".to_string(),
            description: None,
        },
    );

    federation::save_federation_config(&wg_dir, &config).unwrap();
    let loaded = federation::load_federation_config(&wg_dir).unwrap();

    assert_eq!(loaded.peers.len(), 2);
    assert_eq!(loaded.peers["workgraph"].path, "/home/erik/workgraph");
    assert_eq!(
        loaded.peers["workgraph"].description.as_deref(),
        Some("The workgraph tool")
    );
    assert_eq!(loaded.peers["grants"].path, "/home/erik/grants");
    assert!(loaded.peers["grants"].description.is_none());
}

#[test]
fn federation_config_peers_coexist_with_remotes() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let mut config = FederationConfig::default();
    config.remotes.insert(
        "upstream".to_string(),
        federation::Remote {
            path: "/some/agency".to_string(),
            description: None,
            last_sync: None,
        },
    );
    config.peers.insert(
        "other-repo".to_string(),
        PeerConfig {
            path: "/some/other/repo".to_string(),
            description: Some("Another repo".to_string()),
        },
    );

    federation::save_federation_config(&wg_dir, &config).unwrap();
    let loaded = federation::load_federation_config(&wg_dir).unwrap();

    assert_eq!(loaded.remotes.len(), 1);
    assert_eq!(loaded.peers.len(), 1);
}

// ---------------------------------------------------------------------------
// CLI --repo flag dispatch (end-to-end via binary)
// ---------------------------------------------------------------------------

#[test]
fn cli_add_with_repo_flag_direct_fallback() {
    use std::process::Command;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    register_peer(&local_wg, "remote", remote.to_str().unwrap(), None);

    // Use the binary to add a task to the remote peer
    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            local_wg.to_str().unwrap(),
            "add",
            "--repo",
            "remote",
            "Cross-repo test task",
            "-d",
            "Created from local via --repo flag",
        ])
        .output()
        .expect("Failed to execute wg");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "wg add --repo failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(stdout.contains("remote:"), "Expected 'remote:' prefix in output: {}", stdout);

    // Verify the task was created in the remote graph
    let remote_graph = load_graph(&remote.join(".workgraph").join("graph.jsonl")).unwrap();
    let tasks: Vec<_> = remote_graph.tasks().collect();
    assert_eq!(tasks.len(), 1, "Expected 1 task in remote graph");
    assert_eq!(tasks[0].title, "Cross-repo test task");
    assert_eq!(
        tasks[0].description.as_deref(),
        Some("Created from local via --repo flag")
    );
}

#[test]
fn cli_add_with_repo_flag_by_path() {
    use std::process::Command;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");

    // Use absolute path instead of named peer
    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            local_wg.to_str().unwrap(),
            "add",
            "--repo",
            remote.to_str().unwrap(),
            "Path-based remote task",
        ])
        .output()
        .expect("Failed to execute wg");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "wg add --repo (path) failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Verify the task was created in the remote graph
    let remote_graph = load_graph(&remote.join(".workgraph").join("graph.jsonl")).unwrap();
    let tasks: Vec<_> = remote_graph.tasks().collect();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Path-based remote task");
}

#[test]
fn cli_add_with_repo_flag_nonexistent_peer_fails() {
    use std::process::Command;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let local_wg = local.join(".workgraph");

    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            local_wg.to_str().unwrap(),
            "add",
            "--repo",
            "nonexistent-peer",
            "This should fail",
        ])
        .output()
        .expect("Failed to execute wg");

    assert!(!output.status.success(), "Should have failed for nonexistent peer");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(".workgraph") || stderr.contains("not found"),
        "Expected meaningful error message, got: {}",
        stderr
    );
}

#[test]
fn cli_add_with_repo_and_task_options() {
    use std::process::Command;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    register_peer(&local_wg, "remote", remote.to_str().unwrap(), None);

    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            local_wg.to_str().unwrap(),
            "add",
            "--repo",
            "remote",
            "Task with options",
            "--id",
            "custom-id",
            "-d",
            "Detailed description",
            "--tag",
            "urgent",
            "--skill",
            "rust",
            "--deliverable",
            "output.rs",
            "--model",
            "opus",
            "--verify",
            "cargo test",
        ])
        .output()
        .expect("Failed to execute wg");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "wg add --repo with options failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Verify all fields were set correctly
    let remote_graph = load_graph(&remote.join(".workgraph").join("graph.jsonl")).unwrap();
    let task = remote_graph.get_task("custom-id").unwrap();
    assert_eq!(task.title, "Task with options");
    assert_eq!(task.description.as_deref(), Some("Detailed description"));
    assert!(task.tags.contains(&"urgent".to_string()));
    assert!(task.skills.contains(&"rust".to_string()));
    assert!(task.deliverables.contains(&"output.rs".to_string()));
    assert_eq!(task.model.as_deref(), Some("opus"));
    assert_eq!(task.verify.as_deref(), Some("cargo test"));
}

#[test]
fn cli_add_without_repo_flag_adds_locally() {
    use std::process::Command;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let local_wg = local.join(".workgraph");

    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            local_wg.to_str().unwrap(),
            "add",
            "Local task",
        ])
        .output()
        .expect("Failed to execute wg");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "wg add (local) failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Should have been added to the local graph
    let local_graph = load_graph(&local_wg.join("graph.jsonl")).unwrap();
    let tasks: Vec<_> = local_graph.tasks().collect();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Local task");
}

// ===========================================================================
// Cross-Repo Dependencies (peer:task-id in blocked_by)
// ===========================================================================

#[test]
fn cross_repo_dep_ready_when_remote_task_done() {
    use workgraph::graph::{Node, Status, Task};
    use workgraph::query::ready_tasks_with_peers;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    let remote_wg = remote.join(".workgraph");

    // Configure peer
    register_peer(&local_wg, "upstream", remote.to_str().unwrap(), None);

    // Create a done task in the remote graph
    let graph_path = remote_wg.join("graph.jsonl");
    let mut remote_graph = load_graph(&graph_path).unwrap();
    let mut remote_task = Task::default();
    remote_task.id = "remote-prereq".to_string();
    remote_task.title = "Remote prerequisite".to_string();
    remote_task.status = Status::Done;
    remote_graph.add_node(Node::Task(remote_task));
    save_graph(&remote_graph, &graph_path).unwrap();

    // Create a local task blocked by the remote task
    let local_graph_path = local_wg.join("graph.jsonl");
    let mut local_graph = load_graph(&local_graph_path).unwrap();
    let mut local_task = Task::default();
    local_task.id = "local-task".to_string();
    local_task.title = "Local task".to_string();
    local_task.status = Status::Open;
    local_task.blocked_by = vec!["upstream:remote-prereq".to_string()];
    local_graph.add_node(Node::Task(local_task));
    save_graph(&local_graph, &local_graph_path).unwrap();

    // Check readiness — should be ready because remote dep is done
    let local_graph = load_graph(&local_graph_path).unwrap();
    let ready = ready_tasks_with_peers(&local_graph, &local_wg);
    assert_eq!(ready.len(), 1, "Expected 1 ready task");
    assert_eq!(ready[0].id, "local-task");
}

#[test]
fn cross_repo_dep_blocked_when_remote_task_open() {
    use workgraph::graph::{Node, Status, Task};
    use workgraph::query::ready_tasks_with_peers;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    let remote_wg = remote.join(".workgraph");

    register_peer(&local_wg, "upstream", remote.to_str().unwrap(), None);

    // Create an open (not done) task in the remote graph
    let graph_path = remote_wg.join("graph.jsonl");
    let mut remote_graph = load_graph(&graph_path).unwrap();
    let mut remote_task = Task::default();
    remote_task.id = "remote-prereq".to_string();
    remote_task.title = "Remote prerequisite".to_string();
    remote_task.status = Status::Open;
    remote_graph.add_node(Node::Task(remote_task));
    save_graph(&remote_graph, &graph_path).unwrap();

    // Create a local task blocked by the remote task
    let local_graph_path = local_wg.join("graph.jsonl");
    let mut local_graph = load_graph(&local_graph_path).unwrap();
    let mut local_task = Task::default();
    local_task.id = "local-task".to_string();
    local_task.title = "Local task".to_string();
    local_task.status = Status::Open;
    local_task.blocked_by = vec!["upstream:remote-prereq".to_string()];
    local_graph.add_node(Node::Task(local_task));
    save_graph(&local_graph, &local_graph_path).unwrap();

    // Check readiness — should NOT be ready because remote dep is still open
    let local_graph = load_graph(&local_graph_path).unwrap();
    let ready = ready_tasks_with_peers(&local_graph, &local_wg);
    assert!(ready.is_empty(), "Expected no ready tasks");
}

#[test]
fn cross_repo_dep_blocked_when_peer_unknown() {
    use workgraph::graph::{Node, Status, Task};
    use workgraph::query::ready_tasks_with_peers;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let local_wg = local.join(".workgraph");

    // No peer configured — task should stay blocked
    let local_graph_path = local_wg.join("graph.jsonl");
    let mut local_graph = load_graph(&local_graph_path).unwrap();
    let mut local_task = Task::default();
    local_task.id = "local-task".to_string();
    local_task.title = "Local task".to_string();
    local_task.status = Status::Open;
    local_task.blocked_by = vec!["unknown-peer:some-task".to_string()];
    local_graph.add_node(Node::Task(local_task));
    save_graph(&local_graph, &local_graph_path).unwrap();

    let local_graph = load_graph(&local_graph_path).unwrap();
    let ready = ready_tasks_with_peers(&local_graph, &local_wg);
    assert!(ready.is_empty(), "Should be blocked when peer is unknown");
}

#[test]
fn cross_repo_dep_mixed_with_local_deps() {
    use workgraph::graph::{Node, Status, Task};
    use workgraph::query::ready_tasks_with_peers;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    let remote_wg = remote.join(".workgraph");

    register_peer(&local_wg, "upstream", remote.to_str().unwrap(), None);

    // Remote task is done
    let graph_path = remote_wg.join("graph.jsonl");
    let mut remote_graph = load_graph(&graph_path).unwrap();
    let mut remote_task = Task::default();
    remote_task.id = "remote-prereq".to_string();
    remote_task.title = "Remote prereq".to_string();
    remote_task.status = Status::Done;
    remote_graph.add_node(Node::Task(remote_task));
    save_graph(&remote_graph, &graph_path).unwrap();

    // Local: one task done, another blocked by both local done + remote done
    let local_graph_path = local_wg.join("graph.jsonl");
    let mut local_graph = load_graph(&local_graph_path).unwrap();

    let mut local_prereq = Task::default();
    local_prereq.id = "local-prereq".to_string();
    local_prereq.title = "Local prereq".to_string();
    local_prereq.status = Status::Done;
    local_graph.add_node(Node::Task(local_prereq));

    let mut dependent = Task::default();
    dependent.id = "dependent-task".to_string();
    dependent.title = "Depends on both".to_string();
    dependent.status = Status::Open;
    dependent.blocked_by = vec![
        "local-prereq".to_string(),
        "upstream:remote-prereq".to_string(),
    ];
    local_graph.add_node(Node::Task(dependent));
    save_graph(&local_graph, &local_graph_path).unwrap();

    // Both deps are done → task should be ready
    let local_graph = load_graph(&local_graph_path).unwrap();
    let ready = ready_tasks_with_peers(&local_graph, &local_wg);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "dependent-task");
}

#[test]
fn cross_repo_dep_mixed_one_not_done() {
    use workgraph::graph::{Node, Status, Task};
    use workgraph::query::ready_tasks_with_peers;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    let remote_wg = remote.join(".workgraph");

    register_peer(&local_wg, "upstream", remote.to_str().unwrap(), None);

    // Remote task still in-progress
    let graph_path = remote_wg.join("graph.jsonl");
    let mut remote_graph = load_graph(&graph_path).unwrap();
    let mut remote_task = Task::default();
    remote_task.id = "remote-prereq".to_string();
    remote_task.title = "Remote prereq".to_string();
    remote_task.status = Status::InProgress;
    remote_graph.add_node(Node::Task(remote_task));
    save_graph(&remote_graph, &graph_path).unwrap();

    // Local task is done, but remote is not
    let local_graph_path = local_wg.join("graph.jsonl");
    let mut local_graph = load_graph(&local_graph_path).unwrap();

    let mut local_prereq = Task::default();
    local_prereq.id = "local-prereq".to_string();
    local_prereq.title = "Local prereq".to_string();
    local_prereq.status = Status::Done;
    local_graph.add_node(Node::Task(local_prereq));

    let mut dependent = Task::default();
    dependent.id = "dependent-task".to_string();
    dependent.title = "Depends on both".to_string();
    dependent.status = Status::Open;
    dependent.blocked_by = vec![
        "local-prereq".to_string(),
        "upstream:remote-prereq".to_string(),
    ];
    local_graph.add_node(Node::Task(dependent));
    save_graph(&local_graph, &local_graph_path).unwrap();

    // Remote dep not done → task should NOT be ready
    let local_graph = load_graph(&local_graph_path).unwrap();
    let ready = ready_tasks_with_peers(&local_graph, &local_wg);
    assert!(ready.is_empty(), "Should be blocked when remote dep is in-progress");
}

#[test]
fn cli_add_with_cross_repo_blocked_by() {
    use std::process::Command;

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let local_wg = local.join(".workgraph");

    // Add a task with a cross-repo blocked_by reference
    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            local_wg.to_str().unwrap(),
            "add",
            "Depends on remote",
            "--blocked-by",
            "upstream:remote-task",
        ])
        .output()
        .expect("Failed to execute wg");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "wg add --blocked-by peer:task failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Verify blocked_by was stored with the peer:task-id reference
    let graph = load_graph(&local_wg.join("graph.jsonl")).unwrap();
    let tasks: Vec<_> = graph.tasks().collect();
    assert_eq!(tasks.len(), 1);
    assert!(
        tasks[0].blocked_by.contains(&"upstream:remote-task".to_string()),
        "Expected 'upstream:remote-task' in blocked_by, got: {:?}",
        tasks[0].blocked_by
    );
}

#[test]
fn resolve_remote_task_status_direct_access() {
    use workgraph::federation::{RemoteResolution, resolve_remote_task_status};
    use workgraph::graph::{Node, Status, Task};

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let remote = setup_project(&tmp, "remote");

    let local_wg = local.join(".workgraph");
    let remote_wg = remote.join(".workgraph");
    register_peer(&local_wg, "myremote", remote.to_str().unwrap(), None);

    // Add a done task to remote
    let graph_path = remote_wg.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    let mut task = Task::default();
    task.id = "the-task".to_string();
    task.title = "The task".to_string();
    task.status = Status::Done;
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    let result = resolve_remote_task_status("myremote", "the-task", &local_wg);
    assert_eq!(result.status, Status::Done);
    assert_eq!(result.title.as_deref(), Some("The task"));
    assert_eq!(result.resolution, RemoteResolution::DirectFileAccess);
}

#[test]
fn resolve_remote_task_status_peer_not_found() {
    use workgraph::federation::{RemoteResolution, resolve_remote_task_status};

    let tmp = TempDir::new().unwrap();
    let local = setup_project(&tmp, "local");
    let local_wg = local.join(".workgraph");

    let result = resolve_remote_task_status("nonexistent", "any-task", &local_wg);
    assert_eq!(result.status, workgraph::graph::Status::Open);
    assert!(matches!(result.resolution, RemoteResolution::Unreachable(_)));
}

// ===========================================================================
// End-to-End Cross-Repo Communication Integration Test
// ===========================================================================
//
// Validates all four cross-repo subsystems working together:
//   1. Peer registration (federation.yaml)
//   2. Cross-repo task dispatch (wg add --repo)
//   3. Cross-repo dependencies (peer:task-id in blocked_by)
//   4. Trace function portability (instantiate --from peer)

/// Helper: save a trace function to a workgraph's functions directory.
fn setup_trace_function(wg_dir: &Path) {
    use workgraph::trace_function::{
        FunctionInput, InputType, TaskTemplate, TraceFunction,
    };

    let func = TraceFunction {
        kind: "trace-function".to_string(),
        version: 1,
        id: "deploy-service".to_string(),
        name: "Deploy Service".to_string(),
        description: "Build, test, and deploy a service".to_string(),
        extracted_from: vec![],
        extracted_by: None,
        extracted_at: None,
        tags: vec!["deployment".to_string()],
        inputs: vec![FunctionInput {
            name: "service_name".to_string(),
            input_type: InputType::String,
            description: "Name of the service to deploy".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        }],
        tasks: vec![
            TaskTemplate {
                template_id: "build".to_string(),
                title: "Build {{input.service_name}}".to_string(),
                description: "Build the service binary".to_string(),
                skills: vec!["rust".to_string()],
                blocked_by: vec![],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
            TaskTemplate {
                template_id: "test".to_string(),
                title: "Test {{input.service_name}}".to_string(),
                description: "Run the test suite".to_string(),
                skills: vec!["testing".to_string()],
                blocked_by: vec!["build".to_string()],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
            TaskTemplate {
                template_id: "deploy".to_string(),
                title: "Deploy {{input.service_name}}".to_string(),
                description: "Push to production".to_string(),
                skills: vec!["ops".to_string()],
                blocked_by: vec!["test".to_string()],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
        ],
        outputs: vec![],
    };

    let func_dir = workgraph::trace_function::functions_dir(wg_dir);
    workgraph::trace_function::save_function(&func, &func_dir).unwrap();
}

#[test]
fn end_to_end_cross_repo_all_four_subsystems() {
    use std::process::Command;
    use workgraph::federation::{RemoteResolution, resolve_remote_task_status};
    use workgraph::graph::{Node, Status, Task};
    use workgraph::query::ready_tasks_with_peers;

    let tmp = TempDir::new().unwrap();

    // ── Step 1: Set up two workgraph instances ──────────────────────────
    let project_a = setup_project(&tmp, "project-a");
    let project_b = setup_project(&tmp, "project-b");
    let wg_a = project_a.join(".workgraph");
    let wg_b = project_b.join(".workgraph");

    // ── Step 2: Register each as a peer of the other ───────────────────
    register_peer(&wg_a, "project-b", project_b.to_str().unwrap(), Some("Project B"));
    register_peer(&wg_b, "project-a", project_a.to_str().unwrap(), Some("Project A"));

    // Verify bidirectional peer resolution
    let resolved_b = federation::resolve_peer("project-b", &wg_a).unwrap();
    assert_eq!(resolved_b.workgraph_dir, wg_b.canonicalize().unwrap());

    let resolved_a = federation::resolve_peer("project-a", &wg_b).unwrap();
    assert_eq!(resolved_a.workgraph_dir, wg_a.canonicalize().unwrap());

    // ── Step 3: Dispatch a task from A to B (cross-repo dispatch) ──────
    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            wg_a.to_str().unwrap(),
            "add",
            "--repo",
            "project-b",
            "Build shared library",
            "--id",
            "build-lib",
            "-d",
            "Build the shared library that project-a depends on",
            "--tag",
            "cross-repo",
            "--skill",
            "rust",
        ])
        .output()
        .expect("Failed to execute wg");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Cross-repo dispatch failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("project-b:"),
        "Expected 'project-b:' prefix in output: {}",
        stdout
    );

    // Verify the task landed in project B's graph
    let graph_b = load_graph(&wg_b.join("graph.jsonl")).unwrap();
    let dispatched = graph_b.get_task("build-lib").unwrap();
    assert_eq!(dispatched.title, "Build shared library");
    assert_eq!(
        dispatched.description.as_deref(),
        Some("Build the shared library that project-a depends on")
    );
    assert!(dispatched.tags.contains(&"cross-repo".to_string()));
    assert!(dispatched.skills.contains(&"rust".to_string()));

    // ── Step 4: Create a cross-repo dependency (A blocked by B:task) ───
    // Add a task in A that depends on the remote task in B
    let graph_path_a = wg_a.join("graph.jsonl");
    let mut graph_a = load_graph(&graph_path_a).unwrap();
    let dependent = Task {
        id: "integrate-lib".to_string(),
        title: "Integrate shared library".to_string(),
        description: Some("Use the shared lib after B builds it".to_string()),
        status: Status::Open,
        blocked_by: vec!["project-b:build-lib".to_string()],
        ..Task::default()
    };
    graph_a.add_node(Node::Task(dependent));
    save_graph(&graph_a, &graph_path_a).unwrap();

    // ── Step 5: Verify the dependency blocks the task ──────────────────
    // The remote task is still open, so integrate-lib should be blocked
    let graph_a = load_graph(&graph_path_a).unwrap();
    let ready = ready_tasks_with_peers(&graph_a, &wg_a);
    assert!(
        !ready.iter().any(|t| t.id == "integrate-lib"),
        "integrate-lib should be blocked while project-b:build-lib is open"
    );

    // Verify via resolve_remote_task_status
    let remote_status = resolve_remote_task_status("project-b", "build-lib", &wg_a);
    assert_eq!(remote_status.status, Status::Open);
    assert_eq!(remote_status.title.as_deref(), Some("Build shared library"));
    assert_eq!(remote_status.resolution, RemoteResolution::DirectFileAccess);

    // ── Step 6: Mark remote task done and verify dependency resolves ───
    let graph_path_b = wg_b.join("graph.jsonl");
    let mut graph_b = load_graph(&graph_path_b).unwrap();
    if let Some(task) = graph_b.get_task_mut("build-lib") {
        task.status = Status::Done;
    }
    save_graph(&graph_b, &graph_path_b).unwrap();

    // Now integrate-lib should be ready
    let graph_a = load_graph(&graph_path_a).unwrap();
    let ready = ready_tasks_with_peers(&graph_a, &wg_a);
    assert!(
        ready.iter().any(|t| t.id == "integrate-lib"),
        "integrate-lib should be ready after project-b:build-lib is done"
    );

    // Double-check via resolve_remote_task_status
    let remote_status = resolve_remote_task_status("project-b", "build-lib", &wg_a);
    assert_eq!(remote_status.status, Status::Done);

    // ── Step 7: Instantiate a trace function from the peer ─────────────
    // Save a trace function in project B
    setup_trace_function(&wg_b);

    // Verify B has the function
    let peer_func_dir = workgraph::trace_function::functions_dir(&wg_b);
    let peer_funcs = workgraph::trace_function::load_all_functions(&peer_func_dir).unwrap();
    assert_eq!(peer_funcs.len(), 1);
    assert_eq!(peer_funcs[0].id, "deploy-service");

    // Instantiate the function from peer B into project A using CLI
    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            wg_a.to_str().unwrap(),
            "trace",
            "instantiate",
            "deploy-service",
            "--from",
            "project-b:deploy-service",
            "--input",
            "service_name=api-gateway",
            "--prefix",
            "gw",
        ])
        .output()
        .expect("Failed to execute wg trace instantiate");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Trace instantiate from peer failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Verify the instantiated tasks exist in project A's graph
    let graph_a = load_graph(&graph_path_a).unwrap();
    let build = graph_a.get_task("gw-build").unwrap();
    assert_eq!(build.title, "Build api-gateway");
    assert!(build.blocked_by.is_empty(), "Root task should have no blockers");

    let test = graph_a.get_task("gw-test").unwrap();
    assert_eq!(test.title, "Test api-gateway");
    assert!(test.blocked_by.contains(&"gw-build".to_string()));

    let deploy = graph_a.get_task("gw-deploy").unwrap();
    assert_eq!(deploy.title, "Deploy api-gateway");
    assert!(deploy.blocked_by.contains(&"gw-test".to_string()));

    // ── Step 8: Verify the full graph state ────────────────────────────
    // Project A should now have: integrate-lib (ready), gw-build (ready),
    // gw-test (blocked by gw-build), gw-deploy (blocked by gw-test)
    let ready = ready_tasks_with_peers(&graph_a, &wg_a);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&"integrate-lib"),
        "integrate-lib should still be ready: {:?}",
        ready_ids
    );
    assert!(
        ready_ids.contains(&"gw-build"),
        "gw-build should be ready (no blockers): {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"gw-test"),
        "gw-test should be blocked by gw-build: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"gw-deploy"),
        "gw-deploy should be blocked by gw-test: {:?}",
        ready_ids
    );
}

#[test]
fn end_to_end_cross_repo_mixed_local_and_remote_deps() {
    //! A task blocked by both a local task and a remote task.
    //! Both must be done before the task becomes ready.
    use workgraph::graph::{Node, Status, Task};
    use workgraph::query::ready_tasks_with_peers;

    let tmp = TempDir::new().unwrap();
    let project_a = setup_project(&tmp, "project-a");
    let project_b = setup_project(&tmp, "project-b");
    let wg_a = project_a.join(".workgraph");
    let wg_b = project_b.join(".workgraph");

    register_peer(&wg_a, "project-b", project_b.to_str().unwrap(), None);

    // Create a task in B
    let graph_path_b = wg_b.join("graph.jsonl");
    let mut graph_b = load_graph(&graph_path_b).unwrap();
    graph_b.add_node(Node::Task(Task {
        id: "remote-dep".to_string(),
        title: "Remote dependency".to_string(),
        status: Status::Open,
        ..Task::default()
    }));
    save_graph(&graph_b, &graph_path_b).unwrap();

    // Create tasks in A: local-dep and final-task (blocked by both)
    let graph_path_a = wg_a.join("graph.jsonl");
    let mut graph_a = load_graph(&graph_path_a).unwrap();
    graph_a.add_node(Node::Task(Task {
        id: "local-dep".to_string(),
        title: "Local dependency".to_string(),
        status: Status::Open,
        ..Task::default()
    }));
    graph_a.add_node(Node::Task(Task {
        id: "final-task".to_string(),
        title: "Needs both".to_string(),
        status: Status::Open,
        blocked_by: vec![
            "local-dep".to_string(),
            "project-b:remote-dep".to_string(),
        ],
        ..Task::default()
    }));
    save_graph(&graph_a, &graph_path_a).unwrap();

    // Neither dep is done → final-task blocked
    let graph_a = load_graph(&graph_path_a).unwrap();
    let ready = ready_tasks_with_peers(&graph_a, &wg_a);
    assert!(
        !ready.iter().any(|t| t.id == "final-task"),
        "final-task should be blocked (both deps open)"
    );
    assert!(
        ready.iter().any(|t| t.id == "local-dep"),
        "local-dep should be ready"
    );

    // Complete local dep only → still blocked (remote dep open)
    let mut graph_a = load_graph(&graph_path_a).unwrap();
    graph_a.get_task_mut("local-dep").unwrap().status = Status::Done;
    save_graph(&graph_a, &graph_path_a).unwrap();

    let graph_a = load_graph(&graph_path_a).unwrap();
    let ready = ready_tasks_with_peers(&graph_a, &wg_a);
    assert!(
        !ready.iter().any(|t| t.id == "final-task"),
        "final-task should still be blocked (remote dep open)"
    );

    // Complete remote dep → now ready
    let mut graph_b = load_graph(&graph_path_b).unwrap();
    graph_b.get_task_mut("remote-dep").unwrap().status = Status::Done;
    save_graph(&graph_b, &graph_path_b).unwrap();

    let graph_a = load_graph(&graph_path_a).unwrap();
    let ready = ready_tasks_with_peers(&graph_a, &wg_a);
    assert!(
        ready.iter().any(|t| t.id == "final-task"),
        "final-task should be ready (both deps done)"
    );
}

#[test]
fn end_to_end_trace_function_list_includes_peers() {
    //! Verify --include-peers on trace list-functions shows peer functions.
    use std::process::Command;

    let tmp = TempDir::new().unwrap();
    let project_a = setup_project(&tmp, "project-a");
    let project_b = setup_project(&tmp, "project-b");
    let wg_a = project_a.join(".workgraph");
    let wg_b = project_b.join(".workgraph");

    register_peer(&wg_a, "project-b", project_b.to_str().unwrap(), None);

    // Save a trace function in B
    setup_trace_function(&wg_b);

    // List functions from A with --include-peers --json
    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            wg_a.to_str().unwrap(),
            "trace",
            "list-functions",
            "--include-peers",
            "--json",
        ])
        .output()
        .expect("Failed to execute wg trace list-functions");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "trace list-functions --include-peers failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Parse JSON output and verify peer function is listed
    let entries: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("Failed to parse JSON output: {}\nstdout: {}", e, stdout);
    });

    assert!(
        !entries.is_empty(),
        "Expected at least one function entry, got empty list"
    );

    let peer_func = entries.iter().find(|e| e["id"] == "deploy-service");
    assert!(
        peer_func.is_some(),
        "Expected to find deploy-service in list: {:?}",
        entries
    );

    let source = peer_func.unwrap()["source"].as_str().unwrap();
    assert_eq!(source, "peer:project-b", "Source should indicate peer origin");
}

#[test]
fn end_to_end_dispatch_and_query_roundtrip() {
    //! Dispatch from A to B, then verify we can query the task's status
    //! from A using resolve_remote_task_status.
    use std::process::Command;
    use workgraph::federation::{RemoteResolution, resolve_remote_task_status};
    use workgraph::graph::Status;

    let tmp = TempDir::new().unwrap();
    let project_a = setup_project(&tmp, "project-a");
    let project_b = setup_project(&tmp, "project-b");
    let wg_a = project_a.join(".workgraph");
    let wg_b = project_b.join(".workgraph");

    register_peer(&wg_a, "project-b", project_b.to_str().unwrap(), None);

    // Dispatch task from A to B
    let output = Command::new(env!("CARGO_BIN_EXE_wg"))
        .args([
            "--dir",
            wg_a.to_str().unwrap(),
            "add",
            "--repo",
            "project-b",
            "Remote analysis task",
            "--id",
            "analyze",
        ])
        .output()
        .expect("Failed to execute wg");

    assert!(output.status.success(), "Dispatch failed");

    // Query status from A → should be Open (just created)
    let status = resolve_remote_task_status("project-b", "analyze", &wg_a);
    assert_eq!(status.status, Status::Open);
    assert_eq!(status.title.as_deref(), Some("Remote analysis task"));
    assert_eq!(status.resolution, RemoteResolution::DirectFileAccess);

    // Simulate work: mark as in-progress in B
    let graph_path_b = wg_b.join("graph.jsonl");
    let mut graph_b = load_graph(&graph_path_b).unwrap();
    graph_b.get_task_mut("analyze").unwrap().status = Status::InProgress;
    save_graph(&graph_b, &graph_path_b).unwrap();

    let status = resolve_remote_task_status("project-b", "analyze", &wg_a);
    assert_eq!(status.status, Status::InProgress);

    // Complete in B
    let mut graph_b = load_graph(&graph_path_b).unwrap();
    graph_b.get_task_mut("analyze").unwrap().status = Status::Done;
    save_graph(&graph_b, &graph_path_b).unwrap();

    let status = resolve_remote_task_status("project-b", "analyze", &wg_a);
    assert_eq!(status.status, Status::Done);
}

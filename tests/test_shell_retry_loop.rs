//! End-to-end integration test: shell executor + retry loop.
//!
//! Scenario:
//! 1. A shell task runs a command that fails the first 2 times, succeeds on the 3rd.
//!    (Uses a counter file to track attempts.)
//! 2. A checker task inspects the result.
//! 3. They are wired in a cycle (shell → checker → shell back-edge) with max-iterations: 5.
//! 4. The cycle runs: shell fails → cycle failure-restart → shell fails → restart →
//!    shell succeeds → checker runs → checker signals convergence → cycle stops.
//! 5. Verifications:
//!    - Shell executor runs the command correctly
//!    - Failure on attempts 1-2 triggers cycle failure restart
//!    - Attempt 3 succeeds, checker marks done --converged
//!    - All logs from all 3 attempts visible in graph
//!    - Max iterations would have stopped at 5 if it never succeeded

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{
    CycleConfig, Node, Status, Task, WorkGraph, evaluate_cycle_iteration, evaluate_cycle_on_failure,
};
use workgraph::parser::{load_graph, save_graph};

// ===========================================================================
// Helpers
// ===========================================================================

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

fn make_task(id: &str, title: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        ..Task::default()
    }
}

fn setup_workgraph(tmp: &TempDir, tasks: Vec<Task>) -> PathBuf {
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

fn build_graph(tasks: Vec<Task>) -> WorkGraph {
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    graph
}

// ===========================================================================
// End-to-end test: shell executor + retry loop
// ===========================================================================

/// Full end-to-end test simulating the coordinator-driven shell-retry-loop pattern.
///
/// Graph topology:
///   run-job (shell, exec="counter script") → check-job (checker)
///                      ↑                            |
///                      └────── back-edge ───────────┘
///
/// check-job owns the CycleConfig (max_iterations=5, restart_on_failure=true).
///
/// The counter script increments a file-based counter on each invocation.
/// It exits non-zero on attempts 1 and 2, exits 0 on attempt 3.
///
/// Flow:
///   Attempt 1: run-job fails → cycle failure restart (both reset to Open)
///   Attempt 2: run-job fails → cycle failure restart
///   Attempt 3: run-job succeeds → mark done → check-job ready →
///              check-job succeeds → mark done --converged → cycle stops
#[test]
fn test_shell_retry_loop() {
    let tmp = TempDir::new().unwrap();
    let counter_file = tmp.path().join("counter.txt");

    // Write the counter script that fails first 2 times, succeeds on 3rd
    let script_path = tmp.path().join("attempt.sh");
    fs::write(
        &script_path,
        format!(
            r#"#!/bin/bash
COUNTER_FILE="{counter}"
# Read current counter (default 0)
if [ -f "$COUNTER_FILE" ]; then
    COUNT=$(cat "$COUNTER_FILE")
else
    COUNT=0
fi
# Increment
COUNT=$((COUNT + 1))
echo "$COUNT" > "$COUNTER_FILE"
# Fail on attempts 1 and 2, succeed on 3+
if [ "$COUNT" -lt 3 ]; then
    echo "Attempt $COUNT: not ready yet"
    exit 1
else
    echo "Attempt $COUNT: success!"
    exit 0
fi
"#,
            counter = counter_file.display()
        ),
    )
    .unwrap();

    // Make script executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let script_cmd = format!("bash {}", script_path.display());

    // Checker script: reads the counter, exits 0 if count >= 3
    let checker_path = tmp.path().join("check.sh");
    fs::write(
        &checker_path,
        format!(
            r#"#!/bin/bash
COUNTER_FILE="{counter}"
if [ -f "$COUNTER_FILE" ]; then
    COUNT=$(cat "$COUNTER_FILE")
    if [ "$COUNT" -ge 3 ]; then
        echo "Check passed: count=$COUNT"
        exit 0
    else
        echo "Check failed: count=$COUNT"
        exit 1
    fi
else
    echo "Check failed: no counter file"
    exit 1
fi
"#,
            counter = counter_file.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&checker_path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let checker_cmd = format!("bash {}", checker_path.display());

    // --- Build the graph ---
    // run-job → check-job (forward edge)
    // check-job → run-job (back-edge, making a cycle)
    // check-job owns the CycleConfig
    let mut run_job = make_task("run-job", "Run Job");
    run_job.exec = Some(script_cmd.clone());
    run_job.exec_mode = Some("shell".to_string());
    run_job.after = vec!["check-job".to_string()]; // back-edge

    let mut check_job = make_task("check-job", "Check Job");
    check_job.exec = Some(checker_cmd.clone());
    check_job.exec_mode = Some("shell".to_string());
    check_job.after = vec!["run-job".to_string()]; // forward edge
    check_job.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: Some(5), // generous budget for test
    });

    let wg_dir = setup_workgraph(&tmp, vec![run_job, check_job]);

    // =====================================================================
    // Attempt 1: run-job fails (counter=1)
    // =====================================================================
    // Use wg exec --shell to run the shell task
    let output = wg_cmd(&wg_dir, &["exec", "run-job", "--shell"]);
    assert!(
        !output.status.success(),
        "Attempt 1 should fail (counter=1)"
    );

    // Verify counter file
    let count: u32 = fs::read_to_string(&counter_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(count, 1, "Counter should be 1 after first attempt");

    // Verify task is Failed
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("run-job").unwrap();
    assert_eq!(
        task.status,
        Status::Failed,
        "run-job should be Failed after attempt 1"
    );
    assert!(
        task.log.iter().any(|e| e.message.contains("failed")),
        "Should have failure log entry"
    );

    // Simulate cycle failure restart (what the coordinator would do)
    // wg exec --shell doesn't trigger cycle evaluation, so we use the graph API
    {
        let graph_path = wg_dir.join("graph.jsonl");
        let mut graph = load_graph(&graph_path).unwrap();
        let analysis = graph.compute_cycle_analysis();
        let reactivated = evaluate_cycle_on_failure(&mut graph, "run-job", &analysis);
        assert!(
            !reactivated.is_empty(),
            "Cycle failure restart should reactivate tasks after attempt 1"
        );
        save_graph(&graph, &graph_path).unwrap();
    }

    // Verify both tasks are back to Open
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("run-job").unwrap().status,
        Status::Open,
        "run-job should be Open after failure restart"
    );
    assert_eq!(
        graph.get_task("check-job").unwrap().status,
        Status::Open,
        "check-job should be Open after failure restart"
    );

    // Verify logs survived the restart
    let run_job_logs = &graph.get_task("run-job").unwrap().log;
    assert!(
        run_job_logs.len() >= 2,
        "run-job should have execution + restart logs after attempt 1, got {}",
        run_job_logs.len()
    );

    // =====================================================================
    // Attempt 2: run-job fails again (counter=2)
    // =====================================================================
    let output = wg_cmd(&wg_dir, &["exec", "run-job", "--shell"]);
    assert!(
        !output.status.success(),
        "Attempt 2 should fail (counter=2)"
    );

    let count: u32 = fs::read_to_string(&counter_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(count, 2, "Counter should be 2 after second attempt");

    // Verify task is Failed again
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("run-job").unwrap().status,
        Status::Failed,
        "run-job should be Failed after attempt 2"
    );

    // Cycle failure restart #2
    {
        let graph_path = wg_dir.join("graph.jsonl");
        let mut graph = load_graph(&graph_path).unwrap();
        let analysis = graph.compute_cycle_analysis();
        let reactivated = evaluate_cycle_on_failure(&mut graph, "run-job", &analysis);
        assert!(
            !reactivated.is_empty(),
            "Cycle failure restart should reactivate tasks after attempt 2"
        );
        save_graph(&graph, &graph_path).unwrap();
    }

    // Verify logs from BOTH attempts survived
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let run_job = graph.get_task("run-job").unwrap();
    assert_eq!(
        run_job.status,
        Status::Open,
        "run-job should be Open after 2nd restart"
    );
    // Should have: attempt 1 start + attempt 1 fail + restart 1 + attempt 2 start + attempt 2 fail + restart 2
    assert!(
        run_job.log.len() >= 4,
        "run-job should have logs from both attempts + restarts, got {} entries",
        run_job.log.len()
    );
    // Loop iteration should still be 0 (failure restarts don't increment)
    assert_eq!(
        run_job.loop_iteration, 0,
        "loop_iteration should stay 0 during failure restarts"
    );

    // =====================================================================
    // Attempt 3: run-job succeeds (counter=3)
    // =====================================================================
    let output = wg_cmd(&wg_dir, &["exec", "run-job", "--shell"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Attempt 3 should succeed (counter=3). stdout: {}",
        stdout
    );
    assert!(
        stdout.contains("success"),
        "Output should contain 'success'"
    );

    let count: u32 = fs::read_to_string(&counter_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(count, 3, "Counter should be 3 after third attempt");

    // run-job should be Done now
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("run-job").unwrap().status,
        Status::Done,
        "run-job should be Done after successful attempt 3"
    );

    // =====================================================================
    // Checker: check-job runs and signals convergence
    // =====================================================================
    // check-job depends on run-job (forward edge), which is now Done.
    // Run the checker via wg exec --shell
    let output = wg_cmd(&wg_dir, &["exec", "check-job", "--shell"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Checker should succeed (counter=3). stdout: {}",
        stdout
    );
    assert!(
        stdout.contains("Check passed"),
        "Checker output should indicate pass"
    );

    // check-job is now Done via wg exec --shell
    // Signal convergence by evaluating cycle iteration with converged tag
    {
        let graph_path = wg_dir.join("graph.jsonl");
        let mut graph = load_graph(&graph_path).unwrap();

        // Add converged tag (simulating what wg done --converged would do)
        let check = graph.get_task_mut("check-job").unwrap();
        assert_eq!(check.status, Status::Done, "check-job should be Done");
        if !check.tags.contains(&"converged".to_string()) {
            check.tags.push("converged".to_string());
        }

        let analysis = graph.compute_cycle_analysis();
        let reactivated = evaluate_cycle_iteration(&mut graph, "check-job", &analysis);
        assert!(
            reactivated.is_empty(),
            "Converged cycle should NOT reactivate any tasks, but got: {:?}",
            reactivated
        );

        save_graph(&graph, &graph_path).unwrap();
    }

    // =====================================================================
    // Final verification
    // =====================================================================
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();

    // Both tasks should remain Done
    let run_job = graph.get_task("run-job").unwrap();
    let check_job = graph.get_task("check-job").unwrap();
    assert_eq!(
        run_job.status,
        Status::Done,
        "run-job should be Done at end"
    );
    assert_eq!(
        check_job.status,
        Status::Done,
        "check-job should be Done at end"
    );

    // check-job should have converged tag
    assert!(
        check_job.tags.contains(&"converged".to_string()),
        "check-job should have converged tag"
    );

    // All logs from all 3 attempts should be preserved in run-job
    // Expected log entries: start+fail (attempt 1), restart 1, start+fail (attempt 2),
    // restart 2, start+success (attempt 3) = at least 7 entries
    assert!(
        run_job.log.len() >= 6,
        "run-job should have logs from all 3 attempts preserved. Got {} entries: {:?}",
        run_job.log.len(),
        run_job.log.iter().map(|e| &e.message).collect::<Vec<_>>()
    );

    // Verify log messages reference the execution
    let all_messages: Vec<&str> = run_job.log.iter().map(|e| e.message.as_str()).collect();
    assert!(
        all_messages
            .iter()
            .any(|m| m.contains("failed") || m.contains("Failed")),
        "Should have failure log entries. Messages: {:?}",
        all_messages
    );
    assert!(
        all_messages.iter().any(|m| m.contains("success")
            || m.contains("Successfully")
            || m.contains("completed")),
        "Should have success log entry. Messages: {:?}",
        all_messages
    );
    assert!(
        all_messages
            .iter()
            .any(|m| m.contains("failure restart") || m.contains("Cycle failure restart")),
        "Should have cycle restart log entries. Messages: {:?}",
        all_messages
    );

    // check-job should also have logs
    assert!(
        !check_job.log.is_empty(),
        "check-job should have at least one log entry"
    );

    // Loop iteration should be 0 (never reached iteration 1 because failure restarts
    // don't increment, and the cycle converged on the first successful iteration)
    assert_eq!(
        run_job.loop_iteration, 0,
        "loop_iteration should be 0 (converged on first iteration)"
    );

    // cycle_failure_restarts should be 2 (on the config owner)
    assert_eq!(
        check_job.cycle_failure_restarts, 2,
        "check-job (config owner) should have 2 failure restarts"
    );
}

/// Test that max_iterations would stop the cycle if it never succeeded.
/// Sets up a cycle at iteration 4 (of max 5) and verifies it stops.
#[test]
fn test_shell_retry_loop_max_iterations_safety_net() {
    // Build graph where the cycle is at iteration 4 with max_iterations=5
    let mut shell_task = Task {
        id: "run-batch".to_string(),
        title: "Run Batch".to_string(),
        status: Status::Done,
        exec: Some("echo done".to_string()),
        exec_mode: Some("shell".to_string()),
        after: vec!["check-batch".to_string()], // back-edge
        loop_iteration: 4,
        ..Task::default()
    };
    shell_task.log.push(workgraph::graph::LogEntry {
        timestamp: "2026-04-07T12:00:00Z".to_string(),
        message: "Iteration 4 completed".to_string(),
        actor: None,
        user: None,
    });

    let mut checker = Task {
        id: "check-batch".to_string(),
        title: "Check Batch".to_string(),
        status: Status::Done,
        after: vec!["run-batch".to_string()], // forward edge
        loop_iteration: 4,
        ..Task::default()
    };
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 5, // max=5, iteration 4 is the last (0..4 = 5 iterations)
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    checker.log.push(workgraph::graph::LogEntry {
        timestamp: "2026-04-07T12:05:00Z".to_string(),
        message: "Iteration 4 checked".to_string(),
        actor: None,
        user: None,
    });

    let mut graph = build_graph(vec![shell_task, checker]);
    let analysis = graph.compute_cycle_analysis();

    // Without converged tag, evaluate should still stop at max iterations
    let reactivated = evaluate_cycle_iteration(&mut graph, "check-batch", &analysis);
    assert!(
        reactivated.is_empty(),
        "Should NOT reactivate when max iterations (5) reached at iteration 4"
    );

    // Both should remain Done
    assert_eq!(graph.get_task("run-batch").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("check-batch").unwrap().status, Status::Done);

    // Logs should be preserved
    assert!(
        graph
            .get_task("run-batch")
            .unwrap()
            .log
            .iter()
            .any(|e| e.message.contains("Iteration 4")),
        "Shell task logs should be preserved"
    );
}

/// Test that a non-converged cycle at iteration 2 (max=5) DOES reactivate.
#[test]
fn test_shell_retry_loop_continues_below_max() {
    let mut shell_task = Task {
        id: "run-batch".to_string(),
        title: "Run Batch".to_string(),
        status: Status::Done,
        exec: Some("echo done".to_string()),
        exec_mode: Some("shell".to_string()),
        after: vec!["check-batch".to_string()],
        loop_iteration: 2,
        ..Task::default()
    };
    shell_task.log.push(workgraph::graph::LogEntry {
        timestamp: "2026-04-07T12:00:00Z".to_string(),
        message: "Attempt 3 log".to_string(),
        actor: None,
        user: None,
    });

    let mut checker = Task {
        id: "check-batch".to_string(),
        title: "Check Batch".to_string(),
        status: Status::Done,
        after: vec!["run-batch".to_string()],
        loop_iteration: 2,
        ..Task::default()
    };
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![shell_task, checker]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "check-batch", &analysis);
    assert_eq!(
        reactivated.len(),
        2,
        "Should reactivate both tasks at iteration 2 (below max 5)"
    );

    // Iteration should increment to 3
    assert_eq!(graph.get_task("run-batch").unwrap().loop_iteration, 3);
    assert_eq!(graph.get_task("check-batch").unwrap().loop_iteration, 3);

    // Both back to Open
    assert_eq!(graph.get_task("run-batch").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("check-batch").unwrap().status, Status::Open);

    // Logs from previous iteration preserved
    assert!(
        graph
            .get_task("run-batch")
            .unwrap()
            .log
            .iter()
            .any(|e| e.message.contains("Attempt 3")),
        "Previous logs should survive cycle reset"
    );
}

/// Test CLI-driven flow: wg exec --shell → wg fail → verify cycle restart.
#[test]
fn test_shell_retry_loop_cli_fail_triggers_restart() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");

    // Set up via wg init + add
    wg_ok(&wg_dir, &["init", "--executor", "shell"]);

    // Create shell task that always fails
    let mut run_task = make_task("run-fail", "Run Fail");
    run_task.exec = Some("exit 1".to_string());
    run_task.exec_mode = Some("shell".to_string());
    run_task.after = vec!["check-fail".to_string()]; // back-edge

    let mut checker = make_task("check-fail", "Check Fail");
    checker.exec = Some("exit 0".to_string());
    checker.exec_mode = Some("shell".to_string());
    checker.after = vec!["run-fail".to_string()]; // forward edge
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: Some(5),
    });

    // Overwrite the graph with our cycle setup
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(run_task));
    graph.add_node(Node::Task(checker));
    save_graph(&graph, &graph_path).unwrap();

    // Run shell task via CLI — it will fail (exit 1)
    let output = wg_cmd(&wg_dir, &["exec", "run-fail", "--shell"]);
    assert!(!output.status.success(), "exit 1 command should fail");

    // Verify it's Failed
    let graph = load_graph(&graph_path).unwrap();
    assert_eq!(graph.get_task("run-fail").unwrap().status, Status::Failed);

    // Use wg fail to re-trigger failure (which triggers cycle evaluation)
    // But task is already Failed from wg exec --shell. We need to drive
    // the cycle evaluation via the graph API (as the coordinator would).
    {
        let mut graph = load_graph(&graph_path).unwrap();
        let analysis = graph.compute_cycle_analysis();
        let reactivated = evaluate_cycle_on_failure(&mut graph, "run-fail", &analysis);
        assert!(
            !reactivated.is_empty(),
            "Cycle failure restart should reactivate after CLI exec failure"
        );
        save_graph(&graph, &graph_path).unwrap();
    }

    let graph = load_graph(&graph_path).unwrap();
    assert_eq!(
        graph.get_task("run-fail").unwrap().status,
        Status::Open,
        "run-fail should be reset to Open by cycle failure restart"
    );
    assert_eq!(
        graph.get_task("check-fail").unwrap().status,
        Status::Open,
        "check-fail should also be reset to Open"
    );
}

/// Test that wg done --converged via CLI stops a cycle.
#[test]
fn test_shell_retry_loop_cli_done_converged() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init", "--executor", "shell"]);

    // Create shell task + checker in a cycle
    let mut run_task = make_task("run-ok", "Run OK");
    run_task.exec = Some("echo success".to_string());
    run_task.exec_mode = Some("shell".to_string());
    run_task.after = vec!["check-ok".to_string()]; // back-edge
    run_task.status = Status::Done;

    let mut checker = make_task("check-ok", "Check OK");
    checker.exec = Some("echo checked".to_string());
    checker.exec_mode = Some("shell".to_string());
    checker.after = vec!["run-ok".to_string()]; // forward edge
    checker.status = Status::InProgress; // simulate it's running
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    // Write graph
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(run_task));
    graph.add_node(Node::Task(checker));
    save_graph(&graph, &graph_path).unwrap();

    // Call wg done --converged via CLI
    let output = wg_ok(&wg_dir, &["done", "check-ok", "--converged"]);
    assert!(
        output.contains("done") || output.contains("Done") || output.contains("check-ok"),
        "wg done should succeed. Got: {}",
        output
    );

    // Verify the cycle stopped — both tasks should be Done
    let graph = load_graph(&graph_path).unwrap();
    let run_task = graph.get_task("run-ok").unwrap();
    let checker = graph.get_task("check-ok").unwrap();

    assert_eq!(run_task.status, Status::Done, "run-ok should remain Done");
    assert_eq!(checker.status, Status::Done, "check-ok should be Done");
    assert!(
        checker.tags.contains(&"converged".to_string()),
        "check-ok should have converged tag"
    );
}

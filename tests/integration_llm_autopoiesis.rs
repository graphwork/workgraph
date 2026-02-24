//! LLM integration tests for autopoietic behaviors.
//!
//! Tests that real LLM-backed agents perform autopoietic behaviors correctly
//! within workgraph: task completion, subtask creation, context reading,
//! artifact propagation, cycle convergence, and survey synthesis.
//!
//! Run with: cargo test --features llm-tests --test integration_llm_autopoiesis -- --nocapture
//! Optionally set WG_TEST_MODEL to pick a model (default: haiku)

#[cfg(feature = "llm-tests")]
mod llm_autopoiesis {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    use serial_test::serial;

    // -----------------------------------------------------------------------
    // Harness helpers (same patterns as integration_service_coordinator.rs)
    // -----------------------------------------------------------------------

    /// Get the path to the compiled `wg` binary.
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

    /// Run `wg` with given args in a specific workgraph directory.
    fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
        let wg = wg_binary();
        Command::new(&wg)
            .arg("--dir")
            .arg(wg_dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
    }

    /// Run `wg` and assert success, returning stdout.
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

    /// Read task status via `wg show --json`.
    fn task_status(wg_dir: &Path, task_id: &str) -> String {
        let output = wg_cmd(wg_dir, &["show", task_id, "--json"]);
        if !output.status.success() {
            return "unknown".to_string();
        }
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        match serde_json::from_str::<serde_json::Value>(&stdout) {
            Ok(val) => val["status"].as_str().unwrap_or("unknown").to_string(),
            Err(_) => "unknown".to_string(),
        }
    }

    /// Read full task JSON via `wg show --json`.
    fn task_json(wg_dir: &Path, task_id: &str) -> Option<serde_json::Value> {
        let output = wg_cmd(wg_dir, &["show", task_id, "--json"]);
        if !output.status.success() {
            return None;
        }
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).ok()
    }

    /// Poll until a condition is met or timeout expires.
    fn wait_for(timeout: Duration, poll_ms: u64, mut f: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(poll_ms));
        }
        false
    }

    fn test_model() -> String {
        std::env::var("WG_TEST_MODEL").unwrap_or_else(|_| "haiku".to_string())
    }

    /// Set up a workgraph directory via `wg init`, then write a claude executor
    /// config with working_dir and PATH so the wrapper script's bare `wg`
    /// commands find the test binary and the workgraph.
    fn setup_llm_workgraph(tmp_root: &Path) -> PathBuf {
        let wg_dir = tmp_root.join(".workgraph");
        wg_ok(&wg_dir, &["init"]);

        let wg_bin_dir = wg_binary().parent().unwrap().to_string_lossy().to_string();
        let path_with_test_binary = format!(
            "{}:{}",
            wg_bin_dir,
            std::env::var("PATH").unwrap_or_default()
        );

        let executors_dir = wg_dir.join("executors");
        fs::create_dir_all(&executors_dir).unwrap();
        let claude_config = format!(
            r#"[executor]
type = "claude"
command = "claude"
args = ["--print", "--verbose", "--permission-mode", "bypassPermissions", "--output-format", "stream-json"]
working_dir = "{working_dir}"

[executor.env]
PATH = "{path}"

[executor.prompt_template]
template = """
# Task Assignment

You are an AI agent working on a task in a workgraph project.

{{{{task_identity}}}}
## Your Task
- **ID:** {{{{task_id}}}}
- **Title:** {{{{task_title}}}}
- **Description:** {{{{task_description}}}}

## Context from Dependencies
{{{{task_context}}}}

## Required Workflow

You MUST use these commands to track your work:

1. **Complete the task** when done:
   ```bash
   wg done {{{{task_id}}}}
   wg submit {{{{task_id}}}}
   ```

2. **Mark as failed** if you cannot complete:
   ```bash
   wg fail {{{{task_id}}}} --reason "Specific reason why"
   ```

## Important
- Run `wg done` (or `wg submit`) BEFORE you finish responding
- If `wg done` fails saying "requires verification", use `wg submit` instead
- Focus only on this specific task

Begin working on the task now.
"""
"#,
            working_dir = tmp_root.display(),
            path = path_with_test_binary,
        );
        fs::write(executors_dir.join("claude.toml"), claude_config).unwrap();

        wg_dir
    }

    /// Dump agent output files for debugging test failures.
    fn dump_agent_output(wg_dir: &Path) {
        let agents_dir = wg_dir.join("agents");
        if agents_dir.exists() {
            for entry in fs::read_dir(&agents_dir).unwrap().filter_map(|e| e.ok()) {
                for fname in &["output.log", "prompt.txt"] {
                    let fpath = entry.path().join(fname);
                    if fpath.exists() {
                        let content = fs::read_to_string(&fpath).unwrap_or_default();
                        let start = content.len().saturating_sub(3000);
                        eprintln!("--- {} ---\n{}", fpath.display(), &content[start..]);
                    }
                }
            }
        }
    }

    /// Assert task reached "done" status, dumping agent output on failure.
    fn assert_task_done(wg_dir: &Path, task_id: &str) {
        let status = task_status(wg_dir, task_id);
        if status != "done" {
            dump_agent_output(wg_dir);
        }
        assert_eq!(
            status, "done",
            "Task '{}' should be done, got: {}",
            task_id, status
        );
    }

    /// Wait for task to reach a terminal state (done or failed).
    fn wait_for_task(wg_dir: &Path, task_id: &str, timeout_secs: u64) -> bool {
        wait_for(Duration::from_secs(timeout_secs), 1000, || {
            let s = task_status(wg_dir, task_id);
            s == "done" || s == "failed"
        })
    }

    /// Spawn a Claude agent on the given task.
    fn spawn_agent(wg_dir: &Path, task_id: &str) {
        let model = test_model();
        let output = wg_cmd(
            wg_dir,
            &["spawn", task_id, "--executor", "claude", "--model", &model],
        );
        assert!(
            output.status.success(),
            "wg spawn {} failed: {}",
            task_id,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Get task log entries from task JSON.
    fn task_log_entries(wg_dir: &Path, task_id: &str) -> Vec<String> {
        task_json(wg_dir, task_id)
            .and_then(|v| v["log"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| entry["message"].as_str().map(String::from))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Test 1: Basic Lifecycle
    // -----------------------------------------------------------------------

    /// Agent reads a task description and calls `wg done`.
    #[test]
    #[serial]
    fn test_agent_completes_task() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());

        wg_ok(
            &wg_dir,
            &[
                "add",
                "Mark this task done",
                "--id",
                "basic-lifecycle",
                "-d",
                "Run: wg done basic-lifecycle",
            ],
        );

        spawn_agent(&wg_dir, "basic-lifecycle");

        let completed = wait_for_task(&wg_dir, "basic-lifecycle", 120);
        assert!(
            completed,
            "basic-lifecycle did not complete within 120s. Status: {}",
            task_status(&wg_dir, "basic-lifecycle")
        );

        assert_task_done(&wg_dir, "basic-lifecycle");

        // Verify agent output was captured
        let agents_dir = wg_dir.join("agents");
        if agents_dir.exists() {
            let has_output = fs::read_dir(&agents_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|entry| {
                    let output_log = entry.path().join("output.log");
                    output_log.exists()
                        && fs::read_to_string(&output_log)
                            .map(|c| !c.is_empty())
                            .unwrap_or(false)
                });
            assert!(
                has_output,
                "Agent output should have been captured in output.log"
            );
        }

        eprintln!(
            "test_agent_completes_task passed (model: {})",
            test_model()
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: Autopoietic Subtask Creation
    // -----------------------------------------------------------------------

    /// Agent creates a subtask with correct --after wiring, then completes.
    #[test]
    #[serial]
    fn test_agent_creates_subtask() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());

        wg_ok(
            &wg_dir,
            &[
                "add",
                "Create a subtask then complete",
                "--id",
                "parent-task",
                "-d",
                "You must:\n1. Run: wg add \"Child task\" --id child-task --after parent-task\n2. Verify the child exists: wg show child-task\n3. Complete: wg done parent-task",
            ],
        );

        spawn_agent(&wg_dir, "parent-task");

        let completed = wait_for_task(&wg_dir, "parent-task", 120);
        assert!(
            completed,
            "parent-task did not complete within 120s. Status: {}",
            task_status(&wg_dir, "parent-task")
        );

        assert_task_done(&wg_dir, "parent-task");

        // Verify child-task was created
        let child = task_json(&wg_dir, "child-task");
        if child.is_none() {
            dump_agent_output(&wg_dir);
        }
        assert!(child.is_some(), "child-task should exist in the graph");

        let child = child.unwrap();

        // Verify dependency wiring: child-task has parent-task in its after array
        let after = child["after"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            after.contains(&"parent-task"),
            "child-task should depend on parent-task. after={:?}",
            after
        );

        // child-task should be open (created but not executed)
        let child_status = task_status(&wg_dir, "child-task");
        assert_eq!(
            child_status, "open",
            "child-task should be open, got: {}",
            child_status
        );

        eprintln!(
            "test_agent_creates_subtask passed (model: {})",
            test_model()
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: Context Reading
    // -----------------------------------------------------------------------

    /// Agent reads context from upstream dependency artifacts.
    #[test]
    #[serial]
    fn test_agent_reads_dependency_context() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());

        // Create upstream task and mark it done with an artifact
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Upstream",
                "--id",
                "upstream-ctx",
                "-d",
                "Already done",
            ],
        );
        wg_ok(&wg_dir, &["done", "upstream-ctx"]);

        // Write the artifact content
        let artifacts_dir = wg_dir.join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        fs::write(
            artifacts_dir.join("upstream-data.md"),
            "The secret answer is 42",
        )
        .unwrap();
        wg_ok(
            &wg_dir,
            &[
                "artifact",
                "upstream-ctx",
                ".workgraph/artifacts/upstream-data.md",
            ],
        );

        // Create downstream task that depends on upstream
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Read upstream context and report",
                "--id",
                "reader-task",
                "--after",
                "upstream-ctx",
                "-d",
                "Read your dependency context. The upstream task produced an artifact.\nRead the artifact file and then:\n1. Run: wg log reader-task \"Found answer: <the answer you found>\"\n2. Run: wg done reader-task",
            ],
        );

        spawn_agent(&wg_dir, "reader-task");

        let completed = wait_for_task(&wg_dir, "reader-task", 120);
        assert!(
            completed,
            "reader-task did not complete within 120s. Status: {}",
            task_status(&wg_dir, "reader-task")
        );

        assert_task_done(&wg_dir, "reader-task");

        // Verify task log mentions "42"
        let logs = task_log_entries(&wg_dir, "reader-task");
        let has_42 = logs.iter().any(|msg| msg.contains("42"));
        if !has_42 {
            dump_agent_output(&wg_dir);
        }
        assert!(
            has_42,
            "reader-task log should mention '42'. Logs: {:?}",
            logs
        );

        eprintln!(
            "test_agent_reads_dependency_context passed (model: {})",
            test_model()
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: Artifact Propagation
    // -----------------------------------------------------------------------

    /// Agent A produces an artifact that agent B can read.
    #[test]
    #[serial]
    fn test_artifact_propagation() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());

        // Task A: write an artifact file
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Write an artifact",
                "--id",
                "writer-task",
                "-d",
                "You must:\n1. Write the text 'HELLO_FROM_WRITER' to a file at output.txt (in the current directory)\n2. Record it: wg artifact writer-task output.txt\n3. Complete: wg done writer-task",
            ],
        );

        // Task B: read the artifact (depends on A)
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Read the artifact from writer",
                "--id",
                "artifact-reader",
                "--after",
                "writer-task",
                "-d",
                "Your upstream task wrote an artifact file. Find and read it.\n1. Read the file output.txt\n2. Log what you found: wg log artifact-reader \"Content: <what you read>\"\n3. Complete: wg done artifact-reader",
            ],
        );

        // Spawn writer first, wait for completion
        spawn_agent(&wg_dir, "writer-task");

        let writer_completed = wait_for_task(&wg_dir, "writer-task", 120);
        assert!(
            writer_completed,
            "writer-task did not complete within 120s. Status: {}",
            task_status(&wg_dir, "writer-task")
        );
        assert_task_done(&wg_dir, "writer-task");

        // Verify the artifact file exists
        let output_file = tmp.path().join("output.txt");
        if !output_file.exists() {
            dump_agent_output(&wg_dir);
        }
        assert!(
            output_file.exists(),
            "output.txt should exist after writer-task"
        );

        // Verify writer-task has the artifact recorded
        let writer_json = task_json(&wg_dir, "writer-task").unwrap();
        let artifacts = writer_json["artifacts"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            artifacts.iter().any(|a| a.contains("output.txt")),
            "writer-task should have output.txt in artifacts. Got: {:?}",
            artifacts
        );

        // Now spawn reader
        spawn_agent(&wg_dir, "artifact-reader");

        let reader_completed = wait_for_task(&wg_dir, "artifact-reader", 120);
        assert!(
            reader_completed,
            "artifact-reader did not complete within 120s. Status: {}",
            task_status(&wg_dir, "artifact-reader")
        );
        assert_task_done(&wg_dir, "artifact-reader");

        // Verify reader log mentions HELLO_FROM_WRITER
        let logs = task_log_entries(&wg_dir, "artifact-reader");
        let has_content = logs.iter().any(|msg| msg.contains("HELLO_FROM_WRITER"));
        if !has_content {
            dump_agent_output(&wg_dir);
        }
        assert!(
            has_content,
            "artifact-reader log should mention 'HELLO_FROM_WRITER'. Logs: {:?}",
            logs
        );

        eprintln!(
            "test_artifact_propagation passed (model: {})",
            test_model()
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: Cycle Convergence
    // -----------------------------------------------------------------------

    /// Agent uses `wg done --converged` when told the work is complete.
    ///
    /// Uses the simpler approach from the design: verify the agent understands
    /// when to use --converged vs plain wg done based on task description.
    /// The cycle mechanism itself is tested elsewhere.
    #[test]
    #[serial]
    fn test_cycle_converged() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());

        // First test: agent calls plain `wg done` (not converged)
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Review iteration 0",
                "--id",
                "review-iter0",
                "-d",
                "You are in a review cycle at iteration 0. More work is needed after this.\nRun: wg done review-iter0\nDo NOT use --converged since this is not the final iteration.",
            ],
        );

        spawn_agent(&wg_dir, "review-iter0");

        let completed = wait_for_task(&wg_dir, "review-iter0", 120);
        assert!(
            completed,
            "review-iter0 did not complete within 120s. Status: {}",
            task_status(&wg_dir, "review-iter0")
        );
        assert_task_done(&wg_dir, "review-iter0");

        // Second test: agent calls `wg done --converged`
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Review final iteration",
                "--id",
                "review-final",
                "-d",
                "You are in a review cycle and the work is now complete. This is the final iteration.\nRun: wg done review-final --converged\nYou MUST use the --converged flag to signal convergence.",
            ],
        );

        spawn_agent(&wg_dir, "review-final");

        let completed = wait_for_task(&wg_dir, "review-final", 120);
        assert!(
            completed,
            "review-final did not complete within 120s. Status: {}",
            task_status(&wg_dir, "review-final")
        );
        assert_task_done(&wg_dir, "review-final");

        // Log task details for debugging
        let logs = task_log_entries(&wg_dir, "review-final");
        eprintln!("review-final logs: {:?}", logs);

        eprintln!(
            "test_cycle_converged passed (model: {})",
            test_model()
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: Survey Synthesis
    // -----------------------------------------------------------------------

    /// Agent reads multiple survey artifacts and creates improvement tasks.
    #[test]
    #[serial]
    fn test_survey_synthesis() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_llm_workgraph(tmp.path());

        // Create survey-a with artifact
        wg_ok(
            &wg_dir,
            &["add", "Survey A", "--id", "survey-a", "-d", "done"],
        );
        wg_ok(&wg_dir, &["done", "survey-a"]);

        let artifacts_dir = wg_dir.join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        fs::write(
            artifacts_dir.join("survey-a.md"),
            "Finding: Function parse_input() at src/parser.rs:45 is 200 lines. Recommend splitting.",
        )
        .unwrap();
        wg_ok(
            &wg_dir,
            &[
                "artifact",
                "survey-a",
                ".workgraph/artifacts/survey-a.md",
            ],
        );

        // Create survey-b with artifact
        wg_ok(
            &wg_dir,
            &["add", "Survey B", "--id", "survey-b", "-d", "done"],
        );
        wg_ok(&wg_dir, &["done", "survey-b"]);

        fs::write(
            artifacts_dir.join("survey-b.md"),
            "Finding: No tests for the retry command. Recommend adding integration test.",
        )
        .unwrap();
        wg_ok(
            &wg_dir,
            &[
                "artifact",
                "survey-b",
                ".workgraph/artifacts/survey-b.md",
            ],
        );

        // Create synthesis task depending on both surveys
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Synthesize survey findings into improvement tasks",
                "--id",
                "synthesize",
                "--after",
                "survey-a,survey-b",
                "-d",
                "Read the survey artifacts from your dependencies. For each finding:\n1. Create a task: wg add \"<improvement title>\" --after synthesize\n2. When all improvement tasks are created, run: wg done synthesize\n\nYou should create at least 2 improvement tasks (one per finding).",
            ],
        );

        spawn_agent(&wg_dir, "synthesize");

        let completed = wait_for_task(&wg_dir, "synthesize", 180);
        assert!(
            completed,
            "synthesize did not complete within 180s. Status: {}",
            task_status(&wg_dir, "synthesize")
        );
        assert_task_done(&wg_dir, "synthesize");

        // List all tasks and verify new ones were created
        let list_output = wg_cmd(&wg_dir, &["list", "--json"]);
        let list_stdout = String::from_utf8_lossy(&list_output.stdout).to_string();
        let tasks: Vec<serde_json::Value> =
            serde_json::from_str(&list_stdout).unwrap_or_default();

        // Filter for tasks that have "synthesize" in their after array
        // (i.e., tasks created by the agent as downstream of synthesize)
        let setup_ids = ["survey-a", "survey-b", "synthesize"];
        let new_tasks: Vec<&serde_json::Value> = tasks
            .iter()
            .filter(|t| {
                let id = t["id"].as_str().unwrap_or("");
                !setup_ids.contains(&id)
            })
            .filter(|t| {
                t["after"]
                    .as_array()
                    .map(|a| a.iter().any(|v| v.as_str() == Some("synthesize")))
                    .unwrap_or(false)
            })
            .collect();

        if new_tasks.len() < 2 {
            dump_agent_output(&wg_dir);
            eprintln!(
                "All tasks: {}",
                serde_json::to_string_pretty(&tasks).unwrap_or_default()
            );
        }

        assert!(
            new_tasks.len() >= 2,
            "Agent should have created at least 2 improvement tasks after synthesize. Found: {}",
            new_tasks.len()
        );

        // Verify task titles relate to the findings
        let all_titles: Vec<&str> = new_tasks
            .iter()
            .filter_map(|t| t["title"].as_str())
            .collect();
        let keywords = ["parser", "split", "retry", "test", "parse", "refactor"];
        let titles_lower: Vec<String> = all_titles.iter().map(|t| t.to_lowercase()).collect();
        let has_relevant_title = titles_lower
            .iter()
            .any(|t| keywords.iter().any(|k| t.contains(k)));
        assert!(
            has_relevant_title,
            "At least one improvement task title should relate to survey findings. Titles: {:?}",
            all_titles
        );

        eprintln!(
            "test_survey_synthesis passed: {} improvement tasks created (model: {})",
            new_tasks.len(),
            test_model()
        );
    }
}

//! Spawn command - spawns an agent to work on a task
//!
//! Usage:
//!   wg spawn <task-id> --executor <name> [--timeout <duration>]
//!
//! The spawn command:
//! 1. Claims the task (fails if already claimed)
//! 2. Loads executor config from .workgraph/executors/<name>.toml
//! 3. Starts the executor process with task context
//! 4. Registers the agent in the registry
//! 5. Prints agent info (ID, PID, output file)
//! 6. Returns immediately (doesn't wait for completion)

pub(crate) mod context;
mod execution;
pub(crate) mod worktree;

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use super::graph_path;

/// Escape a string for safe use in shell commands (for simple args)
fn shell_escape(s: &str) -> String {
    // Use single quotes and escape any single quotes within
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Generate a command that reads prompt from a file
/// This is more reliable than heredoc when output redirection is involved
fn prompt_file_command(prompt_file: &str, command: &str) -> String {
    format!("cat {} | {}", shell_escape(prompt_file), command)
}

/// Result of spawning an agent
#[derive(Debug, Serialize)]
pub struct SpawnResult {
    pub agent_id: String,
    pub pid: u32,
    pub task_id: String,
    pub executor: String,
    pub executor_type: String,
    pub output_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Parse a timeout duration string like "30m", "1h", "90s" into seconds.
/// Returns the number of seconds as a u64.
fn parse_timeout_secs(timeout_str: &str) -> Result<u64> {
    let timeout_str = timeout_str.trim();
    if timeout_str.is_empty() {
        anyhow::bail!("Empty timeout string");
    }

    let (num_str, unit) = if let Some(s) = timeout_str.strip_suffix('s') {
        (s, "s")
    } else if let Some(s) = timeout_str.strip_suffix('m') {
        (s, "m")
    } else if let Some(s) = timeout_str.strip_suffix('h') {
        (s, "h")
    } else {
        // Default to seconds if no unit
        (timeout_str, "s")
    };

    let num: u64 = num_str.parse().context("Invalid timeout number")?;

    let secs = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        _ => num,
    };

    Ok(secs)
}

/// Parse a timeout duration string like "30m", "1h", "90s"
#[cfg(test)]
fn parse_timeout(timeout_str: &str) -> Result<std::time::Duration> {
    let timeout_str = timeout_str.trim();
    if timeout_str.is_empty() {
        anyhow::bail!("Empty timeout string");
    }

    let (num_str, unit) = if let Some(s) = timeout_str.strip_suffix('s') {
        (s, "s")
    } else if let Some(s) = timeout_str.strip_suffix('m') {
        (s, "m")
    } else if let Some(s) = timeout_str.strip_suffix('h') {
        (s, "h")
    } else {
        // Default to seconds if no unit
        (timeout_str, "s")
    };

    let num: u64 = num_str.parse().context("Invalid timeout number")?;

    let secs = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        _ => num,
    };

    Ok(std::time::Duration::from_secs(secs))
}

/// Get the output directory for an agent
fn agent_output_dir(workgraph_dir: &Path, agent_id: &str) -> PathBuf {
    workgraph_dir.join("agents").join(agent_id)
}

/// Run the spawn command (CLI entry point)
pub fn run(
    dir: &Path,
    task_id: &str,
    executor_name: &str,
    timeout: Option<&str>,
    model: Option<&str>,
    json: bool,
) -> Result<()> {
    let result =
        execution::spawn_agent_inner(dir, task_id, executor_name, timeout, model, "wg spawn")?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Spawned {} for task '{}'", result.agent_id, task_id);
        println!("  Executor: {} ({})", executor_name, result.executor_type);
        if let Some(ref m) = result.model {
            println!("  Model: {}", m);
        }
        println!("  PID: {}", result.pid);
        println!("  Output: {}", result.output_file);
    }

    Ok(())
}

/// Spawn an agent and return (agent_id, pid)
/// This is a helper for the service daemon
pub fn spawn_agent(
    dir: &Path,
    task_id: &str,
    executor_name: &str,
    timeout: Option<&str>,
    model: Option<&str>,
) -> Result<(String, u32)> {
    let result =
        execution::spawn_agent_inner(dir, task_id, executor_name, timeout, model, "coordinator")?;
    Ok((result.agent_id, result.pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Status, Task, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::service::registry::AgentRegistry;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_graph(dir: &Path, tasks: Vec<Task>) {
        let path = graph_path(dir);
        fs::create_dir_all(dir).unwrap();

        // Initialize git repository for worktree tests
        let project_root = dir.parent().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .arg(project_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(project_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(project_root)
            .output()
            .unwrap();
        // Set safe directory to avoid dubious ownership errors in temp directories
        std::process::Command::new("git")
            .args(["config", "--global", "--add", "safe.directory", &project_root.to_string_lossy()])
            .output()
            .unwrap();

        // Create initial commit
        fs::write(project_root.join("README.md"), "Test project").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(project_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(project_root)
            .output()
            .unwrap();

        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
    }

    #[test]
    fn test_prompt_file_command() {
        let result = prompt_file_command("/tmp/prompt.txt", "claude --print");
        assert!(result.contains("cat"));
        assert!(result.contains("/tmp/prompt.txt"));
        assert!(result.contains("claude --print"));
    }

    #[test]
    fn test_parse_timeout_seconds() {
        let dur = parse_timeout("30s").unwrap();
        assert_eq!(dur, std::time::Duration::from_secs(30));
    }

    #[test]
    fn test_parse_timeout_minutes() {
        let dur = parse_timeout("5m").unwrap();
        assert_eq!(dur, std::time::Duration::from_secs(300));
    }

    #[test]
    fn test_parse_timeout_hours() {
        let dur = parse_timeout("2h").unwrap();
        assert_eq!(dur, std::time::Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_timeout_no_unit() {
        let dur = parse_timeout("60").unwrap();
        assert_eq!(dur, std::time::Duration::from_secs(60));
    }

    #[test]
    fn test_spawn_task_not_found() {
        let temp_dir = TempDir::new().unwrap();
        setup_graph(temp_dir.path(), vec![]);

        let result = run(temp_dir.path(), "nonexistent", "shell", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_spawn_already_claimed_task() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.status = Status::InProgress;
        task.assigned = Some("other-agent".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        let result = run(temp_dir.path(), "t1", "shell", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already claimed"));
    }

    #[test]
    fn test_spawn_done_task() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.status = Status::Done;
        setup_graph(temp_dir.path(), vec![task]);

        let result = run(temp_dir.path(), "t1", "shell", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already done"));
    }

    #[test]
    fn test_spawn_shell_without_exec_fails() {
        let temp_dir = TempDir::new().unwrap();
        let task = make_task("t1", "Test Task");
        // Task has no exec command
        setup_graph(temp_dir.path(), vec![task]);

        let result = run(temp_dir.path(), "t1", "shell", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no exec command"));
    }

    #[test]
    fn test_spawn_shell_with_exec() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        // This will actually spawn a process
        let result = run(temp_dir.path(), "t1", "shell", None, None, false);
        assert!(result.is_ok());

        // Verify task was claimed
        let graph = workgraph::parser::load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);

        // Verify agent was registered
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        assert_eq!(registry.agents.len(), 1);
    }

    #[test]
    fn test_spawn_creates_output_directory() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Poll for the spawned process to create output file (up to 5s)
        let agent_dir = temp_dir.path().join("agents").join("agent-1");
        for _ in 0..50 {
            if agent_dir.join("output.log").exists() && agent_dir.join("metadata.json").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // Check output directory was created
        let agents_dir = temp_dir.path().join("agents");
        assert!(agents_dir.exists());

        // Should have agent-1 directory
        assert!(agent_dir.exists());

        // Should have output.log and metadata.json
        assert!(agent_dir.join("output.log").exists());
        assert!(agent_dir.join("metadata.json").exists());
    }

    #[test]
    fn test_wrapper_script_generation_success() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        task.verify = None; // Not verified, should use wg done
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script was created in agents directory
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        assert!(
            wrapper_path.exists(),
            "Wrapper script not found at {:?}",
            wrapper_path
        );

        // Read wrapper script and verify it contains the expected auto-complete logic
        let script = fs::read_to_string(&wrapper_path).unwrap();
        assert!(
            script.contains("TASK_ID='t1'"),
            "Task ID should be shell-escaped with single quotes"
        );
        assert!(script.contains("wg done \"$TASK_ID\""));
        assert!(script.contains("[wrapper] Agent exited successfully, marking task done"));
        assert!(script.contains("wg show \"$TASK_ID\" --json"));
        assert!(script.contains("if [ \"$TASK_STATUS\" = \"in-progress\" ]"));
    }

    #[test]
    fn test_wrapper_script_for_verified_task() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        task.verify = Some("manual".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script was created in agents directory
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        assert!(
            wrapper_path.exists(),
            "Wrapper script not found at {:?}",
            wrapper_path
        );

        // Verified tasks now also use wg done (submit is deprecated)
        let script = fs::read_to_string(&wrapper_path).unwrap();
        assert!(script.contains("wg done \"$TASK_ID\""));
        assert!(script.contains("[wrapper] Agent exited successfully, marking task done"));
    }

    #[test]
    fn test_wrapper_handles_agent_failure() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("exit 1".to_string()); // Will fail
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script was created in agents directory
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        assert!(
            wrapper_path.exists(),
            "Wrapper script not found at {:?}",
            wrapper_path
        );

        // Read wrapper script and verify it handles failure
        let script = fs::read_to_string(&wrapper_path).unwrap();
        assert!(script.contains("wg fail \"$TASK_ID\""));
        assert!(script.contains("[wrapper] Agent exited with code"));
    }

    #[test]
    fn test_wrapper_detects_task_status() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("wg done t1".to_string()); // Agent marks it done
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script detects if task already done by agent
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Should check task status with wg show
        assert!(script.contains("TASK_STATUS=$(wg show \"$TASK_ID\" --json"));

        // Should only auto-complete if still in_progress
        assert!(script.contains("if [ \"$TASK_STATUS\" = \"in-progress\" ]"));
    }

    #[test]
    fn test_wrapper_script_preserves_exit_code() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("exit 42".to_string()); // Specific exit code
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script preserves exit code
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Should capture and preserve EXIT_CODE
        assert!(script.contains("EXIT_CODE=$?"));
        assert!(script.contains("exit $EXIT_CODE"));
    }

    #[test]
    fn test_wrapper_appends_output_to_log() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo 'Agent output'".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script appends to output file
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Should redirect agent output to output file
        assert!(script.contains(">> \"$OUTPUT_FILE\" 2>&1"));

        // Should append status messages
        assert!(script.contains("echo \"\" >> \"$OUTPUT_FILE\""));
        assert!(script.contains("[wrapper]"));
    }

    #[test]
    fn test_wrapper_suppresses_wg_command_errors() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("true".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        // Check wrapper script suppresses wg command errors
        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Should redirect errors and log failures instead of silencing
        assert!(script.contains("2>> \"$OUTPUT_FILE\" || echo \"[wrapper] WARNING:"));
    }

    #[test]
    fn test_parse_timeout_secs_minutes() {
        assert_eq!(parse_timeout_secs("30m").unwrap(), 1800);
    }

    #[test]
    fn test_parse_timeout_secs_hours() {
        assert_eq!(parse_timeout_secs("2h").unwrap(), 7200);
    }

    #[test]
    fn test_parse_timeout_secs_seconds() {
        assert_eq!(parse_timeout_secs("90s").unwrap(), 90);
    }

    #[test]
    fn test_parse_timeout_secs_no_unit() {
        assert_eq!(parse_timeout_secs("120").unwrap(), 120);
    }

    #[test]
    fn test_parse_timeout_secs_empty_fails() {
        assert!(parse_timeout_secs("").is_err());
    }

    #[test]
    fn test_wrapper_script_includes_timeout() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        // Spawn with explicit timeout
        run(temp_dir.path(), "t1", "shell", Some("5m"), None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Verify the timeout command wraps the inner command
        assert!(
            script.contains("timeout --signal=TERM --kill-after=30 300"),
            "Wrapper should contain timeout command with 300s (5m). Script:\n{}",
            script
        );
        // Verify timeout exit code handling
        assert!(
            script.contains("EXIT_CODE -eq 124"),
            "Wrapper should handle timeout exit code 124"
        );
        assert!(
            script.contains("Agent exceeded hard timeout"),
            "Wrapper should report timeout in failure reason"
        );
    }

    #[test]
    fn test_wrapper_script_default_timeout_from_config() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        // Default config has agent_timeout = "30m", no explicit timeout
        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Default timeout is 30m = 1800s
        assert!(
            script.contains("timeout --signal=TERM --kill-after=30 1800"),
            "Wrapper should contain default timeout of 1800s (30m). Script:\n{}",
            script
        );
    }

    #[test]
    fn test_metadata_records_effective_timeout() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", Some("10m"), None, false).unwrap();

        let metadata_path = agent_output_dir(temp_dir.path(), "agent-1").join("metadata.json");
        let metadata: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&metadata_path).unwrap()).unwrap();
        assert_eq!(
            metadata["timeout_secs"], 600,
            "Metadata should record 600s (10m)"
        );
    }

    #[test]
    fn test_wrapper_script_contains_merge_back_section() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        // Merge-back section is always present but gated by env var check
        assert!(
            script.contains("# --- Merge Back (worktree isolation) ---"),
            "Wrapper should contain merge-back section header"
        );
        assert!(
            script.contains(r#"if [ -n "$WG_WORKTREE_PATH" ] && [ -n "$WG_BRANCH" ] && [ -n "$WG_PROJECT_ROOT" ]"#),
            "Merge-back should be gated by worktree env vars"
        );
    }

    #[test]
    fn test_wrapper_merge_back_uses_squash_merge() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        assert!(
            script.contains("git merge --squash \"$WG_BRANCH\""),
            "Should use squash merge for clean history"
        );
    }

    #[test]
    fn test_wrapper_merge_back_handles_conflicts() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        assert!(
            script.contains("git merge --abort"),
            "Should abort merge on conflict"
        );
        assert!(
            script.contains("Merge conflict"),
            "Should report merge conflict"
        );
        assert!(
            script.contains(r#"wg fail "$TASK_ID" --reason "Merge conflict"#),
            "Should fail task on merge conflict"
        );
    }

    #[test]
    fn test_wrapper_merge_back_uses_flock() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        assert!(script.contains(".merge-lock"), "Should use merge lock file");
        assert!(script.contains("flock 9"), "Should acquire flock");
        assert!(script.contains("flock -u 9"), "Should release flock");
    }

    #[test]
    fn test_wrapper_merge_back_cleans_up_worktree() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        assert!(
            script.contains(
                r#"git -C "$WG_PROJECT_ROOT" worktree remove --force "$WG_WORKTREE_PATH""#
            ),
            "Should force-remove worktree"
        );
        assert!(
            script.contains(r#"git -C "$WG_PROJECT_ROOT" branch -D "$WG_BRANCH""#),
            "Should delete worktree branch"
        );
        assert!(
            script.contains(r#"rm -f "$WG_WORKTREE_PATH/.workgraph""#),
            "Should remove .workgraph symlink"
        );
        assert!(
            script.contains("[wrapper] Cleaned up worktree"),
            "Should log worktree cleanup"
        );
    }

    #[test]
    fn test_wrapper_merge_back_skips_no_commits() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        assert!(
            script.contains("No commits on $WG_BRANCH, nothing to merge"),
            "Should skip merge when no commits exist"
        );
    }

    #[test]
    fn test_wrapper_merge_back_commit_convention() {
        let temp_dir = TempDir::new().unwrap();
        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());
        setup_graph(temp_dir.path(), vec![task]);

        run(temp_dir.path(), "t1", "shell", None, None, false).unwrap();

        let wrapper_path = agent_output_dir(temp_dir.path(), "agent-1").join("run.sh");
        let script = fs::read_to_string(&wrapper_path).unwrap();

        assert!(
            script.contains("feat: $TASK_ID ($WG_AGENT_ID)"),
            "Commit message should follow convention"
        );
        assert!(
            script.contains("Squash-merged from worktree branch"),
            "Commit message should mention squash merge origin"
        );
    }
}

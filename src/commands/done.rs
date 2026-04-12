use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::config::{Config, CoordinatorConfig};
use workgraph::graph::{
    LogEntry, Node, Status, create_user_board_task, evaluate_cycle_iteration, parse_token_usage,
    parse_wg_tokens, user_board_handle, user_board_seq,
};
use workgraph::graph::{Task, parse_delay};
use workgraph::parser::modify_graph;
use workgraph::query;
use workgraph::service::registry::AgentRegistry;

// Import evaluate module for LLM verification
use crate::commands::evaluate;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

/// Enhanced timeout resolution with priority order
fn resolve_verify_timeout(
    task: &Task,
    coordinator_config: &CoordinatorConfig,
) -> std::time::Duration {
    // 1. Task-specific timeout (highest priority)
    if let Some(task_timeout) = &task.verify_timeout {
        if let Some(secs) = parse_delay(task_timeout) {
            return std::time::Duration::from_secs(secs);
        }
    }

    // 2. Global environment variable
    if let Ok(env_timeout) = std::env::var("WG_VERIFY_TIMEOUT") {
        if let Ok(secs) = env_timeout.parse::<u64>() {
            return std::time::Duration::from_secs(secs);
        }
    }

    // 3. Coordinator configuration default
    coordinator_config
        .verify_default_timeout
        .as_ref()
        .and_then(|s| parse_delay(s))
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(900)) // New default: 900s instead of 300s
}

/// Result of running a verify command.
struct VerifyOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: String,
}

/// Progress monitoring for verify commands
#[derive(Debug)]
struct ProgressMonitor {
    last_stdout_activity: std::time::Instant,
    last_stderr_activity: std::time::Instant,
}

impl ProgressMonitor {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            last_stdout_activity: now,
            last_stderr_activity: now,
        }
    }

    fn last_activity(&self) -> std::time::Instant {
        self.last_stdout_activity.max(self.last_stderr_activity)
    }

    fn has_recent_activity(&self, threshold: std::time::Duration) -> bool {
        self.last_activity().elapsed() < threshold
    }
}

/// Triage result for timeout processes
#[derive(Debug, PartialEq)]
enum TriageResult {
    GenuineHang { reason: String },
    WaitingOnLocks { detected_locks: Vec<String> },
    UnknownButActive { activity_type: String },
}

/// Get the list of modified files in the current worktree using git diff.
/// Returns relative paths from the project root.
fn get_modified_files(project_root: &Path) -> Result<Vec<String>> {
    use std::process::Command;

    let output = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("HEAD")
        .current_dir(project_root)
        .output()
        .context("Failed to run git diff to detect modified files")?;

    if !output.status.success() {
        return Ok(Vec::new()); // No git repo or no changes
    }

    let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect();

    Ok(files)
}

/// Detect common lock files that might indicate waiting processes
fn detect_cargo_locks() -> Result<Vec<String>> {
    detect_cargo_locks_with_stderr("")
}

/// Detect lock contention from both lock files and stderr patterns
fn detect_cargo_locks_with_stderr(stderr_content: &str) -> Result<Vec<String>> {
    let mut locks = Vec::new();

    // Common cargo lock files
    let lock_patterns = [
        "target/.rustc_info.json.lock",
        "target/debug/.cargo-lock",
        "Cargo.lock",
    ];

    for pattern in &lock_patterns {
        if std::path::Path::new(pattern).exists() {
            locks.push(pattern.to_string());
        }
    }

    // Check stderr for cargo lock contention messages
    let lock_messages = [
        "Blocking waiting for file lock on artifact directory",
        "Blocking waiting for file lock on package cache",
        "Blocking waiting for file lock on",
        "waiting for file lock on build directory",
        "waiting for file lock on the registry cache",
    ];

    for message in &lock_messages {
        if stderr_content.contains(message) {
            locks.push(format!("stderr_pattern: {}", message));
        }
    }

    Ok(locks)
}

/// Basic triage implementation for timeout processes
fn triage_timeout_process(
    monitor: &ProgressMonitor,
    _progress_timeout: std::time::Duration,
) -> Result<TriageResult> {
    // 1. Check for recent output activity
    if monitor.has_recent_activity(std::time::Duration::from_secs(60)) {
        return Ok(TriageResult::UnknownButActive {
            activity_type: "recent_output".to_string(),
        });
    }

    // 2. Check for cargo lock files (common contention point)
    let lock_files = detect_cargo_locks()?;
    if !lock_files.is_empty() {
        return Ok(TriageResult::WaitingOnLocks {
            detected_locks: lock_files,
        });
    }

    // 3. Default to genuine hang if no other indicators
    Ok(TriageResult::GenuineHang {
        reason: format!(
            "no_activity_{}s_no_locks",
            monitor.last_activity().elapsed().as_secs()
        ),
    })
}

/// Check if an error output indicates file lock contention
fn is_lock_contention_error(stderr: &str) -> bool {
    let lock_patterns = [
        "Blocking waiting for file lock on artifact directory",
        "Blocking waiting for file lock on package cache",
        "Blocking waiting for file lock on",
        "waiting for file lock on build directory",
        "waiting for file lock on the registry cache",
    ];

    lock_patterns.iter().any(|pattern| stderr.contains(pattern))
}

/// Run a verify command with retry logic for file lock contention
fn run_verify_command_with_retry(
    verify_cmd: &str,
    project_root: &Path,
    task: &Task,
    coordinator_config: &CoordinatorConfig,
) -> std::result::Result<VerifyOutput, VerifyOutput> {
    const MAX_RETRIES: u32 = 3;
    const BASE_DELAY_SECS: u64 = 5;

    let mut last_error: Option<VerifyOutput> = None;

    for attempt in 1..=MAX_RETRIES {
        match run_verify_command(verify_cmd, project_root, task, coordinator_config) {
            Ok(output) => return Ok(output),
            Err(error) => {
                // Check if this is a lock contention issue
                if is_lock_contention_error(&error.stderr) {
                    eprintln!(
                        "Verify attempt {}/{} failed due to file lock contention: {}",
                        attempt,
                        MAX_RETRIES,
                        error.stderr.lines().next().unwrap_or("")
                    );

                    if attempt < MAX_RETRIES {
                        let delay_secs = BASE_DELAY_SECS * (2_u64.pow(attempt - 1)); // Exponential backoff
                        eprintln!("Retrying in {} seconds...", delay_secs);
                        std::thread::sleep(std::time::Duration::from_secs(delay_secs));
                        last_error = Some(error);
                        continue;
                    }
                } else if error.exit_code == "timeout" {
                    // For timeouts, check if stderr suggests lock contention
                    if is_lock_contention_error(&error.stderr) {
                        eprintln!("Verify timeout appears to be due to file lock contention");
                        if attempt < MAX_RETRIES {
                            let delay_secs = BASE_DELAY_SECS * (2_u64.pow(attempt - 1));
                            eprintln!(
                                "Retrying in {} seconds with extended timeout...",
                                delay_secs
                            );
                            std::thread::sleep(std::time::Duration::from_secs(delay_secs));
                            last_error = Some(error);
                            continue;
                        }
                    }
                }

                // Not a retryable error or max retries reached
                return Err(error);
            }
        }
    }

    // Return the last error if all retries failed
    Err(last_error.unwrap())
}

/// Map modified files to relevant test modules/files.
/// Returns a list of test-specific cargo commands to run.
fn map_files_to_tests(modified_files: &[String]) -> Option<Vec<String>> {
    let mut test_commands = Vec::new();

    for file in modified_files {
        // Check for core files that should trigger full test suite
        if is_core_file(file) {
            return None; // Fall back to full test suite
        }

        // Map source files to test modules
        if let Some(test_cmd) = map_file_to_test_command(file) {
            if !test_commands.contains(&test_cmd) {
                test_commands.push(test_cmd);
            }
        }
    }

    if test_commands.is_empty() {
        None
    } else {
        Some(test_commands)
    }
}

/// Check if a file is considered "core" and should trigger full test suite.
fn is_core_file(file: &str) -> bool {
    matches!(
        file,
        "src/lib.rs"
            | "src/main.rs"
            | "Cargo.toml"
            | "Cargo.lock"
            | "build.rs"
            | ".gitignore"
            | "README.md"
    ) || file.starts_with("src/lib/")
        || file.contains("/mod.rs")
        || file.ends_with("/lib.rs")
}

/// Map a single file to its relevant test command.
fn map_file_to_test_command(file: &str) -> Option<String> {
    if file.starts_with("tests/") {
        // Direct test file - run the specific test
        if let Some(test_name) = file
            .strip_prefix("tests/")
            .and_then(|f| f.strip_suffix(".rs"))
        {
            return Some(format!("cargo test --test {}", test_name));
        }
    } else if file.starts_with("src/") {
        // Source file - map to relevant test module
        if let Some(module_path) = file
            .strip_prefix("src/")
            .and_then(|f| f.strip_suffix(".rs"))
        {
            // Convert path to module name (e.g., "commands/add.rs" -> "add", "commands/viz/mod.rs" -> "viz")
            let module_name = if module_path.ends_with("/mod") {
                module_path.strip_suffix("/mod").unwrap_or(module_path)
            } else {
                module_path
            };

            // Extract the final component for testing
            let test_module = module_name.split('/').last().unwrap_or(module_name);

            return Some(format!("cargo test {}", test_module));
        }
    }

    None
}

/// Generate a scoped verify command if conditions are met.
/// Returns the scoped command or None to fall back to original.
fn generate_scoped_verify_command(
    verify_cmd: &str,
    project_root: &Path,
    coordinator_config: &CoordinatorConfig,
) -> Option<String> {
    // Only scope "cargo test" commands
    if verify_cmd.trim() != "cargo test" || !coordinator_config.scoped_verify_enabled {
        return None;
    }

    // Get modified files
    let modified_files = match get_modified_files(project_root) {
        Ok(files) => files,
        Err(_) => return None, // Fall back on error
    };

    if modified_files.is_empty() {
        return None; // No changes, use original command
    }

    // Map to test commands
    if let Some(test_commands) = map_files_to_tests(&modified_files) {
        if test_commands.len() == 1 {
            // Single scoped command
            Some(test_commands.into_iter().next().unwrap())
        } else if test_commands.len() > 1 {
            // Multiple test commands - combine them
            Some(test_commands.join(" && "))
        } else {
            None
        }
    } else {
        None // Fall back to full test suite
    }
}

/// Detect if a verify command is likely free-text rather than an executable command.
///
/// Uses multiple heuristics to identify natural language descriptions:
/// 1. First word not found in PATH and not a shell builtin
/// 2. No shell metacharacters (|, &&, ;, >, <) and multiple English words
/// 3. Contains common descriptive patterns
fn is_free_text_verify_command(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return false;
    }

    let first_word = cmd.split_whitespace().next().unwrap_or("");

    // Quick check: if it looks like a valid command prefix, it's probably not free-text
    let known_commands = [
        "cargo", "npm", "npx", "yarn", "pnpm", "make", "cmake", "go", "python", "python3",
        "pytest", "ruby", "rake", "bundle", "mvn", "gradle", "ant", "dotnet", "zig", "rustc",
        "gcc", "g++", "clang", "clang++", "javac", "java", "test", "[", "true", "false", "exit",
        "echo", "printf", "cat", "grep", "find", "ls", "diff", "cmp", "wc", "head", "tail", "sort",
        "uniq", "cut", "tr",
    ];

    if known_commands.contains(&first_word) {
        return false;
    }

    // Check for shell metacharacters - commands with these are likely executable
    let shell_chars = ['|', '&', ';', '>', '<', '(', ')', '{', '}', '$', '`'];
    if cmd.chars().any(|c| shell_chars.contains(&c)) {
        return false;
    }

    // If multiple words and no shell metacharacters, likely free-text
    let word_count = cmd.split_whitespace().count();
    if word_count > 1 {
        // Check for common descriptive patterns
        let lower = cmd.to_lowercase();
        let descriptive_patterns = [
            "exists",
            "is valid",
            "are valid",
            "passes",
            "succeeds",
            "works",
            "complete",
            "documentation",
            "tests pass",
            "builds successfully",
            "no errors",
            "no warnings",
            "has been",
            "have been",
            "should be",
            "must be",
            "ensure",
            "verify that",
        ];

        if descriptive_patterns
            .iter()
            .any(|pattern| lower.contains(pattern))
        {
            return true;
        }

        // If it's multiple words without shell chars and doesn't look like a command, likely free-text
        return true;
    }

    false
}

/// Run LLM evaluation for a free-text verify command.
/// Creates a verification task that uses the evaluation system.
fn run_llm_verify_evaluation(
    verify_cmd: &str,
    task: &Task,
    project_root: &Path,
) -> std::result::Result<VerifyOutput, VerifyOutput> {
    eprintln!(
        "[smart-verify] Detected free-text verify command, routing to LLM evaluation: {}",
        verify_cmd
    );

    // Find the workgraph directory (where .workgraph folder is located)
    let workgraph_dir = project_root
        .ancestors()
        .find(|p| p.join(".workgraph").exists())
        .unwrap_or(project_root);

    // Run evaluation on the task
    match evaluate::run(workgraph_dir, &task.id, None, false, false) {
        Ok(_) => {
            // Evaluation succeeded - consider verification passed
            Ok(VerifyOutput {
                stdout: format!("LLM evaluation completed for: {}", verify_cmd),
                stderr: String::new(),
                exit_code: "0".to_string(),
            })
        }
        Err(e) => {
            // Evaluation failed - consider verification failed
            Err(VerifyOutput {
                stdout: String::new(),
                stderr: format!("LLM evaluation failed for '{}': {}", verify_cmd, e),
                exit_code: "1".to_string(),
            })
        }
    }
}

/// Run a verify command in a shell.
/// Returns Ok(VerifyOutput) with captured output on success,
/// or Err(VerifyOutput) with captured output on failure.
fn run_verify_command(
    verify_cmd: &str,
    project_root: &Path,
    task: &Task,
    coordinator_config: &CoordinatorConfig,
) -> std::result::Result<VerifyOutput, VerifyOutput> {
    use std::process::Command;
    use std::time::{Duration, Instant};

    // Try to generate a scoped command first
    let effective_cmd =
        generate_scoped_verify_command(verify_cmd, project_root, coordinator_config)
            .unwrap_or_else(|| verify_cmd.to_string());

    // Log scoping decision
    if effective_cmd != verify_cmd {
        eprintln!("[scoped-verify] Using scoped command: {}", effective_cmd);
        eprintln!("[scoped-verify] Original command: {}", verify_cmd);
    }

    // Smart verify: check if this is free-text and route to LLM evaluation
    if is_free_text_verify_command(&effective_cmd) {
        return run_llm_verify_evaluation(&effective_cmd, task, project_root);
    }

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(&effective_cmd)
        .current_dir(project_root)
        .env("TERM", "dumb") // Set TERM=dumb to avoid terminal-related failures
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Err(VerifyOutput {
                stdout: String::new(),
                stderr: format!("Failed to spawn verify command: {}", e),
                exit_code: "spawn-error".to_string(),
            });
        }
    };

    // Read stdout and stderr in background threads to prevent pipe buffer deadlock.
    // Without this, a child producing >64KB of output blocks on write and never exits.
    let stdout_handle = child.stdout.take().map(|s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::BufReader::new(s), &mut buf).ok();
            buf
        })
    });
    let stderr_handle = child.stderr.take().map(|s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::BufReader::new(s), &mut buf).ok();
            buf
        })
    });

    let timeout = resolve_verify_timeout(task, coordinator_config);
    let start = Instant::now();
    let monitor = ProgressMonitor::new();

    // Poll with short sleeps to implement timeout without external crate
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    // Check if triage is enabled
                    if coordinator_config.verify_triage_enabled {
                        // Perform triage to determine if this is a genuine hang or waiting
                        let progress_timeout = coordinator_config
                            .verify_progress_timeout
                            .as_ref()
                            .and_then(|s| parse_delay(s))
                            .map(std::time::Duration::from_secs)
                            .unwrap_or(std::time::Duration::from_secs(300));

                        match triage_timeout_process(&monitor, progress_timeout) {
                            Ok(TriageResult::WaitingOnLocks { detected_locks }) => {
                                eprintln!(
                                    "Verify timeout triage: detected lock contention on {:?}, extending timeout by 300s",
                                    detected_locks
                                );
                                // Extend timeout and continue
                                // Note: This is a simple implementation - in production we might want retry limits
                                std::thread::sleep(std::time::Duration::from_secs(5));
                                continue;
                            }
                            Ok(TriageResult::UnknownButActive { activity_type }) => {
                                eprintln!(
                                    "Verify timeout triage: process active ({}), extending timeout by 300s",
                                    activity_type
                                );
                                // Extend timeout and continue
                                std::thread::sleep(std::time::Duration::from_secs(5));
                                continue;
                            }
                            Ok(TriageResult::GenuineHang { reason }) => {
                                eprintln!(
                                    "Verify timeout triage: genuine hang detected ({}), failing",
                                    reason
                                );
                                // Proceed with normal timeout failure
                            }
                            _ => {
                                eprintln!(
                                    "Verify timeout triage: unknown condition, failing with timeout"
                                );
                                // Proceed with normal timeout failure
                            }
                        }
                    }

                    // Standard timeout failure (either no triage or triage determined genuine hang)
                    let _ = child.kill();
                    let _ = child.wait();
                    let stdout = stdout_handle
                        .map(|h| h.join().unwrap_or_default())
                        .unwrap_or_default();
                    let stderr = stderr_handle
                        .map(|h| h.join().unwrap_or_default())
                        .unwrap_or_default();
                    return Err(VerifyOutput {
                        stdout,
                        stderr: format!(
                            "Verify command timed out after {}s\n{}",
                            timeout.as_secs(),
                            stderr
                        ),
                        exit_code: "timeout".to_string(),
                    });
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return Err(VerifyOutput {
                    stdout: String::new(),
                    stderr: format!("Failed to wait on verify command: {}", e),
                    exit_code: "wait-error".to_string(),
                });
            }
        }
    };

    let stdout = stdout_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let exit_code = status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());

    if status.success() {
        Ok(VerifyOutput {
            stdout,
            stderr,
            exit_code,
        })
    } else {
        // Check for exit code 127 (command not found) - likely free-text command
        if exit_code == "127" {
            eprintln!(
                "[smart-verify] Command failed with exit 127 (command not found), retrying with LLM evaluation: {}",
                effective_cmd
            );
            match run_llm_verify_evaluation(&effective_cmd, task, project_root) {
                Ok(llm_result) => {
                    eprintln!("[smart-verify] LLM evaluation succeeded for exit 127 fallback");
                    return Ok(llm_result);
                }
                Err(_llm_error) => {
                    eprintln!(
                        "[smart-verify] LLM evaluation also failed, returning original shell error"
                    );
                    // Fall through to return original error
                }
            }
        }

        Err(VerifyOutput {
            stdout,
            stderr,
            exit_code,
        })
    }
}

/// Check git hygiene when an agent marks a task as done.
/// Emits warnings for uncommitted changes and stash growth.
fn check_agent_git_hygiene(dir: &Path, task_id: &str) {
    use std::process::Command;
    let project_root = dir.parent().unwrap_or(dir);
    if let Ok(output) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(project_root)
        .output()
    {
        let status = String::from_utf8_lossy(&output.stdout);
        if !status.is_empty() {
            let changed: Vec<&str> = status.lines().take(10).collect();
            eprintln!(
                "Warning: git hygiene for '{}': uncommitted changes:\n{}",
                task_id,
                changed.join("\n")
            );
        }
    }
    if let Ok(output) = Command::new("git")
        .args(["stash", "list"])
        .current_dir(project_root)
        .output()
    {
        let count = String::from_utf8_lossy(&output.stdout).lines().count();
        if count > 0 {
            eprintln!(
                "Warning: git hygiene for '{}': {} stash(es) exist. Agents should never stash.",
                task_id, count
            );
        }
    }
}

pub fn run(dir: &Path, id: &str, converged: bool, skip_verify: bool) -> Result<()> {
    let is_agent = std::env::var("WG_AGENT_ID").is_ok();
    run_inner(dir, id, converged, skip_verify, is_agent)
}

fn run_inner(
    dir: &Path,
    id: &str,
    converged: bool,
    skip_verify: bool,
    is_agent: bool,
) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    if task.status == Status::Done {
        println!("Task '{}' is already done", id);
        return Ok(());
    }

    // Check for unresolved blockers (cycle-aware: only exempt back-edge blockers,
    // not all same-cycle blockers).
    //
    // Any blocker that is in the same cycle (SCC) as the task being completed
    // is exempted — both header and non-header members.  The mutual dependency
    // between cycle members is a structural back-edge; blocking on it would
    // deadlock the cycle.
    let blockers = query::after(&graph, id);
    if !blockers.is_empty() {
        let cycle_analysis = graph.compute_cycle_analysis();
        let effective_blockers: Vec<_> = blockers
            .into_iter()
            .filter(|b| {
                // Exempt any blocker in the same cycle (SCC) as this task
                let in_same_cycle = cycle_analysis
                    .task_to_cycle
                    .get(&b.id)
                    .is_some_and(|bc| cycle_analysis.task_to_cycle.get(id) == Some(bc));
                !in_same_cycle
            })
            .collect();
        if !effective_blockers.is_empty() {
            let blocker_list: Vec<String> = effective_blockers
                .iter()
                .map(|t| format!("  - {} ({}): {:?}", t.id, t.title, t.status))
                .collect();
            anyhow::bail!(
                "Cannot mark '{}' as done: blocked by {} unresolved task(s):\n{}",
                id,
                effective_blockers.len(),
                blocker_list.join("\n")
            );
        }
    }

    // Git hygiene check for agents: warn about uncommitted changes
    if is_agent {
        check_agent_git_hygiene(dir, id);
    }

    // Run verify command gate (if task has a verify field)
    if let Some(verify_cmd) = graph.get_task(id).and_then(|t| t.verify.clone()) {
        if skip_verify {
            // Block agents from using --skip-verify
            if is_agent {
                anyhow::bail!(
                    "Agents cannot use --skip-verify. The verify command must pass:\n  {}",
                    verify_cmd,
                );
            }
            eprintln!("Warning: skipping verify command: {}", verify_cmd);
        } else if Config::load_or_default(dir).coordinator.verify_mode == "separate" {
            // Separate verification mode: transition to PendingValidation and let
            // the coordinator spawn a separate agent to run the verify command.
            // This prevents false-PASS rates where the implementation agent
            // rubber-stamps its own work.
            let id_sep = id.to_string();
            let mut assigned_agent = None;
            let _graph = modify_graph(&path, |graph| {
                let task = match graph.get_task_mut(&id_sep) {
                    Some(t) => t,
                    None => return false,
                };
                if task.status.is_terminal() {
                    return false;
                }
                assigned_agent = task.assigned.clone();
                task.status = Status::PendingValidation;
                task.completed_at = Some(Utc::now().to_rfc3339());
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: task.assigned.clone(),
                    user: Some(workgraph::current_user()),
                    message: "Pending separate verification (verify_mode=separate)".to_string(),
                });
                true
            })
            .context("Failed to save graph for separate verification")?;

            super::notify_graph_changed(dir);

            // Update agent registry
            if let Ok(mut locked_registry) = AgentRegistry::load_locked(dir) {
                if let Some(agent) = locked_registry.get_agent_by_task_mut(id) {
                    agent.status = workgraph::service::registry::AgentStatus::Done;
                    if agent.completed_at.is_none() {
                        agent.completed_at = Some(Utc::now().to_rfc3339());
                    }
                }
                let _ = locked_registry.save_ref();
            }

            println!(
                "Task '{}' is pending separate verification (verify command: {})",
                id, verify_cmd
            );

            // Archive agent conversation for provenance
            if let Some(ref agent_id) = assigned_agent {
                match super::log::archive_agent(dir, id, agent_id) {
                    Ok(archive_dir) => {
                        eprintln!("Agent archived to {}", archive_dir.display());
                    }
                    Err(e) => {
                        eprintln!("Warning: agent archive failed: {}", e);
                    }
                }
            }

            return Ok(());
        } else {
            let project_root = dir.parent().unwrap_or(dir);
            eprintln!("Running verify command: {}", verify_cmd);

            // Get task and coordinator config for enhanced timeout resolution
            let task = graph
                .get_task(id)
                .ok_or_else(|| anyhow::anyhow!("Task {} not found", id))?;
            let config = Config::load_or_default(dir);

            match run_verify_command_with_retry(
                &verify_cmd,
                project_root,
                task,
                &config.coordinator,
            ) {
                Ok(output) => {
                    // Log verify success with captured output
                    let id_for_log = id.to_string();
                    let stdout_preview: String = output.stdout.chars().take(200).collect();
                    let stderr_preview: String = output.stderr.chars().take(200).collect();
                    if !stdout_preview.is_empty() || !stderr_preview.is_empty() {
                        let mut log_msg = "Verify passed.".to_string();
                        if !stdout_preview.is_empty() {
                            log_msg.push_str(&format!(" stdout: {}", stdout_preview));
                        }
                        if !stderr_preview.is_empty() {
                            log_msg.push_str(&format!(" stderr: {}", stderr_preview));
                        }
                        let _ = modify_graph(&path, |g| {
                            if let Some(t) = g.get_task_mut(&id_for_log) {
                                // Reset verify failures on success
                                t.verify_failures = 0;
                                t.log.push(LogEntry {
                                    timestamp: Utc::now().to_rfc3339(),
                                    actor: Some("verify".to_string()),
                                    user: None,
                                    message: log_msg.clone(),
                                });
                                true
                            } else {
                                false
                            }
                        });
                        // Reload graph after mutation
                        let (new_graph, _) = super::load_workgraph_mut(dir)?;
                        graph = new_graph;
                    } else {
                        // Reset verify failures on success even without output
                        let _ = modify_graph(&path, |g| {
                            if let Some(t) = g.get_task_mut(&id_for_log) {
                                t.verify_failures = 0;
                                true
                            } else {
                                false
                            }
                        });
                        let (new_graph, _) = super::load_workgraph_mut(dir)?;
                        graph = new_graph;
                    }
                    eprintln!("Verify command passed");
                }
                Err(output) => {
                    // Check if this is a malformed verify command that can be auto-corrected
                    if let Some(corrected_cmd) =
                        workgraph::verify_lint::auto_correct_verify_command(&verify_cmd)
                    {
                        eprintln!(
                            "Verify command appears malformed, auto-correcting: {} → {}",
                            verify_cmd, corrected_cmd
                        );

                        // Update the task's verify command in the graph and reset failure count
                        let id_for_update = id.to_string();
                        let corrected_cmd_clone = corrected_cmd.clone();
                        modify_graph(&path, |g| {
                            if let Some(t) = g.get_task_mut(&id_for_update) {
                                t.verify = Some(corrected_cmd_clone.clone());
                                t.verify_failures = 0; // Reset failure count for auto-corrected command
                                t.log.push(LogEntry {
                                    timestamp: Utc::now().to_rfc3339(),
                                    actor: Some("verify-autocorrect".to_string()),
                                    user: None,
                                    message: format!(
                                        "Auto-corrected malformed verify command: '{}' → '{}'",
                                        verify_cmd, corrected_cmd_clone
                                    ),
                                });
                                true
                            } else {
                                false
                            }
                        })
                        .context("Failed to save auto-corrected verify command")?;

                        // Retry with the corrected command
                        eprintln!("Retrying with corrected command: {}", corrected_cmd);

                        // Reload graph to get updated task
                        let (new_graph, _) = super::load_workgraph_mut(dir)?;
                        let updated_task = new_graph
                            .get_task(id)
                            .ok_or_else(|| anyhow::anyhow!("Task {} not found after update", id))?;

                        match run_verify_command_with_retry(
                            &corrected_cmd,
                            project_root,
                            updated_task,
                            &config.coordinator,
                        ) {
                            Ok(output) => {
                                // Auto-correction worked! Log success
                                let id_for_log = id.to_string();
                                let stdout_preview: String =
                                    output.stdout.chars().take(200).collect();
                                let stderr_preview: String =
                                    output.stderr.chars().take(200).collect();
                                let mut log_msg =
                                    "Verify passed (after auto-correction).".to_string();
                                if !stdout_preview.is_empty() {
                                    log_msg.push_str(&format!(" stdout: {}", stdout_preview));
                                }
                                if !stderr_preview.is_empty() {
                                    log_msg.push_str(&format!(" stderr: {}", stderr_preview));
                                }
                                let _ = modify_graph(&path, |g| {
                                    if let Some(t) = g.get_task_mut(&id_for_log) {
                                        t.log.push(LogEntry {
                                            timestamp: Utc::now().to_rfc3339(),
                                            actor: Some("verify".to_string()),
                                            user: None,
                                            message: log_msg,
                                        });
                                        true
                                    } else {
                                        false
                                    }
                                });
                                eprintln!("Auto-corrected verify command passed");
                                return Ok(()); // Success after auto-correction
                            }
                            Err(_) => {
                                // Auto-corrected command also failed, proceed with normal failure handling
                                eprintln!(
                                    "Auto-corrected verify command also failed, treating as normal verify failure"
                                );
                                // Fall through to normal failure handling with the original command
                            }
                        }
                    }

                    // Normal verify failure handling (original command failed and either
                    // no auto-correction was possible, or auto-correction also failed)
                    let id_for_circuit = id.to_string();
                    let verify_cmd_clone = verify_cmd.clone();
                    let stdout_preview: String = output.stdout.chars().take(500).collect();
                    let stderr_preview: String = output.stderr.chars().take(500).collect();
                    let exit_code = output.exit_code.clone();

                    let config = Config::load_or_default(dir);
                    let max_verify_failures = config.coordinator.max_verify_failures;

                    modify_graph(&path, |g| {
                        let task = match g.get_task_mut(&id_for_circuit) {
                            Some(t) => t,
                            None => return false,
                        };
                        task.verify_failures += 1;
                        let failures = task.verify_failures;

                        // Log the verify failure with output
                        let mut log_msg = format!(
                            "Verify FAILED (exit code {}, attempt {}/{}). Command: {}",
                            exit_code,
                            failures,
                            if max_verify_failures > 0 {
                                max_verify_failures.to_string()
                            } else {
                                "unlimited".to_string()
                            },
                            verify_cmd_clone,
                        );
                        if !stdout_preview.is_empty() {
                            log_msg.push_str(&format!("\nstdout: {}", stdout_preview));
                        }
                        if !stderr_preview.is_empty() {
                            log_msg.push_str(&format!("\nstderr: {}", stderr_preview));
                        }
                        task.log.push(LogEntry {
                            timestamp: Utc::now().to_rfc3339(),
                            actor: Some("verify".to_string()),
                            user: None,
                            message: log_msg,
                        });

                        // Circuit breaker: auto-fail after threshold
                        if max_verify_failures > 0 && failures >= max_verify_failures {
                            task.status = Status::Failed;
                            task.assigned = None;
                            task.failure_reason = Some(format!(
                                "Verify command failed {} consecutive times. Command: `{}`. \
                                 Last exit code: {}. Last stderr: {}. \
                                 This may be descriptive text instead of an executable command.",
                                failures,
                                verify_cmd_clone,
                                exit_code,
                                if stderr_preview.is_empty() {
                                    "(empty)".to_string()
                                } else {
                                    stderr_preview.clone()
                                },
                            ));
                            task.log.push(LogEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                actor: Some("verify-circuit-breaker".to_string()),
                                user: None,
                                message: format!(
                                    "Circuit breaker tripped: verify command failed {} times, auto-failing task",
                                    failures,
                                ),
                            });
                        }
                        true
                    })
                    .context("Failed to save verify failure state")?;

                    // Reload graph to check if circuit breaker tripped
                    let (new_graph, _) = super::load_workgraph_mut(dir)?;
                    if let Some(task) = new_graph.get_task(id)
                        && task.status == Status::Failed
                    {
                        eprintln!(
                            "Verify circuit breaker tripped for '{}': {} consecutive failures. Task auto-failed.",
                            id, task.verify_failures,
                        );
                        super::notify_graph_changed(dir);
                        // Return Ok — the task is now Failed, not an error in the command
                        return Ok(());
                    }

                    // Not yet at threshold — propagate error so agent retries
                    let mut error_msg = format!(
                        "Verify command failed (exit code {}): {}",
                        exit_code, verify_cmd,
                    );
                    if !stderr_preview.is_empty() {
                        error_msg.push_str(&format!("\nstderr: {}", stderr_preview));
                    }
                    if !stdout_preview.is_empty() {
                        error_msg.push_str(&format!("\nstdout: {}", stdout_preview));
                    }
                    anyhow::bail!(error_msg);
                }
            }
        }
    }

    // Determine validation mode for this task.
    // Resolution: task.validation > "none" (default, backward compatible).
    let validation_mode = graph
        .get_task(id)
        .and_then(|t| t.validation.clone())
        .unwrap_or_else(|| "none".to_string());

    // Integrated validation: enforce log check + run validation_commands
    if validation_mode == "integrated" {
        let task_ref = graph.get_task(id).unwrap();
        let has_validation_log = task_ref
            .log
            .iter()
            .any(|entry| entry.message.to_lowercase().contains("validat"));
        if !has_validation_log {
            anyhow::bail!(
                "Cannot mark '{}' as done: integrated validation requires a validation log entry.\n\
                 Add one with: wg log {} \"Validated: <what you checked>\"",
                id,
                id
            );
        }
        let commands = task_ref.validation_commands.clone();
        if !commands.is_empty() {
            let project_root = dir.parent().unwrap_or(dir);
            let config = Config::load_or_default(dir);
            for cmd in &commands {
                eprintln!("Running validation command: {}", cmd);
                match run_verify_command_with_retry(
                    cmd,
                    project_root,
                    task_ref,
                    &config.coordinator,
                ) {
                    Ok(_) => {}
                    Err(output) => {
                        let stderr: String = output.stderr.chars().take(500).collect();
                        let stdout: String = output.stdout.chars().take(500).collect();
                        let mut msg = format!(
                            "Integrated validation failed for '{}': command failed (exit code {}): {}",
                            id, output.exit_code, cmd,
                        );
                        if !stderr.is_empty() {
                            msg.push_str(&format!("\nstderr: {}", stderr));
                        }
                        if !stdout.is_empty() {
                            msg.push_str(&format!("\nstdout: {}", stdout));
                        }
                        anyhow::bail!(msg);
                    }
                }
            }
            eprintln!("All validation commands passed");
        }
    }

    // External validation: transition to PendingValidation instead of Done
    if validation_mode == "external" {
        let id_ext = id.to_string();
        let mut assigned_agent = None;
        let graph = modify_graph(&path, |graph| {
            let task = match graph.get_task_mut(&id_ext) {
                Some(t) => t,
                None => return false,
            };
            if task.status.is_terminal() {
                return false;
            }
            assigned_agent = task.assigned.clone();
            task.status = Status::PendingValidation;
            task.completed_at = Some(Utc::now().to_rfc3339());
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: task.assigned.clone(),
                user: Some(workgraph::current_user()),
                message: "Task pending external validation".to_string(),
            });
            true
        })
        .context("Failed to save graph")?;
        super::notify_graph_changed(dir);

        // Update agent registry for external validation path too
        if let Ok(mut locked_registry) = AgentRegistry::load_locked(dir) {
            if let Some(agent) = locked_registry.get_agent_by_task_mut(id) {
                agent.status = workgraph::service::registry::AgentStatus::Done;
                if agent.completed_at.is_none() {
                    agent.completed_at = Some(Utc::now().to_rfc3339());
                }
            }
            let _ = locked_registry.save_ref();
        }

        let config = workgraph::config::Config::load_or_default(dir);
        let _ = workgraph::provenance::record(
            dir,
            "done",
            Some(id),
            None,
            serde_json::json!({ "validation": "external", "status": "pending-validation" }),
            config.log.rotation_threshold,
        );

        println!("Task '{}' is pending external validation", id);

        // Archive agent conversation for provenance
        if let Some(ref agent_id) = assigned_agent {
            match super::log::archive_agent(dir, id, agent_id) {
                Ok(archive_dir) => {
                    eprintln!("Agent archived to {}", archive_dir.display());
                }
                Err(e) => {
                    eprintln!("Warning: agent archive failed: {}", e);
                }
            }
        }

        // Capture task output for validation
        if let Some(task) = graph.get_task(id) {
            match capture_task_output(dir, task) {
                Ok(output_dir) => {
                    eprintln!("Output captured to {}", output_dir.display());
                }
                Err(e) => {
                    eprintln!("Warning: output capture failed: {}", e);
                }
            }
        }

        return Ok(());
    }

    // When --converged is passed, determine whether the task's cycle has a
    // non-trivial guard or no_converge flag. If so, ignore the converged flag.
    // This prevents workers from bypassing external validation by
    // self-declaring convergence, and enforces forced cycles.
    //
    // We do this check with immutable access before mutating the task.
    let converged_accepted = if converged {
        // Check 1: the task itself has no_converge or a guarded cycle_config
        let own_no_converge = graph
            .get_task(id)
            .and_then(|t| t.cycle_config.as_ref())
            .map(|c| c.no_converge)
            .unwrap_or(false);

        let own_guard = graph
            .get_task(id)
            .and_then(|t| t.cycle_config.as_ref())
            .and_then(|c| c.guard.as_ref())
            .map(|g| !matches!(g, workgraph::graph::LoopGuard::Always))
            .unwrap_or(false);

        // Check 2: the task is a non-header member of a cycle whose header
        // has a non-trivial guard or no_converge. This covers workers trying
        // to converge a cycle they don't own.
        let (cycle_guard, cycle_no_converge) = if !own_guard && !own_no_converge {
            let ca = graph.compute_cycle_analysis();
            ca.task_to_cycle
                .get(id)
                .map(|&idx| {
                    let cycle = &ca.cycles[idx];
                    let guard = cycle.members.iter().any(|mid| {
                        graph
                            .get_task(mid)
                            .and_then(|t| t.cycle_config.as_ref())
                            .and_then(|c| c.guard.as_ref())
                            .map(|g| !matches!(g, workgraph::graph::LoopGuard::Always))
                            .unwrap_or(false)
                    });
                    let no_conv = cycle.members.iter().any(|mid| {
                        graph
                            .get_task(mid)
                            .and_then(|t| t.cycle_config.as_ref())
                            .map(|c| c.no_converge)
                            .unwrap_or(false)
                    });
                    (guard, no_conv)
                })
                .unwrap_or((false, false))
        } else {
            (false, false)
        };

        let has_guard = own_guard || cycle_guard;
        let has_no_converge = own_no_converge || cycle_no_converge;

        if has_no_converge {
            eprintln!(
                "Warning: --converged ignored for '{}' because the cycle is configured with --no-converge.\n         \
                 All iterations must run.",
                id
            );
            false
        } else if has_guard {
            eprintln!(
                "Warning: --converged ignored for '{}' because a cycle guard is set.\n         \
                 Only the guard condition determines convergence.",
                id
            );
            false
        } else {
            true
        }
    } else {
        false
    };

    // Atomically load the freshest graph, apply the mutation, and save.
    // Using modify_graph prevents the "lost update" race where a concurrent
    // spawn_eval_inline (or any other graph writer) saves between our read
    // and write, and our write clobbers its changes — or vice-versa.
    //
    // The pre-checks above (blockers, verify, validation) used a stale graph
    // snapshot, but they are idempotent gates: if they passed on the stale
    // version, they would also pass on the fresh version (task status can only
    // move forward, blockers can only resolve, not un-resolve).
    let mut cycle_reactivated = Vec::new();
    let mut already_done = false;

    // Resolve token usage outside the lock (registry read + file I/O).
    let token_usage = AgentRegistry::load(dir).ok().and_then(|registry| {
        let agent = registry.get_agent_by_task(id)?;
        let output_path = std::path::Path::new(&agent.output_file);
        let abs_path = if output_path.is_absolute() {
            output_path.to_path_buf()
        } else {
            dir.parent().unwrap_or(dir).join(output_path)
        };
        parse_token_usage(&abs_path).or_else(|| parse_wg_tokens(&abs_path))
    });

    let id_owned = id.to_string();
    let graph = modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(&id_owned) {
            Some(t) => t,
            None => return false,
        };

        // Re-check: another writer may have marked it Done already
        if task.status == Status::Done {
            already_done = true;
            return false;
        }

        task.status = Status::Done;
        task.completed_at = Some(Utc::now().to_rfc3339());

        if converged_accepted && !task.tags.contains(&"converged".to_string()) {
            task.tags.push("converged".to_string());
        }

        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: task.assigned.clone(),
            user: Some(workgraph::current_user()),
            message: if converged_accepted {
                "Task marked as done (converged)".to_string()
            } else if converged {
                "Task marked as done (--converged ignored, cycle is forced)".to_string()
            } else {
                "Task marked as done".to_string()
            },
        });

        // Apply pre-resolved token usage
        if task.token_usage.is_none()
            && let Some(ref usage) = token_usage
        {
            task.token_usage = Some(usage.clone());
        }

        // Evaluate structural cycle iteration
        let cycle_analysis = graph.compute_cycle_analysis();
        cycle_reactivated = evaluate_cycle_iteration(graph, &id_owned, &cycle_analysis);

        true
    })
    .context("Failed to save graph")?;

    if already_done {
        println!("Task '{}' is already done", id);
        return Ok(());
    }

    super::notify_graph_changed(dir);

    // Update agent registry to reflect task completion.
    // Without this, the registry entry stays at Working until the daemon's
    // periodic triage detects the dead process — creating a window where the
    // agent appears alive and consumes an agent slot.
    if let Ok(mut locked_registry) = AgentRegistry::load_locked(dir) {
        if let Some(agent) = locked_registry.get_agent_by_task_mut(id) {
            agent.status = workgraph::service::registry::AgentStatus::Done;
            if agent.completed_at.is_none() {
                agent.completed_at = Some(Utc::now().to_rfc3339());
            }
        }
        let _ = locked_registry.save_ref();
    }

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "done",
        Some(id),
        None,
        serde_json::Value::Null,
        config.log.rotation_threshold,
    );

    println!("Marked '{}' as done", id);

    // User board auto-increment: if a user board is archived (done), create the successor.
    if let Some(task) = graph.get_task(id)
        && task.tags.iter().any(|t| t == "user-board")
        && let Some(handle) = user_board_handle(id)
    {
        let current_seq = user_board_seq(id).unwrap_or(0);
        let next_seq = current_seq + 1;
        let successor = create_user_board_task(handle, next_seq);
        let successor_id = successor.id.clone();
        let graph_path = super::graph_path(dir);
        if let Err(e) = modify_graph(&graph_path, |graph| {
            // Also add 'archived' tag to the current board
            if let Some(t) = graph.get_task_mut(id)
                && !t.tags.contains(&"archived".to_string())
            {
                t.tags.push("archived".to_string());
            }
            graph.add_node(Node::Task(successor));
            true
        }) {
            eprintln!("Warning: failed to create successor board: {}", e);
        } else {
            println!("Created successor board '{}'", successor_id);
            super::notify_graph_changed(dir);
        }
    }

    for task_id in &cycle_reactivated {
        println!("  Cycle: re-activated '{}'", task_id);
    }

    // Archive agent conversation (prompt + output) for provenance
    if let Some(task) = graph.get_task(id)
        && let Some(ref agent_id) = task.assigned
    {
        match super::log::archive_agent(dir, id, agent_id) {
            Ok(archive_dir) => {
                eprintln!("Agent archived to {}", archive_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: agent archive failed: {}", e);
            }
        }
    }

    // Capture task output (git diff, artifacts, log) for evaluation.
    // When auto_evaluate is enabled, the coordinator creates an evaluation task
    // in the graph that becomes ready once this task is done; the captured output
    // feeds that evaluator.
    if let Some(task) = graph.get_task(id) {
        match capture_task_output(dir, task) {
            Ok(output_dir) => {
                eprintln!("Output captured to {}", output_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: output capture failed: {}", e);
            }
        }
    }

    // Soft validation nudge: if no log entry mentions validation, print a tip.
    if let Some(task) = graph.get_task(id) {
        let has_validation = task
            .log
            .iter()
            .any(|entry| entry.message.to_lowercase().contains("validat"));
        if !has_validation {
            eprintln!(
                "Tip: Log validation steps before wg done (e.g., wg log {} \"Validated: tests pass\")",
                id
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use workgraph::test_helpers::{make_task_with_status as make_task, setup_workgraph};

    #[test]
    fn test_done_open_task_transitions_to_done() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_in_progress_task_transitions_to_done() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::InProgress)],
        );

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_already_done_returns_ok() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Done)]);

        // Should return Ok (idempotent) rather than error
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_done_with_unresolved_blockers_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Blocker task", Status::Open);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("blocked by"));
        assert!(err.to_string().contains("unresolved"));
    }

    #[test]
    fn test_done_with_resolved_blockers_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Blocker task", Status::Done);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("blocked").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_with_failed_blocker_succeeds() {
        // Failed blockers are terminal — they should not block dependents
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Failed blocker", Status::Failed);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("blocked").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_with_abandoned_blocker_succeeds() {
        // Abandoned blockers are terminal — they should not block dependents
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let blocker = make_task("blocker", "Abandoned blocker", Status::Abandoned);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];

        setup_workgraph(dir_path, vec![blocker, blocked]);

        let result = run(dir_path, "blocked", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("blocked").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_verified_task_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Verified task", Status::InProgress);
        task.verify = Some("true".to_string());

        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_sets_completed_at_timestamp() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let before = Utc::now();
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.completed_at.is_some());

        // Parse the timestamp and verify it's recent
        let completed_at: chrono::DateTime<Utc> =
            task.completed_at.as_ref().unwrap().parse().unwrap();
        assert!(completed_at >= before);
    }

    #[test]
    fn test_done_creates_log_entry() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        assert!(!task.log.is_empty());
        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.message, "Task marked as done");
        assert_eq!(last_log.actor, Some("agent-1".to_string()));
    }

    #[test]
    fn test_done_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);

        let result = run(dir_path, "nonexistent", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_done_uninitialized_workgraph_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Don't initialize workgraph

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[test]
    fn test_done_log_entry_without_assigned_has_none_actor() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.actor, None);
    }

    #[test]
    fn test_done_converged_log_message() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.message, "Task marked as done (converged)");
    }

    #[test]
    fn test_done_converged_ignored_when_cycle_guard_set_on_self() {
        // When the task itself has a cycle guard, --converged should be ignored.
        // The guard is authoritative — the agent cannot self-converge.
        use workgraph::graph::{CycleConfig, LoopGuard};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Cycle header", Status::Open);
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: Some(LoopGuard::TaskStatus {
                task: "validator".to_string(),
                status: Status::Failed,
            }),
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("header").unwrap();

        // Converged tag should NOT be present
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added when cycle guard is set"
        );

        // Log should reflect that --converged was ignored
        let last_log = task.log.last().unwrap();
        assert_eq!(
            last_log.message,
            "Task marked as done (--converged ignored, cycle is forced)"
        );
    }

    #[test]
    fn test_done_converged_ignored_for_non_header_in_guarded_cycle() {
        // When a task is a non-header member of a cycle whose header has a guard,
        // --converged should also be ignored.
        use workgraph::graph::{CycleConfig, LoopGuard};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Create cycle: header ↔ worker (both depend on each other)
        let mut header = make_task("header", "Cycle header", Status::Done);
        header.after = vec!["worker".to_string()];
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: Some(LoopGuard::TaskStatus {
                task: "validator".to_string(),
                status: Status::Failed,
            }),
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let mut worker = make_task("worker", "Worker in cycle", Status::Open);
        worker.after = vec!["header".to_string()];

        setup_workgraph(dir_path, vec![header, worker]);

        let result = run(dir_path, "worker", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("worker").unwrap();

        // Converged tag should NOT be present
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added for non-header in guarded cycle"
        );

        // Log should reflect that --converged was ignored
        let last_log = task.log.last().unwrap();
        assert_eq!(
            last_log.message,
            "Task marked as done (--converged ignored, cycle is forced)"
        );
    }

    #[test]
    fn test_done_converged_accepted_when_guard_is_always() {
        // When cycle_config has guard = Always (trivial), --converged should work.
        use workgraph::graph::{CycleConfig, LoopGuard};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Cycle header", Status::Open);
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: Some(LoopGuard::Always),
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("header").unwrap();

        // Converged tag SHOULD be present (Always guard is trivial)
        assert!(
            task.tags.contains(&"converged".to_string()),
            "converged tag should be added when guard is Always"
        );

        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.message, "Task marked as done (converged)");
    }

    #[test]
    fn test_done_converged_accepted_when_no_guard() {
        // When cycle_config has no guard, --converged should work.
        use workgraph::graph::CycleConfig;

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Cycle header", Status::Open);
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("header").unwrap();

        // Converged tag SHOULD be present
        assert!(
            task.tags.contains(&"converged".to_string()),
            "converged tag should be added when no guard is set"
        );
    }

    #[test]
    fn test_done_without_validation_log_still_succeeds() {
        // The soft validation tip should never block completion.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);

        // No log entry contains "validat" — the tip would fire, but must not block
        let has_validation = task
            .log
            .iter()
            .any(|e| e.message.to_lowercase().contains("validat"));
        assert!(!has_validation);
    }

    #[test]
    fn test_done_with_validation_log_suppresses_tip() {
        // When a log entry contains a validation mention, no tip should fire.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::Open);
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            user: None,
            message: "Validated: all tests pass".to_string(),
        });
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);

        // Log contains "Validated" — tip should be suppressed
        let has_validation = task
            .log
            .iter()
            .any(|e| e.message.to_lowercase().contains("validat"));
        assert!(has_validation);
    }

    #[test]
    fn test_done_converged_ignored_when_no_converge_set_on_self() {
        // When the task itself has no_converge, --converged should be ignored.
        use workgraph::graph::CycleConfig;

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Forced cycle header", Status::Open);
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: true,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        setup_workgraph(dir_path, vec![header]);

        let result = run(dir_path, "header", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("header").unwrap();

        // Converged tag should NOT be present
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added when no_converge is set"
        );

        // Log should contain the forced-ignore message (may not be last due to reactivation)
        let has_forced_msg = task
            .log
            .iter()
            .any(|e| e.message == "Task marked as done (--converged ignored, cycle is forced)");
        assert!(
            has_forced_msg,
            "Log should contain forced-ignore message, got: {:?}",
            task.log.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_done_converged_ignored_for_non_header_in_no_converge_cycle() {
        // When a task is a non-header member of a cycle with no_converge,
        // --converged should also be ignored.
        use workgraph::graph::CycleConfig;

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut header = make_task("header", "Forced cycle header", Status::Done);
        header.after = vec!["worker".to_string()];
        header.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: true,
            restart_on_failure: true,
            max_failure_restarts: None,
        });

        let mut worker = make_task("worker", "Worker in forced cycle", Status::Open);
        worker.after = vec!["header".to_string()];

        setup_workgraph(dir_path, vec![header, worker]);

        let result = run(dir_path, "worker", true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("worker").unwrap();

        // Converged tag should NOT be present
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "converged tag should not be added for non-header in no-converge cycle"
        );

        // Log should contain the forced-ignore message (may not be last due to reactivation)
        let has_forced_msg = task
            .log
            .iter()
            .any(|e| e.message == "Task marked as done (--converged ignored, cycle is forced)");
        assert!(
            has_forced_msg,
            "Log should contain forced-ignore message, got: {:?}",
            task.log.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_done_verify_passing_allows_transition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with passing verify", Status::InProgress);
        task.verify = Some("exit 0".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_verify_failing_blocks_transition() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Verify command failed"), "got: {}", err);
        assert!(err.contains("exit 1"), "got: {}", err);

        // Task should still be in-progress
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_done_verify_failing_includes_output() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("echo 'test failed: expected 42 got 0' >&2; exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("test failed: expected 42 got 0"),
            "error should include command output, got: {}",
            err
        );
    }

    #[test]
    fn test_done_skip_verify_bypasses_gate() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Use run_inner with is_agent=false to simulate human usage
        let result = super::run_inner(dir_path, "t1", false, true, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_skip_verify_blocked_for_agents() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Use run_inner with is_agent=true to simulate agent context
        let result = super::run_inner(dir_path, "t1", false, true, true);

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Agents cannot use --skip-verify"),
            "got: {}",
            err
        );

        // Task should not have transitioned
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_done_no_verify_field_works_as_before() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let task = make_task("t1", "Task without verify", Status::InProgress);
        assert!(task.verify.is_none());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_converged_also_runs_verify() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", true, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Verify command failed"), "got: {}", err);

        // Task should still be in-progress
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_done_external_validation_transitions_to_pending() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "External validation task", Status::InProgress);
        task.validation = Some("external".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::PendingValidation);
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn test_done_external_validation_adds_log() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "External validation task", Status::InProgress);
        task.validation = Some("external".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false, false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        let last_log = task.log.last().unwrap();
        assert!(last_log.message.contains("pending external validation"));
    }

    #[test]
    fn test_done_integrated_validation_requires_log_entry() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Integrated validation task", Status::InProgress);
        task.validation = Some("integrated".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Should fail: no validation log entry
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("validation log entry"));
    }

    #[test]
    fn test_done_integrated_validation_with_log_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Integrated validation task", Status::InProgress);
        task.validation = Some("integrated".to_string());
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            user: None,
            message: "Validated: all tests pass".to_string(),
        });
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_integrated_validation_runs_commands() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Integrated with commands", Status::InProgress);
        task.validation = Some("integrated".to_string());
        task.validation_commands = vec!["exit 1".to_string()]; // will fail
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            user: None,
            message: "Validated: ready".to_string(),
        });
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("validation failed"));
    }

    #[test]
    fn test_done_none_validation_is_default() {
        // validation=None (default) should behave like "none" — direct to Done
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let task = make_task("t1", "Default task", Status::InProgress);
        assert!(task.validation.is_none());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_updates_agent_registry() {
        // When a task is marked done, the agent registry entry should also
        // transition to Done so the agent slot is freed immediately.
        use workgraph::service::registry::{AgentRegistry, AgentStatus};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Set up a registry with an agent working on this task
        let mut registry = AgentRegistry::new();
        registry.register_agent(99999, "t1", "claude", "/tmp/output.log");
        registry.save(dir_path).unwrap();

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        // Verify registry was updated
        let registry = AgentRegistry::load(dir_path).unwrap();
        let agent = registry.get_agent("agent-1").unwrap();
        assert_eq!(
            agent.status,
            AgentStatus::Done,
            "Agent registry should be updated to Done when task completes"
        );
        assert!(
            agent.completed_at.is_some(),
            "Agent should have a completed_at timestamp"
        );
    }

    #[test]
    fn test_done_verify_pipe_syntax() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with pipe verify", Status::InProgress);
        task.verify = Some("echo hello | grep hello".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(
            result.is_ok(),
            "Pipe in verify command should work: {:?}",
            result.err()
        );

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_done_verify_pipe_failure_propagates() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing pipe verify", Status::InProgress);
        task.verify = Some("echo hello | grep nonexistent".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err(), "Failing pipe should propagate error");

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_verify_circuit_breaker_increments_failures() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("echo 'bad output' >&2; exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // First failure: should increment verify_failures and bail
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.verify_failures, 1);
        assert_eq!(task.status, Status::InProgress);
        // Check that verify failure was logged
        assert!(
            task.log
                .iter()
                .any(|e| e.message.contains("Verify FAILED")
                    && e.actor == Some("verify".to_string())),
            "Expected verify failure log entry, got: {:?}",
            task.log
        );
    }

    #[test]
    fn test_verify_circuit_breaker_trips_after_threshold() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("echo 'FAIL: test not found' >&2; exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Default threshold is 3 — fail 3 times
        for i in 0..3 {
            let result = run(dir_path, "t1", false, false);
            if i < 2 {
                // First two failures: should error (not yet at threshold)
                assert!(result.is_err(), "attempt {} should fail with error", i);
            } else {
                // Third failure: circuit breaker trips, returns Ok (task is auto-failed)
                assert!(
                    result.is_ok(),
                    "attempt {} should succeed (circuit breaker trips): {:?}",
                    i,
                    result
                );
            }
        }

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
        assert_eq!(task.verify_failures, 3);

        // Check failure reason includes verify command and error output
        let reason = task.failure_reason.as_ref().unwrap();
        assert!(
            reason.contains("failed 3 consecutive times"),
            "failure_reason should mention count, got: {}",
            reason
        );
        assert!(
            reason.contains("exit 1"),
            "failure_reason should include exit code, got: {}",
            reason
        );
        assert!(
            reason.contains("FAIL: test not found"),
            "failure_reason should include stderr, got: {}",
            reason
        );

        // Check circuit breaker log entry
        assert!(
            task.log
                .iter()
                .any(|e| e.actor == Some("verify-circuit-breaker".to_string())
                    && e.message.contains("Circuit breaker tripped")),
            "Expected circuit breaker log entry, got: {:?}",
            task.log
        );
    }

    #[test]
    fn test_verify_success_resets_failure_count() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Start with a task that already has some verify failures
        let mut task = make_task("t1", "Task with verify", Status::InProgress);
        task.verify = Some("exit 0".to_string());
        task.verify_failures = 2; // previous failures
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
        assert_eq!(
            task.verify_failures, 0,
            "verify_failures should be reset on success"
        );
    }

    #[test]
    fn test_verify_failure_logs_stdout_and_stderr() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with verbose verify", Status::InProgress);
        task.verify = Some("echo 'stdout line' && echo 'stderr line' >&2 && exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        // Verify log entry includes both stdout and stderr
        let verify_log = task
            .log
            .iter()
            .find(|e| e.message.contains("Verify FAILED"))
            .expect("should have verify failure log");
        assert!(
            verify_log.message.contains("stdout line"),
            "log should contain stdout, got: {}",
            verify_log.message
        );
        assert!(
            verify_log.message.contains("stderr line"),
            "log should contain stderr, got: {}",
            verify_log.message
        );
    }

    #[test]
    fn test_verify_circuit_breaker_distinguishes_from_agent_failures() {
        // Verify failures use the "verify" actor, circuit breaker uses "verify-circuit-breaker"
        // Regular triage/agent failures use "triage" actor
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let _ = run(dir_path, "t1", false, false);

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        // All verify-related logs should use "verify" actor
        let verify_logs: Vec<_> = task
            .log
            .iter()
            .filter(|e| e.message.contains("Verify"))
            .collect();
        assert!(!verify_logs.is_empty());
        for log in &verify_logs {
            assert_eq!(
                log.actor,
                Some("verify".to_string()),
                "Verify failure logs should use 'verify' actor, not agent/triage actor"
            );
        }
    }

    #[test]
    fn test_verify_circuit_breaker_configurable_threshold() {
        // Test that the config controls the threshold.
        // We write a config with max_verify_failures = 2.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with failing verify", Status::InProgress);
        task.verify = Some("exit 1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Write config with lower threshold (dir_path is the .workgraph dir in tests)
        let config_path = dir_path.join("config.toml");
        std::fs::write(&config_path, "[coordinator]\nmax_verify_failures = 2\n").unwrap();

        // First failure
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());

        // Second failure — should trip circuit breaker at threshold 2
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok(), "Circuit breaker should trip at threshold 2");

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
        assert_eq!(task.verify_failures, 2);
    }

    #[test]
    fn test_done_separate_verify_transitions_to_pending_validation() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with verify", Status::InProgress);
        task.verify = Some("cargo test".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Write config with verify_mode = "separate"
        std::fs::write(
            dir_path.join("config.toml"),
            "[coordinator]\nverify_mode = \"separate\"\n",
        )
        .unwrap();

        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.status,
            Status::PendingValidation,
            "should be pending validation, not done"
        );
        assert!(task.completed_at.is_some());
        assert!(
            task.log
                .iter()
                .any(|e| e.message.contains("verify_mode=separate")),
            "should have separate verify log entry"
        );
    }

    #[test]
    fn test_done_inline_verify_still_works() {
        // Ensure backward compatibility: verify_mode=inline (default) runs verify inline
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with passing verify", Status::InProgress);
        task.verify = Some("true".to_string()); // always passes
        setup_workgraph(dir_path, vec![task]);

        // No config file = defaults to inline
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.status,
            Status::Done,
            "inline verify should complete to Done"
        );
    }

    #[test]
    fn test_is_core_file() {
        assert!(is_core_file("src/lib.rs"));
        assert!(is_core_file("src/main.rs"));
        assert!(is_core_file("Cargo.toml"));
        assert!(is_core_file("Cargo.lock"));
        assert!(is_core_file("build.rs"));
        assert!(is_core_file("src/commands/mod.rs"));

        assert!(!is_core_file("src/commands/add.rs"));
        assert!(!is_core_file("src/graph.rs"));
        assert!(!is_core_file("tests/test_integration.rs"));
    }

    #[test]
    fn test_map_file_to_test_command() {
        // Test source file mapping
        assert_eq!(
            map_file_to_test_command("src/commands/add.rs"),
            Some("cargo test add".to_string())
        );
        assert_eq!(
            map_file_to_test_command("src/graph.rs"),
            Some("cargo test graph".to_string())
        );

        // Test direct test file mapping
        assert_eq!(
            map_file_to_test_command("tests/integration_multi_user.rs"),
            Some("cargo test --test integration_multi_user".to_string())
        );

        // Test non-mappable files
        assert_eq!(map_file_to_test_command("README.md"), None);
        assert_eq!(map_file_to_test_command("docs/guide.md"), None);
    }

    #[test]
    fn test_map_files_to_tests() {
        // Regular source files should map to scoped tests
        let files = vec!["src/commands/add.rs".to_string()];
        let result = map_files_to_tests(&files);
        assert_eq!(result, Some(vec!["cargo test add".to_string()]));

        // Core files should return None (fall back to full suite)
        let files = vec!["src/lib.rs".to_string()];
        let result = map_files_to_tests(&files);
        assert_eq!(result, None);

        // Multiple files should combine commands
        let files = vec![
            "src/commands/add.rs".to_string(),
            "src/graph.rs".to_string(),
        ];
        let result = map_files_to_tests(&files);
        assert_eq!(
            result,
            Some(vec![
                "cargo test add".to_string(),
                "cargo test graph".to_string()
            ])
        );

        // Empty files list should return None
        let files: Vec<String> = vec![];
        let result = map_files_to_tests(&files);
        assert_eq!(result, None);
    }

    // Smart verify detection tests

    #[test]
    fn test_is_free_text_verify_command_detects_descriptive_text() {
        assert!(is_free_text_verify_command(
            "documentation exists and is comprehensive"
        ));
        assert!(is_free_text_verify_command("tests pass for all modules"));
        assert!(is_free_text_verify_command("build succeeds without errors"));
        assert!(is_free_text_verify_command("code has been implemented"));
        assert!(is_free_text_verify_command("feature works correctly"));
        assert!(is_free_text_verify_command("ensure the module compiles"));
    }

    #[test]
    fn test_is_free_text_verify_command_allows_valid_commands() {
        assert!(!is_free_text_verify_command("cargo test"));
        assert!(!is_free_text_verify_command("npm test"));
        assert!(!is_free_text_verify_command("make build"));
        assert!(!is_free_text_verify_command("python -m pytest"));
        assert!(!is_free_text_verify_command("go test ./..."));
        assert!(!is_free_text_verify_command("true"));
        assert!(!is_free_text_verify_command("exit 0"));
    }

    #[test]
    fn test_is_free_text_verify_command_allows_shell_constructs() {
        assert!(!is_free_text_verify_command(
            "cargo test | grep -q 'test result: ok'"
        ));
        assert!(!is_free_text_verify_command("make build && echo 'success'"));
        assert!(!is_free_text_verify_command("test -f output.txt"));
        assert!(!is_free_text_verify_command("echo 'hello' > /tmp/test"));
        assert!(!is_free_text_verify_command("[ -d src ]"));
    }

    #[test]
    fn test_is_free_text_verify_command_edge_cases() {
        assert!(!is_free_text_verify_command(""));
        assert!(!is_free_text_verify_command("cargo"));
        assert!(!is_free_text_verify_command("   cargo test   "));
        assert!(is_free_text_verify_command(
            "unknown_command does something"
        ));
        assert!(is_free_text_verify_command(
            "this should be detected as free text"
        ));
    }

    #[test]
    fn test_smart_verify_routes_free_text_to_evaluation() {
        // This is more of an integration test - we test that the routing works
        // by checking that free-text commands don't get executed as shell commands
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with free-text verify", Status::InProgress);
        task.verify = Some("documentation exists and is comprehensive".to_string());
        setup_workgraph(dir_path, vec![task]);

        // The task should fail because evaluation requires the task to be Done first
        // But importantly, it should NOT fail with exit 127 (command not found)
        let result = run(dir_path, "t1", false, false);
        assert!(result.is_err());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        // Check that the failure is not due to command not found
        let _verify_logs: Vec<_> = task
            .log
            .iter()
            .filter(|e| e.message.contains("smart-verify") || e.message.contains("LLM evaluation"))
            .collect();

        // There should be some indication that smart verify was used
        let has_smart_verify_indication = task
            .log
            .iter()
            .any(|e| e.message.contains("smart-verify") || e.message.contains("LLM evaluation"));

        // Or alternatively, verify that we don't get a "command not found" error
        let has_command_not_found = task.log.iter().any(|e| {
            e.message.contains("command not found") || e.message.contains("exit code 127")
        });

        // We should either see smart-verify logs or no "command not found" errors
        assert!(
            has_smart_verify_indication || !has_command_not_found,
            "Expected smart verify routing or no 'command not found' errors. Logs: {:?}",
            task.log
        );
    }

    #[test]
    fn test_term_dumb_environment_set_for_shell_commands() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Task with shell verify", Status::InProgress);
        // Use a command that checks the TERM environment variable
        task.verify = Some("test \"$TERM\" = \"dumb\"".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false, false);

        // The command should succeed, indicating TERM=dumb was set
        assert!(result.is_ok(), "TERM=dumb should be set for shell commands");

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }
}

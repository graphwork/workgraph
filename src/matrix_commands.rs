//! Shared Matrix command parser and executor for workgraph
//!
//! This module contains the command parsing and execution logic shared between
//! the full Matrix SDK implementation (`matrix`) and the lightweight HTTP
//! implementation (`matrix_lite`). The parser has no external dependencies;
//! the executor only depends on core workgraph types.
//!
//! Parses human-friendly commands from Matrix messages:
//! - `claim <task>` - Claim a task for work
//! - `done <task>` - Mark a task as done
//! - `fail <task> [reason]` - Mark a task as failed
//! - `input <task> <text>` - Add input/log entry to a task
//! - `unclaim <task>` - Release a claimed task
//! - `status` - Show current status
//! - `ready` - List ready tasks
//! - `help` - Show help

use std::path::Path;

use chrono::Utc;

use crate::graph::{LogEntry, Status};
use crate::parser::{load_graph, save_graph};

/// A parsed command from a Matrix message
#[derive(Debug, Clone, PartialEq)]
pub enum MatrixCommand {
    /// Claim a task for work
    Claim {
        task_id: String,
        actor: Option<String>,
    },
    /// Mark a task as done
    Done { task_id: String },
    /// Mark a task as failed
    Fail {
        task_id: String,
        reason: Option<String>,
    },
    /// Add input/log entry to a task
    Input { task_id: String, text: String },
    /// Release a claimed task
    Unclaim { task_id: String },
    /// Show current status summary
    Status,
    /// List ready tasks
    Ready,
    /// Show help
    Help,
    /// Unknown command
    Unknown { command: String },
}

impl MatrixCommand {
    /// Parse a command from a message body
    ///
    /// Commands can be prefixed with optional markers like `wg`, `!wg`, or `/wg`
    /// to distinguish them from regular chat messages.
    pub fn parse(message: &str) -> Option<Self> {
        let message = message.trim();

        // Skip empty messages
        if message.is_empty() {
            return None;
        }

        // Strip optional command prefixes
        let stripped = strip_prefix(message);

        // If no prefix was found, check if it looks like a command
        // (starts with a known command word)
        let words: Vec<&str> = stripped.split_whitespace().collect();
        if words.is_empty() {
            return None;
        }

        let command_word = words[0].to_lowercase();

        // Only parse if we had a prefix OR if it starts with a known command
        let has_prefix = stripped.len() < message.len();
        if !has_prefix && !is_known_command(&command_word) {
            return None;
        }

        Some(parse_command(&words))
    }

    /// Get a human-readable description of what this command does
    pub fn description(&self) -> String {
        match self {
            MatrixCommand::Claim { task_id, actor } => match actor {
                Some(a) => format!("Claim task '{}' for '{}'", task_id, a),
                None => format!("Claim task '{}'", task_id),
            },
            MatrixCommand::Done { task_id } => format!("Mark task '{}' as done", task_id),
            MatrixCommand::Fail { task_id, reason } => match reason {
                Some(r) => format!("Mark task '{}' as failed: {}", task_id, r),
                None => format!("Mark task '{}' as failed", task_id),
            },
            MatrixCommand::Input { task_id, text } => {
                format!("Add input to task '{}': {}", task_id, text)
            }
            MatrixCommand::Unclaim { task_id } => format!("Unclaim task '{}'", task_id),
            MatrixCommand::Status => "Show status".to_string(),
            MatrixCommand::Ready => "List ready tasks".to_string(),
            MatrixCommand::Help => "Show help".to_string(),
            MatrixCommand::Unknown { command } => format!("Unknown command: {}", command),
        }
    }
}

/// Strip optional command prefixes like `wg`, `!wg`, `/wg`
fn strip_prefix(message: &str) -> &str {
    let prefixes = ["!wg ", "/wg ", "wg ", "!wg: ", "/wg: ", "wg: "];
    for prefix in &prefixes {
        if let Some(rest) = message.strip_prefix(prefix) {
            return rest.trim();
        }
    }
    // Also check case-insensitive
    let lower = message.to_lowercase();
    for prefix in &prefixes {
        if lower.starts_with(prefix) {
            return message[prefix.len()..].trim();
        }
    }
    message
}

/// Check if a word is a known command
fn is_known_command(word: &str) -> bool {
    matches!(
        word,
        "claim"
            | "done"
            | "fail"
            | "input"
            | "log"
            | "note"
            | "unclaim"
            | "release"
            | "status"
            | "ready"
            | "list"
            | "tasks"
            | "help"
            | "?"
    )
}

/// Parse the actual command from words
fn parse_command(words: &[&str]) -> MatrixCommand {
    if words.is_empty() {
        return MatrixCommand::Unknown {
            command: "".to_string(),
        };
    }

    let command = words[0].to_lowercase();

    match command.as_str() {
        "claim" => {
            if words.len() < 2 {
                return MatrixCommand::Unknown {
                    command: "claim (missing task ID)".to_string(),
                };
            }
            let task_id = words[1].to_string();
            // Check for optional actor: "claim task-1 as erik" or "claim task-1 --actor erik"
            let actor = parse_actor_arg(&words[2..]);
            MatrixCommand::Claim { task_id, actor }
        }
        "done" => {
            if words.len() < 2 {
                return MatrixCommand::Unknown {
                    command: "done (missing task ID)".to_string(),
                };
            }
            MatrixCommand::Done {
                task_id: words[1].to_string(),
            }
        }
        "fail" => {
            if words.len() < 2 {
                return MatrixCommand::Unknown {
                    command: "fail (missing task ID)".to_string(),
                };
            }
            let task_id = words[1].to_string();
            let reason = if words.len() > 2 {
                Some(words[2..].join(" "))
            } else {
                None
            };
            MatrixCommand::Fail { task_id, reason }
        }
        "input" | "log" | "note" => {
            if words.len() < 3 {
                return MatrixCommand::Unknown {
                    command: format!("{} (missing task ID or text)", command),
                };
            }
            let task_id = words[1].to_string();
            let text = words[2..].join(" ");
            MatrixCommand::Input { task_id, text }
        }
        "unclaim" | "release" => {
            if words.len() < 2 {
                return MatrixCommand::Unknown {
                    command: "unclaim (missing task ID)".to_string(),
                };
            }
            MatrixCommand::Unclaim {
                task_id: words[1].to_string(),
            }
        }
        "status" => MatrixCommand::Status,
        "ready" | "list" | "tasks" => MatrixCommand::Ready,
        "help" | "?" => MatrixCommand::Help,
        _ => MatrixCommand::Unknown {
            command: command.to_string(),
        },
    }
}

/// Parse optional actor argument from remaining words
fn parse_actor_arg(words: &[&str]) -> Option<String> {
    if words.is_empty() {
        return None;
    }

    // Support "as <actor>" syntax
    if words.len() >= 2 && words[0].to_lowercase() == "as" {
        return Some(words[1].to_string());
    }

    // Support "--actor <actor>" syntax
    if words.len() >= 2 && (words[0] == "--actor" || words[0] == "-a") {
        return Some(words[1].to_string());
    }

    // Support "for <actor>" syntax
    if words.len() >= 2 && words[0].to_lowercase() == "for" {
        return Some(words[1].to_string());
    }

    None
}

/// Generate help text for Matrix commands
pub fn help_text() -> String {
    r#"**Workgraph Commands**

• `claim <task>` - Claim a task (e.g., `claim implement-feature`)
• `claim <task> as <actor>` - Claim for a specific actor
• `done <task>` - Mark a task as done
• `fail <task> [reason]` - Mark a task as failed
• `input <task> <text>` - Add a log entry to a task
• `unclaim <task>` - Release a claimed task
• `ready` - List tasks ready to work on
• `status` - Show project status
• `help` - Show this help

Prefix commands with `wg` if needed (e.g., `wg claim task-1`)"#
        .to_string()
}

/// Extract the localpart from a Matrix user ID (e.g., "@user:server" -> "user")
pub fn extract_localpart(user_id: &str) -> String {
    user_id
        .strip_prefix('@')
        .and_then(|s| s.split(':').next())
        .unwrap_or(user_id)
        .to_string()
}

// ── Command execution (shared graph-manipulation logic) ────────────────

/// Execute a full command dispatch, returning the response message.
///
/// The `sender` is used as the fallback actor for claim/input commands.
pub fn execute_command(workgraph_dir: &Path, command: &MatrixCommand, sender: &str) -> String {
    match command {
        MatrixCommand::Claim { task_id, actor } => {
            let actor_id = actor.clone().unwrap_or_else(|| extract_localpart(sender));
            execute_claim(workgraph_dir, task_id, Some(&actor_id))
        }
        MatrixCommand::Done { task_id } => execute_done(workgraph_dir, task_id),
        MatrixCommand::Fail { task_id, reason } => {
            execute_fail(workgraph_dir, task_id, reason.as_deref())
        }
        MatrixCommand::Input { task_id, text } => {
            let actor = extract_localpart(sender);
            execute_input(workgraph_dir, task_id, text, &actor)
        }
        MatrixCommand::Unclaim { task_id } => execute_unclaim(workgraph_dir, task_id),
        MatrixCommand::Status => execute_status(workgraph_dir),
        MatrixCommand::Ready => execute_ready(workgraph_dir),
        MatrixCommand::Help => help_text(),
        MatrixCommand::Unknown { command } => {
            format!(
                "Unknown command: '{}'. Type 'help' for available commands.",
                command
            )
        }
    }
}

/// Execute claim command
pub fn execute_claim(workgraph_dir: &Path, task_id: &str, actor: Option<&str>) -> String {
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        return "Error: Workgraph not initialized".to_string();
    }

    let mut graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return format!("Error loading graph: {}", e),
    };

    let task = match graph.get_task_mut(task_id) {
        Some(t) => t,
        None => return format!("Error: Task '{}' not found", task_id),
    };

    match task.status {
        Status::Open | Status::Blocked => {}
        Status::InProgress => {
            let holder = task
                .assigned
                .as_ref()
                .map(|a| format!(" by {}", a))
                .unwrap_or_default();
            return format!("Task '{}' is already claimed{}", task_id, holder);
        }
        Status::Done => {
            return format!("Task '{}' is already done", task_id);
        }
        Status::Failed => {
            return format!(
                "Cannot claim task '{}': task is Failed. Use 'wg retry' first.",
                task_id
            );
        }
        Status::Abandoned => {
            return format!("Cannot claim task '{}': task is Abandoned", task_id);
        }
    }

    task.status = Status::InProgress;
    task.started_at = Some(Utc::now().to_rfc3339());
    if let Some(actor_id) = actor {
        task.assigned = Some(actor_id.to_string());
    }

    if let Err(e) = save_graph(&graph, &graph_path) {
        return format!("Error saving graph: {}", e);
    }

    match actor {
        Some(actor_id) => format!("Claimed '{}' for '{}'", task_id, actor_id),
        None => format!("Claimed '{}'", task_id),
    }
}

/// Execute done command
pub fn execute_done(workgraph_dir: &Path, task_id: &str) -> String {
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        return "Error: Workgraph not initialized".to_string();
    }

    let mut graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return format!("Error loading graph: {}", e),
    };

    let task = match graph.get_task_mut(task_id) {
        Some(t) => t,
        None => return format!("Error: Task '{}' not found", task_id),
    };

    if task.status == Status::Done {
        return format!("Task '{}' is already done", task_id);
    }

    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());

    if let Err(e) = save_graph(&graph, &graph_path) {
        return format!("Error saving graph: {}", e);
    }

    format!("Marked '{}' as done", task_id)
}

/// Execute fail command
pub fn execute_fail(workgraph_dir: &Path, task_id: &str, reason: Option<&str>) -> String {
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        return "Error: Workgraph not initialized".to_string();
    }

    let mut graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return format!("Error loading graph: {}", e),
    };

    let task = match graph.get_task_mut(task_id) {
        Some(t) => t,
        None => return format!("Error: Task '{}' not found", task_id),
    };

    if task.status == Status::Done {
        return format!(
            "Task '{}' is already done and cannot be marked as failed",
            task_id
        );
    }

    if task.status == Status::Failed {
        return format!("Task '{}' is already failed", task_id);
    }

    task.status = Status::Failed;
    task.retry_count += 1;
    task.failure_reason = reason.map(String::from);

    let retry_count = task.retry_count;

    if let Err(e) = save_graph(&graph, &graph_path) {
        return format!("Error saving graph: {}", e);
    }

    let reason_msg = reason.map(|r| format!(" ({})", r)).unwrap_or_default();
    format!(
        "Marked '{}' as failed{} (retry #{})",
        task_id, reason_msg, retry_count
    )
}

/// Execute input/log command
pub fn execute_input(workgraph_dir: &Path, task_id: &str, text: &str, actor: &str) -> String {
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        return "Error: Workgraph not initialized".to_string();
    }

    let mut graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return format!("Error loading graph: {}", e),
    };

    let task = match graph.get_task_mut(task_id) {
        Some(t) => t,
        None => return format!("Error: Task '{}' not found", task_id),
    };

    let entry = LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(actor.to_string()),
        message: text.to_string(),
    };

    task.log.push(entry);

    if let Err(e) = save_graph(&graph, &graph_path) {
        return format!("Error saving graph: {}", e);
    }

    format!("Added log entry to '{}' from {}", task_id, actor)
}

/// Execute unclaim command
pub fn execute_unclaim(workgraph_dir: &Path, task_id: &str) -> String {
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        return "Error: Workgraph not initialized".to_string();
    }

    let mut graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return format!("Error loading graph: {}", e),
    };

    let task = match graph.get_task_mut(task_id) {
        Some(t) => t,
        None => return format!("Error: Task '{}' not found", task_id),
    };

    task.status = Status::Open;
    task.assigned = None;

    if let Err(e) = save_graph(&graph, &graph_path) {
        return format!("Error saving graph: {}", e);
    }

    format!("Unclaimed '{}'", task_id)
}

/// Execute status command
pub fn execute_status(workgraph_dir: &Path) -> String {
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        return "Error: Workgraph not initialized".to_string();
    }

    let graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return format!("Error loading graph: {}", e),
    };

    let total = graph.tasks().count();
    let done = graph.tasks().filter(|t| t.status == Status::Done).count();
    let in_progress = graph
        .tasks()
        .filter(|t| t.status == Status::InProgress)
        .count();
    let open = graph.tasks().filter(|t| t.status == Status::Open).count();
    let blocked = graph
        .tasks()
        .filter(|t| t.status == Status::Blocked)
        .count();
    let failed = graph.tasks().filter(|t| t.status == Status::Failed).count();

    format!(
        "**Project Status**\n• Total: {} tasks\n• Done: {}\n• In Progress: {}\n• Open: {}\n• Blocked: {}\n• Failed: {}",
        total, done, in_progress, open, blocked, failed
    )
}

/// Execute ready command
pub fn execute_ready(workgraph_dir: &Path) -> String {
    let graph_path = workgraph_dir.join("graph.jsonl");

    if !graph_path.exists() {
        return "Error: Workgraph not initialized".to_string();
    }

    let graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return format!("Error loading graph: {}", e),
    };

    // Find ready tasks (open, not blocked)
    let ready_tasks: Vec<_> = graph
        .tasks()
        .filter(|t| {
            t.status == Status::Open
                && t.after.iter().all(|dep| {
                    graph
                        .get_task(dep)
                        .map(|d| d.status.is_terminal())
                        .unwrap_or(true)
                })
        })
        .collect();

    if ready_tasks.is_empty() {
        return "No tasks ready to work on".to_string();
    }

    let mut response = format!("**Ready Tasks** ({})\n", ready_tasks.len());
    for task in ready_tasks.iter().take(10) {
        response.push_str(&format!("• `{}`: {}\n", task.id, task.title));
    }

    if ready_tasks.len() > 10 {
        response.push_str(&format!("...and {} more", ready_tasks.len() - 10));
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_claim() {
        let cmd = MatrixCommand::parse("claim task-1").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Claim {
                task_id: "task-1".to_string(),
                actor: None
            }
        );
    }

    #[test]
    fn test_parse_claim_with_actor() {
        let cmd = MatrixCommand::parse("claim task-1 as erik").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Claim {
                task_id: "task-1".to_string(),
                actor: Some("erik".to_string())
            }
        );
    }

    #[test]
    fn test_parse_claim_with_for() {
        let cmd = MatrixCommand::parse("claim task-1 for agent-1").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Claim {
                task_id: "task-1".to_string(),
                actor: Some("agent-1".to_string())
            }
        );
    }

    #[test]
    fn test_parse_done() {
        let cmd = MatrixCommand::parse("done task-1").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Done {
                task_id: "task-1".to_string()
            }
        );
    }

    #[test]
    fn test_parse_fail_no_reason() {
        let cmd = MatrixCommand::parse("fail task-1").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Fail {
                task_id: "task-1".to_string(),
                reason: None
            }
        );
    }

    #[test]
    fn test_parse_fail_with_reason() {
        let cmd = MatrixCommand::parse("fail task-1 compilation error").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Fail {
                task_id: "task-1".to_string(),
                reason: Some("compilation error".to_string())
            }
        );
    }

    #[test]
    fn test_parse_input() {
        let cmd = MatrixCommand::parse("input task-1 This is my update").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Input {
                task_id: "task-1".to_string(),
                text: "This is my update".to_string()
            }
        );
    }

    #[test]
    fn test_parse_unclaim() {
        let cmd = MatrixCommand::parse("unclaim task-1").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Unclaim {
                task_id: "task-1".to_string()
            }
        );
    }

    #[test]
    fn test_parse_release() {
        let cmd = MatrixCommand::parse("release task-1").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Unclaim {
                task_id: "task-1".to_string()
            }
        );
    }

    #[test]
    fn test_parse_status() {
        let cmd = MatrixCommand::parse("status").unwrap();
        assert_eq!(cmd, MatrixCommand::Status);
    }

    #[test]
    fn test_parse_ready() {
        let cmd = MatrixCommand::parse("ready").unwrap();
        assert_eq!(cmd, MatrixCommand::Ready);
    }

    #[test]
    fn test_parse_help() {
        let cmd = MatrixCommand::parse("help").unwrap();
        assert_eq!(cmd, MatrixCommand::Help);
    }

    #[test]
    fn test_parse_with_wg_prefix() {
        let cmd = MatrixCommand::parse("wg claim task-1").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Claim {
                task_id: "task-1".to_string(),
                actor: None
            }
        );
    }

    #[test]
    fn test_parse_with_slash_prefix() {
        let cmd = MatrixCommand::parse("/wg done task-1").unwrap();
        assert_eq!(
            cmd,
            MatrixCommand::Done {
                task_id: "task-1".to_string()
            }
        );
    }

    #[test]
    fn test_parse_with_bang_prefix() {
        let cmd = MatrixCommand::parse("!wg ready").unwrap();
        assert_eq!(cmd, MatrixCommand::Ready);
    }

    #[test]
    fn test_parse_ignores_regular_messages() {
        assert!(MatrixCommand::parse("hello everyone").is_none());
        assert!(MatrixCommand::parse("how are you?").is_none());
        assert!(MatrixCommand::parse("the task is done").is_none());
    }

    #[test]
    fn test_parse_empty_message() {
        assert!(MatrixCommand::parse("").is_none());
        assert!(MatrixCommand::parse("   ").is_none());
    }

    #[test]
    fn test_parse_unknown_command() {
        let cmd = MatrixCommand::parse("wg foo").unwrap();
        assert!(matches!(cmd, MatrixCommand::Unknown { .. }));
    }

    #[test]
    fn test_parse_missing_task_id() {
        let cmd = MatrixCommand::parse("wg claim").unwrap();
        assert!(matches!(cmd, MatrixCommand::Unknown { .. }));
    }

    #[test]
    fn test_description() {
        let cmd = MatrixCommand::Claim {
            task_id: "task-1".to_string(),
            actor: Some("erik".to_string()),
        };
        assert_eq!(cmd.description(), "Claim task 'task-1' for 'erik'");
    }

    #[test]
    fn test_case_insensitive() {
        let cmd = MatrixCommand::parse("CLAIM task-1").unwrap();
        assert!(matches!(cmd, MatrixCommand::Claim { .. }));

        let cmd = MatrixCommand::parse("Done TASK-1").unwrap();
        assert!(matches!(cmd, MatrixCommand::Done { .. }));
    }

    #[test]
    fn test_extract_localpart() {
        assert_eq!(extract_localpart("@user:server.com"), "user");
        assert_eq!(extract_localpart("plainuser"), "plainuser");
        assert_eq!(extract_localpart("@bot:matrix.org"), "bot");
    }
}

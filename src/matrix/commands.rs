//! Matrix command parser for workgraph
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
            MatrixCommand::Claim { task_id, actor } => {
                match actor {
                    Some(a) => format!("Claim task '{}' for '{}'", task_id, a),
                    None => format!("Claim task '{}'", task_id),
                }
            }
            MatrixCommand::Done { task_id } => format!("Mark task '{}' as done", task_id),
            MatrixCommand::Fail { task_id, reason } => {
                match reason {
                    Some(r) => format!("Mark task '{}' as failed: {}", task_id, r),
                    None => format!("Mark task '{}' as failed", task_id),
                }
            }
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
        "claim" | "done" | "fail" | "input" | "log" | "note" | "unclaim" | "release"
        | "status" | "ready" | "list" | "tasks" | "help" | "?"
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
}

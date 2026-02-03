//! Matrix command parser (re-exported from main matrix module pattern)
//!
//! This is a copy of the commands module to avoid feature-flag complexity.
//! The command parser has no SDK dependencies.

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
    pub fn parse(message: &str) -> Option<Self> {
        let message = message.trim();
        if message.is_empty() {
            return None;
        }

        let stripped = strip_prefix(message);
        let words: Vec<&str> = stripped.split_whitespace().collect();
        if words.is_empty() {
            return None;
        }

        let command_word = words[0].to_lowercase();
        let has_prefix = stripped.len() < message.len();
        if !has_prefix && !is_known_command(&command_word) {
            return None;
        }

        Some(parse_command(&words))
    }
}

fn strip_prefix(message: &str) -> &str {
    let prefixes = ["!wg ", "/wg ", "wg ", "!wg: ", "/wg: ", "wg: "];
    for prefix in &prefixes {
        if let Some(rest) = message.strip_prefix(prefix) {
            return rest.trim();
        }
    }
    let lower = message.to_lowercase();
    for prefix in &prefixes {
        if lower.starts_with(prefix) {
            return message[prefix.len()..].trim();
        }
    }
    message
}

fn is_known_command(word: &str) -> bool {
    matches!(
        word,
        "claim" | "done" | "fail" | "input" | "log" | "note" | "unclaim" | "release"
        | "status" | "ready" | "list" | "tasks" | "help" | "?"
    )
}

fn parse_command(words: &[&str]) -> MatrixCommand {
    if words.is_empty() {
        return MatrixCommand::Unknown { command: "".to_string() };
    }

    let command = words[0].to_lowercase();
    match command.as_str() {
        "claim" => {
            if words.len() < 2 {
                return MatrixCommand::Unknown { command: "claim (missing task ID)".to_string() };
            }
            let task_id = words[1].to_string();
            let actor = parse_actor_arg(&words[2..]);
            MatrixCommand::Claim { task_id, actor }
        }
        "done" => {
            if words.len() < 2 {
                return MatrixCommand::Unknown { command: "done (missing task ID)".to_string() };
            }
            MatrixCommand::Done { task_id: words[1].to_string() }
        }
        "fail" => {
            if words.len() < 2 {
                return MatrixCommand::Unknown { command: "fail (missing task ID)".to_string() };
            }
            let task_id = words[1].to_string();
            let reason = if words.len() > 2 { Some(words[2..].join(" ")) } else { None };
            MatrixCommand::Fail { task_id, reason }
        }
        "input" | "log" | "note" => {
            if words.len() < 3 {
                return MatrixCommand::Unknown { command: format!("{} (missing task ID or text)", command) };
            }
            MatrixCommand::Input { task_id: words[1].to_string(), text: words[2..].join(" ") }
        }
        "unclaim" | "release" => {
            if words.len() < 2 {
                return MatrixCommand::Unknown { command: "unclaim (missing task ID)".to_string() };
            }
            MatrixCommand::Unclaim { task_id: words[1].to_string() }
        }
        "status" => MatrixCommand::Status,
        "ready" | "list" | "tasks" => MatrixCommand::Ready,
        "help" | "?" => MatrixCommand::Help,
        _ => MatrixCommand::Unknown { command: command.to_string() },
    }
}

fn parse_actor_arg(words: &[&str]) -> Option<String> {
    if words.is_empty() {
        return None;
    }
    if words.len() >= 2 && words[0].to_lowercase() == "as" {
        return Some(words[1].to_string());
    }
    if words.len() >= 2 && (words[0] == "--actor" || words[0] == "-a") {
        return Some(words[1].to_string());
    }
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

Prefix commands with `wg` if needed (e.g., `wg claim task-1`)"#.to_string()
}

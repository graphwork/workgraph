//! Matrix message listener for workgraph (lightweight version)
//!
//! Background listener that processes commands from Matrix rooms using
//! the lightweight reqwest-based client.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::config::MatrixConfig;
use crate::graph::{LogEntry, Status};
use crate::parser::{load_graph, save_graph};

use super::commands::{MatrixCommand, help_text};
use super::{IncomingMessage, MatrixClient};

/// Configuration for the Matrix listener
#[derive(Debug, Clone, Default)]
pub struct ListenerConfig {
    /// Rooms to listen to (if empty, listens to all joined rooms)
    pub rooms: Vec<String>,
    /// Whether to require a command prefix (wg, !wg, etc.)
    pub require_prefix: bool,
    /// User IDs to ignore (e.g., bots)
    pub ignore_users: Vec<String>,
}

/// Matrix message listener
pub struct MatrixListener {
    client: MatrixClient,
    workgraph_dir: PathBuf,
    config: ListenerConfig,
    allowed_rooms: HashSet<String>,
}

impl MatrixListener {
    /// Create a new Matrix listener
    pub async fn new(
        workgraph_dir: &Path,
        matrix_config: &MatrixConfig,
        listener_config: ListenerConfig,
    ) -> Result<Self> {
        let client = MatrixClient::new(workgraph_dir, matrix_config)
            .await
            .context("Failed to create Matrix client")?;

        // Parse allowed rooms
        let mut allowed_rooms = HashSet::new();
        for room in &listener_config.rooms {
            allowed_rooms.insert(room.clone());
        }

        // If no specific rooms configured but default_room is set, use that
        if allowed_rooms.is_empty() {
            if let Some(default_room) = &matrix_config.default_room {
                allowed_rooms.insert(default_room.clone());
            }
        }

        Ok(Self {
            client,
            workgraph_dir: workgraph_dir.to_path_buf(),
            config: listener_config,
            allowed_rooms,
        })
    }

    /// Get the underlying Matrix client
    pub fn client(&self) -> &MatrixClient {
        &self.client
    }

    /// Join configured rooms
    pub async fn join_rooms(&self) -> Result<()> {
        for room_id in &self.allowed_rooms {
            if let Err(e) = self.client.join_room(room_id).await {
                eprintln!("Warning: Failed to join room {}: {}", room_id, e);
            }
        }
        Ok(())
    }

    /// Run the listener loop
    pub async fn run(&mut self) -> Result<()> {
        // Register message handler
        let (mut rx, filter) = self.client.register_message_handler(true);

        // Do initial sync
        self.client.sync_once().await?;

        // Join configured rooms
        self.join_rooms().await?;

        println!("Matrix listener started (lite), waiting for messages...");

        loop {
            tokio::select! {
                // Run sync with filter - sends messages to rx
                sync_result = self.client.sync_once_with_filter(&filter) => {
                    if let Err(e) = sync_result {
                        eprintln!("Sync error: {}", e);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }

                // Process incoming messages
                msg = rx.recv() => {
                    match msg {
                        Some(msg) => {
                            if let Err(e) = self.handle_message(&msg).await {
                                eprintln!("Error handling message: {}", e);
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle a single incoming message
    async fn handle_message(&self, msg: &IncomingMessage) -> Result<()> {
        // Check if we should process this room
        if !self.allowed_rooms.is_empty() && !self.allowed_rooms.contains(&msg.room_id) {
            return Ok(());
        }

        // Check if we should ignore this user
        if self.config.ignore_users.iter().any(|u| u == &msg.sender) {
            return Ok(());
        }

        // Parse the command
        let command = match MatrixCommand::parse(&msg.body) {
            Some(cmd) => cmd,
            None => return Ok(()),
        };

        // Execute the command and get response
        let response = self.execute_command(&command, msg).await;

        // Send response back to room
        self.client.send_message(&msg.room_id, &response).await?;

        Ok(())
    }

    /// Execute a parsed command
    async fn execute_command(&self, command: &MatrixCommand, msg: &IncomingMessage) -> String {
        match command {
            MatrixCommand::Claim { task_id, actor } => {
                let actor_id = actor
                    .clone()
                    .unwrap_or_else(|| extract_localpart(&msg.sender));
                self.execute_claim(task_id, Some(&actor_id))
            }
            MatrixCommand::Done { task_id } => self.execute_done(task_id),
            MatrixCommand::Fail { task_id, reason } => {
                self.execute_fail(task_id, reason.as_deref())
            }
            MatrixCommand::Input { task_id, text } => {
                let actor = extract_localpart(&msg.sender);
                self.execute_input(task_id, text, &actor)
            }
            MatrixCommand::Unclaim { task_id } => self.execute_unclaim(task_id),
            MatrixCommand::Status => self.execute_status(),
            MatrixCommand::Ready => self.execute_ready(),
            MatrixCommand::Help => help_text(),
            MatrixCommand::Unknown { command } => {
                format!(
                    "Unknown command: '{}'. Type 'help' for available commands.",
                    command
                )
            }
        }
    }

    fn execute_claim(&self, task_id: &str, actor: Option<&str>) -> String {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
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
            Status::InProgress => {
                let holder = task
                    .assigned
                    .as_ref()
                    .map(|a| format!(" by {}", a))
                    .unwrap_or_default();
                return format!("Task '{}' is already claimed{}", task_id, holder);
            }
            Status::Done => return format!("Task '{}' is already done", task_id),
            _ => {}
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

    fn execute_done(&self, task_id: &str) -> String {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
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

    fn execute_fail(&self, task_id: &str, reason: Option<&str>) -> String {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
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

    fn execute_input(&self, task_id: &str, text: &str, actor: &str) -> String {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
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

    fn execute_unclaim(&self, task_id: &str) -> String {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
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

    fn execute_status(&self) -> String {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
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

    fn execute_ready(&self) -> String {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        if !graph_path.exists() {
            return "Error: Workgraph not initialized".to_string();
        }

        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(e) => return format!("Error loading graph: {}", e),
        };

        let ready_tasks: Vec<_> = graph
            .tasks()
            .filter(|t| {
                t.status == Status::Open
                    && t.blocked_by.iter().all(|dep| {
                        graph
                            .get_task(dep)
                            .map(|d| d.status == Status::Done)
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
}

/// Extract the localpart from a Matrix user ID
fn extract_localpart(user_id: &str) -> String {
    // "@user:server.com" -> "user"
    user_id
        .strip_prefix('@')
        .and_then(|s| s.split(':').next())
        .unwrap_or(user_id)
        .to_string()
}

/// Run the Matrix listener as a standalone process
pub async fn run_listener(workgraph_dir: &Path) -> Result<()> {
    let matrix_config = MatrixConfig::load().context("Failed to load Matrix config")?;

    if !matrix_config.has_credentials() {
        anyhow::bail!("Matrix not configured. Run 'wg config --matrix' to set up credentials.");
    }

    let listener_config = ListenerConfig::default();

    let mut listener = MatrixListener::new(workgraph_dir, &matrix_config, listener_config)
        .await
        .context("Failed to create Matrix listener")?;

    listener.run().await
}

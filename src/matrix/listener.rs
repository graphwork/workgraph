//! Matrix message listener for workgraph
//!
//! Background listener that processes commands from Matrix rooms:
//! - Listens to configured room(s) for commands
//! - Parses human responses: 'claim <task>', 'done <task>', 'input <task> <text>'
//! - Updates workgraph accordingly
//! - Sends confirmation back to room
//!
//! Command execution logic is shared via `matrix_commands`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use matrix_sdk::ruma::OwnedRoomId;

use crate::config::MatrixConfig;
use crate::matrix_commands;

use super::commands::MatrixCommand;
use super::{IncomingMessage, MatrixClient};

/// Configuration for the Matrix listener
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    /// Rooms to listen to (if empty, listens to all joined rooms)
    pub rooms: Vec<String>,
    /// Whether to require a command prefix (wg, !wg, etc.)
    pub require_prefix: bool,
    /// User IDs to ignore (e.g., bots)
    pub ignore_users: Vec<String>,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            rooms: vec![],
            require_prefix: false,
            ignore_users: vec![],
        }
    }
}

/// Matrix message listener
pub struct MatrixListener {
    client: MatrixClient,
    workgraph_dir: PathBuf,
    config: ListenerConfig,
    allowed_rooms: HashSet<OwnedRoomId>,
}

impl MatrixListener {
    /// Create a new Matrix listener
    ///
    /// # Arguments
    /// * `workgraph_dir` - Path to the .workgraph directory
    /// * `matrix_config` - Matrix credentials and settings
    /// * `listener_config` - Listener-specific configuration
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
            if let Ok(room_id) = room.parse() {
                allowed_rooms.insert(room_id);
            }
        }

        // If no specific rooms configured but default_room is set, use that
        if allowed_rooms.is_empty() {
            if let Some(default_room) = &matrix_config.default_room {
                if let Ok(room_id) = default_room.parse() {
                    allowed_rooms.insert(room_id);
                }
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
            if let Err(e) = self.client.join_room(room_id.as_str()).await {
                eprintln!("Warning: Failed to join room {}: {}", room_id, e);
            }
        }
        Ok(())
    }

    /// Run the listener loop
    ///
    /// This method runs forever, processing incoming messages and executing commands.
    /// Uses `tokio::select!` to run sync and message handling concurrently - the sync
    /// must run in the same async runtime for event handlers to work correctly.
    pub async fn run(&self) -> Result<()> {
        // Register message handler - this creates a channel that receives messages
        // when the sync processes them
        let mut rx = self.client.register_message_handler(true);

        // Do initial sync to get current state
        self.client.sync_once().await?;

        // Join configured rooms
        self.join_rooms().await?;

        println!("Matrix listener started, waiting for messages...");

        // Clone the client for the sync task
        let client = self.client.inner().clone();

        // Use tokio::select! to run sync and message handling concurrently
        loop {
            tokio::select! {
                // Run a sync cycle - this will trigger event handlers which send to rx
                sync_result = client.sync_once(matrix_sdk::config::SyncSettings::default().timeout(std::time::Duration::from_secs(30))) => {
                    if let Err(e) = sync_result {
                        eprintln!("Sync error: {}", e);
                        // Brief pause before retrying on error
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }

                // Process incoming messages from the channel
                msg = rx.recv() => {
                    match msg {
                        Some(msg) => {
                            if let Err(e) = self.handle_message(&msg).await {
                                eprintln!("Error handling message: {}", e);
                            }
                        }
                        None => {
                            // Channel closed, exit
                            break;
                        }
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
        if self
            .config
            .ignore_users
            .iter()
            .any(|u| u == msg.sender.as_str())
        {
            return Ok(());
        }

        // Parse the command
        let command = match MatrixCommand::parse(&msg.body) {
            Some(cmd) => cmd,
            None => return Ok(()), // Not a command, ignore
        };

        // Execute via shared logic
        let response =
            matrix_commands::execute_command(&self.workgraph_dir, &command, msg.sender.as_str());

        // Send response back to room
        self.client
            .send_message(msg.room_id.as_str(), &response)
            .await?;

        Ok(())
    }
}

/// Run the Matrix listener as a standalone process
pub async fn run_listener(workgraph_dir: &Path) -> Result<()> {
    let matrix_config = MatrixConfig::load().context("Failed to load Matrix config")?;

    if !matrix_config.has_credentials() {
        anyhow::bail!("Matrix not configured. Run 'wg config --matrix' to set up credentials.");
    }

    let listener_config = ListenerConfig::default();

    let listener = MatrixListener::new(workgraph_dir, &matrix_config, listener_config)
        .await
        .context("Failed to create Matrix listener")?;

    listener.run().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_listener_config_default() {
        let config = ListenerConfig::default();
        assert!(config.rooms.is_empty());
        assert!(!config.require_prefix);
        assert!(config.ignore_users.is_empty());
    }
}

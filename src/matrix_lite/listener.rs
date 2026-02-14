//! Matrix message listener for workgraph (lightweight version)
//!
//! Background listener that processes commands from Matrix rooms using
//! the lightweight reqwest-based client. Command execution logic is shared
//! via `matrix_commands`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::MatrixConfig;
use crate::matrix_commands;

use super::commands::MatrixCommand;
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
        if allowed_rooms.is_empty()
            && let Some(default_room) = &matrix_config.default_room
        {
            allowed_rooms.insert(default_room.clone());
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

        // Execute via shared logic
        let response = matrix_commands::execute_command(&self.workgraph_dir, &command, &msg.sender);

        // Send response back to room
        self.client.send_message(&msg.room_id, &response).await?;

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

    let mut listener = MatrixListener::new(workgraph_dir, &matrix_config, listener_config)
        .await
        .context("Failed to create Matrix listener")?;

    listener.run().await
}

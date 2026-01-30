//! Matrix commands for workgraph CLI
//!
//! Provides commands for interacting with Matrix:
//! - `wg matrix listen` - Start the Matrix message listener
//! - `wg matrix send` - Send a message to the configured room

use anyhow::{Context, Result};
use std::path::Path;
use tokio::runtime::Runtime;

use workgraph::config::MatrixConfig;
use workgraph::{ListenerConfig, MatrixClient, MatrixListener};

/// Run the Matrix listener
///
/// This starts a background process that listens for commands in configured Matrix rooms
/// and executes them against the workgraph.
pub fn run_listen(dir: &Path, room: Option<&str>) -> Result<()> {
    let matrix_config = MatrixConfig::load().context("Failed to load Matrix config")?;

    if !matrix_config.has_credentials() {
        anyhow::bail!(
            "Matrix not configured. Run 'wg config --matrix' to set up credentials.\n\n\
             Example:\n  \
             wg config --homeserver https://matrix.org \\\n    \
             --username @user:matrix.org \\\n    \
             --access-token syt_... \\\n    \
             --room '!roomid:matrix.org'"
        );
    }

    // Build listener config
    let mut listener_config = ListenerConfig::default();
    if let Some(room_id) = room {
        listener_config.rooms.push(room_id.to_string());
    } else if let Some(default_room) = &matrix_config.default_room {
        listener_config.rooms.push(default_room.clone());
    }

    if listener_config.rooms.is_empty() {
        anyhow::bail!(
            "No room specified. Use --room or configure a default room:\n  \
             wg config --room '!roomid:matrix.org'"
        );
    }

    println!("Starting Matrix listener...");
    println!("Listening in rooms: {:?}", listener_config.rooms);
    println!("Press Ctrl+C to stop\n");

    // Create tokio runtime and run the listener
    let rt = Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let listener = MatrixListener::new(dir, &matrix_config, listener_config)
            .await
            .context("Failed to create Matrix listener")?;

        listener.run().await
    })
}

/// Send a message to a Matrix room
pub fn run_send(dir: &Path, room: Option<&str>, message: &str) -> Result<()> {
    let matrix_config = MatrixConfig::load().context("Failed to load Matrix config")?;

    if !matrix_config.has_credentials() {
        anyhow::bail!(
            "Matrix not configured. Run 'wg config --matrix' to set up credentials."
        );
    }

    let room_id = room
        .map(String::from)
        .or_else(|| matrix_config.default_room.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No room specified. Use --room or configure a default room:\n  \
                 wg config --room '!roomid:matrix.org'"
            )
        })?;

    // Create tokio runtime and send the message
    let rt = Runtime::new().context("Failed to create async runtime")?;

    rt.block_on(async {
        let client = MatrixClient::new(dir, &matrix_config)
            .await
            .context("Failed to create Matrix client")?;

        // Do initial sync
        client.sync_once().await?;

        // Join the room if not already joined
        if let Err(e) = client.join_room(&room_id).await {
            eprintln!("Warning: Failed to join room: {}", e);
        }

        // Send the message
        client.send_message(&room_id, message).await?;

        println!("Message sent to {}", room_id);
        Ok(())
    })
}

/// Show Matrix connection status
pub fn run_status(dir: &Path, json: bool) -> Result<()> {
    let matrix_config = MatrixConfig::load().context("Failed to load Matrix config")?;

    if json {
        let status = serde_json::json!({
            "configured": matrix_config.has_credentials(),
            "homeserver": matrix_config.homeserver_url,
            "username": matrix_config.username,
            "default_room": matrix_config.default_room,
            "has_access_token": matrix_config.access_token.is_some(),
            "has_password": matrix_config.password.is_some(),
        });
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    if !matrix_config.has_credentials() {
        println!("Matrix: not configured");
        println!("\nRun 'wg config --matrix' to set up credentials.");
        return Ok(());
    }

    println!("Matrix: configured");
    if let Some(ref hs) = matrix_config.homeserver_url {
        println!("  Homeserver: {}", hs);
    }
    if let Some(ref user) = matrix_config.username {
        println!("  Username: {}", user);
    }
    if let Some(ref room) = matrix_config.default_room {
        println!("  Default room: {}", room);
    }
    println!(
        "  Auth: {}",
        if matrix_config.access_token.is_some() {
            "access token"
        } else {
            "password"
        }
    );

    // Try to connect and check if we can log in
    let rt = Runtime::new().context("Failed to create async runtime")?;
    let connected = rt.block_on(async {
        match MatrixClient::new(dir, &matrix_config).await {
            Ok(client) => {
                if client.is_logged_in() {
                    if let Some(user_id) = client.user_id() {
                        println!("  Status: connected as {}", user_id);
                    } else {
                        println!("  Status: connected");
                    }
                    true
                } else {
                    println!("  Status: not connected");
                    false
                }
            }
            Err(e) => {
                println!("  Status: connection failed ({})", e);
                false
            }
        }
    });

    if !connected {
        println!("\nTip: Check your credentials with 'wg config --matrix'");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Tests would require Matrix test infrastructure
}

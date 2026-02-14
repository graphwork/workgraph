//! Lightweight Matrix client using reqwest
//!
//! A minimal Matrix client (~150 lines) that provides:
//! - Send messages (plain text and HTML)
//! - Receive messages via sync long-polling
//! - Join rooms
//!
//! No E2EE, no SQLite, no heavy dependencies.

pub mod commands;
pub mod listener;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::MatrixConfig;

/// State directory name within .workgraph
const MATRIX_STATE_DIR: &str = "matrix";

/// Incoming Matrix message from a room
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// Room the message was received in
    pub room_id: String,
    /// Sender's user ID
    pub sender: String,
    /// Message body text
    pub body: String,
    /// Event ID
    pub event_id: String,
    /// Whether the message is from the current user
    pub is_own: bool,
}

/// Lightweight Matrix client using HTTP
pub struct MatrixClient {
    http: HttpClient,
    homeserver_url: String,
    access_token: String,
    user_id: String,
    #[allow(dead_code)]
    workgraph_dir: PathBuf,
    /// Sync token for incremental sync
    sync_token: Option<String>,
}

/// Login response from Matrix API
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct LoginResponse {
    access_token: String,
    user_id: String,
    device_id: String,
}

impl MatrixClient {
    /// Create a new Matrix client
    ///
    /// Authentication priority:
    /// 1. Cached access token from disk (from previous login)
    /// 2. access_token from config
    /// 3. Login with password to get new token
    pub async fn new(workgraph_dir: &Path, config: &MatrixConfig) -> Result<Self> {
        let homeserver_url = config
            .homeserver_url
            .as_ref()
            .context("homeserver_url is required")?
            .trim_end_matches('/')
            .to_string();

        let user_id = config
            .username
            .as_ref()
            .context("username is required")?
            .clone();

        let state_dir = workgraph_dir.join(MATRIX_STATE_DIR);
        std::fs::create_dir_all(&state_dir)?;

        let http = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;

        // Try to get access token: cached > config > login
        let access_token = if let Some(cached) = Self::load_access_token(&state_dir) {
            cached
        } else if let Some(token) = &config.access_token {
            // Save config token as cached for future use
            Self::save_access_token_static(&state_dir, token);
            token.clone()
        } else if let Some(password) = &config.password {
            // Login with password
            let token = Self::login(&http, &homeserver_url, &user_id, password).await?;
            Self::save_access_token_static(&state_dir, &token);
            token
        } else {
            anyhow::bail!(
                "No access_token or password configured. Set one in ~/.config/workgraph/matrix.toml"
            );
        };

        // Try to load sync token from disk
        let sync_token = Self::load_sync_token(&state_dir);

        Ok(Self {
            http,
            homeserver_url,
            access_token,
            user_id,
            workgraph_dir: workgraph_dir.to_path_buf(),
            sync_token,
        })
    }

    /// Login with username and password to get an access token
    async fn login(
        http: &HttpClient,
        homeserver: &str,
        user_id: &str,
        password: &str,
    ) -> Result<String> {
        // Extract localpart from user_id (@user:server -> user)
        let localpart = user_id
            .strip_prefix('@')
            .and_then(|s| s.split(':').next())
            .unwrap_or(user_id);

        let url = format!("{}/_matrix/client/v3/login", homeserver);

        let body = serde_json::json!({
            "type": "m.login.password",
            "identifier": {
                "type": "m.id.user",
                "user": localpart
            },
            "password": password,
            "initial_device_display_name": "workgraph"
        });

        let resp = http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Login request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Login failed: {} - {}", status, body);
        }

        let login_resp: LoginResponse = resp
            .json()
            .await
            .context("Failed to parse login response")?;
        Ok(login_resp.access_token)
    }

    fn load_access_token(state_dir: &Path) -> Option<String> {
        let path = state_dir.join("access_token");
        std::fs::read_to_string(path).ok().filter(|s| !s.is_empty())
    }

    fn save_access_token_static(state_dir: &Path, token: &str) {
        let path = state_dir.join("access_token");
        let _ = std::fs::write(path, token);
    }

    #[allow(dead_code)]
    fn save_access_token(&self) {
        Self::save_access_token_static(&self.state_dir(), &self.access_token);
    }

    /// Create a new client by logging in with password (ignores cached/config tokens)
    pub async fn login_with_password(workgraph_dir: &Path, config: &MatrixConfig) -> Result<Self> {
        let homeserver_url = config
            .homeserver_url
            .as_ref()
            .context("homeserver_url is required")?
            .trim_end_matches('/')
            .to_string();

        let user_id = config
            .username
            .as_ref()
            .context("username is required")?
            .clone();
        let password = config
            .password
            .as_ref()
            .context("password is required for login")?;

        let state_dir = workgraph_dir.join(MATRIX_STATE_DIR);
        std::fs::create_dir_all(&state_dir)?;

        let http = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;

        // Force login with password, ignoring any cached or config tokens
        let access_token = Self::login(&http, &homeserver_url, &user_id, password).await?;
        Self::save_access_token_static(&state_dir, &access_token);

        let sync_token = Self::load_sync_token(&state_dir);

        Ok(Self {
            http,
            homeserver_url,
            access_token,
            user_id,
            workgraph_dir: workgraph_dir.to_path_buf(),
            sync_token,
        })
    }

    /// Clear cached credentials (forces re-login on next use)
    pub fn clear_cache(workgraph_dir: &Path) {
        let state_dir = workgraph_dir.join(MATRIX_STATE_DIR);
        let _ = std::fs::remove_file(state_dir.join("access_token"));
        let _ = std::fs::remove_file(state_dir.join("sync_token"));
    }

    fn state_dir(&self) -> PathBuf {
        self.workgraph_dir.join(MATRIX_STATE_DIR)
    }

    fn load_sync_token(state_dir: &Path) -> Option<String> {
        let path = state_dir.join("sync_token");
        std::fs::read_to_string(path).ok()
    }

    fn save_sync_token(&self) {
        if let Some(token) = &self.sync_token {
            let path = self.state_dir().join("sync_token");
            let _ = std::fs::write(path, token);
        }
    }

    /// Check if the client is logged in (has access token)
    pub fn is_logged_in(&self) -> bool {
        !self.access_token.is_empty()
    }

    /// Get the user ID
    pub fn user_id(&self) -> Option<&str> {
        Some(&self.user_id)
    }

    /// Join a room by room ID or alias
    pub async fn join_room(&self, room_id_or_alias: &str) -> Result<()> {
        let url = format!(
            "{}/_matrix/client/v3/join/{}",
            self.homeserver_url,
            urlencoding::encode(room_id_or_alias)
        );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({}))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to join room: {} - {}", status, body);
        }

        Ok(())
    }

    /// Send a text message to a room
    pub async fn send_message(&self, room_id: &str, message: &str) -> Result<()> {
        self.send_event(
            room_id,
            "m.room.message",
            serde_json::json!({
                "msgtype": "m.text",
                "body": message
            }),
        )
        .await
    }

    /// Send an HTML message to a room
    pub async fn send_html_message(
        &self,
        room_id: &str,
        plain_text: &str,
        html: &str,
    ) -> Result<()> {
        self.send_event(
            room_id,
            "m.room.message",
            serde_json::json!({
                "msgtype": "m.text",
                "body": plain_text,
                "format": "org.matrix.custom.html",
                "formatted_body": html
            }),
        )
        .await
    }

    /// Send an event to a room
    async fn send_event(
        &self,
        room_id: &str,
        event_type: &str,
        content: serde_json::Value,
    ) -> Result<()> {
        let txn_id = format!(
            "wg_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/{}/{}",
            self.homeserver_url,
            urlencoding::encode(room_id),
            urlencoding::encode(event_type),
            urlencoding::encode(&txn_id)
        );

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&content)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Failed to send message: {} - {}", status, body);
        }

        Ok(())
    }

    /// Register a message handler and return a receiver
    ///
    /// Note: Unlike the full matrix-sdk, this doesn't use event handlers.
    /// Instead, call sync_once_with_filter() and messages will be sent to the returned channel.
    pub fn register_message_handler(
        &self,
        filter_own: bool,
    ) -> (mpsc::Receiver<IncomingMessage>, MessageFilter) {
        let (tx, rx) = mpsc::channel(100);
        let filter = MessageFilter {
            tx,
            own_user_id: self.user_id.clone(),
            filter_own,
        };
        (rx, filter)
    }

    /// Run a single sync cycle
    pub async fn sync_once(&mut self) -> Result<()> {
        self.sync_once_with_timeout(30).await
    }

    /// Run a single sync cycle with custom timeout
    pub async fn sync_once_with_timeout(&mut self, timeout_secs: u64) -> Result<()> {
        let mut url = format!(
            "{}/_matrix/client/v3/sync?timeout={}",
            self.homeserver_url,
            timeout_secs * 1000
        );

        if let Some(token) = &self.sync_token {
            url.push_str(&format!("&since={}", urlencoding::encode(token)));
        }

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Sync request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Sync failed: {} - {}", status, body);
        }

        let sync_resp: SyncResponse = resp.json().await.context("Failed to parse sync response")?;
        self.sync_token = Some(sync_resp.next_batch);
        self.save_sync_token();

        Ok(())
    }

    /// Run sync and process messages into the filter's channel
    pub async fn sync_once_with_filter(&mut self, filter: &MessageFilter) -> Result<()> {
        let mut url = format!(
            "{}/_matrix/client/v3/sync?timeout=30000",
            self.homeserver_url
        );

        if let Some(token) = &self.sync_token {
            url.push_str(&format!("&since={}", urlencoding::encode(token)));
        }

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Sync request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Sync failed: {} - {}", status, body);
        }

        let sync_resp: SyncResponse = resp.json().await.context("Failed to parse sync response")?;

        // Process room events
        if let Some(rooms) = sync_resp.rooms
            && let Some(join) = rooms.join
        {
            for (room_id, room_data) in join {
                if let Some(timeline) = room_data.timeline {
                    for event in timeline.events {
                        if event.event_type == "m.room.message"
                            && let Some(content) = event.content
                        {
                            let body = content.body.unwrap_or_default();
                            let is_own = event.sender == filter.own_user_id;

                            if filter.filter_own && is_own {
                                continue;
                            }

                            let msg = IncomingMessage {
                                room_id: room_id.clone(),
                                sender: event.sender,
                                body,
                                event_id: event.event_id.unwrap_or_default(),
                                is_own,
                            };

                            let _ = filter.tx.send(msg).await;
                        }
                    }
                }
            }
        }

        self.sync_token = Some(sync_resp.next_batch);
        self.save_sync_token();

        Ok(())
    }
}

/// Message filter configuration
pub struct MessageFilter {
    tx: mpsc::Sender<IncomingMessage>,
    own_user_id: String,
    filter_own: bool,
}

// Matrix sync response types (minimal)
#[derive(Debug, Deserialize)]
struct SyncResponse {
    next_batch: String,
    rooms: Option<RoomsResponse>,
}

#[derive(Debug, Deserialize)]
struct RoomsResponse {
    join: Option<HashMap<String, JoinedRoom>>,
}

#[derive(Debug, Deserialize)]
struct JoinedRoom {
    timeline: Option<Timeline>,
}

#[derive(Debug, Deserialize)]
struct Timeline {
    events: Vec<TimelineEvent>,
}

#[derive(Debug, Deserialize)]
struct TimelineEvent {
    #[serde(rename = "type")]
    event_type: String,
    sender: String,
    event_id: Option<String>,
    content: Option<MessageContent>,
}

#[derive(Debug, Deserialize)]
struct MessageContent {
    body: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    msgtype: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VerificationEvent {
    // Stub for API compatibility - not used in lite version
    Request {
        sender: String,
        transaction_id: String,
    },
    Started {
        sender: String,
    },
    EmojisReady {
        sender: String,
        emojis: Vec<String>,
    },
    Done {
        sender: String,
    },
    Cancelled {
        sender: String,
        reason: String,
    },
}

/// Send a notification to Matrix (one-shot, no persistent client)
///
/// This is a convenience function for sending quick notifications without
/// maintaining a persistent Matrix client. It loads config, sends the message,
/// and disconnects.
///
/// # Arguments
/// * `workgraph_dir` - Path to the .workgraph directory
/// * `message` - The notification message to send
pub async fn send_notification(workgraph_dir: &Path, message: &str) -> Result<()> {
    let config = MatrixConfig::load()?;

    if !config.is_complete() {
        anyhow::bail!(
            "Matrix not configured. Set homeserver, username, token, and room in ~/.config/workgraph/matrix.toml"
        );
    }

    let room_id = config.default_room.as_ref().unwrap();
    let client = MatrixClient::new(workgraph_dir, &config).await?;
    client.send_message(room_id, message).await?;

    Ok(())
}

/// Send a notification to Matrix with a specific room (one-shot)
pub async fn send_notification_to_room(
    workgraph_dir: &Path,
    room_id: &str,
    message: &str,
) -> Result<()> {
    let config = MatrixConfig::load()?;

    if !config.has_credentials() {
        anyhow::bail!(
            "Matrix not configured. Set homeserver, username, and token in ~/.config/workgraph/matrix.toml"
        );
    }

    let client = MatrixClient::new(workgraph_dir, &config).await?;
    client.send_message(room_id, message).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_incoming_message_clone() {
        let msg = IncomingMessage {
            room_id: "!test:example.com".to_string(),
            sender: "@user:example.com".to_string(),
            body: "Hello".to_string(),
            event_id: "$event".to_string(),
            is_own: false,
        };
        let cloned = msg.clone();
        assert_eq!(cloned.body, "Hello");
    }
}

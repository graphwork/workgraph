//! Matrix client wrapper for workgraph
//!
//! Provides a high-level interface for Matrix communication:
//! - Login and session management (with persistent session storage)
//! - Room joining and creation
//! - Message sending
//! - Async message listening via streams
//! - E2EE key verification handling
//! - Command parsing and message listener for interactive control
//!
//! State is stored in `.workgraph/matrix/` for session reuse.

pub mod commands;
pub mod listener;

use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result};
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::verification::Verification,
    room::Room,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
        },
        OwnedRoomId, OwnedRoomOrAliasId, OwnedUserId, RoomAliasId, RoomId, UserId,
    },
    Client, SessionMeta, SessionTokens,
};
use tokio::sync::mpsc;

use crate::config::MatrixConfig;

/// State directory name within .workgraph
const MATRIX_STATE_DIR: &str = "matrix";

/// Incoming Matrix message from a room
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// Room the message was received in
    pub room_id: OwnedRoomId,
    /// Sender's user ID
    pub sender: OwnedUserId,
    /// Message body text
    pub body: String,
    /// Event ID
    pub event_id: String,
    /// Whether the message is from the current user
    pub is_own: bool,
}

/// Matrix client wrapper
///
/// Wraps the matrix-sdk Client with workgraph-specific functionality
/// including session persistence and simplified API.
pub struct MatrixClient {
    /// The underlying matrix-sdk client
    client: Client,
    /// Path to the workgraph directory
    workgraph_dir: PathBuf,
    /// Our own user ID (after login)
    user_id: Option<OwnedUserId>,
}

impl MatrixClient {
    /// Create a new Matrix client
    ///
    /// # Arguments
    /// * `workgraph_dir` - Path to the .workgraph directory
    /// * `config` - Matrix configuration with credentials
    ///
    /// This will:
    /// 1. Try to restore a previous session from disk
    /// 2. If no session exists, log in with the provided credentials
    /// 3. Store the session for future reuse
    pub async fn new(workgraph_dir: &Path, config: &MatrixConfig) -> Result<Self> {
        let homeserver_url = config
            .homeserver_url
            .as_ref()
            .context("homeserver_url is required in Matrix config")?;

        let state_dir = workgraph_dir.join(MATRIX_STATE_DIR);
        std::fs::create_dir_all(&state_dir).context("Failed to create matrix state directory")?;

        // Build the client with SQLite state store for session/crypto persistence
        let client = Client::builder()
            .homeserver_url(homeserver_url)
            .sqlite_store(&state_dir, None)
            .build()
            .await
            .context("Failed to build Matrix client")?;

        let mut matrix_client = Self {
            client,
            workgraph_dir: workgraph_dir.to_path_buf(),
            user_id: None,
        };

        // Try to restore session or login
        matrix_client.ensure_logged_in(config).await?;

        Ok(matrix_client)
    }

    /// Get the state directory path
    fn state_dir(&self) -> PathBuf {
        self.workgraph_dir.join(MATRIX_STATE_DIR)
    }

    /// Ensure we're logged in, either by restoring session or logging in fresh
    async fn ensure_logged_in(&mut self, config: &MatrixConfig) -> Result<()> {
        // Check if we already have a session stored
        if self.client.matrix_auth().logged_in() {
            self.user_id = self.client.user_id().map(|u| u.to_owned());
            return Ok(());
        }

        // Need to login
        let username = config
            .username
            .as_ref()
            .context("username is required in Matrix config")?;

        if let Some(access_token) = &config.access_token {
            // Login with access token (preferred)
            self.login_with_token(username, access_token).await?;
        } else if let Some(password) = &config.password {
            // Login with password
            self.login_with_password(username, password).await?;
        } else {
            anyhow::bail!("Either access_token or password is required in Matrix config");
        }

        self.user_id = self.client.user_id().map(|u| u.to_owned());
        Ok(())
    }

    /// Login with access token
    async fn login_with_token(&self, user_id: &str, access_token: &str) -> Result<()> {
        let user_id = UserId::parse(user_id).context("Invalid user ID format")?;

        let session = MatrixSession {
            meta: SessionMeta {
                user_id: user_id.to_owned(),
                device_id: self.get_or_create_device_id()?,
            },
            tokens: SessionTokens {
                access_token: access_token.to_string(),
                refresh_token: None,
            },
        };

        self.client
            .restore_session(session)
            .await
            .context("Failed to restore session with access token")?;

        Ok(())
    }

    /// Get or create a device ID for this workgraph instance
    fn get_or_create_device_id(&self) -> Result<matrix_sdk::ruma::OwnedDeviceId> {
        let device_id_path = self.state_dir().join("device_id");

        if device_id_path.exists() {
            let device_id =
                std::fs::read_to_string(&device_id_path).context("Failed to read device ID")?;
            Ok(device_id.trim().into())
        } else {
            // Generate a new device ID
            let device_id: String = format!("workgraph_{}", uuid_v4_simple());
            std::fs::write(&device_id_path, &device_id).context("Failed to write device ID")?;
            Ok(device_id.into())
        }
    }

    /// Login with username and password
    async fn login_with_password(&self, username: &str, password: &str) -> Result<()> {
        let user_id = UserId::parse(username).context("Invalid user ID format")?;

        self.client
            .matrix_auth()
            .login_username(user_id.localpart(), password)
            .initial_device_display_name("workgraph")
            .await
            .context("Failed to login with password")?;

        Ok(())
    }

    /// Get the logged-in user ID
    pub fn user_id(&self) -> Option<&OwnedUserId> {
        self.user_id.as_ref()
    }

    /// Check if the client is logged in
    pub fn is_logged_in(&self) -> bool {
        self.client.matrix_auth().logged_in()
    }

    /// Get the underlying matrix-sdk Client
    pub fn inner(&self) -> &Client {
        &self.client
    }

    /// Join a room by room ID or alias
    ///
    /// # Arguments
    /// * `room_id_or_alias` - Room ID (e.g., "!roomid:server") or alias (e.g., "#room:server")
    pub async fn join_room(&self, room_id_or_alias: &str) -> Result<Room> {
        // Try to parse as room ID first
        if let Ok(room_id) = RoomId::parse(room_id_or_alias) {
            let room = self
                .client
                .join_room_by_id(&room_id)
                .await
                .context("Failed to join room by ID")?;
            return Ok(room);
        }

        // Try as alias
        let alias =
            RoomAliasId::parse(room_id_or_alias).context("Invalid room ID or alias format")?;
        let room_or_alias: OwnedRoomOrAliasId = alias.into();
        let room = self
            .client
            .join_room_by_id_or_alias(&room_or_alias, &[])
            .await
            .context("Failed to join room by alias")?;
        Ok(room)
    }

    /// Create a new room
    ///
    /// # Arguments
    /// * `name` - Optional room name
    /// * `alias` - Optional local alias (without the leading # and :server)
    /// * `invite` - List of user IDs to invite
    /// * `is_direct` - Whether this is a direct message room
    pub async fn create_room(
        &self,
        name: Option<&str>,
        alias: Option<&str>,
        invite: &[&str],
        is_direct: bool,
    ) -> Result<Room> {
        let mut request = CreateRoomRequest::new();

        if let Some(name) = name {
            request.name = Some(name.to_string());
        }

        if let Some(alias) = alias {
            request.room_alias_name = Some(alias.to_string());
        }

        let invite_ids: Result<Vec<_>> = invite
            .iter()
            .map(|u| UserId::parse(*u).context("Invalid user ID in invite list"))
            .collect();
        request.invite = invite_ids?;

        request.is_direct = is_direct;

        let room = self
            .client
            .create_room(request)
            .await
            .context("Failed to create room")?;

        Ok(room)
    }

    /// Get a room by ID
    pub fn get_room(&self, room_id: &str) -> Result<Option<Room>> {
        let room_id = RoomId::parse(room_id).context("Invalid room ID")?;
        Ok(self.client.get_room(&room_id))
    }

    /// Send a text message to a room
    ///
    /// # Arguments
    /// * `room_id` - The room ID to send to
    /// * `message` - The message text
    pub async fn send_message(&self, room_id: &str, message: &str) -> Result<()> {
        let room_id = RoomId::parse(room_id).context("Invalid room ID")?;
        let room = self
            .client
            .get_room(&room_id)
            .context("Room not found - are you joined?")?;

        let content = RoomMessageEventContent::text_plain(message);
        room.send(content).await.context("Failed to send message")?;

        Ok(())
    }

    /// Send a formatted (HTML) message to a room
    ///
    /// # Arguments
    /// * `room_id` - The room ID to send to
    /// * `plain_text` - Plain text fallback
    /// * `html` - HTML formatted message
    pub async fn send_html_message(
        &self,
        room_id: &str,
        plain_text: &str,
        html: &str,
    ) -> Result<()> {
        let room_id = RoomId::parse(room_id).context("Invalid room ID")?;
        let room = self
            .client
            .get_room(&room_id)
            .context("Room not found - are you joined?")?;

        let content = RoomMessageEventContent::text_html(plain_text, html);
        room.send(content).await.context("Failed to send message")?;

        Ok(())
    }

    /// Register message handlers and return a receiver for incoming messages
    ///
    /// This sets up event handlers but does NOT start the sync loop.
    /// You must call `sync_loop()` or `start_sync_thread()` separately to receive messages.
    ///
    /// # Arguments
    /// * `filter_own` - If true, filter out messages from the current user
    pub fn register_message_handler(&self, filter_own: bool) -> mpsc::Receiver<IncomingMessage> {
        let (tx, rx) = mpsc::channel(100);
        let own_user_id = self.user_id.clone();

        // Register message handler
        self.client.add_event_handler({
            move |event: OriginalSyncRoomMessageEvent, room: Room| {
                let tx = tx.clone();
                let own_user_id = own_user_id.clone();
                async move {
                    let is_own = own_user_id
                        .as_ref()
                        .map(|id| id == &event.sender)
                        .unwrap_or(false);

                    if filter_own && is_own {
                        return;
                    }

                    // Extract message body
                    let body = match &event.content.msgtype {
                        MessageType::Text(text) => text.body.clone(),
                        MessageType::Notice(notice) => notice.body.clone(),
                        _ => return, // Ignore non-text messages
                    };

                    let msg = IncomingMessage {
                        room_id: room.room_id().to_owned(),
                        sender: event.sender.clone(),
                        body,
                        event_id: event.event_id.to_string(),
                        is_own,
                    };

                    let _ = tx.send(msg).await;
                }
            }
        });

        rx
    }

    /// Run the sync loop continuously
    ///
    /// This method runs forever and must be awaited. It will sync with the
    /// server and dispatch events to registered handlers.
    ///
    /// For background sync, use `start_sync_thread()` instead.
    pub async fn sync_loop(&self) -> Result<()> {
        self.client
            .sync(SyncSettings::default())
            .await
            .context("Sync loop failed")?;
        Ok(())
    }

    /// Start sync in a background OS thread
    ///
    /// This spawns a new OS thread with its own tokio runtime to run the sync loop.
    /// Use this when you need background sync but can't await `sync_loop()`.
    ///
    /// Returns a handle to the thread.
    pub fn start_sync_thread(&self) -> thread::JoinHandle<()> {
        let client = self.client.clone();

        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            rt.block_on(async {
                let _ = client.sync(SyncSettings::default()).await;
            });
        })
    }

    /// Run a single sync cycle
    ///
    /// Useful for one-shot operations where you don't need continuous sync.
    pub async fn sync_once(&self) -> Result<()> {
        self.client
            .sync_once(SyncSettings::default())
            .await
            .context("Sync failed")?;
        Ok(())
    }

    /// Handle E2EE key verification
    ///
    /// This sets up handlers for incoming verification requests.
    /// When a verification request comes in, it will auto-accept and
    /// complete the verification using SAS (emoji comparison).
    ///
    /// Returns a receiver that yields verification events for manual handling
    /// if needed.
    pub fn setup_verification_handler(&self) -> mpsc::Receiver<VerificationEvent> {
        let (tx, rx) = mpsc::channel(10);
        let client = self.client.clone();

        // Handle verification requests
        client.add_event_handler({
            let tx = tx.clone();
            let client = client.clone();
            move |event: matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent| {
                let tx = tx.clone();
                let client = client.clone();
                async move {
                    if let Some(request) = client
                        .encryption()
                        .get_verification_request(&event.sender, event.content.transaction_id.as_str())
                        .await
                    {
                        let _ = tx.send(VerificationEvent::Request {
                            sender: event.sender.to_string(),
                            transaction_id: event.content.transaction_id.to_string(),
                        }).await;

                        // Auto-accept the request
                        if let Err(e) = request.accept().await {
                            eprintln!("Failed to accept verification: {}", e);
                        }
                    }
                }
            }
        });

        // Handle SAS verification start
        client.add_event_handler({
            let tx = tx.clone();
            let client = client.clone();
            move |event: matrix_sdk::ruma::events::key::verification::start::ToDeviceKeyVerificationStartEvent| {
                let tx = tx.clone();
                let client = client.clone();
                async move {
                    if let Some(Verification::SasV1(sas)) = client
                        .encryption()
                        .get_verification(&event.sender, event.content.transaction_id.as_str())
                        .await
                    {
                        let _ = tx.send(VerificationEvent::Started {
                            sender: event.sender.to_string(),
                        }).await;

                        // Accept the SAS verification
                        if let Err(e) = sas.accept().await {
                            eprintln!("Failed to accept SAS: {}", e);
                        }
                    }
                }
            }
        });

        rx
    }

    /// Accept a SAS verification and confirm emojis match
    ///
    /// This should be called after the user has confirmed the emojis match.
    pub async fn confirm_verification(&self, user_id: &str, flow_id: &str) -> Result<()> {
        let user_id = UserId::parse(user_id).context("Invalid user ID")?;

        if let Some(Verification::SasV1(sas)) = self
            .client
            .encryption()
            .get_verification(&user_id, flow_id)
            .await
        {
            sas.confirm()
                .await
                .context("Failed to confirm verification")?;
        } else {
            anyhow::bail!("Verification not found");
        }

        Ok(())
    }

    /// Get emojis for SAS verification
    pub async fn get_verification_emojis(
        &self,
        user_id: &str,
        flow_id: &str,
    ) -> Result<Option<Vec<String>>> {
        let user_id = UserId::parse(user_id).context("Invalid user ID")?;

        if let Some(Verification::SasV1(sas)) = self
            .client
            .encryption()
            .get_verification(&user_id, flow_id)
            .await
        {
            if let Some(emojis) = sas.emoji() {
                return Ok(Some(emojis.iter().map(|e| e.symbol.to_string()).collect()));
            }
        }

        Ok(None)
    }

    /// Logout and clear session
    pub async fn logout(&self) -> Result<()> {
        self.client
            .matrix_auth()
            .logout()
            .await
            .context("Failed to logout")?;
        Ok(())
    }
}

/// Events from E2EE verification process
#[derive(Debug, Clone)]
pub enum VerificationEvent {
    /// Verification request received
    Request {
        sender: String,
        transaction_id: String,
    },
    /// SAS verification started
    Started { sender: String },
    /// Emojis are ready for comparison
    EmojisReady { sender: String, emojis: Vec<String> },
    /// Verification completed
    Done { sender: String },
    /// Verification cancelled
    Cancelled { sender: String, reason: String },
}

/// Generate a simple UUID v4-like string
fn uuid_v4_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let random = std::process::id() as u128 ^ now;
    format!("{:032x}", random)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uuid_generation() {
        let uuid1 = uuid_v4_simple();
        // UUIDs should be 32 hex chars
        assert_eq!(uuid1.len(), 32);
        assert!(uuid1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

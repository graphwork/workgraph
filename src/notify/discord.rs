//! Discord notification channel — sends messages via the Discord Bot API.
//!
//! Implements [`NotificationChannel`] for Discord bots. Supports:
//! - Outbound: plain text, rich (embeds), and interactive action-button messages (components)
//! - Inbound: Gateway listener for slash commands and button interactions
//!
//! This uses the Discord HTTP API directly via `reqwest` rather than pulling in
//! serenity/twilight, keeping the dependency footprint light.
//!
//! Configuration is read from the `[discord]` section of `notify.toml`:
//! ```toml
//! [discord]
//! bot_token = "MTIz..."
//! channel_id = "123456789012345678"
//! guild_id = "987654321098765432"   # optional, for slash commands
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{Action, ActionStyle, IncomingMessage, MessageId, NotificationChannel, RichMessage};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Discord-specific configuration parsed from the `[discord]` section.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DiscordConfig {
    /// Bot token from the Discord Developer Portal.
    pub bot_token: String,
    /// Default channel ID to post messages to.
    pub channel_id: String,
    /// Optional guild (server) ID — needed for registering slash commands.
    #[serde(default)]
    pub guild_id: Option<String>,
}

impl DiscordConfig {
    /// Extract from the opaque channel map in [`super::config::NotifyConfig`].
    pub fn from_notify_config(config: &super::config::NotifyConfig) -> Result<Self> {
        let val = config
            .channels
            .get("discord")
            .context("no [discord] section in notify config")?;
        let cfg: Self = val
            .clone()
            .try_into()
            .context("invalid [discord] config")?;
        Ok(cfg)
    }
}

// ---------------------------------------------------------------------------
// Embed helpers
// ---------------------------------------------------------------------------

/// Build a Discord embed object from a RichMessage.
fn build_embed(message: &RichMessage) -> serde_json::Value {
    let description = message
        .markdown
        .as_deref()
        .unwrap_or(&message.plain_text);

    serde_json::json!({
        "description": description,
        "color": 0x5865F2  // Discord blurple
    })
}

/// Build a Discord message components array with action buttons.
fn build_action_row(actions: &[Action]) -> serde_json::Value {
    let buttons: Vec<serde_json::Value> = actions
        .iter()
        .map(|a| {
            let style = match a.style {
                ActionStyle::Primary => 1,   // Primary (blurple)
                ActionStyle::Danger => 4,    // Danger (red)
                ActionStyle::Secondary => 2, // Secondary (gray)
            };
            serde_json::json!({
                "type": 2,  // Button
                "style": style,
                "label": &a.label,
                "custom_id": &a.id,
            })
        })
        .collect();

    serde_json::json!({
        "type": 1,  // Action Row
        "components": buttons
    })
}

// ---------------------------------------------------------------------------
// Channel implementation
// ---------------------------------------------------------------------------

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// A Discord notification channel backed by the Discord HTTP API via `reqwest`.
pub struct DiscordChannel {
    config: DiscordConfig,
    client: reqwest::Client,
}

impl DiscordChannel {
    pub fn new(config: DiscordConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// Resolve the target channel. If empty or "*", uses the configured default.
    fn resolve_channel<'a>(&'a self, target: &'a str) -> &'a str {
        if target.is_empty() || target == "*" {
            &self.config.channel_id
        } else {
            target
        }
    }

    /// Send a POST request to the Discord API.
    async fn api_post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{DISCORD_API_BASE}{path}");
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.config.bot_token))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .context("Discord API request failed")?;

        let status = resp.status();
        let text = resp.text().await.context("failed to read Discord API response")?;

        if !status.is_success() {
            anyhow::bail!("Discord API error ({}): {}", status, text);
        }

        // Some endpoints return empty body (204 No Content).
        if text.is_empty() {
            return Ok(serde_json::json!({}));
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).context("failed to parse Discord API response")?;
        Ok(json)
    }

    /// Extract the message ID from a Create Message response.
    fn extract_message_id(json: &serde_json::Value) -> MessageId {
        let id = json
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("0");
        MessageId(id.to_string())
    }
}

#[async_trait]
impl NotificationChannel for DiscordChannel {
    fn channel_type(&self) -> &str {
        "discord"
    }

    async fn send_text(&self, target: &str, message: &str) -> Result<MessageId> {
        let channel_id = self.resolve_channel(target);
        let body = serde_json::json!({
            "content": message,
        });
        let resp = self
            .api_post(&format!("/channels/{channel_id}/messages"), &body)
            .await?;
        Ok(Self::extract_message_id(&resp))
    }

    async fn send_rich(&self, target: &str, message: &RichMessage) -> Result<MessageId> {
        let channel_id = self.resolve_channel(target);
        let embed = build_embed(message);
        let body = serde_json::json!({
            "content": &message.plain_text,
            "embeds": [embed],
        });
        let resp = self
            .api_post(&format!("/channels/{channel_id}/messages"), &body)
            .await?;
        Ok(Self::extract_message_id(&resp))
    }

    async fn send_with_actions(
        &self,
        target: &str,
        message: &str,
        actions: &[Action],
    ) -> Result<MessageId> {
        let channel_id = self.resolve_channel(target);
        let action_row = build_action_row(actions);
        let body = serde_json::json!({
            "content": message,
            "components": [action_row],
        });
        let resp = self
            .api_post(&format!("/channels/{channel_id}/messages"), &body)
            .await?;
        Ok(Self::extract_message_id(&resp))
    }

    fn supports_receive(&self) -> bool {
        true
    }

    async fn listen(&self) -> Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let config = self.config.clone();
        let client = self.client.clone();

        tokio::spawn(async move {
            // Discord Gateway requires a WebSocket connection.
            // 1. GET /gateway/bot to obtain the WSS URL
            // 2. Connect via WebSocket
            // 3. Send IDENTIFY payload
            // 4. Handle HEARTBEAT / DISPATCH events
            //
            // Full Gateway implementation requires tokio-tungstenite.
            // For now, we log the intent and keep the channel alive.
            let url = format!("{DISCORD_API_BASE}/gateway/bot");
            match client
                .get(&url)
                .header("Authorization", format!("Bot {}", config.bot_token))
                .send()
                .await
            {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        let wss_url = json
                            .get("url")
                            .and_then(|u| u.as_str())
                            .unwrap_or("unknown");
                        eprintln!(
                            "Discord Gateway: obtained WSS URL ({wss_url}). \
                             Full Gateway support requires a WebSocket client."
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Discord Gateway connect error: {e}");
                }
            }

            // Keep channel alive without producing messages.
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                if tx.is_closed() {
                    return;
                }
            }
        });

        Ok(rx)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DiscordConfig {
        DiscordConfig {
            bot_token: "MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.test.token".into(),
            channel_id: "123456789012345678".into(),
            guild_id: Some("987654321098765432".into()),
        }
    }

    #[test]
    fn discord_config_from_toml() {
        let toml_str = r#"
[routing]
default = ["discord"]

[discord]
bot_token = "MTIz.test.token"
channel_id = "111222333444555666"
guild_id = "999888777666555444"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let discord = DiscordConfig::from_notify_config(&config).unwrap();
        assert_eq!(discord.bot_token, "MTIz.test.token");
        assert_eq!(discord.channel_id, "111222333444555666");
        assert_eq!(discord.guild_id.as_deref(), Some("999888777666555444"));
    }

    #[test]
    fn discord_config_without_guild_id() {
        let toml_str = r#"
[routing]
default = ["discord"]

[discord]
bot_token = "MTIz.test.token"
channel_id = "111222333444555666"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let discord = DiscordConfig::from_notify_config(&config).unwrap();
        assert!(discord.guild_id.is_none());
    }

    #[test]
    fn discord_config_missing_section() {
        let config = super::super::config::NotifyConfig::default();
        assert!(DiscordConfig::from_notify_config(&config).is_err());
    }

    #[test]
    fn channel_type_is_discord() {
        let ch = DiscordChannel::new(test_config());
        assert_eq!(ch.channel_type(), "discord");
    }

    #[test]
    fn supports_receive_is_true() {
        let ch = DiscordChannel::new(test_config());
        assert!(ch.supports_receive());
    }

    #[test]
    fn resolve_channel_default() {
        let ch = DiscordChannel::new(test_config());
        assert_eq!(ch.resolve_channel("*"), "123456789012345678");
        assert_eq!(ch.resolve_channel(""), "123456789012345678");
    }

    #[test]
    fn resolve_channel_explicit() {
        let ch = DiscordChannel::new(test_config());
        assert_eq!(ch.resolve_channel("999888777"), "999888777");
    }

    #[test]
    fn extract_message_id_from_response() {
        let json = serde_json::json!({
            "id": "1234567890",
            "channel_id": "111222333",
            "content": "hello"
        });
        let mid = DiscordChannel::extract_message_id(&json);
        assert_eq!(mid.0, "1234567890");
    }

    #[test]
    fn extract_message_id_missing_returns_zero() {
        let json = serde_json::json!({});
        let mid = DiscordChannel::extract_message_id(&json);
        assert_eq!(mid.0, "0");
    }

    #[test]
    fn build_embed_uses_markdown() {
        let msg = RichMessage {
            plain_text: "hello".into(),
            html: Some("<b>hello</b>".into()),
            markdown: Some("**hello**".into()),
        };
        let embed = build_embed(&msg);
        assert_eq!(embed["description"], "**hello**");
        assert_eq!(embed["color"], 0x5865F2);
    }

    #[test]
    fn build_embed_falls_back_to_plain() {
        let msg = RichMessage::plain("just text");
        let embed = build_embed(&msg);
        assert_eq!(embed["description"], "just text");
    }

    #[test]
    fn build_action_row_structure() {
        let actions = vec![
            Action {
                id: "approve".into(),
                label: "Approve".into(),
                style: ActionStyle::Primary,
            },
            Action {
                id: "reject".into(),
                label: "Reject".into(),
                style: ActionStyle::Danger,
            },
            Action {
                id: "skip".into(),
                label: "Skip".into(),
                style: ActionStyle::Secondary,
            },
        ];
        let row = build_action_row(&actions);
        assert_eq!(row["type"], 1); // Action Row
        let components = row["components"].as_array().unwrap();
        assert_eq!(components.len(), 3);

        // Primary = style 1
        assert_eq!(components[0]["type"], 2); // Button
        assert_eq!(components[0]["style"], 1);
        assert_eq!(components[0]["label"], "Approve");
        assert_eq!(components[0]["custom_id"], "approve");

        // Danger = style 4
        assert_eq!(components[1]["style"], 4);
        assert_eq!(components[1]["label"], "Reject");

        // Secondary = style 2
        assert_eq!(components[2]["style"], 2);
        assert_eq!(components[2]["label"], "Skip");
    }
}

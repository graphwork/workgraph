//! Telegram notification channel implementation using teloxide.
//!
//! Implements [`NotificationChannel`] for Telegram bots. Supports:
//! - Outbound: text, rich (Markdown), and action-button messages (inline keyboards)
//! - Inbound: long-polling listener that yields [`IncomingMessage`]s
//!
//! Configuration is read from the `[telegram]` section of `notify.toml`:
//! ```toml
//! [telegram]
//! bot_token = "123456:ABC-DEF..."
//! chat_id = "12345678"
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{Action, ActionStyle, IncomingMessage, MessageId, NotificationChannel, RichMessage};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Telegram-specific configuration parsed from the `[telegram]` section.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
}

impl TelegramConfig {
    /// Extract from the opaque channel map in [`super::config::NotifyConfig`].
    pub fn from_notify_config(config: &super::config::NotifyConfig) -> Result<Self> {
        let val = config
            .channels
            .get("telegram")
            .context("no [telegram] section in notify config")?;
        let cfg: Self = val
            .clone()
            .try_into()
            .context("invalid [telegram] config")?;
        Ok(cfg)
    }
}

// ---------------------------------------------------------------------------
// Channel implementation
// ---------------------------------------------------------------------------

/// A Telegram notification channel backed by the Telegram Bot API via `reqwest`.
///
/// This uses the Bot API directly over HTTP rather than pulling in the full
/// teloxide runtime, keeping the non-listener path lightweight.
pub struct TelegramChannel {
    config: TelegramConfig,
    client: reqwest::Client,
}

impl TelegramChannel {
    pub fn new(config: TelegramConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{}",
            self.config.bot_token, method
        )
    }

    /// Send a request to the Telegram Bot API and return the result.
    pub async fn api_call(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let resp = self
            .client
            .post(self.api_url(method))
            .json(body)
            .send()
            .await
            .context("Telegram API request failed")?;

        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse Telegram API response")?;

        if !status.is_success() || json.get("ok") != Some(&serde_json::Value::Bool(true)) {
            let desc = json
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Telegram API error ({}): {}", status, desc);
        }

        Ok(json)
    }

    /// Extract the message_id from a sendMessage response.
    fn extract_message_id(json: &serde_json::Value) -> MessageId {
        let mid = json
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|m| m.as_i64())
            .unwrap_or(0);
        MessageId(mid.to_string())
    }
}

#[async_trait]
impl NotificationChannel for TelegramChannel {
    fn channel_type(&self) -> &str {
        "telegram"
    }

    async fn send_text(&self, target: &str, message: &str) -> Result<MessageId> {
        let body = serde_json::json!({
            "chat_id": target,
            "text": message,
        });
        let resp = self.api_call("sendMessage", &body).await?;
        Ok(Self::extract_message_id(&resp))
    }

    async fn send_rich(&self, target: &str, message: &RichMessage) -> Result<MessageId> {
        // Prefer Markdown, fall back to HTML, then plain text.
        let (text, parse_mode) = if let Some(ref md) = message.markdown {
            (md.clone(), Some("MarkdownV2"))
        } else if let Some(ref html) = message.html {
            (html.clone(), Some("HTML"))
        } else {
            (message.plain_text.clone(), None)
        };

        let mut body = serde_json::json!({
            "chat_id": target,
            "text": text,
        });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = serde_json::Value::String(mode.to_string());
        }

        let resp = self.api_call("sendMessage", &body).await?;
        Ok(Self::extract_message_id(&resp))
    }

    async fn send_with_actions(
        &self,
        target: &str,
        message: &str,
        actions: &[Action],
    ) -> Result<MessageId> {
        // Build inline keyboard from actions.
        let buttons: Vec<serde_json::Value> = actions
            .iter()
            .map(|a| {
                serde_json::json!({
                    "text": format_button_label(&a.label, a.style),
                    "callback_data": &a.id,
                })
            })
            .collect();

        let body = serde_json::json!({
            "chat_id": target,
            "text": message,
            "reply_markup": {
                "inline_keyboard": [buttons],
            },
        });

        let resp = self.api_call("sendMessage", &body).await?;
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
            let mut offset: i64 = 0;
            loop {
                let url = format!(
                    "https://api.telegram.org/bot{}/getUpdates",
                    config.bot_token
                );
                let body = serde_json::json!({
                    "offset": offset,
                    "timeout": 30,
                    "allowed_updates": ["message", "callback_query"],
                });

                let resp = match client.post(&url).json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("Telegram poll error: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let json: serde_json::Value = match resp.json().await {
                    Ok(j) => j,
                    Err(e) => {
                        eprintln!("Telegram parse error: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let updates = match json.get("result").and_then(|r| r.as_array()) {
                    Some(arr) => arr.clone(),
                    None => continue,
                };

                for update in &updates {
                    if let Some(uid) = update.get("update_id").and_then(|u| u.as_i64()) {
                        offset = uid + 1;
                    }

                    // Handle callback queries (button presses)
                    if let Some(cb) = update.get("callback_query") {
                        let sender = cb
                            .get("from")
                            .and_then(|f| f.get("username"))
                            .and_then(|u| u.as_str())
                            .or_else(|| {
                                cb.get("from")
                                    .and_then(|f| f.get("id"))
                                    .and_then(|i| i.as_i64())
                                    .map(|_| "unknown")
                            })
                            .unwrap_or("unknown");

                        let action_id = cb
                            .get("data")
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string();

                        let reply_to = cb
                            .get("message")
                            .and_then(|m| m.get("message_id"))
                            .and_then(|m| m.as_i64())
                            .map(|mid| MessageId(mid.to_string()));

                        let msg = IncomingMessage {
                            channel: "telegram".to_string(),
                            sender: sender.to_string(),
                            body: action_id.clone(),
                            action_id: Some(action_id),
                            reply_to,
                        };

                        if tx.send(msg).await.is_err() {
                            return; // receiver dropped
                        }
                        continue;
                    }

                    // Handle regular messages
                    if let Some(message) = update.get("message") {
                        let sender = message
                            .get("from")
                            .and_then(|f| f.get("username"))
                            .and_then(|u| u.as_str())
                            .unwrap_or("unknown");

                        let body = message
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();

                        let reply_to = message
                            .get("reply_to_message")
                            .and_then(|r| r.get("message_id"))
                            .and_then(|m| m.as_i64())
                            .map(|mid| MessageId(mid.to_string()));

                        let msg = IncomingMessage {
                            channel: "telegram".to_string(),
                            sender: sender.to_string(),
                            body,
                            action_id: None,
                            reply_to,
                        };

                        if tx.send(msg).await.is_err() {
                            return; // receiver dropped
                        }
                    }
                }
            }
        });

        Ok(rx)
    }
}

/// Add a visual prefix to button labels based on style.
fn format_button_label(label: &str, style: ActionStyle) -> String {
    match style {
        ActionStyle::Primary => format!("✅ {label}"),
        ActionStyle::Danger => format!("❌ {label}"),
        ActionStyle::Secondary => label.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_config_from_toml() {
        let toml_str = r#"
[routing]
default = ["telegram"]

[telegram]
bot_token = "123:ABC"
chat_id = "456"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let tg = TelegramConfig::from_notify_config(&config).unwrap();
        assert_eq!(tg.bot_token, "123:ABC");
        assert_eq!(tg.chat_id, "456");
    }

    #[test]
    fn telegram_config_missing_section() {
        let config = super::super::config::NotifyConfig::default();
        assert!(TelegramConfig::from_notify_config(&config).is_err());
    }

    #[test]
    fn format_button_labels() {
        assert_eq!(
            format_button_label("Approve", ActionStyle::Primary),
            "✅ Approve"
        );
        assert_eq!(
            format_button_label("Reject", ActionStyle::Danger),
            "❌ Reject"
        );
        assert_eq!(format_button_label("Skip", ActionStyle::Secondary), "Skip");
    }

    #[test]
    fn channel_type_is_telegram() {
        let ch = TelegramChannel::new(TelegramConfig {
            bot_token: "test".into(),
            chat_id: "test".into(),
        });
        assert_eq!(ch.channel_type(), "telegram");
    }

    #[test]
    fn supports_receive_is_true() {
        let ch = TelegramChannel::new(TelegramConfig {
            bot_token: "test".into(),
            chat_id: "test".into(),
        });
        assert!(ch.supports_receive());
    }

    #[test]
    fn api_url_format() {
        let ch = TelegramChannel::new(TelegramConfig {
            bot_token: "123:ABC".into(),
            chat_id: "456".into(),
        });
        assert_eq!(
            ch.api_url("sendMessage"),
            "https://api.telegram.org/bot123:ABC/sendMessage"
        );
    }

    #[test]
    fn extract_message_id_from_response() {
        let json = serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 42,
                "chat": { "id": 123 },
                "text": "hello"
            }
        });
        let mid = TelegramChannel::extract_message_id(&json);
        assert_eq!(mid.0, "42");
    }

    #[test]
    fn extract_message_id_missing_returns_zero() {
        let json = serde_json::json!({"ok": true});
        let mid = TelegramChannel::extract_message_id(&json);
        assert_eq!(mid.0, "0");
    }
}

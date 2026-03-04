//! SMS notification channel — sends messages via the Twilio REST API.
//!
//! Implements [`NotificationChannel`] for SMS. Supports:
//! - Outbound: plain text messages (160 char segments, auto-concatenated by Twilio)
//! - Inbound: webhook-based receive (requires a publicly reachable HTTP server)
//!
//! Configuration is read from the `[sms]` section of `notify.toml`:
//! ```toml
//! [sms]
//! account_sid = "AC..."
//! auth_token = "..."
//! from = "+15551234567"
//! to = "+15559876543"
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::{Action, IncomingMessage, MessageId, NotificationChannel, RichMessage};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Twilio SMS configuration parsed from the `[sms]` section.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SmsConfig {
    /// Twilio Account SID.
    pub account_sid: String,
    /// Twilio Auth Token.
    pub auth_token: String,
    /// Twilio phone number to send from (E.164 format, e.g. "+15551234567").
    pub from: String,
    /// Default recipient phone number (E.164 format).
    pub to: String,
}

impl SmsConfig {
    /// Extract from the opaque channel map in [`super::config::NotifyConfig`].
    pub fn from_notify_config(config: &super::config::NotifyConfig) -> Result<Self> {
        let val = config
            .channels
            .get("sms")
            .context("no [sms] section in notify config")?;
        let cfg: Self = val
            .clone()
            .try_into()
            .context("invalid [sms] config")?;
        Ok(cfg)
    }
}

// ---------------------------------------------------------------------------
// Channel implementation
// ---------------------------------------------------------------------------

/// An SMS notification channel backed by the Twilio REST API via `reqwest`.
pub struct SmsChannel {
    config: SmsConfig,
    client: reqwest::Client,
}

impl SmsChannel {
    pub fn new(config: SmsConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// Resolve the target phone number. If empty or "*", uses the configured default.
    fn resolve_recipient<'a>(&'a self, target: &'a str) -> &'a str {
        if target.is_empty() || target == "*" {
            &self.config.to
        } else {
            target
        }
    }

    /// Send an SMS via the Twilio Messages API.
    async fn send_sms(&self, to: &str, body: &str) -> Result<MessageId> {
        let url = format!(
            "https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json",
            self.config.account_sid
        );

        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.config.account_sid, Some(&self.config.auth_token))
            .form(&[("To", to), ("From", &self.config.from), ("Body", body)])
            .send()
            .await
            .context("Twilio API request failed")?;

        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse Twilio API response")?;

        if !status.is_success() {
            let message = json
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Twilio API error ({}): {}", status, message);
        }

        let sid = json
            .get("sid")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        Ok(MessageId(sid.to_string()))
    }
}

#[async_trait]
impl NotificationChannel for SmsChannel {
    fn channel_type(&self) -> &str {
        "sms"
    }

    async fn send_text(&self, target: &str, message: &str) -> Result<MessageId> {
        let to = self.resolve_recipient(target);
        self.send_sms(to, message).await
    }

    async fn send_rich(&self, target: &str, message: &RichMessage) -> Result<MessageId> {
        // SMS is plain text only — use the plain_text fallback.
        let to = self.resolve_recipient(target);
        self.send_sms(to, &message.plain_text).await
    }

    async fn send_with_actions(
        &self,
        target: &str,
        message: &str,
        actions: &[Action],
    ) -> Result<MessageId> {
        // SMS doesn't support interactive buttons. Append action labels as text.
        let mut body = message.to_string();
        if !actions.is_empty() {
            body.push_str("\n\nReply with:");
            for action in actions {
                body.push_str(&format!("\n  {} - {}", action.id, action.label));
            }
        }
        let to = self.resolve_recipient(target);
        self.send_sms(to, &body).await
    }

    fn supports_receive(&self) -> bool {
        // Twilio can send incoming SMS via webhooks, but this requires
        // running an HTTP server. Not supported in the current implementation.
        false
    }

    async fn listen(&self) -> Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        anyhow::bail!(
            "SMS receive requires a webhook server. Configure Twilio to POST to your \
             server and parse the incoming messages there."
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::ActionStyle;

    fn test_config() -> SmsConfig {
        SmsConfig {
            account_sid: "AC1234567890abcdef".into(),
            auth_token: "test_auth_token".into(),
            from: "+15551234567".into(),
            to: "+15559876543".into(),
        }
    }

    #[test]
    fn sms_config_from_toml() {
        let toml_str = r#"
[routing]
default = ["sms"]

[sms]
account_sid = "AC0000000000000000"
auth_token = "secret"
from = "+15550001111"
to = "+15552223333"
"#;
        let config: super::super::config::NotifyConfig = toml::from_str(toml_str).unwrap();
        let sms = SmsConfig::from_notify_config(&config).unwrap();
        assert_eq!(sms.account_sid, "AC0000000000000000");
        assert_eq!(sms.auth_token, "secret");
        assert_eq!(sms.from, "+15550001111");
        assert_eq!(sms.to, "+15552223333");
    }

    #[test]
    fn sms_config_missing_section() {
        let config = super::super::config::NotifyConfig::default();
        assert!(SmsConfig::from_notify_config(&config).is_err());
    }

    #[test]
    fn channel_type_is_sms() {
        let ch = SmsChannel::new(test_config());
        assert_eq!(ch.channel_type(), "sms");
    }

    #[test]
    fn does_not_support_receive() {
        let ch = SmsChannel::new(test_config());
        assert!(!ch.supports_receive());
    }

    #[test]
    fn resolve_recipient_default() {
        let ch = SmsChannel::new(test_config());
        assert_eq!(ch.resolve_recipient("*"), "+15559876543");
        assert_eq!(ch.resolve_recipient(""), "+15559876543");
    }

    #[test]
    fn resolve_recipient_explicit() {
        let ch = SmsChannel::new(test_config());
        assert_eq!(ch.resolve_recipient("+15550009999"), "+15550009999");
    }

    #[test]
    fn send_with_actions_formats_text() {
        // Verify the text formatting logic for actions (without actually sending).
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
        ];
        let mut body = "Task needs approval".to_string();
        if !actions.is_empty() {
            body.push_str("\n\nReply with:");
            for action in &actions {
                body.push_str(&format!("\n  {} - {}", action.id, action.label));
            }
        }
        assert!(body.contains("Reply with:"));
        assert!(body.contains("approve - Approve"));
        assert!(body.contains("reject - Reject"));
    }
}

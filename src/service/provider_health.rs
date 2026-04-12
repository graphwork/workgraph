//! Provider health detection and auto-pause system
//!
//! Tracks provider failure patterns and implements circuit-breaker logic
//! to pause the service when providers repeatedly fail with fatal errors.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Classification of provider errors based on exit codes and stderr patterns
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderErrorKind {
    /// Temporary network issues, rate limits - should retry with backoff
    Transient,
    /// Provider-level failures: auth, quota, CLI missing - should pause provider
    FatalProvider,
    /// Task-level failures: context too long, malformed input - should fail task
    FatalTask,
}

/// Health status of a single provider/executor combination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderHealthStatus {
    /// Provider/executor identifier (e.g., "claude", "native:anthropic")
    pub provider_id: String,
    /// Count of consecutive fatal-provider errors
    pub consecutive_failures: u32,
    /// Timestamp of last fatal-provider error
    pub last_failure_at: Option<String>,
    /// Last error message that caused failure
    pub last_error: Option<String>,
    /// Whether this provider is currently paused
    pub is_paused: bool,
    /// When the provider was paused (if paused)
    pub paused_at: Option<String>,
    /// Reason for pausing
    pub pause_reason: Option<String>,
}

impl ProviderHealthStatus {
    pub fn new(provider_id: String) -> Self {
        Self {
            provider_id,
            consecutive_failures: 0,
            last_failure_at: None,
            last_error: None,
            is_paused: false,
            paused_at: None,
            pause_reason: None,
        }
    }

    /// Record a failure for this provider
    pub fn record_failure(&mut self, error_kind: ProviderErrorKind, error_message: String) {
        match error_kind {
            ProviderErrorKind::FatalProvider => {
                self.consecutive_failures += 1;
                self.last_failure_at = Some(Utc::now().to_rfc3339());
                self.last_error = Some(error_message);
            }
            ProviderErrorKind::Transient | ProviderErrorKind::FatalTask => {
                // Don't count transient or task-level errors for provider health
            }
        }
    }

    /// Record a successful task completion - resets failure count
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.last_failure_at = None;
        self.last_error = None;
    }

    /// Pause this provider with a reason
    pub fn pause(&mut self, reason: String) {
        self.is_paused = true;
        self.paused_at = Some(Utc::now().to_rfc3339());
        self.pause_reason = Some(reason);
    }

    /// Resume this provider (clear pause state)
    pub fn resume(&mut self) {
        self.is_paused = false;
        self.paused_at = None;
        self.pause_reason = None;
        // Also reset failure count on resume
        self.consecutive_failures = 0;
        self.last_failure_at = None;
        self.last_error = None;
    }

    /// Check if this provider should be paused based on failure threshold
    pub fn should_pause(&self, threshold: u32) -> bool {
        !self.is_paused && self.consecutive_failures >= threshold
    }
}

/// Global provider health tracker
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderHealth {
    /// Health status per provider/executor
    pub providers: HashMap<String, ProviderHealthStatus>,
    /// Global service pause state
    pub service_paused: bool,
    /// Why the service is paused (if paused)
    pub pause_reason: Option<String>,
    /// When the service was paused
    pub paused_at: Option<String>,
    /// Auto-resume cooldown period (if configured)
    pub auto_resume_at: Option<String>,
}


impl ProviderHealth {
    /// Load provider health from disk
    pub fn load(dir: &Path) -> Result<Self> {
        let path = provider_health_path(dir);
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read provider health from {:?}", path))?;
        let health: ProviderHealth = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse provider health from {:?}", path))?;
        Ok(health)
    }

    /// Save provider health to disk
    pub fn save(&self, dir: &Path) -> Result<()> {
        let service_dir = dir.join("service");
        if !service_dir.exists() {
            fs::create_dir_all(&service_dir).with_context(|| {
                format!("Failed to create service directory at {:?}", service_dir)
            })?;
        }

        let path = provider_health_path(dir);
        let content =
            serde_json::to_string_pretty(self).context("Failed to serialize provider health")?;
        fs::write(&path, content)
            .with_context(|| format!("Failed to write provider health to {:?}", path))?;
        Ok(())
    }

    /// Get or create health status for a provider
    pub fn get_or_create_provider(&mut self, provider_id: &str) -> &mut ProviderHealthStatus {
        self.providers
            .entry(provider_id.to_string())
            .or_insert_with(|| ProviderHealthStatus::new(provider_id.to_string()))
    }

    /// Record a failure for a provider
    pub fn record_failure(
        &mut self,
        provider_id: &str,
        error_kind: ProviderErrorKind,
        error_message: String,
    ) {
        let provider = self.get_or_create_provider(provider_id);
        provider.record_failure(error_kind, error_message);
    }

    /// Record a success for a provider
    pub fn record_success(&mut self, provider_id: &str) {
        let provider = self.get_or_create_provider(provider_id);
        provider.record_success();
    }

    /// Check if any providers should be paused and apply pause
    pub fn check_and_apply_pauses(&mut self, threshold: u32, behavior: &str) -> Vec<String> {
        let mut paused_providers = Vec::new();

        for provider in self.providers.values_mut() {
            if provider.should_pause(threshold) {
                let reason = format!(
                    "{} consecutive fatal-provider errors (threshold: {}). Last error: {}",
                    provider.consecutive_failures,
                    threshold,
                    provider.last_error.as_deref().unwrap_or("unknown")
                );
                provider.pause(reason.clone());
                paused_providers.push(provider.provider_id.clone());

                match behavior {
                    "pause" => {
                        // Pause the entire service
                        self.service_paused = true;
                        self.pause_reason = Some(format!(
                            "Provider '{}' failed {} consecutive times",
                            provider.provider_id, provider.consecutive_failures
                        ));
                        self.paused_at = Some(Utc::now().to_rfc3339());
                    }
                    "fallback" => {
                        // Just pause this provider, service continues with others
                        // Fallback logic will be handled by the coordinator
                    }
                    "continue" => {
                        // Just log the failure, don't pause anything
                        provider.resume(); // Immediately unpause
                    }
                    _ => {
                        // Default to pause behavior
                        self.service_paused = true;
                        self.pause_reason = Some(format!(
                            "Provider '{}' failed {} consecutive times",
                            provider.provider_id, provider.consecutive_failures
                        ));
                        self.paused_at = Some(Utc::now().to_rfc3339());
                    }
                }
            }
        }

        paused_providers
    }

    /// Resume the service (clear global pause state)
    pub fn resume_service(&mut self) {
        self.service_paused = false;
        self.pause_reason = None;
        self.paused_at = None;
        self.auto_resume_at = None;

        // Also resume all paused providers
        for provider in self.providers.values_mut() {
            if provider.is_paused {
                provider.resume();
            }
        }
    }

    /// Check if the service should be paused
    pub fn should_pause_spawning(&self) -> bool {
        self.service_paused
    }

    /// Get a summary of current health status
    pub fn get_status_summary(&self) -> String {
        if self.service_paused {
            format!(
                "Service PAUSED: {}",
                self.pause_reason.as_deref().unwrap_or("unknown reason")
            )
        } else {
            let paused_count = self.providers.values().filter(|p| p.is_paused).count();
            let total_count = self.providers.len();
            if paused_count > 0 {
                format!(
                    "Service running, {}/{} providers paused",
                    paused_count, total_count
                )
            } else {
                "Service running, all providers healthy".to_string()
            }
        }
    }
}

/// Path to the provider health state file
fn provider_health_path(dir: &Path) -> PathBuf {
    dir.join("service").join("provider_health.json")
}

/// Classify an error based on exit code and stderr content
pub fn classify_error(exit_code: Option<i32>, stderr: &str) -> ProviderErrorKind {
    // Classification based on the research in provider_error_patterns.md

    // Handle exit codes first
    if let Some(code) = exit_code {
        match code {
            0 => return ProviderErrorKind::FatalTask, // Success but marked as failure - weird state
            124 => return ProviderErrorKind::FatalTask, // Hard timeout - task complexity issue
            143 => return ProviderErrorKind::Transient, // SIGTERM - likely coordinator shutdown
            _ => {}                                   // Continue to stderr analysis
        }
    }

    // Analyze stderr patterns
    let stderr_lower = stderr.to_lowercase();

    // Auth/Authorization failures (Fatal-Provider)
    if stderr_lower.contains("authentication failed")
        || stderr_lower.contains("http 401")
        || stderr_lower.contains("access denied")
        || stderr_lower.contains("http 403")
        || stderr_lower.contains("check your api key")
        || stderr_lower.contains("insufficient permissions")
    {
        return ProviderErrorKind::FatalProvider;
    }

    // CLI/Infrastructure failures (Fatal-Provider)
    if stderr_lower.contains("claude' cli is required but was not found")
        || stderr_lower.contains("command not found")
        || stderr_lower.contains("failed to spawn claude cli")
        || stderr_lower.contains("failed to create tokio runtime")
        || stderr_lower.contains("failed to create anthropic client")
    {
        return ProviderErrorKind::FatalProvider;
    }

    // Quota/Billing failures (Fatal-Provider)
    if stderr_lower.contains("quota")
        || stderr_lower.contains("balance exhausted")
        || stderr_lower.contains("monthly")
        || stderr_lower.contains("daily")
        || stderr_lower.contains("cost cap")
        || stderr_lower.contains("billing")
    {
        return ProviderErrorKind::FatalProvider;
    }

    // Rate limiting (Transient)
    if stderr_lower.contains("http 429")
        || stderr_lower.contains("rate limit")
        || stderr_lower.contains("rate_limit_event")
        || stderr_lower.contains("retry-after")
    {
        return ProviderErrorKind::Transient;
    }

    // Network/Connectivity (Transient)
    if stderr_lower.contains("timeout")
        || stderr_lower.contains("connection refused")
        || stderr_lower.contains("dns resolution")
        || stderr_lower.contains("network")
        || stderr_lower.contains("timed out")
        || stderr_lower.contains("connection reset")
    {
        return ProviderErrorKind::Transient;
    }

    // Context length issues (Fatal-Task)
    if stderr_lower.contains("http 413")
        || stderr_lower.contains("payload too large")
        || (stderr_lower.contains("http 400")
            && (stderr_lower.contains("context")
                || stderr_lower.contains("too long")
                || stderr_lower.contains("too large")
                || stderr_lower.contains("token")
                || stderr_lower.contains("maximum")
                || stderr_lower.contains("prompt")))
    {
        return ProviderErrorKind::FatalTask;
    }

    // Empty response (Fatal-Task)
    if stderr_lower.contains("empty response")
        || stderr_lower.contains("failed to parse json")
        || stderr_lower.contains("malformed json")
    {
        return ProviderErrorKind::FatalTask;
    }

    // Lock contention (Transient)
    if stderr_lower.contains("lock contention")
        || stderr_lower.contains("file lock")
        || stderr_lower.contains("index.lock")
        || stderr_lower.contains("cargo.lock")
    {
        return ProviderErrorKind::Transient;
    }

    // Default to transient for unknown errors (conservative approach)
    ProviderErrorKind::Transient
}

/// Extract provider/executor identifier from configuration
pub fn extract_provider_id(executor: &str, model: Option<&str>) -> String {
    match executor {
        "claude" => "claude".to_string(),
        "native" => {
            if let Some(model) = model {
                if model.contains("gpt") || model.contains("openai") {
                    "native:openai".to_string()
                } else if model.contains("claude") || model.contains("anthropic") {
                    "native:anthropic".to_string()
                } else {
                    format!("native:{}", model.split(':').next().unwrap_or("unknown"))
                }
            } else {
                "native:unknown".to_string()
            }
        }
        "amplifier" => "amplifier".to_string(),
        "shell" => "shell".to_string(),
        _ => executor.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_classification() {
        // Auth failures
        assert_eq!(
            classify_error(Some(1), "Authentication failed (HTTP 401)"),
            ProviderErrorKind::FatalProvider
        );
        assert_eq!(
            classify_error(Some(1), "Access denied (HTTP 403)"),
            ProviderErrorKind::FatalProvider
        );

        // Rate limiting
        assert_eq!(
            classify_error(Some(1), "HTTP 429: Rate limit exceeded"),
            ProviderErrorKind::Transient
        );

        // Context length
        assert_eq!(
            classify_error(Some(1), "HTTP 413: Payload too large"),
            ProviderErrorKind::FatalTask
        );

        // Hard timeout
        assert_eq!(
            classify_error(Some(124), "Agent exceeded hard timeout"),
            ProviderErrorKind::FatalTask
        );

        // Unknown error defaults to transient
        assert_eq!(
            classify_error(Some(1), "Some random error"),
            ProviderErrorKind::Transient
        );
    }

    #[test]
    fn test_provider_health_tracking() {
        let mut health = ProviderHealth::default();
        let provider_id = "claude";

        // Record a fatal provider error
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            "Auth failed".to_string(),
        );

        let provider = health.get_or_create_provider(provider_id);
        assert_eq!(provider.consecutive_failures, 1);
        assert!(!provider.should_pause(3)); // Below threshold

        // Record more failures
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            "Auth failed again".to_string(),
        );
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            "Still failing".to_string(),
        );

        let provider = health.get_or_create_provider(provider_id);
        assert_eq!(provider.consecutive_failures, 3);
        assert!(provider.should_pause(3)); // At threshold

        // Success should reset count
        health.record_success(provider_id);
        let provider = health.get_or_create_provider(provider_id);
        assert_eq!(provider.consecutive_failures, 0);
        assert!(!provider.should_pause(3));
    }

    #[test]
    fn test_provider_id_extraction() {
        assert_eq!(extract_provider_id("claude", None), "claude");
        assert_eq!(
            extract_provider_id("native", Some("gpt-4")),
            "native:openai"
        );
        assert_eq!(
            extract_provider_id("native", Some("claude-3-sonnet")),
            "native:anthropic"
        );
        assert_eq!(
            extract_provider_id("native", Some("custom:model")),
            "native:custom"
        );
    }
}

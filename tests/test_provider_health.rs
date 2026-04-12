use anyhow::Result;
use serial_test::serial;
use std::fs;
use tempfile::TempDir;
use workgraph::config::Config;
use workgraph::service::{
    ProviderErrorKind, ProviderHealth, ProviderHealthStatus, classify_error, extract_provider_id,
};

/// Test provider error classification
#[test]
fn test_provider_health_error_classification() {
    // Auth failures should be FatalProvider
    assert_eq!(
        classify_error(Some(1), "Authentication failed (HTTP 401): Invalid API key"),
        ProviderErrorKind::FatalProvider
    );
    assert_eq!(
        classify_error(
            Some(1),
            "Access denied (HTTP 403): Insufficient permissions"
        ),
        ProviderErrorKind::FatalProvider
    );

    // CLI missing should be FatalProvider
    assert_eq!(
        classify_error(
            Some(1),
            "The 'claude' CLI is required but was not found in PATH"
        ),
        ProviderErrorKind::FatalProvider
    );

    // Quota exhaustion should be FatalProvider
    assert_eq!(
        classify_error(Some(1), "quota exceeded for this billing period"),
        ProviderErrorKind::FatalProvider
    );

    // Rate limiting should be Transient
    assert_eq!(
        classify_error(Some(1), "HTTP 429: Rate limit exceeded"),
        ProviderErrorKind::Transient
    );

    // Network timeouts should be Transient
    assert_eq!(
        classify_error(Some(1), "Native Anthropic call timed out"),
        ProviderErrorKind::Transient
    );

    // Context length should be FatalTask
    assert_eq!(
        classify_error(Some(1), "HTTP 413: Payload too large"),
        ProviderErrorKind::FatalTask
    );

    // Hard timeout should be FatalTask
    assert_eq!(
        classify_error(Some(124), "Agent exceeded hard timeout"),
        ProviderErrorKind::FatalTask
    );

    // Unknown errors default to Transient
    assert_eq!(
        classify_error(Some(1), "Some mysterious error"),
        ProviderErrorKind::Transient
    );
}

/// Test provider ID extraction
#[test]
fn test_provider_health_provider_id_extraction() {
    assert_eq!(extract_provider_id("claude", None), "claude");
    assert_eq!(
        extract_provider_id("native", Some("gpt-4-turbo")),
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
    assert_eq!(extract_provider_id("amplifier", None), "amplifier");
    assert_eq!(extract_provider_id("shell", None), "shell");
}

/// Test consecutive failures trigger pause
#[test]
fn test_provider_health_consecutive_failures_trigger_pause() {
    let mut health = ProviderHealth::default();
    let provider_id = "claude";

    // Record consecutive fatal provider errors
    for i in 1..=3 {
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            format!("Auth failure {}", i),
        );

        let provider = health.get_or_create_provider(provider_id);
        assert_eq!(provider.consecutive_failures, i);

        if i < 3 {
            assert!(!provider.should_pause(3)); // Below threshold
        } else {
            assert!(provider.should_pause(3)); // At threshold
        }
    }

    // Apply pause
    let paused = health.check_and_apply_pauses(3, "pause");
    assert_eq!(paused, vec![provider_id]);
    assert!(health.service_paused);

    let provider = health.get_or_create_provider(provider_id);
    assert!(provider.is_paused);
}

/// Test transient errors don't trigger pause
#[test]
fn test_provider_health_transient_errors_dont_trigger_pause() {
    let mut health = ProviderHealth::default();
    let provider_id = "claude";

    // Record many transient errors
    for i in 1..=10 {
        health.record_failure(
            provider_id,
            ProviderErrorKind::Transient,
            format!("Rate limit {}", i),
        );
    }

    // Transient errors should not count for provider health
    let provider = health.get_or_create_provider(provider_id);
    assert_eq!(provider.consecutive_failures, 0);
    assert!(!provider.should_pause(3));

    // Apply pause check - should not pause
    let paused = health.check_and_apply_pauses(3, "pause");
    assert!(paused.is_empty());
    assert!(!health.service_paused);
}

/// Test fallback mode switches provider
#[test]
fn test_provider_health_fallback_mode() {
    let mut health = ProviderHealth::default();
    let provider_id = "claude";

    // Record consecutive fatal provider errors
    for i in 1..=3 {
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            format!("Auth failure {}", i),
        );
    }

    // Apply pause with fallback behavior
    let paused = health.check_and_apply_pauses(3, "fallback");
    assert_eq!(paused, vec![provider_id]);

    // Service should not be globally paused in fallback mode
    assert!(!health.service_paused);

    // But the specific provider should be paused
    let provider = health.get_or_create_provider(provider_id);
    assert!(provider.is_paused);
}

/// Test continue mode doesn't pause
#[test]
fn test_provider_health_continue_mode() {
    let mut health = ProviderHealth::default();
    let provider_id = "claude";

    // Record consecutive fatal provider errors
    for i in 1..=3 {
        health.record_failure(
            provider_id,
            ProviderErrorKind::FatalProvider,
            format!("Auth failure {}", i),
        );
    }

    // Apply pause with continue behavior
    let paused = health.check_and_apply_pauses(3, "continue");
    assert_eq!(paused, vec![provider_id]); // Still returned as would-be paused

    // But nothing should actually be paused
    assert!(!health.service_paused);
    let provider = health.get_or_create_provider(provider_id);
    assert!(!provider.is_paused); // Immediately unpaused in continue mode
}

/// Test success resets failure count
#[test]
fn test_provider_health_success_resets_failure_count() {
    let mut health = ProviderHealth::default();
    let provider_id = "claude";

    // Build up some failures
    health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Auth failure 1".to_string(),
    );
    health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Auth failure 2".to_string(),
    );

    let provider = health.get_or_create_provider(provider_id);
    assert_eq!(provider.consecutive_failures, 2);

    // Success should reset count
    health.record_success(provider_id);
    let provider = health.get_or_create_provider(provider_id);
    assert_eq!(provider.consecutive_failures, 0);
    assert!(!provider.should_pause(3));
}

/// Test resume clears all pause state
#[test]
fn test_provider_health_resume_clears_pause_state() {
    let mut health = ProviderHealth::default();

    // Simulate a paused state
    health.service_paused = true;
    health.pause_reason = Some("Provider failures".to_string());
    health.paused_at = Some("2024-01-01T00:00:00Z".to_string());

    let provider_id = "claude";
    let mut provider = ProviderHealthStatus::new(provider_id.to_string());
    provider.pause("Too many failures".to_string());
    health.providers.insert(provider_id.to_string(), provider);

    // Resume should clear everything
    health.resume_service();

    assert!(!health.service_paused);
    assert!(health.pause_reason.is_none());
    assert!(health.paused_at.is_none());

    let provider = health.get_or_create_provider(provider_id);
    assert!(!provider.is_paused);
    assert!(provider.pause_reason.is_none());
    assert!(provider.paused_at.is_none());
    assert_eq!(provider.consecutive_failures, 0);
}

/// Test provider health persistence
#[test]
fn test_provider_health_persistence() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let service_dir = temp_dir.path().join("service");
    fs::create_dir_all(&service_dir)?;

    // Create initial health state
    let mut health = ProviderHealth::default();
    health.record_failure(
        "claude",
        ProviderErrorKind::FatalProvider,
        "Test failure".to_string(),
    );
    health.save(temp_dir.path())?;

    // Load and verify persistence
    let loaded_health = ProviderHealth::load(temp_dir.path())?;
    assert_eq!(loaded_health.providers.len(), 1);

    let provider = loaded_health.providers.get("claude").unwrap();
    assert_eq!(provider.consecutive_failures, 1);
    assert_eq!(provider.last_error, Some("Test failure".to_string()));

    Ok(())
}

/// Integration test with config.toml settings
#[test]
fn test_provider_health_config_integration() {
    let mut config = Config::default();

    // Test default values
    assert_eq!(config.coordinator.on_provider_failure, "pause");
    assert_eq!(config.coordinator.provider_failure_threshold, 3);
    assert_eq!(config.coordinator.provider_failure_cooldown, "");

    // Test setting different values
    config.coordinator.on_provider_failure = "fallback".to_string();
    config.coordinator.provider_failure_threshold = 5;
    config.coordinator.provider_failure_cooldown = "10m".to_string();

    assert_eq!(config.coordinator.on_provider_failure, "fallback");
    assert_eq!(config.coordinator.provider_failure_threshold, 5);
    assert_eq!(config.coordinator.provider_failure_cooldown, "10m");
}

/// End-to-end integration test for provider health pipeline
/// Tests: error simulation → auto-pause → state persistence → resume functionality
#[test]
#[serial]
fn test_provider_health_end_to_end_integration() -> Result<()> {
    let temp_dir = TempDir::new()?;

    // Step 1: Verify error classification works correctly for different scenarios
    assert_eq!(
        classify_error(Some(1), "Authentication failed (HTTP 401): Invalid API key"),
        ProviderErrorKind::FatalProvider
    );
    assert_eq!(
        classify_error(Some(1), "HTTP 429: Rate limit exceeded"),
        ProviderErrorKind::Transient
    );
    assert_eq!(
        classify_error(Some(1), "HTTP 413: Payload too large"),
        ProviderErrorKind::FatalTask
    );

    // Step 2: Test provider ID extraction for different executors
    assert_eq!(extract_provider_id("claude", None), "claude");
    assert_eq!(
        extract_provider_id("native", Some("gpt-4-turbo")),
        "native:openai"
    );
    assert_eq!(
        extract_provider_id("native", Some("claude-3-sonnet")),
        "native:anthropic"
    );

    // Step 3: Simulate provider failures and test auto-pause behavior
    let mut provider_health = ProviderHealth::default();
    let provider_id = "claude";

    // Record consecutive fatal provider errors (threshold = 2 for testing)
    provider_health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Authentication failed (HTTP 401): Invalid API key".to_string(),
    );
    provider_health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Authentication failed (HTTP 401): Invalid API key".to_string(),
    );

    // Verify failure count increased
    let provider = provider_health.get_or_create_provider(provider_id);
    assert_eq!(provider.consecutive_failures, 2);
    assert!(provider.should_pause(2));

    // Step 4: Test auto-pause application with different behaviors

    // Test "pause" behavior - service should be globally paused
    let paused_providers = provider_health.check_and_apply_pauses(2, "pause");
    assert_eq!(paused_providers, vec![provider_id]);
    assert!(provider_health.service_paused);
    assert!(provider_health.pause_reason.is_some());
    assert!(
        provider_health
            .providers
            .get(provider_id)
            .unwrap()
            .is_paused
    );

    // Step 5: Test state persistence
    provider_health.save(&temp_dir.path())?;
    let loaded_health = ProviderHealth::load(&temp_dir.path())?;

    // Verify persistence worked correctly
    assert!(loaded_health.service_paused);
    assert!(loaded_health.pause_reason.is_some());
    assert!(loaded_health.paused_at.is_some());

    let loaded_provider = loaded_health.providers.get(provider_id).unwrap();
    assert!(loaded_provider.is_paused);
    assert!(loaded_provider.pause_reason.is_some());
    assert!(loaded_provider.paused_at.is_some());
    assert_eq!(loaded_provider.consecutive_failures, 2);
    assert!(loaded_provider.last_error.is_some());

    // Step 6: Test resume functionality
    let mut resume_health = ProviderHealth::load(&temp_dir.path())?;
    let was_paused = resume_health.service_paused;

    // Apply resume logic
    resume_health.resume_service();

    // Verify resume cleared all pause state
    assert!(was_paused); // Confirm it was paused before
    assert!(!resume_health.service_paused);
    assert!(resume_health.pause_reason.is_none());
    assert!(resume_health.paused_at.is_none());

    let resumed_provider = resume_health.providers.get(provider_id).unwrap();
    assert!(!resumed_provider.is_paused);
    assert!(resumed_provider.pause_reason.is_none());
    assert!(resumed_provider.paused_at.is_none());
    assert_eq!(resumed_provider.consecutive_failures, 0); // Reset on resume

    // Test resume persistence
    resume_health.save(&temp_dir.path())?;
    let final_health = ProviderHealth::load(&temp_dir.path())?;
    assert!(!final_health.service_paused);
    assert!(final_health.pause_reason.is_none());

    // Step 7: Test different pause behavior modes

    // Test "fallback" behavior - only provider paused, not service
    let mut fallback_health = ProviderHealth::default();
    fallback_health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Auth failure".to_string(),
    );
    fallback_health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Auth failure".to_string(),
    );

    let paused = fallback_health.check_and_apply_pauses(2, "fallback");
    assert_eq!(paused, vec![provider_id]);
    assert!(!fallback_health.service_paused); // Service not globally paused in fallback
    assert!(
        fallback_health
            .providers
            .get(provider_id)
            .unwrap()
            .is_paused
    ); // But provider is paused

    // Test "continue" behavior - nothing actually paused
    let mut continue_health = ProviderHealth::default();
    continue_health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Auth failure".to_string(),
    );
    continue_health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Auth failure".to_string(),
    );

    let paused = continue_health.check_and_apply_pauses(2, "continue");
    assert_eq!(paused, vec![provider_id]); // Still reported as would-be paused
    assert!(!continue_health.service_paused);
    assert!(
        !continue_health
            .providers
            .get(provider_id)
            .unwrap()
            .is_paused
    ); // Not actually paused

    // Step 8: Test transient errors don't trigger pause
    let mut transient_health = ProviderHealth::default();
    for _ in 0..5 {
        transient_health.record_failure(
            provider_id,
            ProviderErrorKind::Transient,
            "HTTP 429: Rate limit exceeded".to_string(),
        );
    }

    let provider = transient_health.get_or_create_provider(provider_id);
    assert_eq!(provider.consecutive_failures, 0); // Transient errors don't count
    assert!(!provider.should_pause(2));

    // Step 9: Test success resets failure count
    let mut reset_health = ProviderHealth::default();
    reset_health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Error 1".to_string(),
    );
    reset_health.record_failure(
        provider_id,
        ProviderErrorKind::FatalProvider,
        "Error 2".to_string(),
    );

    let provider = reset_health.get_or_create_provider(provider_id);
    assert_eq!(provider.consecutive_failures, 2);

    // Record success
    reset_health.record_success(provider_id);
    let provider = reset_health.get_or_create_provider(provider_id);
    assert_eq!(provider.consecutive_failures, 0); // Reset by success
    assert!(!provider.should_pause(2));

    // Step 10: Test service status summary
    let mut status_health = ProviderHealth::default();
    assert!(
        status_health
            .get_status_summary()
            .contains("all providers healthy")
    );

    status_health.service_paused = true;
    status_health.pause_reason = Some("Provider 'claude' failed 3 consecutive times".to_string());
    assert!(status_health.get_status_summary().contains("PAUSED"));
    assert!(status_health.get_status_summary().contains("claude"));

    // Step 11: Test pause state detection for spawning
    assert!(status_health.should_pause_spawning()); // Service is paused

    status_health.service_paused = false;
    assert!(!status_health.should_pause_spawning()); // Service not paused

    Ok(())
}

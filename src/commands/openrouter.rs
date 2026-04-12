//! OpenRouter cost monitoring and management commands.

use crate::cli::OpenRouterCommands;
use crate::commands::service::CoordinatorState;
use anyhow::Result;
use std::path::Path;
use workgraph::config::Config;
use workgraph::executor::native::openai_client::{
    fetch_openrouter_key_status_blocking, resolve_openai_api_key_from_dir,
};

/// Run an OpenRouter subcommand.
pub fn run(dir: &Path, command: &OpenRouterCommands, json: bool) -> Result<()> {
    match command {
        OpenRouterCommands::Status => run_status(dir, json),
        OpenRouterCommands::Session => run_session(dir, json),
        OpenRouterCommands::SetLimit {
            global,
            session,
            task,
        } => run_set_limit(dir, *global, *session, *task),
    }
}

/// Show OpenRouter API key status and usage.
fn run_status(dir: &Path, json: bool) -> Result<()> {
    let config = Config::load_or_default(dir);

    // Get API key
    let api_key = resolve_openai_api_key_from_dir(dir)?;

    // Fetch key status
    let key_status = fetch_openrouter_key_status_blocking(&api_key, None)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&key_status)?);
    } else {
        println!("OpenRouter API Key Status:");
        println!("  Credit Limit: ${:.2}", key_status.limit);
        println!("  Remaining: ${:.2}", key_status.limit_remaining);
        println!("  Usage: ${:.2}", key_status.usage);
        println!("  Daily: ${:.2}", key_status.usage_daily);
        println!("  Weekly: ${:.2}", key_status.usage_weekly);
        println!("  Monthly: ${:.2}", key_status.usage_monthly);
        println!("  Free Tier: {}", key_status.is_free_tier);
        println!("  Usage Percentage: {:.1}%", key_status.usage_percentage());

        // Show warnings if approaching limits
        if key_status.is_above_threshold(config.openrouter.warn_at_usage_percent as f64) {
            println!(
                "\n⚠️  Warning: Usage above {}% threshold",
                config.openrouter.warn_at_usage_percent
            );
        }

        // Show cost cap configuration
        println!("\nCost Cap Configuration:");
        if let Some(global) = config.openrouter.cost_cap_global_usd {
            println!("  Global Cap: ${:.2}", global);
        } else {
            println!("  Global Cap: Not set");
        }
        if let Some(session) = config.openrouter.cost_cap_session_usd {
            println!("  Session Cap: ${:.2}", session);
        } else {
            println!("  Session Cap: Not set");
        }
        if let Some(task) = config.openrouter.cost_cap_task_usd {
            println!("  Task Cap: ${:.2}", task);
        } else {
            println!("  Task Cap: Not set");
        }
        println!("  Cap Behavior: {:?}", config.openrouter.cap_behavior);
    }

    Ok(())
}

/// Show session cost summary.
fn run_session(dir: &Path, json: bool) -> Result<()> {
    let service_dir = dir.join(".workgraph/service");

    // Load coordinator state
    let coord_state = CoordinatorState::load_for(&service_dir, 0).unwrap_or_default();

    let cost_tracking = &coord_state.cost_tracking;

    if json {
        let session_info = serde_json::json!({
            "session_cost_usd": cost_tracking.session_cost_usd,
            "session_start": cost_tracking.session_start,
            "last_key_check": cost_tracking.last_key_check,
            "key_status": cost_tracking.key_status
        });
        println!("{}", serde_json::to_string_pretty(&session_info)?);
    } else {
        println!("Current Session:");
        println!("  Session Cost: ${:.2}", cost_tracking.session_cost_usd);
        println!(
            "  Session Start: {}",
            cost_tracking.session_start.format("%Y-%m-%d %H:%M:%S UTC")
        );

        if let Some(last_check) = cost_tracking.last_key_check {
            println!(
                "  Last Key Check: {}",
                last_check.format("%Y-%m-%d %H:%M:%S UTC")
            );
        } else {
            println!("  Last Key Check: Never");
        }

        if let Some(key_status) = &cost_tracking.key_status {
            println!("\nCached Key Status:");
            println!(
                "  Usage: ${:.2}/${:.2} ({:.1}%)",
                key_status.usage,
                key_status.limit,
                key_status.usage_percentage()
            );
        }
    }

    Ok(())
}

/// Set cost cap limits.
fn run_set_limit(
    _dir: &Path,
    global: Option<f64>,
    session: Option<f64>,
    task: Option<f64>,
) -> Result<()> {
    println!("Note: Cost cap configuration is currently managed via config.toml");
    println!("Add the following to your .workgraph/config.toml file:");
    println!();
    println!("[openrouter]");

    if let Some(global) = global {
        println!("cost_cap_global_usd = {:.2}", global);
    }
    if let Some(session) = session {
        println!("cost_cap_session_usd = {:.2}", session);
    }
    if let Some(task) = task {
        println!("cost_cap_task_usd = {:.2}", task);
    }

    println!();
    println!("Other available options:");
    println!("cap_behavior = \"escalate\"  # fail, fallback, escalate, readonly");
    println!("fallback_model = \"claude:haiku\"  # Used when cap_behavior = \"fallback\"");
    println!("warn_at_usage_percent = 80");

    Ok(())
}

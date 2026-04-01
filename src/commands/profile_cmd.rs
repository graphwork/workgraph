//! Profile management commands: set, show, list provider profiles.

use anyhow::Result;
use std::path::Path;
use workgraph::config::Config;
use workgraph::profile;

/// Set the active provider profile.
pub fn set(dir: &Path, name: &str) -> Result<()> {
    // Validate the profile name
    let prof = profile::get_profile(name).ok_or_else(|| {
        let available: Vec<&str> = profile::builtin_profiles().iter().map(|p| p.name).collect();
        anyhow::anyhow!(
            "Unknown profile '{}'. Available profiles: {}",
            name,
            available.join(", ")
        )
    })?;

    let mut config = Config::load_merged(dir)?;
    config.profile = Some(name.to_string());
    config.save(dir)?;

    println!("Profile set: {}", name);

    if let Some(tiers) = prof.resolve_tiers() {
        println!("  Resolved tier mappings:");
        println!(
            "    fast     → {}",
            tiers.fast.as_deref().unwrap_or("(unset)")
        );
        println!(
            "    standard → {}",
            tiers.standard.as_deref().unwrap_or("(unset)")
        );
        println!(
            "    premium  → {}",
            tiers.premium.as_deref().unwrap_or("(unset)")
        );
    } else {
        println!("  Dynamic profile — tier mappings resolved at runtime.");
        println!("  Run `wg models update` to fetch benchmark data, then `wg profile show` for details.");
    }

    println!();
    println!("  Note: Per-role overrides in [models] still take precedence.");
    println!("  Run `wg profile show` for full details.");

    Ok(())
}

/// Show current profile and resolved model mappings.
pub fn show(dir: &Path, json: bool) -> Result<()> {
    let config = Config::load_merged(dir)?;

    let effective_tiers = config.effective_tiers_public();

    if json {
        let val = serde_json::json!({
            "profile": config.profile,
            "effective_tiers": {
                "fast": effective_tiers.fast,
                "standard": effective_tiers.standard,
                "premium": effective_tiers.premium,
            },
        });
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    // Header
    match config.profile.as_deref() {
        Some(name) => {
            if let Some(prof) = profile::get_profile(name) {
                println!("Profile: {} ({})", name, prof.strategy_label());
                println!("  {}", prof.description);
            } else {
                println!("Profile: {} (unknown — not a built-in profile)", name);
            }
        }
        None => {
            println!("Profile: (none)");
            println!("  Using default Anthropic tier mappings.");
            println!("  Set a profile with: wg profile set <name>");
        }
    }

    println!();
    println!("  Tier Mappings:");
    println!(
        "    fast     → {}",
        effective_tiers.fast.as_deref().unwrap_or("(unset)")
    );
    println!(
        "    standard → {}",
        effective_tiers.standard.as_deref().unwrap_or("(unset)")
    );
    println!(
        "    premium  → {}",
        effective_tiers.premium.as_deref().unwrap_or("(unset)")
    );

    // Show if any explicit tier overrides are active
    let has_overrides = config.tiers.fast.is_some()
        || config.tiers.standard.is_some()
        || config.tiers.premium.is_some();
    if has_overrides {
        println!();
        println!("  Tier overrides (from [tiers] config):");
        if let Some(ref f) = config.tiers.fast {
            println!("    fast     = {}", f);
        }
        if let Some(ref s) = config.tiers.standard {
            println!("    standard = {}", s);
        }
        if let Some(ref p) = config.tiers.premium {
            println!("    premium  = {}", p);
        }
    }

    Ok(())
}

/// List available profiles.
pub fn list(dir: &Path, json: bool) -> Result<()> {
    let config = Config::load_merged(dir)?;
    let active_profile = config.profile.as_deref();

    let profiles = profile::builtin_profiles();

    if json {
        let val: Vec<serde_json::Value> = profiles
            .iter()
            .map(|p| {
                let tiers = p.resolve_tiers();
                serde_json::json!({
                    "name": p.name,
                    "description": p.description,
                    "strategy": p.strategy_label(),
                    "active": active_profile == Some(p.name),
                    "tiers": tiers.as_ref().map(|t| serde_json::json!({
                        "fast": t.fast,
                        "standard": t.standard,
                        "premium": t.premium,
                    })),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    println!("Available profiles:");
    println!();

    for p in &profiles {
        let active_marker = if active_profile == Some(p.name) {
            " *"
        } else {
            ""
        };
        println!(
            "  {:<12} {} ({}){}", p.name, p.description, p.strategy_label(), active_marker
        );

        if let Some(tiers) = p.resolve_tiers() {
            println!(
                "               fast: {}  standard: {}  premium: {}",
                tiers.fast.as_deref().unwrap_or("?"),
                tiers.standard.as_deref().unwrap_or("?"),
                tiers.premium.as_deref().unwrap_or("?"),
            );
        } else {
            println!("               (resolved dynamically from benchmark registry)");
        }
        println!();
    }

    match active_profile {
        Some(name) => println!("  Active: {}", name),
        None => println!("  Active: (none — using default Anthropic tiers)"),
    }

    Ok(())
}

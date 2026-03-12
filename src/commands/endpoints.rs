//! CLI endpoint management: wg endpoints add/list/remove/set-default/test

use anyhow::{Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::path::Path;
use workgraph::config::{Config, EndpointConfig};

/// List all configured endpoints.
pub fn run_list(workgraph_dir: &Path, json: bool) -> Result<()> {
    let config = Config::load_merged(workgraph_dir)?;
    let endpoints = &config.llm_endpoints.endpoints;

    if endpoints.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No endpoints configured.");
            println!("  Add one with: wg endpoints add <name> --provider <provider>");
        }
        return Ok(());
    }

    if json {
        // Build JSON array with masked keys
        let items: Vec<serde_json::Value> = endpoints
            .iter()
            .map(|ep| {
                serde_json::json!({
                    "name": ep.name,
                    "provider": ep.provider,
                    "url": ep.url.as_deref().unwrap_or(EndpointConfig::default_url_for_provider(&ep.provider)),
                    "model": ep.model,
                    "api_key": ep.masked_key(),
                    "is_default": ep.is_default,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    println!("Configured endpoints:");
    println!();
    for ep in endpoints {
        let default_marker = if ep.is_default { " (default)" } else { "" };
        let url = ep
            .url
            .as_deref()
            .unwrap_or(EndpointConfig::default_url_for_provider(&ep.provider));
        println!(
            "  {}{}\n    provider: {}\n    url:      {}\n    model:    {}\n    api_key:  {}",
            ep.name,
            default_marker,
            ep.provider,
            url,
            ep.model.as_deref().unwrap_or("(not set)"),
            ep.masked_key(),
        );
        println!();
    }
    Ok(())
}

/// Add a new endpoint to the config.
pub fn run_add(
    workgraph_dir: &Path,
    name: &str,
    provider: Option<&str>,
    url: Option<&str>,
    model: Option<&str>,
    api_key: Option<&str>,
    api_key_file: Option<&str>,
    set_default: bool,
    global: bool,
) -> Result<()> {
    let mut config = if global {
        Config::load_global()?.unwrap_or_default()
    } else {
        Config::load(workgraph_dir)?
    };

    // Check for duplicate name
    if config
        .llm_endpoints
        .endpoints
        .iter()
        .any(|ep| ep.name == name)
    {
        bail!(
            "Endpoint '{}' already exists. Remove it first or use a different name.",
            name
        );
    }

    let is_first = config.llm_endpoints.endpoints.is_empty();
    let is_default = set_default || is_first;

    // If this becomes default, clear default from others
    if is_default {
        for ep in &mut config.llm_endpoints.endpoints {
            ep.is_default = false;
        }
    }

    let provider_str = provider.unwrap_or("anthropic");

    config.llm_endpoints.endpoints.push(EndpointConfig {
        name: name.to_string(),
        provider: provider_str.to_string(),
        url: url.map(|s| s.to_string()),
        model: model.map(|s| s.to_string()),
        api_key: api_key.map(|s| s.to_string()),
        api_key_file: api_key_file.map(|s| s.to_string()),
        is_default,
    });

    if global {
        config.save_global()?;
    } else {
        config.save(workgraph_dir)?;
    }

    let default_msg = if is_default { " (set as default)" } else { "" };
    println!(
        "Added endpoint '{}' [{}]{}",
        name, provider_str, default_msg
    );
    Ok(())
}

/// Remove an endpoint by name.
pub fn run_remove(workgraph_dir: &Path, name: &str, global: bool) -> Result<()> {
    let mut config = if global {
        Config::load_global()?.unwrap_or_default()
    } else {
        Config::load(workgraph_dir)?
    };

    let initial_len = config.llm_endpoints.endpoints.len();
    let was_default = config
        .llm_endpoints
        .endpoints
        .iter()
        .find(|ep| ep.name == name)
        .map(|ep| ep.is_default)
        .unwrap_or(false);

    config.llm_endpoints.endpoints.retain(|ep| ep.name != name);

    if config.llm_endpoints.endpoints.len() == initial_len {
        bail!("Endpoint '{}' not found.", name);
    }

    // If we removed the default, promote the first remaining endpoint
    if was_default {
        if let Some(ep) = config.llm_endpoints.endpoints.first_mut() {
            ep.is_default = true;
            eprintln!(
                "Note: '{}' was default. Promoted '{}' to default.",
                name, ep.name
            );
        }
    }

    if global {
        config.save_global()?;
    } else {
        config.save(workgraph_dir)?;
    }

    println!("Removed endpoint '{}'.", name);
    Ok(())
}

/// Set an endpoint as the default.
pub fn run_set_default(workgraph_dir: &Path, name: &str, global: bool) -> Result<()> {
    let mut config = if global {
        Config::load_global()?.unwrap_or_default()
    } else {
        Config::load(workgraph_dir)?
    };

    let found = config
        .llm_endpoints
        .endpoints
        .iter()
        .any(|ep| ep.name == name);

    if !found {
        bail!("Endpoint '{}' not found.", name);
    }

    for ep in &mut config.llm_endpoints.endpoints {
        ep.is_default = ep.name == name;
    }

    if global {
        config.save_global()?;
    } else {
        config.save(workgraph_dir)?;
    }

    println!("Set '{}' as default endpoint.", name);
    Ok(())
}

/// Test endpoint connectivity by hitting the /models API.
pub fn run_test(workgraph_dir: &Path, name: &str) -> Result<()> {
    let config = Config::load_merged(workgraph_dir)?;
    let ep = config
        .llm_endpoints
        .endpoints
        .iter()
        .find(|ep| ep.name == name)
        .ok_or_else(|| anyhow::anyhow!("Endpoint '{}' not found.", name))?;

    let base_url = ep
        .url
        .as_deref()
        .unwrap_or(EndpointConfig::default_url_for_provider(&ep.provider));

    if base_url.is_empty() {
        bail!(
            "No URL configured for endpoint '{}' (provider: {})",
            name,
            ep.provider
        );
    }

    let api_key = ep.resolve_api_key(Some(workgraph_dir))?;

    // Build the models URL based on provider
    let models_url = match ep.provider.as_str() {
        "anthropic" => format!("{}/v1/models", base_url.trim_end_matches('/')),
        _ => format!("{}/models", base_url.trim_end_matches('/')),
    };

    println!("Testing endpoint '{}' ...", name);
    println!("  URL: {}", models_url);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let mut headers = HeaderMap::new();
    if let Some(ref key) = api_key {
        match ep.provider.as_str() {
            "anthropic" => {
                headers.insert("x-api-key", HeaderValue::from_str(key)?);
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
            _ => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {}", key))?,
                );
            }
        }
    }

    match client.get(&models_url).headers(headers).send() {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                println!(
                    "  Status: {} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("OK")
                );
                println!("  Connectivity: OK");
                if api_key.is_some() {
                    println!("  Authentication: OK");
                } else {
                    println!("  Authentication: (no key configured, may be required)");
                }
            } else if status.as_u16() == 401 || status.as_u16() == 403 {
                let body = response.text().unwrap_or_default();
                println!("  Status: {}", status.as_u16());
                println!("  Connectivity: OK");
                println!("  Authentication: FAILED — check your API key");
                if !body.is_empty() {
                    let truncated = if body.len() > 200 {
                        format!("{}...", &body[..200])
                    } else {
                        body
                    };
                    println!("  Response: {}", truncated);
                }
            } else {
                let body = response.text().unwrap_or_default();
                println!("  Status: {}", status.as_u16());
                println!("  Connectivity: OK (server responded)");
                if !body.is_empty() {
                    let truncated = if body.len() > 200 {
                        format!("{}...", &body[..200])
                    } else {
                        body
                    };
                    println!("  Response: {}", truncated);
                }
            }
        }
        Err(e) => {
            println!("  Connection FAILED: {}", e);
            bail!("Could not connect to endpoint '{}': {}", name, e);
        }
    }

    Ok(())
}

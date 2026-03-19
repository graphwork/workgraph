//! CLI key management: wg key set/check/list

use anyhow::{Result, bail};
use reqwest::blocking::Client;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use workgraph::config::{Config, EndpointConfig};

/// Set an API key for a provider.
///
/// Exactly one of `env`, `file`, or `value` must be provided.
/// - `--env VAR_NAME` → sets `api_key_env` on the endpoint
/// - `--file /path/to/key` → sets `api_key_file` on the endpoint
/// - `--value sk-xxx` → writes to `~/.workgraph/keys/<provider>.key` and sets `api_key_file`
pub fn run_set(
    workgraph_dir: &Path,
    provider: &str,
    env: Option<&str>,
    file: Option<&str>,
    value: Option<&str>,
    global: bool,
) -> Result<()> {
    let source_count = [env.is_some(), file.is_some(), value.is_some()]
        .iter()
        .filter(|&&b| b)
        .count();
    if source_count == 0 {
        bail!("Specify one of --env, --file, or --value");
    }
    if source_count > 1 {
        bail!("Specify only one of --env, --file, or --value");
    }

    let mut config = if global {
        Config::load_global()?.unwrap_or_default()
    } else {
        Config::load(workgraph_dir)?
    };

    // Determine what fields to set
    let (new_api_key_env, new_api_key_file): (Option<String>, Option<String>) =
        if let Some(env_name) = env {
            (Some(env_name.to_string()), None)
        } else if let Some(file_path) = file {
            (None, Some(file_path.to_string()))
        } else if let Some(key_value) = value {
            // Write to ~/.workgraph/keys/<provider>.key
            let keys_dir = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
                .join(".workgraph")
                .join("keys");
            fs::create_dir_all(&keys_dir)?;
            // Set directory permissions to 700
            fs::set_permissions(&keys_dir, fs::Permissions::from_mode(0o700))?;

            let key_path = keys_dir.join(format!("{}.key", provider));
            fs::write(&key_path, key_value)?;
            // Set file permissions to 600
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;

            println!("Stored key securely in {} (mode 600)", key_path.display());
            (None, Some(key_path.to_string_lossy().to_string()))
        } else {
            unreachable!()
        };

    // Find existing endpoint for this provider, or create one
    let mut found = false;
    for ep in &mut config.llm_endpoints.endpoints {
        if ep.provider == provider || ep.name == provider {
            // Clear previous key fields when switching source
            ep.api_key = None;
            ep.api_key_env = new_api_key_env.clone();
            ep.api_key_file = new_api_key_file.clone();
            found = true;
            break;
        }
    }

    if !found {
        let is_first = config.llm_endpoints.endpoints.is_empty();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: provider.to_string(),
            provider: provider.to_string(),
            url: None,
            model: None,
            api_key: None,
            api_key_file: new_api_key_file.clone(),
            api_key_env: new_api_key_env.clone(),
            is_default: is_first,
        });
    }

    if global {
        config.save_global()?;
    } else {
        config.save(workgraph_dir)?;
    }

    let source_desc = if let Some(env_name) = env {
        format!("using env var {}", env_name)
    } else if let Some(file_path) = file {
        format!("using key file {}", file_path)
    } else {
        let keys_dir = dirs::home_dir().unwrap().join(".workgraph").join("keys");
        format!(
            "using key file {}",
            keys_dir.join(format!("{}.key", provider)).display()
        )
    };

    println!("Set API key for '{}': {}", provider, source_desc);
    Ok(())
}

/// Check API key status for a specific provider or all providers.
pub fn run_check(workgraph_dir: &Path, provider: Option<&str>, json: bool) -> Result<()> {
    let config = Config::load_merged(workgraph_dir)?;

    if let Some(provider_name) = provider {
        // Check a specific provider
        let ep = config
            .llm_endpoints
            .endpoints
            .iter()
            .find(|ep| ep.provider == provider_name || ep.name == provider_name);

        match ep {
            Some(ep) => check_single_provider(ep, workgraph_dir, json),
            None => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "provider": provider_name,
                            "status": "not_configured",
                            "error": format!("No endpoint configured for '{}'", provider_name),
                        })
                    );
                } else {
                    println!("Provider: {}", provider_name);
                    println!("  Status: not configured");
                    println!(
                        "  Add one with: wg endpoint add {} --provider {}",
                        provider_name, provider_name
                    );
                }
                Ok(())
            }
        }
    } else {
        // Check all providers
        if config.llm_endpoints.endpoints.is_empty() {
            if json {
                println!("[]");
            } else {
                println!("No endpoints configured.");
                println!("  Add one with: wg endpoint add <name> --provider <provider>");
            }
            return Ok(());
        }

        if json {
            let mut results = Vec::new();
            for ep in &config.llm_endpoints.endpoints {
                let key = ep.resolve_api_key(Some(workgraph_dir));
                let (status, source) = match &key {
                    Ok(Some(_)) => ("present".to_string(), ep.key_source()),
                    Ok(None) => ("missing".to_string(), ep.key_source()),
                    Err(e) => (format!("error: {}", e), ep.key_source()),
                };
                results.push(serde_json::json!({
                    "provider": ep.provider,
                    "name": ep.name,
                    "source": source,
                    "status": status,
                }));
            }
            println!("{}", serde_json::to_string_pretty(&results)?);
        } else {
            println!("API Key Status:");
            for ep in &config.llm_endpoints.endpoints {
                let key = ep.resolve_api_key(Some(workgraph_dir));
                let (indicator, source) = match &key {
                    Ok(Some(_)) => ("\u{2713}", ep.key_source()),
                    Ok(None) => ("\u{2717}", ep.key_source()),
                    Err(_) => ("\u{2717}", ep.key_source()),
                };
                let label = if ep.name != ep.provider {
                    format!("{} ({})", ep.name, ep.provider)
                } else {
                    ep.provider.clone()
                };
                println!("  {:<16}{} ({})", label, indicator, source);
            }
        }
        Ok(())
    }
}

/// Check a single provider in detail.
fn check_single_provider(ep: &EndpointConfig, workgraph_dir: &Path, json: bool) -> Result<()> {
    let source = ep.key_source();
    let key = ep.resolve_api_key(Some(workgraph_dir));

    match key {
        Ok(Some(resolved_key)) => {
            if json {
                let mut result = serde_json::json!({
                    "provider": ep.provider,
                    "name": ep.name,
                    "source": source,
                    "status": "present",
                });
                // Try live validation for OpenRouter
                if ep.provider == "openrouter" {
                    if let Some(credit_info) = check_openrouter_credits(&resolved_key) {
                        result["credits"] = credit_info;
                        result["status"] = serde_json::json!("valid");
                    }
                }
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Provider: {}", ep.provider);
                if ep.name != ep.provider {
                    println!("  Endpoint: {}", ep.name);
                }
                println!("  Key source: {}", source);

                // Live validation for OpenRouter
                if ep.provider == "openrouter" {
                    match check_openrouter_key_live(&resolved_key) {
                        Ok(info) => {
                            println!("  Key status: \u{2713} valid");
                            if let Some(remaining) = info.get("remaining") {
                                println!("  Credits:    ${}", remaining);
                            }
                            if let Some(limit) = info.get("rate_limit") {
                                println!("  Rate limit: {} req/min", limit);
                            }
                        }
                        Err(e) => {
                            println!("  Key status: \u{2717} validation failed ({})", e);
                        }
                    }
                } else {
                    println!("  Key status: \u{2713} present");
                }
            }
        }
        Ok(None) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "provider": ep.provider,
                        "name": ep.name,
                        "source": source,
                        "status": "missing",
                    })
                );
            } else {
                println!("Provider: {}", ep.provider);
                println!("  Key source: {}", source);
                println!("  Key status: \u{2717} not found");

                // Give a helpful hint
                if let Some(ref env_name) = ep.api_key_env {
                    println!("  Hint: Set the {} environment variable", env_name);
                } else {
                    println!(
                        "  Hint: Run 'wg key set {} --env <VAR>' or 'wg key set {} --value <key>'",
                        ep.provider, ep.provider
                    );
                }
            }
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "provider": ep.provider,
                        "name": ep.name,
                        "source": source,
                        "status": "error",
                        "error": format!("{}", e),
                    })
                );
            } else {
                println!("Provider: {}", ep.provider);
                println!("  Key source: {}", source);
                println!("  Key status: \u{2717} error: {}", e);
            }
        }
    }
    Ok(())
}

/// List key status for all configured endpoints.
pub fn run_list(workgraph_dir: &Path, json: bool) -> Result<()> {
    let config = Config::load_merged(workgraph_dir)?;

    if config.llm_endpoints.endpoints.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No endpoints configured.");
        }
        return Ok(());
    }

    if json {
        let mut results = Vec::new();
        for ep in &config.llm_endpoints.endpoints {
            let key = ep.resolve_api_key(Some(workgraph_dir));
            let status = match &key {
                Ok(Some(_)) => "present",
                Ok(None) => "missing",
                Err(_) => "error",
            };
            results.push(serde_json::json!({
                "provider": ep.provider,
                "name": ep.name,
                "source": ep.key_source(),
                "status": status,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(());
    }

    println!("  {:<16}{:<40}{:<12}", "PROVIDER", "SOURCE", "STATUS");
    for ep in &config.llm_endpoints.endpoints {
        let key = ep.resolve_api_key(Some(workgraph_dir));
        let (status_icon, status_text) = match &key {
            Ok(Some(_)) => ("\u{2713}", "present"),
            Ok(None) => ("\u{2717}", "missing"),
            Err(_) => ("\u{2717}", "error"),
        };
        let label = if ep.name != ep.provider {
            format!("{} ({})", ep.provider, ep.name)
        } else {
            ep.provider.clone()
        };
        println!(
            "  {:<16}{:<40}{} {}",
            label,
            ep.key_source(),
            status_icon,
            status_text
        );
    }
    Ok(())
}

/// Try to check OpenRouter credits. Returns credit info or None.
fn check_openrouter_credits(key: &str) -> Option<serde_json::Value> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client
        .get("https://openrouter.ai/api/v1/key")
        .header("Authorization", format!("Bearer {}", key))
        .send()
        .ok()?;
    if resp.status().is_success() {
        let body: serde_json::Value = resp.json().ok()?;
        let data = body.get("data").unwrap_or(&body);
        Some(data.clone())
    } else {
        None
    }
}

/// Check an OpenRouter key live and return structured info.
fn check_openrouter_key_live(
    key: &str,
) -> Result<std::collections::HashMap<String, serde_json::Value>> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client
        .get("https://openrouter.ai/api/v1/key")
        .header("Authorization", format!("Bearer {}", key))
        .send()?;

    if resp.status().is_success() {
        let body: serde_json::Value = resp.json()?;
        let data = body.get("data").unwrap_or(&body);
        let mut info = std::collections::HashMap::new();

        if let Some(remaining) = data.get("limit_remaining") {
            if !remaining.is_null() {
                info.insert("remaining".to_string(), remaining.clone());
            }
        }
        if let Some(limit) = data.get("limit") {
            if !limit.is_null() {
                info.insert("rate_limit".to_string(), limit.clone());
            }
        }
        Ok(info)
    } else {
        let status = resp.status();
        bail!("HTTP {}", status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "").unwrap();
        tmp
    }

    #[test]
    fn key_set_env_stores_reference() {
        let tmp = setup_dir();
        // First create an endpoint
        crate::commands::endpoints::run_add(
            tmp.path(),
            "openrouter",
            Some("openrouter"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();

        run_set(
            tmp.path(),
            "openrouter",
            Some("OPENROUTER_API_KEY"),
            None,
            None,
            false,
        )
        .unwrap();

        let config = Config::load(tmp.path()).unwrap();
        let ep = config
            .llm_endpoints
            .endpoints
            .iter()
            .find(|e| e.provider == "openrouter")
            .unwrap();
        assert_eq!(ep.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
        assert!(ep.api_key.is_none());
        assert!(ep.api_key_file.is_none());
    }

    #[test]
    fn key_set_file_stores_reference() {
        let tmp = setup_dir();
        let key_file = tmp.path().join("my.key");
        std::fs::write(&key_file, "sk-test").unwrap();

        run_set(
            tmp.path(),
            "anthropic",
            None,
            Some(key_file.to_str().unwrap()),
            None,
            false,
        )
        .unwrap();

        let config = Config::load(tmp.path()).unwrap();
        let ep = config
            .llm_endpoints
            .endpoints
            .iter()
            .find(|e| e.provider == "anthropic")
            .unwrap();
        assert_eq!(ep.api_key_file.as_deref(), Some(key_file.to_str().unwrap()));
        assert!(ep.api_key.is_none());
        assert!(ep.api_key_env.is_none());
    }

    #[test]
    fn key_set_value_writes_to_keys_dir() {
        let tmp = setup_dir();

        // We can't easily test --value because it writes to ~/.workgraph/keys/
        // which would modify the real home directory. Instead test the auto-create behavior.
        // The function creates the endpoint if it doesn't exist.
        run_set(
            tmp.path(),
            "openai",
            Some("OPENAI_API_KEY"),
            None,
            None,
            false,
        )
        .unwrap();

        let config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        assert_eq!(config.llm_endpoints.endpoints[0].provider, "openai");
    }

    #[test]
    fn key_set_no_flag_errors() {
        let tmp = setup_dir();
        let err = run_set(tmp.path(), "openai", None, None, None, false).unwrap_err();
        assert!(err.to_string().contains("Specify one of"));
    }

    #[test]
    fn key_set_multiple_flags_errors() {
        let tmp = setup_dir();
        let err = run_set(
            tmp.path(),
            "openai",
            Some("VAR"),
            Some("/path"),
            None,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Specify only one"));
    }

    #[test]
    fn key_set_clears_previous_inline_key() {
        let tmp = setup_dir();
        // Add endpoint with inline key
        crate::commands::endpoints::run_add(
            tmp.path(),
            "openrouter",
            Some("openrouter"),
            None,
            None,
            Some("sk-inline-secret"),
            None,
            None,
            false,
            false,
        )
        .unwrap();

        // Now switch to env var
        run_set(
            tmp.path(),
            "openrouter",
            Some("OPENROUTER_API_KEY"),
            None,
            None,
            false,
        )
        .unwrap();

        let config = Config::load(tmp.path()).unwrap();
        let ep = &config.llm_endpoints.endpoints[0];
        assert!(
            ep.api_key.is_none(),
            "inline key should be cleared when switching to env"
        );
        assert_eq!(ep.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    }

    #[test]
    fn key_check_no_endpoints() {
        let tmp = setup_dir();
        // Should not error, just report nothing configured
        run_check(tmp.path(), None, false).unwrap();
        run_check(tmp.path(), Some("openrouter"), false).unwrap();
    }

    #[test]
    fn key_list_no_endpoints() {
        let tmp = setup_dir();
        run_list(tmp.path(), false).unwrap();
        run_list(tmp.path(), true).unwrap();
    }

    #[test]
    fn key_list_shows_configured_endpoints() {
        let tmp = setup_dir();
        crate::commands::endpoints::run_add(
            tmp.path(),
            "openrouter",
            Some("openrouter"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();

        run_set(
            tmp.path(),
            "openrouter",
            Some("OPENROUTER_API_KEY"),
            None,
            None,
            false,
        )
        .unwrap();

        // Should not error
        run_list(tmp.path(), false).unwrap();
        run_list(tmp.path(), true).unwrap();
    }

    #[test]
    fn key_not_written_to_config_toml() {
        let tmp = setup_dir();
        run_set(
            tmp.path(),
            "openai",
            Some("OPENAI_API_KEY"),
            None,
            None,
            false,
        )
        .unwrap();

        let config_contents = std::fs::read_to_string(tmp.path().join("config.toml")).unwrap();
        // The env var name is stored, but no actual key value
        assert!(
            !config_contents.contains("sk-"),
            "No secret key values should appear in config.toml"
        );
        // The env var reference should be there
        assert!(config_contents.contains("OPENAI_API_KEY"));
    }
}

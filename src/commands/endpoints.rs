//! CLI endpoint management: wg endpoints add/list/remove/set-default/test

use anyhow::{Result, bail};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::path::Path;
use workgraph::config::{Config, EndpointConfig};

/// Known provider names for inference from endpoint name.
const KNOWN_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "openrouter",
    "gemini",
    "ollama",
    "llamacpp",
    "vllm",
    "local",
];

/// Infer provider from endpoint name if it matches a known provider.
/// Falls back to "anthropic" for unrecognized names (backwards compat).
fn infer_provider_from_name(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    for &provider in KNOWN_PROVIDERS {
        if lower == provider {
            return provider;
        }
    }
    "anthropic"
}

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
                let has_key = ep.resolve_api_key(Some(workgraph_dir)).ok().flatten().is_some();
                serde_json::json!({
                    "name": ep.name,
                    "provider": ep.provider,
                    "url": ep.url.as_deref().unwrap_or(EndpointConfig::default_url_for_provider(&ep.provider)),
                    "model": ep.model,
                    "api_key": ep.masked_key(),
                    "key_env": ep.api_key_env,
                    "key_source": ep.key_source(),
                    "key_present": has_key,
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
        let key_status = match ep.resolve_api_key(Some(workgraph_dir)) {
            Ok(Some(_)) => "\u{2713}",
            _ => "\u{2717}",
        };
        println!(
            "  {}{}\n    provider: {}\n    url:      {}\n    model:    {}\n    api_key:  {} {}",
            ep.name,
            default_marker,
            ep.provider,
            url,
            ep.model.as_deref().unwrap_or("(not set)"),
            ep.masked_key(),
            key_status,
        );
        if let Some(ref env_name) = ep.api_key_env {
            println!("    key_env:  {}", env_name);
        }
        println!();
    }
    Ok(())
}

/// Add a new endpoint to the config.
#[allow(clippy::too_many_arguments)]
pub fn run_add(
    workgraph_dir: &Path,
    name: &str,
    provider: Option<&str>,
    url: Option<&str>,
    model: Option<&str>,
    api_key: Option<&str>,
    api_key_file: Option<&str>,
    key_env: Option<&str>,
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

    let provider_str = provider.unwrap_or_else(|| infer_provider_from_name(name));

    config.llm_endpoints.endpoints.push(EndpointConfig {
        name: name.to_string(),
        provider: provider_str.to_string(),
        url: url.map(|s| s.to_string()),
        model: model.map(|s| s.to_string()),
        api_key: api_key.map(|s| s.to_string()),
        api_key_file: api_key_file.map(|s| s.to_string()),
        api_key_env: key_env.map(|s| s.to_string()),
        is_default,
        context_window: None,
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

/// Update an existing endpoint in place, patching only the specified fields.
#[allow(clippy::too_many_arguments)]
pub fn run_update(
    workgraph_dir: &Path,
    name: &str,
    provider: Option<&str>,
    url: Option<&str>,
    model: Option<&str>,
    api_key: Option<&str>,
    api_key_file: Option<&str>,
    key_env: Option<&str>,
    set_default: bool,
    global: bool,
) -> Result<()> {
    let mut config = if global {
        Config::load_global()?.unwrap_or_default()
    } else {
        Config::load(workgraph_dir)?
    };

    let ep = config
        .llm_endpoints
        .endpoints
        .iter_mut()
        .find(|ep| ep.name == name)
        .ok_or_else(|| anyhow::anyhow!("Endpoint '{}' not found.", name))?;

    let mut changed = Vec::new();

    if let Some(p) = provider {
        ep.provider = p.to_string();
        changed.push("provider");
    }
    if let Some(u) = url {
        ep.url = Some(u.to_string());
        changed.push("url");
    }
    if let Some(m) = model {
        ep.model = Some(m.to_string());
        changed.push("model");
    }
    if let Some(k) = api_key {
        ep.api_key = Some(k.to_string());
        ep.api_key_file = None; // clear file-based key when inline key is set
        changed.push("api_key");
    }
    if let Some(f) = api_key_file {
        ep.api_key_file = Some(f.to_string());
        ep.api_key = None; // clear inline key when file-based key is set
        changed.push("api_key_file");
    }
    if let Some(e) = key_env {
        ep.api_key_env = Some(e.to_string());
        changed.push("key_env");
    }

    if set_default {
        // Need to drop mutable borrow on `ep` before iterating again
        let target_name = name.to_string();
        for ep in &mut config.llm_endpoints.endpoints {
            ep.is_default = ep.name == target_name;
        }
        changed.push("default");
    }

    if changed.is_empty() {
        bail!("No fields specified to update. Use --provider, --url, --model, --api-key, --api-key-file, --key-env, or --default.");
    }

    if global {
        config.save_global()?;
    } else {
        config.save(workgraph_dir)?;
    }

    println!(
        "Updated endpoint '{}': {}",
        name,
        changed.join(", ")
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
    if was_default && let Some(ep) = config.llm_endpoints.endpoints.first_mut() {
        ep.is_default = true;
        eprintln!(
            "Note: '{}' was default. Promoted '{}' to default.",
            name, ep.name
        );
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
                        format!("{}...", &body[..body.floor_char_boundary(200)])
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
                        format!("{}...", &body[..body.floor_char_boundary(200)])
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "").unwrap();
        tmp
    }

    // ── add ────────────────────────────────────────────────────────────

    #[test]
    fn cli_endpoint_add_persists() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "my-ep",
            Some("openai"),
            Some("https://api.openai.com/v1"),
            Some("gpt-4o"),
            Some("sk-test"),
            None,
            None,
            false,
            false,
        )
        .unwrap();

        let config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.name, "my-ep");
        assert_eq!(ep.provider, "openai");
        assert_eq!(ep.url.as_deref(), Some("https://api.openai.com/v1"));
        assert_eq!(ep.model.as_deref(), Some("gpt-4o"));
        assert_eq!(ep.api_key.as_deref(), Some("sk-test"));
        assert!(ep.is_default, "first endpoint auto-defaults");
    }

    #[test]
    fn cli_endpoint_add_with_key_file() {
        let tmp = setup_dir();
        let kf = tmp.path().join("key.txt");
        std::fs::write(&kf, "sk-from-file\n").unwrap();

        run_add(
            tmp.path(),
            "file-ep",
            Some("anthropic"),
            None,
            None,
            None,
            Some(kf.to_str().unwrap()),
            None,
            false,
            false,
        )
        .unwrap();

        let config = Config::load(tmp.path()).unwrap();
        let ep = &config.llm_endpoints.endpoints[0];
        assert!(ep.api_key.is_none());
        assert!(ep.api_key_file.is_some());
        let key = ep.resolve_api_key(Some(tmp.path())).unwrap();
        assert_eq!(key.as_deref(), Some("sk-from-file"));
    }

    #[test]
    fn cli_endpoint_add_unknown_name_defaults_to_anthropic() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "bare",
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.llm_endpoints.endpoints[0].provider, "anthropic");
    }

    #[test]
    fn cli_endpoint_add_infers_openrouter_from_name() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "openrouter",
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.provider, "openrouter");
    }

    #[test]
    fn cli_endpoint_add_infers_anthropic_from_name() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "anthropic",
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.provider, "anthropic");
    }

    #[test]
    fn cli_endpoint_add_explicit_provider_overrides_name_inference() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "openrouter",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.provider, "openai");
    }

    #[test]
    fn infer_provider_known_names() {
        assert_eq!(infer_provider_from_name("openrouter"), "openrouter");
        assert_eq!(infer_provider_from_name("OpenRouter"), "openrouter");
        assert_eq!(infer_provider_from_name("OPENROUTER"), "openrouter");
        assert_eq!(infer_provider_from_name("anthropic"), "anthropic");
        assert_eq!(infer_provider_from_name("openai"), "openai");
        assert_eq!(infer_provider_from_name("gemini"), "gemini");
        assert_eq!(infer_provider_from_name("ollama"), "ollama");
        assert_eq!(infer_provider_from_name("llamacpp"), "llamacpp");
        assert_eq!(infer_provider_from_name("vllm"), "vllm");
        assert_eq!(infer_provider_from_name("local"), "local");
    }

    #[test]
    fn infer_provider_unknown_defaults_to_anthropic() {
        assert_eq!(infer_provider_from_name("my-custom-ep"), "anthropic");
        assert_eq!(infer_provider_from_name("production"), "anthropic");
    }

    #[test]
    fn cli_endpoint_add_duplicate_errors() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "dup",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let err = run_add(
            tmp.path(),
            "dup",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn cli_endpoint_add_first_auto_default() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "first",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let config = Config::load(tmp.path()).unwrap();
        assert!(config.llm_endpoints.endpoints[0].is_default);
    }

    #[test]
    fn cli_endpoint_add_second_not_auto_default() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "a",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        run_add(
            tmp.path(),
            "b",
            Some("anthropic"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        let config = Config::load(tmp.path()).unwrap();
        assert!(config.llm_endpoints.endpoints[0].is_default);
        assert!(!config.llm_endpoints.endpoints[1].is_default);
    }

    #[test]
    fn cli_endpoint_add_explicit_default_clears_others() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "a",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        run_add(
            tmp.path(),
            "b",
            Some("anthropic"),
            None,
            None,
            None,
            None,
            None,
            true,
            false,
        )
        .unwrap();
        let config = Config::load(tmp.path()).unwrap();
        assert!(!config.llm_endpoints.endpoints[0].is_default);
        assert!(config.llm_endpoints.endpoints[1].is_default);
    }

    // ── update ─────────────────────────────────────────────────────────

    #[test]
    fn cli_endpoint_update_patches_api_key_file() {
        let tmp = setup_dir();
        run_add(
            tmp.path(), "ep1", Some("openai"), Some("https://api.openai.com/v1"),
            Some("gpt-4o"), Some("sk-old"), None, None, false, false,
        ).unwrap();

        let kf = tmp.path().join("newkey.txt");
        std::fs::write(&kf, "sk-new-from-file\n").unwrap();

        run_update(
            tmp.path(), "ep1", None, None, None, None,
            Some(kf.to_str().unwrap()), None, false, false,
        ).unwrap();

        let config = Config::load(tmp.path()).unwrap();
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.provider, "openai", "provider unchanged");
        assert_eq!(ep.url.as_deref(), Some("https://api.openai.com/v1"), "url unchanged");
        assert_eq!(ep.model.as_deref(), Some("gpt-4o"), "model unchanged");
        assert!(ep.api_key.is_none(), "inline key cleared when api_key_file set");
        assert!(ep.api_key_file.is_some(), "api_key_file set");
        let key = ep.resolve_api_key(Some(tmp.path())).unwrap();
        assert_eq!(key.as_deref(), Some("sk-new-from-file"));
    }

    #[test]
    fn cli_endpoint_update_patches_provider() {
        let tmp = setup_dir();
        run_add(
            tmp.path(), "ep1", Some("openai"), None, None, None, None, None, false, false,
        ).unwrap();

        run_update(
            tmp.path(), "ep1", Some("anthropic"), None, None, None, None, None, false, false,
        ).unwrap();

        let config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.llm_endpoints.endpoints[0].provider, "anthropic");
    }

    #[test]
    fn cli_endpoint_update_nonexistent_errors() {
        let tmp = setup_dir();
        let err = run_update(
            tmp.path(), "nope", Some("openai"), None, None, None, None, None, false, false,
        ).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn cli_endpoint_update_no_fields_errors() {
        let tmp = setup_dir();
        run_add(
            tmp.path(), "ep1", Some("openai"), None, None, None, None, None, false, false,
        ).unwrap();

        let err = run_update(
            tmp.path(), "ep1", None, None, None, None, None, None, false, false,
        ).unwrap_err();
        assert!(err.to_string().contains("No fields specified"));
    }

    #[test]
    fn cli_endpoint_update_set_default() {
        let tmp = setup_dir();
        run_add(
            tmp.path(), "a", Some("openai"), None, None, None, None, None, true, false,
        ).unwrap();
        run_add(
            tmp.path(), "b", Some("anthropic"), None, None, None, None, None, false, false,
        ).unwrap();

        // "b" is not default
        let config = Config::load(tmp.path()).unwrap();
        assert!(!config.llm_endpoints.endpoints[1].is_default);

        run_update(
            tmp.path(), "b", None, None, None, None, None, None, true, false,
        ).unwrap();

        let config = Config::load(tmp.path()).unwrap();
        assert!(!config.llm_endpoints.endpoints[0].is_default, "a no longer default");
        assert!(config.llm_endpoints.endpoints[1].is_default, "b is now default");
    }

    #[test]
    fn cli_endpoint_update_multiple_fields() {
        let tmp = setup_dir();
        run_add(
            tmp.path(), "ep1", Some("openai"), None, None, None, None, None, false, false,
        ).unwrap();

        run_update(
            tmp.path(), "ep1", Some("anthropic"), Some("https://custom.url/v1"),
            Some("claude-4"), None, None, Some("MY_KEY_ENV"), false, false,
        ).unwrap();

        let config = Config::load(tmp.path()).unwrap();
        let ep = &config.llm_endpoints.endpoints[0];
        assert_eq!(ep.provider, "anthropic");
        assert_eq!(ep.url.as_deref(), Some("https://custom.url/v1"));
        assert_eq!(ep.model.as_deref(), Some("claude-4"));
        assert_eq!(ep.api_key_env.as_deref(), Some("MY_KEY_ENV"));
    }

    // ── list ───────────────────────────────────────────────────────────

    #[test]
    fn cli_endpoint_list_empty() {
        let tmp = setup_dir();
        run_list(tmp.path(), false).unwrap();
        run_list(tmp.path(), true).unwrap();
    }

    #[test]
    fn cli_endpoint_list_with_data() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "ep1",
            Some("openai"),
            None,
            Some("gpt-4o"),
            Some("sk-1"),
            None,
            None,
            true,
            false,
        )
        .unwrap();
        run_add(
            tmp.path(),
            "ep2",
            Some("anthropic"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        run_list(tmp.path(), false).unwrap();
        run_list(tmp.path(), true).unwrap();
    }

    // ── remove ─────────────────────────────────────────────────────────

    #[test]
    fn cli_endpoint_remove_cleans_up() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "x",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            Config::load(tmp.path())
                .unwrap()
                .llm_endpoints
                .endpoints
                .len(),
            1
        );

        run_remove(tmp.path(), "x", false).unwrap();
        assert!(
            Config::load(tmp.path())
                .unwrap()
                .llm_endpoints
                .endpoints
                .is_empty()
        );
    }

    #[test]
    fn cli_endpoint_remove_nonexistent_errors() {
        let tmp = setup_dir();
        let err = run_remove(tmp.path(), "nope", false).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn cli_endpoint_remove_default_promotes_next() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "a",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            true,
            false,
        )
        .unwrap();
        run_add(
            tmp.path(),
            "b",
            Some("anthropic"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();

        run_remove(tmp.path(), "a", false).unwrap();
        let config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);
        assert_eq!(config.llm_endpoints.endpoints[0].name, "b");
        assert!(config.llm_endpoints.endpoints[0].is_default);
    }

    // ── set-default ────────────────────────────────────────────────────

    #[test]
    fn cli_endpoint_set_default_switches() {
        let tmp = setup_dir();
        run_add(
            tmp.path(),
            "a",
            Some("openai"),
            None,
            None,
            None,
            None,
            None,
            true,
            false,
        )
        .unwrap();
        run_add(
            tmp.path(),
            "b",
            Some("anthropic"),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();

        run_set_default(tmp.path(), "b", false).unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let a = config
            .llm_endpoints
            .endpoints
            .iter()
            .find(|e| e.name == "a")
            .unwrap();
        let b = config
            .llm_endpoints
            .endpoints
            .iter()
            .find(|e| e.name == "b")
            .unwrap();
        assert!(!a.is_default);
        assert!(b.is_default);
    }

    #[test]
    fn cli_endpoint_set_default_nonexistent_errors() {
        let tmp = setup_dir();
        let err = run_set_default(tmp.path(), "nope", false).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ── test (connectivity) ────────────────────────────────────────────

    fn mock_server(status: u16, body: &str) -> String {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}", addr.port());
        let body = body.to_string();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body,
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        url
    }

    #[test]
    fn cli_endpoint_test_success() {
        let mock_url = mock_server(200, r#"{"data":[]}"#);
        let tmp = setup_dir();
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "ok-ep".into(),
            provider: "openai".into(),
            url: Some(mock_url),
            model: None,
            api_key: Some("sk-test".into()),
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        });
        config.save(tmp.path()).unwrap();

        run_test(tmp.path(), "ok-ep").unwrap();
    }

    #[test]
    fn cli_endpoint_test_auth_failure_does_not_bail() {
        let mock_url = mock_server(401, r#"{"error":"unauthorized"}"#);
        let tmp = setup_dir();
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "bad-ep".into(),
            provider: "openai".into(),
            url: Some(mock_url),
            model: None,
            api_key: Some("sk-bad".into()),
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        });
        config.save(tmp.path()).unwrap();

        run_test(tmp.path(), "bad-ep").unwrap();
    }

    #[test]
    fn cli_endpoint_test_no_key() {
        let mock_url = mock_server(200, r#"{"data":[]}"#);
        let tmp = setup_dir();
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "nokey-ep".into(),
            provider: "openai".into(),
            url: Some(mock_url),
            model: None,
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        });
        config.save(tmp.path()).unwrap();

        run_test(tmp.path(), "nokey-ep").unwrap();
    }

    #[test]
    fn cli_endpoint_test_nonexistent_errors() {
        let tmp = setup_dir();
        let err = run_test(tmp.path(), "nope").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn cli_endpoint_test_connection_refused() {
        let tmp = setup_dir();
        let mut config = Config::default();
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name: "dead".into(),
            provider: "openai".into(),
            url: Some("http://127.0.0.1:1".into()),
            model: None,
            api_key: Some("sk-x".into()),
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        });
        config.save(tmp.path()).unwrap();

        let err = run_test(tmp.path(), "dead").unwrap_err();
        assert!(err.to_string().contains("Could not connect"));
    }

    // ── full lifecycle ─────────────────────────────────────────────────

    #[test]
    fn cli_endpoint_full_lifecycle() {
        let tmp = setup_dir();

        run_add(
            tmp.path(),
            "ep-a",
            Some("openai"),
            None,
            Some("gpt-4o"),
            Some("sk-a"),
            None,
            None,
            true,
            false,
        )
        .unwrap();
        run_add(
            tmp.path(),
            "ep-b",
            Some("anthropic"),
            None,
            Some("sonnet"),
            Some("sk-b"),
            None,
            None,
            false,
            false,
        )
        .unwrap();

        run_list(tmp.path(), false).unwrap();
        run_list(tmp.path(), true).unwrap();

        run_set_default(tmp.path(), "ep-b", false).unwrap();
        let config = Config::load(tmp.path()).unwrap();
        let b = config
            .llm_endpoints
            .endpoints
            .iter()
            .find(|e| e.name == "ep-b")
            .unwrap();
        assert!(b.is_default);

        run_remove(tmp.path(), "ep-a", false).unwrap();
        let config = Config::load(tmp.path()).unwrap();
        assert_eq!(config.llm_endpoints.endpoints.len(), 1);

        run_remove(tmp.path(), "ep-b", false).unwrap();
        let config = Config::load(tmp.path()).unwrap();
        assert!(config.llm_endpoints.endpoints.is_empty());
    }
}

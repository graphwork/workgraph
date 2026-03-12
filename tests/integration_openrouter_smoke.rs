//! OpenRouter end-to-end smoke test.
//!
//! Validates the full endpoint configuration flow:
//! config → role binding → model resolution → client creation.
//! No actual API calls are made.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;
use workgraph::config::{Config, DispatchRole, EndpointConfig, EndpointsConfig};
use workgraph::graph::WorkGraph;
use workgraph::parser::save_graph;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    stdout
}

fn setup_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

// ===========================================================================
// 1. Config → resolution → client creation (unit-style, in-process)
// ===========================================================================

#[test]
fn openrouter_endpoint_config_roundtrip() {
    // Build a config with an OpenRouter endpoint
    let mut config = Config::default();
    config.llm_endpoints = EndpointsConfig {
        endpoints: vec![EndpointConfig {
            name: "my-openrouter".to_string(),
            provider: "openrouter".to_string(),
            url: Some("https://openrouter.ai/api/v1".to_string()),
            model: Some("anthropic/claude-sonnet-4-20250514".to_string()),
            api_key: Some("sk-or-test-key-1234567890".to_string()),
            api_key_file: None,
            is_default: true,
        }],
    };

    // Verify find_by_name works
    let ep = config.llm_endpoints.find_by_name("my-openrouter");
    assert!(ep.is_some(), "find_by_name should return the endpoint");
    let ep = ep.unwrap();
    assert_eq!(ep.provider, "openrouter");
    assert_eq!(ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
    assert_eq!(
        ep.resolve_api_key(None).unwrap(),
        Some("sk-or-test-key-1234567890".to_string())
    );

    // Verify find_for_provider works
    let ep2 = config.llm_endpoints.find_for_provider("openrouter");
    assert!(ep2.is_some(), "find_for_provider should match openrouter");
    assert_eq!(ep2.unwrap().name, "my-openrouter");
}

#[test]
fn openrouter_endpoint_bound_to_evaluator_resolves_correctly() {
    let mut config = Config::default();

    // Add an OpenRouter endpoint
    config.llm_endpoints = EndpointsConfig {
        endpoints: vec![EndpointConfig {
            name: "my-openrouter".to_string(),
            provider: "openrouter".to_string(),
            url: Some("https://openrouter.ai/api/v1".to_string()),
            model: Some("anthropic/claude-sonnet-4-20250514".to_string()),
            api_key: Some("sk-or-test-key".to_string()),
            api_key_file: None,
            is_default: true,
        }],
    };

    // Bind the endpoint to the evaluator role
    config.models.set_model(
        DispatchRole::Evaluator,
        "anthropic/claude-sonnet-4-20250514",
    );
    config
        .models
        .set_provider(DispatchRole::Evaluator, "openrouter");
    config
        .models
        .set_endpoint(DispatchRole::Evaluator, "my-openrouter");

    // Resolve model for evaluator
    let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
    assert_eq!(resolved.model, "anthropic/claude-sonnet-4-20250514");
    assert_eq!(resolved.provider, Some("openrouter".to_string()));
    assert_eq!(resolved.endpoint, Some("my-openrouter".to_string()));

    // Verify we can look up the endpoint from the resolved config
    let ep = config
        .llm_endpoints
        .find_by_name(resolved.endpoint.as_deref().unwrap());
    assert!(ep.is_some());
    let ep = ep.unwrap();
    assert_eq!(ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
    assert_eq!(
        ep.resolve_api_key(None).unwrap(),
        Some("sk-or-test-key".to_string())
    );
}

#[test]
fn openrouter_client_creation_from_resolved_config() {
    // Simulates what call_openai_native / create_provider_ext does:
    // resolve endpoint → extract key + url → create OpenAiClient
    use workgraph::executor::native::openai_client::OpenAiClient;

    let mut config = Config::default();
    config.llm_endpoints = EndpointsConfig {
        endpoints: vec![EndpointConfig {
            name: "or-prod".to_string(),
            provider: "openrouter".to_string(),
            url: Some("https://openrouter.ai/api/v1".to_string()),
            model: None,
            api_key: Some("sk-or-v1-realkey".to_string()),
            api_key_file: None,
            is_default: true,
        }],
    };

    config.models.set_model(
        DispatchRole::Evaluator,
        "anthropic/claude-sonnet-4-20250514",
    );
    config
        .models
        .set_provider(DispatchRole::Evaluator, "openrouter");
    config
        .models
        .set_endpoint(DispatchRole::Evaluator, "or-prod");

    let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);

    // Look up endpoint by name (what the dispatch code does)
    let endpoint = resolved
        .endpoint
        .as_deref()
        .and_then(|name| config.llm_endpoints.find_by_name(name));
    assert!(endpoint.is_some());
    let endpoint = endpoint.unwrap();

    let endpoint_key = endpoint.resolve_api_key(None).unwrap();
    let endpoint_url = endpoint.url.clone();

    assert_eq!(endpoint_key, Some("sk-or-v1-realkey".to_string()));
    assert_eq!(
        endpoint_url,
        Some("https://openrouter.ai/api/v1".to_string())
    );

    // Create OpenAI client from resolved config (same as call_openai_native)
    let client = OpenAiClient::new(endpoint_key.unwrap(), &resolved.model, None)
        .expect("client creation should succeed");
    let client = client.with_base_url(endpoint_url.as_deref().unwrap());
    let client = client.with_provider_hint("openrouter");

    assert_eq!(client.model, "anthropic/claude-sonnet-4-20250514");
    // The client was created — that's the proof the full path works.
    // We can't inspect base_url directly (private), but we verified it was set.
    let _ = client;
}

// ===========================================================================
// 2. Mixed endpoints — different roles, different providers
// ===========================================================================

#[test]
fn mixed_endpoints_different_roles_different_providers() {
    let mut config = Config::default();

    config.llm_endpoints = EndpointsConfig {
        endpoints: vec![
            EndpointConfig {
                name: "anthropic-direct".to_string(),
                provider: "anthropic".to_string(),
                url: Some("https://api.anthropic.com".to_string()),
                model: None,
                api_key: Some("sk-ant-key-direct".to_string()),
                api_key_file: None,
                is_default: true,
            },
            EndpointConfig {
                name: "openrouter-eval".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: None,
                api_key: Some("sk-or-eval-key".to_string()),
                api_key_file: None,
                is_default: false,
            },
        ],
    };

    // Task agent uses Anthropic direct
    config
        .models
        .set_model(DispatchRole::TaskAgent, "claude-sonnet-4-20250514");
    config
        .models
        .set_provider(DispatchRole::TaskAgent, "anthropic");
    config
        .models
        .set_endpoint(DispatchRole::TaskAgent, "anthropic-direct");

    // Evaluator uses OpenRouter
    config.models.set_model(
        DispatchRole::Evaluator,
        "anthropic/claude-sonnet-4-20250514",
    );
    config
        .models
        .set_provider(DispatchRole::Evaluator, "openrouter");
    config
        .models
        .set_endpoint(DispatchRole::Evaluator, "openrouter-eval");

    // Resolve task_agent
    let task_resolved = config.resolve_model_for_role(DispatchRole::TaskAgent);
    assert_eq!(task_resolved.model, "claude-sonnet-4-20250514");
    assert_eq!(task_resolved.provider, Some("anthropic".to_string()));
    assert_eq!(task_resolved.endpoint, Some("anthropic-direct".to_string()));

    // Resolve evaluator
    let eval_resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
    assert_eq!(eval_resolved.model, "anthropic/claude-sonnet-4-20250514");
    assert_eq!(eval_resolved.provider, Some("openrouter".to_string()));
    assert_eq!(eval_resolved.endpoint, Some("openrouter-eval".to_string()));

    // Verify endpoint lookups get the correct keys
    let task_ep = config
        .llm_endpoints
        .find_by_name(task_resolved.endpoint.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        task_ep.resolve_api_key(None).unwrap(),
        Some("sk-ant-key-direct".to_string())
    );

    let eval_ep = config
        .llm_endpoints
        .find_by_name(eval_resolved.endpoint.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        eval_ep.resolve_api_key(None).unwrap(),
        Some("sk-or-eval-key".to_string())
    );
}

#[test]
fn endpoint_cascades_from_default_role() {
    // If only `default` has an endpoint, other roles inherit it
    let mut config = Config::default();

    config.llm_endpoints = EndpointsConfig {
        endpoints: vec![EndpointConfig {
            name: "global-or".to_string(),
            provider: "openrouter".to_string(),
            url: Some("https://openrouter.ai/api/v1".to_string()),
            model: None,
            api_key: Some("sk-or-global".to_string()),
            api_key_file: None,
            is_default: true,
        }],
    };

    // Set default endpoint + provider
    config
        .models
        .set_endpoint(DispatchRole::Default, "global-or");
    config
        .models
        .set_provider(DispatchRole::Default, "openrouter");

    // Evaluator has no explicit endpoint — should inherit from default
    let resolved = config.resolve_model_for_role(DispatchRole::Evaluator);
    assert_eq!(
        resolved.endpoint,
        Some("global-or".to_string()),
        "Evaluator should inherit endpoint from default"
    );
    assert_eq!(
        resolved.provider,
        Some("openrouter".to_string()),
        "Evaluator should inherit provider from default"
    );
}

// ===========================================================================
// 3. Key file loading end-to-end
// ===========================================================================

#[test]
fn api_key_file_loading_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let key_file = tmp.path().join("openrouter.key");
    fs::write(&key_file, "  sk-or-from-file-12345  \n").unwrap();

    let ep = EndpointConfig {
        name: "or-file".to_string(),
        provider: "openrouter".to_string(),
        url: Some("https://openrouter.ai/api/v1".to_string()),
        model: None,
        api_key: None,
        api_key_file: Some(key_file.to_string_lossy().to_string()),
        is_default: true,
    };

    // resolve_api_key should read and trim the file
    let key = ep.resolve_api_key(None).unwrap();
    assert_eq!(key, Some("sk-or-from-file-12345".to_string()));

    // masked_key should show "(from file)" since api_key is None
    assert_eq!(ep.masked_key(), "(from file)");
}

#[test]
fn api_key_file_relative_to_workgraph_dir() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let key_file = wg_dir.join("secrets").join("or.key");
    fs::create_dir_all(key_file.parent().unwrap()).unwrap();
    fs::write(&key_file, "sk-relative-key").unwrap();

    let ep = EndpointConfig {
        name: "or-relative".to_string(),
        provider: "openrouter".to_string(),
        url: None,
        model: None,
        api_key: None,
        api_key_file: Some("secrets/or.key".to_string()),
        is_default: false,
    };

    // With workgraph_dir, relative path resolves correctly
    let key = ep.resolve_api_key(Some(&wg_dir)).unwrap();
    assert_eq!(key, Some("sk-relative-key".to_string()));
}

#[test]
fn api_key_file_missing_returns_error() {
    let ep = EndpointConfig {
        name: "bad-file".to_string(),
        provider: "openrouter".to_string(),
        url: None,
        model: None,
        api_key: None,
        api_key_file: Some("/nonexistent/path/key.txt".to_string()),
        is_default: false,
    };

    let result = ep.resolve_api_key(None);
    assert!(result.is_err(), "Missing key file should return an error");
}

#[test]
fn api_key_file_empty_returns_error() {
    let tmp = TempDir::new().unwrap();
    let key_file = tmp.path().join("empty.key");
    fs::write(&key_file, "   \n  ").unwrap();

    let ep = EndpointConfig {
        name: "empty-file".to_string(),
        provider: "openrouter".to_string(),
        url: None,
        model: None,
        api_key: None,
        api_key_file: Some(key_file.to_string_lossy().to_string()),
        is_default: false,
    };

    let result = ep.resolve_api_key(None);
    assert!(result.is_err(), "Empty key file should return an error");
}

#[test]
fn api_key_takes_priority_over_key_file() {
    let tmp = TempDir::new().unwrap();
    let key_file = tmp.path().join("should-not-be-read.key");
    fs::write(&key_file, "file-key").unwrap();

    let ep = EndpointConfig {
        name: "priority-test".to_string(),
        provider: "openrouter".to_string(),
        url: None,
        model: None,
        api_key: Some("inline-key".to_string()),
        api_key_file: Some(key_file.to_string_lossy().to_string()),
        is_default: false,
    };

    // api_key should win over api_key_file
    let key = ep.resolve_api_key(None).unwrap();
    assert_eq!(key, Some("inline-key".to_string()));
}

// ===========================================================================
// 4. CLI integration: wg endpoints add/list/remove/set-default
// ===========================================================================

#[test]
fn cli_endpoints_add_and_list() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Add an OpenRouter endpoint
    let output = wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "my-or",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-or-test-key-12345678",
        ],
    );
    assert!(output.contains("Added endpoint 'my-or'"));
    assert!(output.contains("openrouter"));

    // List endpoints
    let list = wg_ok(&wg_dir, &["endpoints", "list"]);
    assert!(list.contains("my-or"));
    assert!(list.contains("openrouter"));
    assert!(list.contains("default")); // first endpoint becomes default

    // JSON list
    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "my-or");
    assert_eq!(arr[0]["provider"], "openrouter");
    assert_eq!(arr[0]["is_default"], true);
}

#[test]
fn cli_endpoints_add_with_key_file() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Write a temp key file
    let key_file = tmp.path().join("or.key");
    {
        let mut f = fs::File::create(&key_file).unwrap();
        writeln!(f, "sk-or-from-file-test").unwrap();
    }

    // Add endpoint with --api-key-file
    let output = wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "or-from-file",
            "--provider",
            "openrouter",
            "--api-key-file",
            &key_file.to_string_lossy(),
        ],
    );
    assert!(output.contains("Added endpoint 'or-from-file'"));

    // List should show "(from file)" for the key
    let list = wg_ok(&wg_dir, &["endpoints", "list"]);
    assert!(list.contains("(from file)"));
}

#[test]
fn cli_endpoints_remove() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Add and then remove
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "temp-ep",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-temp",
        ],
    );
    let output = wg_ok(&wg_dir, &["endpoints", "remove", "temp-ep"]);
    assert!(output.contains("Removed endpoint 'temp-ep'"));

    // List should show no endpoints
    let list = wg_ok(&wg_dir, &["endpoints", "list"]);
    assert!(list.contains("No endpoints configured"));
}

#[test]
fn cli_endpoints_set_default() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Add two endpoints
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "ep-a",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-a",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "ep-b",
            "--provider",
            "openai",
            "--api-key",
            "sk-b",
        ],
    );

    // ep-a is default (first added). Set ep-b as default.
    let output = wg_ok(&wg_dir, &["endpoints", "set-default", "ep-b"]);
    assert!(output.contains("Set 'ep-b' as default"));

    // Verify via JSON
    let json_list = wg_ok(&wg_dir, &["--json", "endpoints", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_list).unwrap();
    let arr = parsed.as_array().unwrap();
    let ep_a = arr.iter().find(|v| v["name"] == "ep-a").unwrap();
    let ep_b = arr.iter().find(|v| v["name"] == "ep-b").unwrap();
    assert_eq!(ep_a["is_default"], false);
    assert_eq!(ep_b["is_default"], true);
}

// ===========================================================================
// 5. CLI: wg endpoints test (with mock server)
// ===========================================================================

#[test]
fn cli_endpoints_test_with_mock_server() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    // Start a tiny HTTP server that returns a 200 for /models
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{}", port);

    let handle = thread::spawn(move || {
        // Accept one connection, return a 200
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"data\":[]}";
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });

    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Add endpoint pointing at mock server
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "mock-or",
            "--provider",
            "openrouter",
            "--url",
            &base_url,
            "--api-key",
            "sk-or-mock",
        ],
    );

    // Test connectivity
    let output = wg_ok(&wg_dir, &["endpoints", "test", "mock-or"]);
    assert!(
        output.contains("OK"),
        "Expected OK in test output, got: {}",
        output
    );

    handle.join().unwrap();
}

// ===========================================================================
// 6. CLI: wg config --set-endpoint binds endpoint to role
// ===========================================================================

#[test]
fn cli_set_endpoint_for_role() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Add an endpoint first
    wg_ok(
        &wg_dir,
        &[
            "endpoints",
            "add",
            "or-eval",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-or-eval",
        ],
    );

    // Bind it to evaluator
    let output = wg_ok(
        &wg_dir,
        &["config", "--set-endpoint", "evaluator", "or-eval"],
    );
    assert!(
        output.contains("endpoint") || output.contains("Set"),
        "Expected confirmation of endpoint binding, got: {}",
        output
    );

    // Verify via config --models or --show
    let show = wg_ok(&wg_dir, &["config", "--models"]);
    assert!(
        show.contains("or-eval"),
        "Expected endpoint name in --models output, got: {}",
        show
    );
}

// ===========================================================================
// 7. Config serialization/deserialization roundtrip with endpoints
// ===========================================================================

#[test]
fn config_toml_roundtrip_with_endpoints() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    let mut config = Config::default();
    config.llm_endpoints = EndpointsConfig {
        endpoints: vec![
            EndpointConfig {
                name: "openrouter-main".to_string(),
                provider: "openrouter".to_string(),
                url: Some("https://openrouter.ai/api/v1".to_string()),
                model: Some("anthropic/claude-sonnet-4-20250514".to_string()),
                api_key: Some("sk-or-roundtrip-key".to_string()),
                api_key_file: None,
                is_default: true,
            },
            EndpointConfig {
                name: "anthropic-direct".to_string(),
                provider: "anthropic".to_string(),
                url: None,
                model: None,
                api_key: Some("sk-ant-roundtrip".to_string()),
                api_key_file: None,
                is_default: false,
            },
        ],
    };

    // Set evaluator to use the OpenRouter endpoint
    config.models.set_model(
        DispatchRole::Evaluator,
        "anthropic/claude-sonnet-4-20250514",
    );
    config
        .models
        .set_provider(DispatchRole::Evaluator, "openrouter");
    config
        .models
        .set_endpoint(DispatchRole::Evaluator, "openrouter-main");

    // Save
    config.save(&wg_dir).unwrap();

    // Reload
    let loaded = Config::load(&wg_dir).unwrap();

    // Verify endpoints survived roundtrip
    assert_eq!(loaded.llm_endpoints.endpoints.len(), 2);
    let or_ep = loaded
        .llm_endpoints
        .find_by_name("openrouter-main")
        .unwrap();
    assert_eq!(or_ep.provider, "openrouter");
    assert_eq!(or_ep.url.as_deref(), Some("https://openrouter.ai/api/v1"));
    assert_eq!(or_ep.api_key.as_deref(), Some("sk-or-roundtrip-key"));
    assert!(or_ep.is_default);

    // Verify model routing survived
    let resolved = loaded.resolve_model_for_role(DispatchRole::Evaluator);
    assert_eq!(resolved.model, "anthropic/claude-sonnet-4-20250514");
    assert_eq!(resolved.provider, Some("openrouter".to_string()));
    assert_eq!(resolved.endpoint, Some("openrouter-main".to_string()));

    // Task agent should still get default Anthropic (no explicit config)
    let task_resolved = loaded.resolve_model_for_role(DispatchRole::TaskAgent);
    assert!(
        task_resolved.endpoint.is_none()
            || task_resolved.endpoint.as_deref() == Some("openrouter-main"),
        "Task agent without explicit endpoint config: {:?}",
        task_resolved.endpoint
    );
}

// ===========================================================================
// 8. Default URL for provider
// ===========================================================================

#[test]
fn default_url_for_known_providers() {
    assert_eq!(
        EndpointConfig::default_url_for_provider("openrouter"),
        "https://openrouter.ai/api/v1"
    );
    assert_eq!(
        EndpointConfig::default_url_for_provider("openai"),
        "https://api.openai.com/v1"
    );
    assert_eq!(
        EndpointConfig::default_url_for_provider("anthropic"),
        "https://api.anthropic.com"
    );
    assert_eq!(
        EndpointConfig::default_url_for_provider("local"),
        "http://localhost:11434/v1"
    );
    assert_eq!(EndpointConfig::default_url_for_provider("unknown"), "");
}

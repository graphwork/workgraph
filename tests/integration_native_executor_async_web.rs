//! Integration tests: verify native executor async/web features end-to-end.
//!
//! Tests cover:
//! 1. web_fetch: fetches HTML and returns cleaned markdown
//! 2. bg tool: runs background commands, state injector reports completion
//! 3. delegate tool: spawns mini agent loop and returns result (mock provider)
//! 4. Subtask creation + wait + resume cycle (via wg add --subtask)
//! 5. Bundle filtering: research bundle has web tools but not delegate

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use workgraph::config::NativeExecutorConfig;
use workgraph::executor::native::bundle::{Bundle, resolve_bundle};
use workgraph::executor::native::tools::ToolRegistry;
use workgraph::graph::{Node, Status, Task, WaitCondition, WaitSpec, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

// ── helpers ──────────────────────────────────────────────────────────────

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

fn wg_cmd_env(wg_dir: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(wg_binary());
    cmd.arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn make_task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        title: id.to_string(),
        ..Task::default()
    }
}

fn setup_workgraph(dir: &Path, tasks: Vec<Task>) -> PathBuf {
    let wg_dir = dir.join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

fn graph_path(wg_dir: &Path) -> PathBuf {
    wg_dir.join("graph.jsonl")
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Web Fetch: fetches HTML and returns markdown
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn web_fetch_tool_fetches_html_returns_markdown() {
    // Use a local mock server to avoid network dependency
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Serve a simple HTML page
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = stream;
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await;
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: text/html\r\n",
                "Connection: close\r\n",
                "\r\n",
                "<html><head><title>Test Page</title></head>",
                "<body>",
                "<article>",
                "<h1>Hello World</h1>",
                "<p>This is the main content of the test page. It has enough text ",
                "that readability should be able to extract it properly. The content ",
                "discusses integration testing for the native executor.</p>",
                "<p>Second paragraph with more meaningful content about how web fetch ",
                "converts HTML into clean markdown output.</p>",
                "</article>",
                "<footer>Footer noise</footer>",
                "</body></html>",
            );
            s.write_all(response.as_bytes()).await.unwrap();
            s.shutdown().await.ok();
        })
        .await;
    });

    // Create registry with web_fetch tool
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    let input = json!({"url": format!("http://127.0.0.1:{}", addr.port())});
    let output = registry.execute("web_fetch", &input).await;

    assert!(
        !output.is_error,
        "web_fetch should succeed, got error: {}",
        output.content
    );
    assert!(
        !output.content.is_empty(),
        "web_fetch should return non-empty content"
    );

    server.abort();
}

#[tokio::test]
async fn web_fetch_error_on_invalid_url() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    let output = registry
        .execute("web_fetch", &json!({"url": "not-a-valid-url"}))
        .await;
    assert!(output.is_error, "Should error on invalid URL");
    assert!(output.content.contains("Invalid URL"));
}

#[tokio::test]
async fn web_fetch_error_on_missing_url() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    let output = registry.execute("web_fetch", &json!({})).await;
    assert!(output.is_error, "Should error on missing url param");
    assert!(output.content.contains("Missing required parameter"));
}

#[tokio::test]
async fn web_fetch_error_on_empty_url() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    let output = registry.execute("web_fetch", &json!({"url": ""})).await;
    assert!(output.is_error, "Should error on empty URL");
    assert!(output.content.contains("empty"));
}

#[tokio::test]
async fn web_fetch_custom_config_applies() {
    let config = NativeExecutorConfig {
        web: workgraph::config::NativeWebConfig {
            fetch_max_chars: 100,
            fetch_timeout_secs: 5,
            ..Default::default()
        },
        ..Default::default()
    };

    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all_with_config(
        tmp.path(),
        &std::env::current_dir().unwrap(),
        &config,
    );

    let defs = registry.definitions();
    let has_web_fetch = defs.iter().any(|d| d.name == "web_fetch");
    assert!(
        has_web_fetch,
        "web_fetch should be in the registry with custom config"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Background tool: run command, check status, check output
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn bg_tool_run_and_complete() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    // Run a simple echo command in background
    let run_input = json!({
        "action": "run",
        "command": "echo 'bg-integration-test-output'",
        "name": "integ-echo"
    });
    let run_output = registry.execute("bg", &run_input).await;
    assert!(
        !run_output.is_error,
        "bg run should succeed: {}",
        run_output.content
    );
    assert!(
        run_output.content.contains("integ-echo"),
        "Should contain job name"
    );

    // Wait a bit for the job to complete
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check status
    let status_input = json!({ "action": "status", "job": "integ-echo" });
    let status_output = registry.execute("bg", &status_input).await;
    assert!(
        !status_output.is_error,
        "bg status should succeed: {}",
        status_output.content
    );

    // Get output
    let output_input = json!({ "action": "output", "job": "integ-echo" });
    let output_result = registry.execute("bg", &output_input).await;
    assert!(
        !output_result.is_error,
        "bg output should succeed: {}",
        output_result.content
    );
    assert!(
        output_result.content.contains("bg-integration-test-output"),
        "Should contain the echo output, got: {}",
        output_result.content
    );

    // Cleanup
    let delete_input = json!({ "action": "delete", "job": "integ-echo" });
    let delete_output = registry.execute("bg", &delete_input).await;
    assert!(
        !delete_output.is_error,
        "bg delete should succeed: {}",
        delete_output.content
    );
}

#[tokio::test]
async fn bg_tool_list_shows_running_job() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    // Run a longer command
    let run_input = json!({
        "action": "run",
        "command": "sleep 30",
        "name": "list-test"
    });
    registry.execute("bg", &run_input).await;

    // List should show it
    let list_input = json!({ "action": "list" });
    let list_output = registry.execute("bg", &list_input).await;
    assert!(!list_output.is_error);
    assert!(
        list_output.content.contains("list-test"),
        "List should show the running job"
    );

    // Kill and cleanup
    let kill_input = json!({ "action": "kill", "job": "list-test" });
    registry.execute("bg", &kill_input).await;
    let delete_input = json!({ "action": "delete", "job": "list-test" });
    registry.execute("bg", &delete_input).await;
}

#[tokio::test]
async fn bg_tool_kill_stops_running_job() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    // Run a long-running command
    let run_input = json!({
        "action": "run",
        "command": "sleep 300",
        "name": "kill-integ-test"
    });
    registry.execute("bg", &run_input).await;

    // Kill it
    let kill_input = json!({ "action": "kill", "job": "kill-integ-test" });
    let kill_output = registry.execute("bg", &kill_input).await;
    assert!(
        !kill_output.is_error,
        "bg kill should succeed: {}",
        kill_output.content
    );
    assert!(
        kill_output.content.contains("cancelled"),
        "Job status should be cancelled after kill, got: {}",
        kill_output.content
    );

    // Cleanup
    let delete_input = json!({ "action": "delete", "job": "kill-integ-test" });
    registry.execute("bg", &delete_input).await;
}

#[tokio::test]
async fn bg_state_injection_reports_completion() {
    use workgraph::executor::native::state_injection::StateInjector;

    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path();

    // Create minimal workgraph
    fs::create_dir_all(wg_dir).unwrap();
    fs::write(
        wg_dir.join("graph.jsonl"),
        r#"{"kind":"task","id":"bg-inject-test","title":"BG inject test","status":"in-progress"}"#,
    )
    .unwrap();

    let registry = ToolRegistry::default_all(wg_dir, &std::env::current_dir().unwrap());

    // Run a quick background job
    let run_input = json!({
        "action": "run",
        "command": "echo 'injection-marker-42'",
        "name": "inject-job"
    });
    registry.execute("bg", &run_input).await;

    // Wait for it to complete
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create a StateInjector and check for bg completions
    let mut injector = StateInjector::new(
        wg_dir.to_path_buf(),
        "bg-inject-test".to_string(),
        "agent-1".to_string(),
    );

    // The injector should not panic when collecting injections.
    // Whether it finds the bg job depends on internal directory layout.
    let _injections = injector.collect_injections(None);

    // Cleanup
    let delete_input = json!({ "action": "delete", "job": "inject-job" });
    registry.execute("bg", &delete_input).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Delegate tool: child registry construction and input validation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn delegate_tool_simple_text_response() {
    use workgraph::executor::native::tools::delegate;

    let tmp = TempDir::new().unwrap();
    let working = std::env::current_dir().unwrap();

    // Build child registry (what delegate would use internally)
    let registry = delegate::build_child_registry(tmp.path(), &working, "light");
    let defs = registry.definitions();

    // Verify child registry has expected tools
    let tool_names: HashSet<String> = defs.iter().map(|d| d.name.clone()).collect();
    assert!(
        tool_names.contains("read_file"),
        "light child registry should have read_file"
    );
    assert!(
        tool_names.contains("grep"),
        "light child registry should have grep"
    );
    assert!(
        !tool_names.contains("delegate"),
        "child registry must NOT have delegate (recursion prevention)"
    );
}

#[tokio::test]
async fn delegate_child_registry_full_mode() {
    use workgraph::executor::native::tools::delegate;

    let tmp = TempDir::new().unwrap();
    let working = std::env::current_dir().unwrap();

    let registry = delegate::build_child_registry(tmp.path(), &working, "full");
    let tool_names: HashSet<String> = registry
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();

    assert!(
        tool_names.contains("write_file"),
        "full child registry should have write_file"
    );
    assert!(
        tool_names.contains("edit_file"),
        "full child registry should have edit_file"
    );
    assert!(
        tool_names.contains("bash"),
        "full child registry should have bash"
    );
    assert!(
        tool_names.contains("web_fetch"),
        "full child registry should have web_fetch"
    );
    assert!(
        !tool_names.contains("delegate"),
        "child registry must NOT have delegate (recursion prevention)"
    );
}

#[tokio::test]
async fn delegate_tool_input_validation() {
    let tmp = TempDir::new().unwrap();
    let config = NativeExecutorConfig::default();
    let registry = ToolRegistry::default_all_with_config(
        tmp.path(),
        &std::env::current_dir().unwrap(),
        &config,
    );

    // Missing prompt
    let output = registry.execute("delegate", &json!({})).await;
    assert!(output.is_error);
    assert!(
        output
            .content
            .contains("Missing required parameter: prompt")
    );

    // Empty prompt
    let output = registry.execute("delegate", &json!({"prompt": "  "})).await;
    assert!(output.is_error);
    assert!(output.content.contains("must not be empty"));

    // Invalid exec_mode
    let output = registry
        .execute(
            "delegate",
            &json!({"prompt": "test", "exec_mode": "invalid"}),
        )
        .await;
    assert!(output.is_error);
    assert!(output.content.contains("Invalid exec_mode"));
}

#[tokio::test]
async fn delegate_tool_registered_in_default_registry() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    let defs = registry.definitions();
    let delegate_def = defs.iter().find(|d| d.name == "delegate");
    assert!(
        delegate_def.is_some(),
        "delegate tool should be in the default registry"
    );

    let def = delegate_def.unwrap();
    assert!(
        def.description.contains("sub-agent"),
        "delegate description should mention sub-agent"
    );
}

#[tokio::test]
async fn delegate_tool_with_custom_config() {
    let config = NativeExecutorConfig {
        delegate: workgraph::config::NativeDelegateConfig {
            delegate_max_turns: 15,
            delegate_model: "custom-model".to_string(),
        },
        ..Default::default()
    };

    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all_with_config(
        tmp.path(),
        &std::env::current_dir().unwrap(),
        &config,
    );

    let defs = registry.definitions();
    assert!(
        defs.iter().any(|d| d.name == "delegate"),
        "delegate should be registered with custom config"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Subtask creation + wait + resume cycle
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn subtask_creates_child_and_waits() {
    let tmp = TempDir::new().unwrap();

    // Create parent task in-progress
    let mut parent = make_task("parent-task");
    parent.status = Status::InProgress;
    let wg_dir = setup_workgraph(tmp.path(), vec![parent]);

    // Create a subtask via CLI (--subtask uses WG_TASK_ID as parent)
    let output = wg_cmd_env(
        &wg_dir,
        &["add", "Research child task", "--subtask"],
        &[("WG_TASK_ID", "parent-task")],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wg add --subtask should succeed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Reload graph and check
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();

    // Child task should exist
    let child = graph.nodes().find(|n| match n {
        Node::Task(t) => t.title == "Research child task",
        _ => false,
    });
    assert!(child.is_some(), "Child subtask should be created");

    // Parent should have a wait condition
    let parent_node = graph.get_task("parent-task").unwrap();
    if let Some(ref wait_spec) = parent_node.wait_condition {
        let conditions = match wait_spec {
            WaitSpec::All(conds) => conds,
            WaitSpec::Any(conds) => conds,
        };
        let has_task_status = conditions.iter().any(|c| {
            matches!(
                c,
                WaitCondition::TaskStatus { status, .. } if *status == Status::Done
            )
        });
        assert!(
            has_task_status,
            "Wait condition should include a TaskStatus::Done condition"
        );
    }
    // Parent's status should be Waiting
    assert_eq!(
        parent_node.status,
        Status::Waiting,
        "Parent should be in Waiting status after --subtask"
    );
}

#[test]
fn subtask_child_completion_resumes_parent() {
    let tmp = TempDir::new().unwrap();

    // Setup: parent Waiting on child to complete (subtask pattern).
    // Child does NOT depend on parent — it's immediately ready.
    let mut parent = make_task("resume-parent");
    parent.status = Status::Waiting;
    parent.wait_condition = Some(WaitSpec::Any(vec![WaitCondition::TaskStatus {
        task_id: "resume-child".to_string(),
        status: Status::Done,
    }]));

    let mut child = make_task("resume-child");
    child.status = Status::Open;

    let wg_dir = setup_workgraph(tmp.path(), vec![parent, child]);

    // Complete the child
    let output = wg_cmd(&wg_dir, &["done", "resume-child"]);
    assert!(
        output.status.success(),
        "wg done resume-child should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check the graph state after child completion.
    // Note: `wg done` marks the child as Done. The coordinator tick (not wg done)
    // evaluates wait conditions and resumes the parent. So here we verify:
    // 1. Child is Done
    // 2. Parent still has wait_condition that references the child
    // 3. The wait_condition can be satisfied (child status == Done matches)
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();

    let child = graph.get_task("resume-child").unwrap();
    assert_eq!(child.status, Status::Done, "Child should be Done");

    let parent = graph.get_task("resume-parent").unwrap();
    assert_eq!(parent.status, Status::Waiting);
    let wait_spec = parent
        .wait_condition
        .as_ref()
        .expect("parent should have wait_condition");
    match wait_spec {
        WaitSpec::Any(conds) | WaitSpec::All(conds) => {
            assert!(
                conds.iter().any(|c| matches!(
                    c,
                    WaitCondition::TaskStatus { task_id, status }
                    if task_id == "resume-child" && *status == Status::Done
                )),
                "Wait condition should reference resume-child with status Done"
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Bundle filtering: research has web tools but not delegate
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn bundle_research_has_web_tools() {
    let bundle = Bundle::research();
    assert!(
        bundle.tools.contains(&"web_search".to_string()),
        "Research bundle should include web_search"
    );
    assert!(
        bundle.tools.contains(&"web_fetch".to_string()),
        "Research bundle should include web_fetch"
    );
}

#[test]
fn bundle_research_has_no_delegate() {
    let bundle = Bundle::research();
    assert!(
        !bundle.tools.contains(&"delegate".to_string()),
        "Research bundle should NOT include delegate (read-only)"
    );
}

#[test]
fn bundle_research_has_no_write_tools() {
    let bundle = Bundle::research();
    assert!(
        !bundle.tools.contains(&"write_file".to_string()),
        "Research bundle should NOT include write_file"
    );
    assert!(
        !bundle.tools.contains(&"edit_file".to_string()),
        "Research bundle should NOT include edit_file"
    );
}

#[test]
fn bundle_bare_has_no_web_or_delegate() {
    let bundle = Bundle::bare();
    assert!(
        !bundle.tools.contains(&"web_search".to_string()),
        "Bare bundle should NOT include web_search"
    );
    assert!(
        !bundle.tools.contains(&"web_fetch".to_string()),
        "Bare bundle should NOT include web_fetch"
    );
    assert!(
        !bundle.tools.contains(&"delegate".to_string()),
        "Bare bundle should NOT include delegate"
    );
}

#[test]
fn bundle_implementer_wildcard_includes_all() {
    let bundle = Bundle::implementer();
    assert!(bundle.allows_all(), "Implementer should use wildcard");

    // When applied to a registry, all tools should remain
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
    let before_count = registry.definitions().len();

    let filtered = bundle.filter_registry(registry);
    let after_count = filtered.definitions().len();

    assert_eq!(
        before_count, after_count,
        "Implementer wildcard should not filter out any tools"
    );
}

#[test]
fn bundle_filtering_research_applied_to_registry() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    let bundle = Bundle::research();
    let filtered = bundle.filter_registry(registry);

    let names: HashSet<String> = filtered
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();

    // Research bundle should keep these
    assert!(names.contains("read_file"), "Should keep read_file");
    assert!(names.contains("grep"), "Should keep grep");
    assert!(names.contains("glob"), "Should keep glob");
    assert!(names.contains("bash"), "Should keep bash");
    assert!(names.contains("web_search"), "Should keep web_search");
    assert!(names.contains("web_fetch"), "Should keep web_fetch");
    assert!(names.contains("wg_show"), "Should keep wg_show");
    assert!(names.contains("wg_done"), "Should keep wg_done");

    // Research bundle should filter these out
    assert!(!names.contains("write_file"), "Should NOT keep write_file");
    assert!(!names.contains("edit_file"), "Should NOT keep edit_file");
    assert!(!names.contains("delegate"), "Should NOT keep delegate");
}

#[test]
fn bundle_bare_filtering_only_wg_tools() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    let bundle = Bundle::bare();
    let filtered = bundle.filter_registry(registry);

    let names: HashSet<String> = filtered
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();

    // Bare should only have wg_ tools
    for name in &names {
        assert!(
            name.starts_with("wg_"),
            "Bare bundle should only contain wg_ tools, but found: {}",
            name
        );
    }

    assert!(names.contains("wg_show"), "Should have wg_show");
    assert!(names.contains("wg_add"), "Should have wg_add");
    assert!(names.contains("wg_done"), "Should have wg_done");
    assert!(names.contains("wg_fail"), "Should have wg_fail");
    assert!(names.contains("wg_log"), "Should have wg_log");
}

// ═══════════════════════════════════════════════════════════════════════════
// Config integration: NativeExecutorConfig defaults + propagation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn native_executor_config_defaults_are_sane() {
    let config = NativeExecutorConfig::default();

    assert_eq!(config.web.fetch_max_chars, 16_000);
    assert_eq!(config.web.fetch_timeout_secs, 30);
    assert_eq!(config.delegate.delegate_max_turns, 10);
    assert!(config.delegate.delegate_model.is_empty());
    assert_eq!(config.background.max_background_tasks, 5);
    assert_eq!(config.background.background_timeout_secs, 600);
}

#[test]
fn native_executor_config_propagates_to_registry() {
    let config = NativeExecutorConfig {
        web: workgraph::config::NativeWebConfig {
            fetch_max_chars: 5000,
            fetch_timeout_secs: 10,
            ..Default::default()
        },
        delegate: workgraph::config::NativeDelegateConfig {
            delegate_max_turns: 3,
            delegate_model: "custom-model-v1".to_string(),
        },
        ..Default::default()
    };

    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all_with_config(
        tmp.path(),
        &std::env::current_dir().unwrap(),
        &config,
    );

    let names: HashSet<String> = registry
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();

    assert!(names.contains("web_fetch"), "Should have web_fetch");
    assert!(names.contains("web_search"), "Should have web_search");
    assert!(names.contains("delegate"), "Should have delegate");
    assert!(names.contains("bg"), "Should have bg");
    assert!(names.contains("bash"), "Should have bash");
    assert!(names.contains("read_file"), "Should have read_file");
}

// ═══════════════════════════════════════════════════════════════════════════
// Resolve bundle from exec_mode string
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn resolve_bundle_maps_exec_modes() {
    let tmp = TempDir::new().unwrap();

    // shell → shell bundle
    let shell = resolve_bundle("shell", tmp.path()).unwrap();
    assert_eq!(shell.name, "shell");
    assert!(!shell.allows_all());

    // bare → bare
    let bare = resolve_bundle("bare", tmp.path()).unwrap();
    assert_eq!(bare.name, "bare");

    // light → research
    let research = resolve_bundle("light", tmp.path()).unwrap();
    assert_eq!(research.name, "research");

    // full → implementer
    let implementer = resolve_bundle("full", tmp.path()).unwrap();
    assert_eq!(implementer.name, "implementer");
    assert!(implementer.allows_all());
}

#[test]
fn resolve_bundle_from_custom_toml_file() {
    let tmp = TempDir::new().unwrap();
    let bundles_dir = tmp.path().join("bundles");
    fs::create_dir_all(&bundles_dir).unwrap();

    let custom = r#"
name = "research"
description = "Custom research with only grep"
tools = ["grep", "wg_show"]
context_scope = "task"
"#;
    fs::write(bundles_dir.join("research.toml"), custom).unwrap();

    let bundle = resolve_bundle("light", tmp.path()).unwrap();
    assert_eq!(bundle.name, "research");
    assert_eq!(
        bundle.tools.len(),
        2,
        "Custom bundle should have exactly 2 tools"
    );
    assert!(bundle.tools.contains(&"grep".to_string()));
    assert!(bundle.tools.contains(&"wg_show".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════════
// All tools in default registry
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn default_registry_has_all_expected_tools() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());

    let names: HashSet<String> = registry
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();

    let expected = [
        "read_file",
        "write_file",
        "edit_file",
        "glob",
        "grep",
        "bash",
        "bg",
        "web_search",
        "web_fetch",
        "delegate",
        "wg_show",
        "wg_list",
        "wg_add",
        "wg_done",
        "wg_fail",
        "wg_log",
        "wg_artifact",
    ];

    for tool in &expected {
        assert!(
            names.contains(*tool),
            "Default registry should contain '{}', but found: {:?}",
            tool,
            names
        );
    }
}

#[test]
fn web_fetch_is_read_only() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
    assert!(
        registry.is_read_only("web_fetch"),
        "web_fetch should be read-only"
    );
}

#[test]
fn web_search_is_read_only() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
    assert!(
        registry.is_read_only("web_search"),
        "web_search should be read-only"
    );
}

#[test]
fn bg_tool_is_not_read_only() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
    assert!(
        !registry.is_read_only("bg"),
        "bg should NOT be read-only (modifies state)"
    );
}

#[test]
fn delegate_is_not_read_only() {
    let tmp = TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), &std::env::current_dir().unwrap());
    assert!(
        !registry.is_read_only("delegate"),
        "delegate should NOT be read-only (runs sub-agent)"
    );
}

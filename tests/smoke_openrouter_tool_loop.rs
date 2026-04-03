//! Smoke tests for native executor multi-turn tool-use loop via OpenRouter.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn wg_binary() -> PathBuf {
    let mut p = std::env::current_exe().expect("could not get current exe");
    p.pop();
    if p.ends_with("deps") { p.pop(); }
    p.push("wg");
    assert!(p.exists(), "wg not found at {:?}", p);
    p
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    let fake_home = wg_dir.parent().unwrap_or(wg_dir).join("fakehome");
    let _ = fs::create_dir_all(&fake_home);
    let mut c = Command::new(wg_binary());
    c.arg("--dir").arg(wg_dir).args(args).env("HOME", &fake_home);
    for (k, v) in std::env::vars() {
        if k.ends_with("API_KEY") || k == "WG_LLM_PROVIDER" {
            c.env(&k, v);
        }
    }
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .output().unwrap_or_else(|_| panic!("wg {:?} failed", args))
}

fn wg_ok(wg_dir: &Path, args: &[&str]) {
    let o = wg_cmd(wg_dir, args);
    let s = String::from_utf8_lossy(&o.stdout);
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "wg {:?} failed\nstdout:{}\nstderr:{}", args, s, e);
}

fn run_native_exec(wg_dir: &Path, prompt: &str, task_id: &str, model: &str, api_key: &str) -> std::process::Output {
    let fake_home = wg_dir.parent().unwrap().join("fakehome");
    let _ = fs::create_dir_all(&fake_home);
    let pf = wg_dir.parent().unwrap().join("prompt.txt");
    fs::write(&pf, prompt).unwrap();
    let mut c = Command::new(wg_binary());
    c.arg("--dir").arg(wg_dir)
     .args(["native-exec", "--prompt-file", &pf.to_string_lossy(), "--exec-mode", "full",
            "--task-id", task_id, "--model", model, "--provider", "openai",
            "--endpoint-url", "https://openrouter.ai/api/v1"])
     .env("HOME", &fake_home).env("OPENROUTER_API_KEY", api_key)
     .env("WG_LLM_PROVIDER", "openai")
     .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c.output().unwrap()
}

#[test]
#[ignore = "requires OPENROUTER_API_KEY"]
fn smoke_native_tool_loop_openrouter() {
    let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    wg_ok(&wg_dir, &["agency", "init"]);
    wg_ok(&wg_dir, &["endpoint", "add", "test-or", "--provider", "openrouter",
                      "--url", "https://openrouter.ai/api/v1", "--key-env", "OPENROUTER_API_KEY"]);
    wg_ok(&wg_dir, &["endpoint", "set-default", "test-or"]);
    wg_ok(&wg_dir, &["add", "Tool test", "--id", "tool-loop-test", "--context-scope", "task"]);

    let prompt = r#"Create /tmp/smoke_test_input.txt with "hello smoke test", read it back, run cat via bash, then wg_done with task_id 'tool-loop-test'."#;
    let out = run_native_exec(&wg_dir, prompt, "tool-loop-test", "minimax/minimax-m2.7", &api_key);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Verify correct model was used
    assert!(stderr.contains("minimax-m2.7") || stderr.contains("OpenRouter"),
        "Should use minimax via OpenRouter. stderr: {}", stderr);

    // Parse NDJSON output
    let lines: Vec<&str> = stdout.lines().collect();
    let tool_call_count = lines.iter().filter(|l| l.contains("tool_call")).count();
    let turn_count = lines.iter().filter(|l| l.contains("turn")).count();

    assert!(tool_call_count >= 2, "Should have >= 2 tool calls, got {}", tool_call_count);
    assert!(turn_count >= 2, "Should have >= 2 turns, got {}", turn_count);

    // Check journal entries
    let agents_dir = wg_dir.join("agents");
    if let Ok(entries) = fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let journal_path = path.join("conversation.jsonl");
                if journal_path.exists() {
                    let content = fs::read_to_string(&journal_path).unwrap();
                    let entries: Vec<serde_json::Value> = content
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .collect();
                    let tool_execs = entries.iter()
                        .filter(|e| e.get("entry_type").and_then(|v| v.as_str()) == Some("tool_execution"))
                        .count();
                    eprintln!("[smoke] Journal: {} entries, {} tool_execution entries",
                             entries.len(), tool_execs);
                }
            }
        }
    }

    eprintln!("[smoke] PASS: {} turns, {} tool_calls", turn_count, tool_call_count);
}

#[test]
#[ignore = "requires OPENROUTER_API_KEY"]
fn smoke_native_tool_loop_terminates_reasonably() {
    let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);

    let prompt = r#"Run `ls /tmp` via bash tool, then wg_done with task_id 'termination-test'."#;
    let start = std::time::Instant::now();
    let _out = run_native_exec(&wg_dir, prompt, "termination-test", "minimax/minimax-m2.7", &api_key);
    let elapsed = start.elapsed().as_secs();

    assert!(elapsed < 180, "Agent took {}s — too long for simple task", elapsed);
    eprintln!("[smoke] Termination test passed in {}s", elapsed);
}

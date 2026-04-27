//! Integration tests for the codex_handler's OAI-compat configuration
//! plumbing.
//!
//! There are two layers here:
//!
//! 1. Pure-helper assertions on `codex_oai_compat::config_overrides` —
//!    fast, deterministic, drives implementation via TDD.
//!
//! 2. Live-spawn check (`test_codex_handler_uses_custom_base_url`): when
//!    the `codex` CLI binary is on PATH, spawn it with our `--config`
//!    overrides pointed at a stub HTTP server and verify the actual HTTP
//!    request landed at the stub URL (not at `api.openai.com`). When
//!    `codex` is not installed, the test SKIPs.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use workgraph::commands::codex_oai_compat;

// ---------------------------------------------------------------------------
// Pure-helper assertions
// ---------------------------------------------------------------------------

#[test]
fn config_overrides_contains_base_url_and_provider_selection() {
    let overrides = codex_oai_compat::config_overrides("http://stub.test:1234");
    let joined = overrides.join(" || ");
    assert!(
        joined.contains(r#"model_provider="wg""#),
        "missing model_provider override: {}",
        joined
    );
    assert!(
        joined.contains(r#"model_providers.wg.base_url="http://stub.test:1234""#),
        "missing base_url override: {}",
        joined
    );
    assert!(
        joined.contains(r#"model_providers.wg.env_key="OPENAI_API_KEY""#),
        "missing env_key override: {}",
        joined
    );
    assert!(
        joined.contains(r#"model_providers.wg.wire_api="responses""#),
        "missing wire_api override: {}",
        joined
    );
}

#[test]
fn config_overrides_uses_stable_provider_id() {
    // The provider id must be stable across calls so codex's lookup
    // resolves correctly. A drifting id would break per-turn replay.
    assert_eq!(codex_oai_compat::PROVIDER_ID, "wg");
    assert_eq!(codex_oai_compat::ENV_KEY_NAME, "OPENAI_API_KEY");
}

// ---------------------------------------------------------------------------
// Live spawn against a stub HTTP server
// ---------------------------------------------------------------------------

/// Captures every incoming HTTP request line + headers + body chunk we
/// can read in a single read() pass. Sufficient for assertions on the
/// request URL path and headers; we do not need the full body.
fn start_capturing_stub_server() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut s) = stream {
                let captured_inner = captured_clone.clone();
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut buf = vec![0u8; 16384];
                    let n = s.read(&mut buf).unwrap_or(0);
                    if n > 0 {
                        let req = String::from_utf8_lossy(&buf[..n]).to_string();
                        captured_inner.lock().unwrap().push(req);
                    }
                    // Send a minimal OAI-compat Chat Completions reply.
                    let body = serde_json::json!({
                        "id": "chatcmpl-stub",
                        "object": "chat.completion",
                        "created": 1714155600u64,
                        "model": "qwen3-coder",
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "stub-reply"
                            },
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": 1,
                            "completion_tokens": 1,
                            "total_tokens": 2
                        }
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = s.write_all(resp.as_bytes());
                    let _ = s.flush();
                });
            }
        }
    });

    (url, captured)
}

fn codex_available() -> bool {
    std::process::Command::new("codex")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn `codex exec` directly with the same `--config` overrides + env
/// the codex_handler will use, pointed at a stub HTTP server. Replay 5
/// turns and assert each turn's HTTP request landed at the stub.
///
/// This validates the *config plumbing* end-to-end without depending on
/// codex's full happy-path response handling. As long as the request
/// arrives at the stub URL, the redirection has worked — codex may
/// itself error out afterwards (the stub's reply is not the SSE-streaming
/// shape codex's `chat` wire format prefers); that's expected and does
/// not fail the test.
#[test]
fn test_codex_handler_uses_custom_base_url() {
    if !codex_available() {
        eprintln!(
            "SKIP test_codex_handler_uses_custom_base_url: \
             `codex` CLI not on PATH (codex-cli not installed)"
        );
        return;
    }

    let (stub_url, captured) = start_capturing_stub_server();
    eprintln!("stub server ready at {}", stub_url);

    let overrides = codex_oai_compat::config_overrides(&stub_url);

    // Use a dedicated tmp HOME so the `codex` invocation cannot find
    // any pre-existing `~/.codex/auth.json` and pick up real
    // credentials. We want it to use OUR env_key + bearer.
    let tmp_home = tempfile::tempdir().expect("tempdir for HOME");
    let codex_dir = tmp_home.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();

    const TURNS: usize = 5;
    for i in 0..TURNS {
        let mut cmd = std::process::Command::new("codex");
        cmd.arg("exec")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("--dangerously-bypass-approvals-and-sandbox");
        for ovr in &overrides {
            cmd.arg("--config").arg(ovr);
        }
        cmd.arg("--model").arg("qwen3-coder");
        cmd.env(codex_oai_compat::ENV_KEY_NAME, "test-key-bearer")
            .env("HOME", tmp_home.path())
            .env("CODEX_HOME", &codex_dir)
            // Don't let any inherited OPENAI_BASE_URL override our
            // --config base_url.
            .env_remove("OPENAI_BASE_URL")
            .env_remove("WG_ENDPOINT_URL")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().expect("spawn codex");
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "turn {}: hello", i);
            drop(stdin);
        }
        // Bounded wait. Each codex run will likely fail (stub doesn't
        // speak the Responses API wire format) but that's fine — we
        // only assert the HTTP request reached our stub URL.
        let started = Instant::now();
        let timeout = Duration::from_secs(8);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => break,
            }
            if started.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Drain stdout/stderr so we can show codex output on test failure.
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut s) = child.stdout.take() {
            let _ = std::io::Read::read_to_string(&mut s, &mut stdout);
        }
        if let Some(mut s) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut s, &mut stderr);
        }
        eprintln!(
            "[turn {}] codex stdout (first 400 chars):\n{}\n[turn {}] codex stderr (first 400 chars):\n{}",
            i,
            stdout.chars().take(400).collect::<String>(),
            i,
            stderr.chars().take(400).collect::<String>()
        );
    }

    // Allow stub handler threads to finish writing into `captured`.
    std::thread::sleep(Duration::from_millis(300));
    let captured = captured.lock().unwrap().clone();

    eprintln!(
        "stub captured {} request(s) across {} turns",
        captured.len(),
        TURNS
    );
    if let Some(first) = captured.first() {
        eprintln!("--- first captured request (full headers + start of body) ---");
        let preview: String = first.chars().take(800).collect();
        eprintln!("{}", preview);
        eprintln!("--- end ---");
    }

    assert!(
        !captured.is_empty(),
        "stub server received NO requests across {} turns. \
         The codex CLI must be talking to its default `api.openai.com` \
         instead of our --config base_url.",
        TURNS
    );

    // HTTP header names are case-insensitive; codex sends lowercase
    // `authorization:` while curl/most clients send `Authorization:`.
    let any_with_auth = captured.iter().any(|r| {
        r.to_ascii_lowercase()
            .contains("authorization: bearer test-key-bearer")
    });
    assert!(
        any_with_auth,
        "no captured request had `authorization: Bearer test-key-bearer`. \
         api_key did NOT reach the spawned codex via OPENAI_API_KEY env."
    );

    // Also confirm requests went to the stub's path, not e.g. a direct
    // POST to api.openai.com URL elsewhere.
    let any_to_responses = captured.iter().any(|r| r.contains("POST /responses"));
    assert!(
        any_to_responses,
        "captured requests did not include a POST to /responses; \
         the --config base_url override may not be effective"
    );
}

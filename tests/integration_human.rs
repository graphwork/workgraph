//! Smoke tests for the human notification and input subsystems.
//!
//! Covers:
//! 1. NotificationChannel trait routing — events dispatch to the correct channel
//! 2. Webhook notification fires — real HTTP to a mock TCP server
//! 3. `wg ask` question/answer flow — cross-executor human input requests

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use workgraph::notify::dispatch::{dispatch_event, TaskEvent, TaskEventKind};
use workgraph::notify::webhook::{WebhookChannel, WebhookConfig};
use workgraph::notify::{
    Action, ActionStyle, EventType, MessageId, NotificationChannel, NotificationRouter,
    RichMessage, RoutingRule,
};
use workgraph::questions::{
    self, QuestionStatus,
};

// ---------------------------------------------------------------------------
// Mock channel
// ---------------------------------------------------------------------------

struct MockChannel {
    name: String,
    fail: bool,
}

#[async_trait::async_trait]
impl NotificationChannel for MockChannel {
    fn channel_type(&self) -> &str {
        &self.name
    }

    async fn send_text(&self, _target: &str, message: &str) -> anyhow::Result<MessageId> {
        if self.fail {
            anyhow::bail!("mock failure");
        }
        Ok(MessageId(format!("{}:{}", self.name, message)))
    }

    async fn send_rich(&self, _target: &str, message: &RichMessage) -> anyhow::Result<MessageId> {
        if self.fail {
            anyhow::bail!("mock failure");
        }
        Ok(MessageId(format!("{}:{}", self.name, message.plain_text)))
    }

    async fn send_with_actions(
        &self,
        _target: &str,
        message: &str,
        _actions: &[Action],
    ) -> anyhow::Result<MessageId> {
        if self.fail {
            anyhow::bail!("mock failure");
        }
        Ok(MessageId(format!("{}:action:{}", self.name, message)))
    }

    fn supports_receive(&self) -> bool {
        false
    }

    async fn listen(
        &self,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<workgraph::notify::IncomingMessage>> {
        anyhow::bail!("not supported")
    }
}

fn mock(name: &str, fail: bool) -> Box<dyn NotificationChannel> {
    Box::new(MockChannel {
        name: name.to_string(),
        fail,
    })
}

/// Spawn a mock HTTP server that returns 200 OK and captures the request body.
async fn mock_http_ok(
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Receiver<Vec<u8>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();

    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 16384];
        let mut total = 0;
        loop {
            let n = stream.read(&mut buf[total..]).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
            if total >= 4 && buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        let header_str = String::from_utf8_lossy(&buf[..total]).to_string();
        let header_end = buf[..total]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(total);

        let body = if let Some(cl) = header_str
            .lines()
            .find(|l| l.to_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            let body_so_far = total - header_end;
            if body_so_far < cl {
                let remaining = cl - body_so_far;
                let mut rest = vec![0u8; remaining];
                let _ = stream.read_exact(&mut rest).await;
                let mut body = buf[header_end..total].to_vec();
                body.extend_from_slice(&rest);
                body
            } else {
                buf[header_end..header_end + cl].to_vec()
            }
        } else {
            buf[header_end..total].to_vec()
        };

        let _ = tx.send(body);

        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
    });

    (addr, rx, handle)
}

// ===========================================================================
// 1. NotificationChannel trait routes events correctly
// ===========================================================================

#[tokio::test]
async fn notification_routes_task_failed_to_telegram() {
    let router = NotificationRouter::new(
        vec![mock("telegram", false), mock("webhook", false)],
        vec![RoutingRule {
            event_type: EventType::TaskFailed,
            channels: vec!["telegram".into()],
            escalation_timeout: None,
        }],
        vec!["webhook".into()],
    );

    let (ch, _) = router
        .send(EventType::TaskFailed, "u", "build broke")
        .await
        .unwrap();
    assert_eq!(ch, "telegram");

    // Non-failed events fall to default webhook
    let (ch, _) = router
        .send(EventType::TaskReady, "u", "ready")
        .await
        .unwrap();
    assert_eq!(ch, "webhook");
}

#[tokio::test]
async fn notification_routes_question_event() {
    let router = NotificationRouter::new(
        vec![mock("telegram", false), mock("webhook", false)],
        vec![RoutingRule {
            event_type: EventType::Question,
            channels: vec!["telegram".into()],
            escalation_timeout: Some(Duration::from_secs(600)),
        }],
        vec!["webhook".into()],
    );

    let (ch, _) = router
        .send(EventType::Question, "u", "Need input on deploy target")
        .await
        .unwrap();
    assert_eq!(ch, "telegram");

    assert_eq!(
        router.escalation_timeout(EventType::Question),
        Some(Duration::from_secs(600))
    );
}

#[tokio::test]
async fn notification_fallthrough_on_channel_failure() {
    let router = NotificationRouter::new(
        vec![
            mock("telegram", true),  // fails
            mock("sms", true),       // fails
            mock("webhook", false),  // succeeds
        ],
        vec![RoutingRule {
            event_type: EventType::Urgent,
            channels: vec!["telegram".into(), "sms".into(), "webhook".into()],
            escalation_timeout: None,
        }],
        vec![],
    );

    let (ch, _) = router
        .send(EventType::Urgent, "u", "server down")
        .await
        .unwrap();
    assert_eq!(ch, "webhook");
}

#[tokio::test]
async fn dispatch_routes_failed_task_event_to_correct_channel() {
    let router = NotificationRouter::new(
        vec![mock("telegram", false), mock("webhook", false)],
        vec![RoutingRule {
            event_type: EventType::TaskFailed,
            channels: vec!["telegram".into()],
            escalation_timeout: None,
        }],
        vec!["webhook".into()],
    );

    let event = TaskEvent {
        task_id: "build-fe".into(),
        title: "Build Frontend".into(),
        kind: TaskEventKind::Failed,
        detail: Some("exit code 1".into()),
    };

    let result = dispatch_event(&router, "user1", &event).await.unwrap();
    let (ch, mid) = result.unwrap();
    assert_eq!(ch, "telegram");
    assert!(mid.0.contains("build-fe"));
}

#[tokio::test]
async fn dispatch_question_event_routes_correctly() {
    let router = NotificationRouter::new(
        vec![mock("telegram", false), mock("webhook", false)],
        vec![RoutingRule {
            event_type: EventType::Question,
            channels: vec!["telegram".into()],
            escalation_timeout: None,
        }],
        vec!["webhook".into()],
    );

    let event = TaskEvent {
        task_id: "deploy-prod".into(),
        title: "Deploy Production".into(),
        kind: TaskEventKind::Question,
        detail: Some("Which region?".into()),
    };

    let result = dispatch_event(&router, "user1", &event).await.unwrap();
    let (ch, mid) = result.unwrap();
    assert_eq!(ch, "telegram");
    assert!(mid.0.contains("deploy-prod"));
}

// ===========================================================================
// 2. Webhook notification fires
// ===========================================================================

#[tokio::test]
async fn webhook_fires_on_task_completion() {
    let (addr, body_rx, server) = mock_http_ok().await;

    let ch = WebhookChannel::new(WebhookConfig {
        url: format!("http://{addr}/webhook"),
        secret: Some("test-secret".into()),
        events: vec![],
        event_urls: Default::default(),
        max_retries: 0,
        initial_backoff_ms: 10,
    });

    let mid = ch
        .send_text("my-task:task_ready", "Task completed successfully")
        .await
        .unwrap();
    assert!(mid.0.starts_with("webhook:"));

    let body_bytes = body_rx.await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(payload["task_id"], "my-task");
    assert_eq!(payload["event_type"], "task_ready");
    assert_eq!(payload["title"], "Task completed successfully");
    assert!(payload["timestamp"].as_str().is_some());

    server.abort();
}

#[tokio::test]
async fn webhook_fires_with_hmac_signature() {
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new((String::new(), Vec::<u8>::new())));
    let captured_clone = captured.clone();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 16384];
        let mut total = 0;
        loop {
            let n = stream.read(&mut buf[total..]).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
            if total >= 4 && buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        let header_str = String::from_utf8_lossy(&buf[..total]).to_string();
        let header_end = buf[..total]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(total);

        let body = if let Some(cl) = header_str
            .lines()
            .find(|l| l.to_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            let body_so_far = total - header_end;
            if body_so_far < cl {
                let remaining = cl - body_so_far;
                let mut rest = vec![0u8; remaining];
                let _ = stream.read_exact(&mut rest).await;
                let mut b = buf[header_end..total].to_vec();
                b.extend_from_slice(&rest);
                b
            } else {
                buf[header_end..header_end + cl].to_vec()
            }
        } else {
            buf[header_end..total].to_vec()
        };

        *captured_clone.lock().await = (header_str, body);

        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
    });

    let secret = "hmac-test-key";
    let ch = WebhookChannel::new(WebhookConfig {
        url: format!("http://{addr}/hook"),
        secret: Some(secret.into()),
        events: vec![],
        event_urls: Default::default(),
        max_retries: 0,
        initial_backoff_ms: 10,
    });

    ch.send_text("task-42:task_failed", "Build failed")
        .await
        .unwrap();

    let (headers, body) = &*captured.lock().await;

    // Verify HMAC signature header present
    let headers_lower = headers.to_lowercase();
    assert!(
        headers_lower.contains("x-webhook-signature: sha256="),
        "Missing HMAC signature header"
    );

    // Verify the signature is correct
    let expected_sig = WebhookChannel::compute_signature(secret, body);
    assert!(
        headers.contains(&expected_sig),
        "HMAC signature mismatch"
    );

    server.abort();
}

#[tokio::test]
async fn webhook_filters_disallowed_events() {
    let ch = WebhookChannel::new(WebhookConfig {
        url: "http://127.0.0.1:1/should-not-be-called".into(),
        secret: None,
        events: vec!["task_failed".into()],
        event_urls: Default::default(),
        max_retries: 0,
        initial_backoff_ms: 10,
    });

    // task_ready is not in the allowed list → should be filtered
    let mid = ch
        .send_text("my-task:task_ready", "Ready")
        .await
        .unwrap();
    assert!(mid.0.starts_with("filtered:"));
}

#[tokio::test]
async fn webhook_uses_per_event_url_override() {
    let (addr, body_rx, server) = mock_http_ok().await;

    let mut event_urls = std::collections::HashMap::new();
    event_urls.insert("task_failed".into(), format!("http://{addr}/fail-hook"));

    let ch = WebhookChannel::new(WebhookConfig {
        url: "http://192.0.2.1:1/should-not-be-called".into(),
        secret: None,
        events: vec![],
        event_urls,
        max_retries: 0,
        initial_backoff_ms: 10,
    });

    let mid = ch
        .send_text("task-x:task_failed", "Build failed")
        .await
        .unwrap();
    assert!(mid.0.starts_with("webhook:"));

    let body_bytes = body_rx.await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(payload["task_id"], "task-x");
    assert_eq!(payload["event_type"], "task_failed");

    server.abort();
}

#[tokio::test]
async fn webhook_payload_includes_actions() {
    let (addr, body_rx, server) = mock_http_ok().await;

    let ch = WebhookChannel::new(WebhookConfig {
        url: format!("http://{addr}/hook"),
        secret: None,
        events: vec![],
        event_urls: Default::default(),
        max_retries: 0,
        initial_backoff_ms: 10,
    });

    let actions = vec![
        Action {
            id: "approve".into(),
            label: "Approve".into(),
            style: ActionStyle::Primary,
        },
        Action {
            id: "reject".into(),
            label: "Reject".into(),
            style: ActionStyle::Danger,
        },
    ];

    ch.send_with_actions("task-deploy:approval", "Deploy to prod?", &actions)
        .await
        .unwrap();

    let body_bytes = body_rx.await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let acts = payload["actions"].as_array().unwrap();
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0]["id"], "approve");
    assert_eq!(acts[0]["style"], "primary");
    assert_eq!(acts[1]["id"], "reject");
    assert_eq!(acts[1]["style"], "danger");

    server.abort();
}

// ===========================================================================
// 3. wg ask — cross-executor human input requests
// ===========================================================================

fn setup_questions_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();
    (tmp, wg_dir)
}

#[test]
fn ask_creates_pending_question() {
    let (_tmp, wg_dir) = setup_questions_dir();
    let q = questions::ask_question(&wg_dir, "task-1", "Which region?", &["us-east".into(), "eu-west".into()], Some("agent-42")).unwrap();

    assert!(q.id.starts_with("q-"));
    assert_eq!(q.task_id, "task-1");
    assert_eq!(q.question, "Which region?");
    assert_eq!(q.options, vec!["us-east", "eu-west"]);
    assert_eq!(q.status, QuestionStatus::Pending);
    assert!(q.answer.is_none());
    assert_eq!(q.agent_id.as_deref(), Some("agent-42"));
}

#[test]
fn answer_resolves_pending_question() {
    let (_tmp, wg_dir) = setup_questions_dir();
    questions::ask_question(&wg_dir, "task-1", "Deploy where?", &[], None).unwrap();

    let answered = questions::answer_question(&wg_dir, "task-1", "us-east-1", Some("human-user")).unwrap();
    assert_eq!(answered.status, QuestionStatus::Answered);
    assert_eq!(answered.answer.as_deref(), Some("us-east-1"));
    assert_eq!(answered.answered_by.as_deref(), Some("human-user"));
    assert!(answered.answered_at.is_some());
}

#[test]
fn answer_targets_most_recent_pending() {
    let (_tmp, wg_dir) = setup_questions_dir();
    let _q1 = questions::ask_question(&wg_dir, "task-1", "First?", &[], None).unwrap();
    let q2 = questions::ask_question(&wg_dir, "task-1", "Second?", &[], None).unwrap();

    let answered = questions::answer_question(&wg_dir, "task-1", "yes", None).unwrap();
    assert_eq!(answered.id, q2.id);
}

#[test]
fn double_answer_fails() {
    let (_tmp, wg_dir) = setup_questions_dir();
    let q = questions::ask_question(&wg_dir, "task-1", "Q?", &[], None).unwrap();
    questions::answer_question_by_id(&wg_dir, "task-1", &q.id, "a1", None).unwrap();

    let err = questions::answer_question_by_id(&wg_dir, "task-1", &q.id, "a2", None);
    assert!(err.is_err());
}

#[test]
fn list_pending_across_tasks() {
    let (_tmp, wg_dir) = setup_questions_dir();
    questions::ask_question(&wg_dir, "task-a", "Q1?", &[], None).unwrap();
    questions::ask_question(&wg_dir, "task-b", "Q2?", &[], None).unwrap();
    questions::ask_question(&wg_dir, "task-c", "Q3?", &[], None).unwrap();

    // Answer one
    questions::answer_question(&wg_dir, "task-b", "done", None).unwrap();

    let pending = questions::list_all_pending(&wg_dir).unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|q| q.status == QuestionStatus::Pending));
}

#[test]
fn check_answer_finds_question_by_id() {
    let (_tmp, wg_dir) = setup_questions_dir();
    let q = questions::ask_question(&wg_dir, "task-1", "Q?", &[], None).unwrap();

    let found = questions::check_answer(&wg_dir, &q.id).unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().status, QuestionStatus::Pending);

    // Answer it and check again
    questions::answer_question_by_id(&wg_dir, "task-1", &q.id, "yes", None).unwrap();
    let found = questions::check_answer(&wg_dir, &q.id).unwrap();
    assert_eq!(found.unwrap().status, QuestionStatus::Answered);
}

#[test]
fn get_latest_answer_returns_most_recent() {
    let (_tmp, wg_dir) = setup_questions_dir();
    let q1 = questions::ask_question(&wg_dir, "task-1", "Q1?", &[], None).unwrap();
    let q2 = questions::ask_question(&wg_dir, "task-1", "Q2?", &[], None).unwrap();

    questions::answer_question_by_id(&wg_dir, "task-1", &q1.id, "a1", None).unwrap();
    questions::answer_question_by_id(&wg_dir, "task-1", &q2.id, "a2", None).unwrap();

    let latest = questions::get_latest_answer(&wg_dir, "task-1").unwrap().unwrap();
    assert_eq!(latest.answer.as_deref(), Some("a2"));
}

#[test]
fn no_pending_questions_returns_error() {
    let (_tmp, wg_dir) = setup_questions_dir();
    let err = questions::answer_question(&wg_dir, "task-1", "answer", None);
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("No pending questions"));
}

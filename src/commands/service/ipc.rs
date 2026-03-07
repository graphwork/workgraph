//! IPC protocol: message types and request handlers for the service daemon.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use workgraph::config::Config;
use workgraph::graph::{Node, Status, Task};
use workgraph::parser::load_graph;
use workgraph::service::registry::AgentRegistry;

use super::{CoordinatorState, DaemonConfig, DaemonLogger, ServiceState};
use crate::commands::graph_path;

/// IPC Request types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Spawn a new agent for a task
    Spawn {
        task_id: String,
        executor: String,
        #[serde(default)]
        timeout: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },
    /// List all agents
    Agents,
    /// Kill an agent
    Kill {
        agent_id: String,
        #[serde(default)]
        force: bool,
    },
    /// Record heartbeat for an agent
    Heartbeat { agent_id: String },
    /// Get service status
    Status,
    /// Shutdown the service
    Shutdown {
        #[serde(default)]
        force: bool,
        /// Whether to also kill running agents (default: false, agents continue independently)
        #[serde(default)]
        kill_agents: bool,
        /// Agent ID that triggered the shutdown (from WG_AGENT_ID env var)
        #[serde(default)]
        triggered_by_agent: Option<String>,
        /// Task ID that triggered the shutdown (from WG_TASK_ID env var)
        #[serde(default)]
        triggered_by_task: Option<String>,
    },
    /// Notify that the graph has changed; triggers an immediate coordinator tick
    GraphChanged,
    /// Pause the coordinator (no new agent spawns, running agents unaffected)
    Pause,
    /// Resume the coordinator (triggers immediate tick)
    Resume,
    /// Reconfigure the coordinator at runtime.
    /// If all fields are None, re-read config.toml from disk.
    Reconfigure {
        #[serde(default)]
        max_agents: Option<usize>,
        #[serde(default)]
        executor: Option<String>,
        #[serde(default)]
        poll_interval: Option<u64>,
        #[serde(default)]
        model: Option<String>,
    },
    /// Create a task in this workgraph (cross-repo dispatch)
    AddTask {
        title: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        after: Vec<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        skills: Vec<String>,
        #[serde(default)]
        deliverables: Vec<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        verify: Option<String>,
        /// Who requested this (for provenance)
        #[serde(default)]
        origin: Option<String>,
    },
    /// Query a task's status (cross-repo query)
    QueryTask { task_id: String },
    /// Send a message to a task's message queue
    SendMessage {
        task_id: String,
        body: String,
        #[serde(default)]
        sender: Option<String>,
        #[serde(default)]
        priority: Option<String>,
    },
    /// Send a chat message from the user to the coordinator agent.
    /// Unlike SendMessage (which targets a specific task's queue), UserChat
    /// targets the coordinator directly and expects a conversational response.
    UserChat {
        /// The user's message text
        message: String,
        /// Unique request ID for correlating this request with a response
        request_id: String,
        /// Optional file attachments
        #[serde(default)]
        attachments: Vec<workgraph::chat::Attachment>,
    },
}

/// IPC Response types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: Option<serde_json::Value>,
}

impl IpcResponse {
    pub fn success(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            data: Some(data),
        }
    }

    pub fn error(msg: &str) -> Self {
        Self {
            ok: false,
            error: Some(msg.to_string()),
            data: None,
        }
    }
}

/// Handle a single IPC connection
#[cfg(unix)]
pub(crate) fn handle_connection(
    dir: &Path,
    stream: UnixStream,
    running: &mut bool,
    wake_coordinator: &mut bool,
    urgent_wake: &mut bool,
    daemon_cfg: &mut DaemonConfig,
    logger: &DaemonLogger,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // Clone stream for writing
    let mut write_stream = stream
        .try_clone()
        .context("Failed to clone stream for writing")?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                let response = IpcResponse::error(&format!("Read error: {}", e));
                if let Err(we) = write_response(&mut write_stream, &response) {
                    logger.warn(&format!("Failed to send error response: {}", we));
                }
                return Ok(());
            }
        };

        if line.is_empty() {
            continue;
        }

        let request: IpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                logger.warn(&format!("Invalid IPC request: {}", e));
                let response = IpcResponse::error(&format!("Invalid request: {}", e));
                write_response(&mut write_stream, &response)?;
                continue;
            }
        };

        let response = handle_request(
            dir,
            request,
            running,
            wake_coordinator,
            urgent_wake,
            daemon_cfg,
            logger,
        );
        write_response(&mut write_stream, &response)?;

        // Check if we should stop
        if !*running {
            break;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn write_response(stream: &mut UnixStream, response: &IpcResponse) -> Result<()> {
    let json = serde_json::to_string(response)?;
    writeln!(stream, "{}", json)?;
    stream.flush()?;
    Ok(())
}

/// Handle an IPC request
fn handle_request(
    dir: &Path,
    request: IpcRequest,
    running: &mut bool,
    wake_coordinator: &mut bool,
    urgent_wake: &mut bool,
    daemon_cfg: &mut DaemonConfig,
    logger: &DaemonLogger,
) -> IpcResponse {
    match request {
        IpcRequest::Spawn {
            task_id,
            executor,
            timeout,
            model,
        } => {
            logger.info(&format!(
                "IPC Spawn: task_id={}, executor={}, timeout={:?}, model={:?}",
                task_id, executor, timeout, model
            ));
            let resp = handle_spawn(
                dir,
                &task_id,
                &executor,
                timeout.as_deref(),
                model.as_deref(),
            );
            if !resp.ok {
                logger.error(&format!(
                    "Spawn failed for task {}: {}",
                    task_id,
                    resp.error.as_deref().unwrap_or("unknown")
                ));
            }
            resp
        }
        IpcRequest::Agents => handle_agents(dir),
        IpcRequest::Kill { agent_id, force } => {
            logger.info(&format!("IPC Kill: agent_id={}, force={}", agent_id, force));
            handle_kill(dir, &agent_id, force)
        }
        IpcRequest::Heartbeat { agent_id } => handle_heartbeat(dir, &agent_id),
        IpcRequest::Status => handle_status(dir),
        IpcRequest::Shutdown {
            force,
            kill_agents,
            triggered_by_agent,
            triggered_by_task,
        } => {
            let caller = match (&triggered_by_agent, &triggered_by_task) {
                (Some(agent), Some(task)) => format!(", triggered_by={} (task: {})", agent, task),
                (Some(agent), None) => format!(", triggered_by={}", agent),
                (None, Some(task)) => format!(", triggered_by_task={}", task),
                (None, None) => String::new(),
            };
            logger.info(&format!(
                "IPC Shutdown: force={}, kill_agents={}{}",
                force, kill_agents, caller
            ));
            *running = false;
            handle_shutdown(dir, kill_agents, logger)
        }
        IpcRequest::GraphChanged => {
            *wake_coordinator = true;
            IpcResponse::success(serde_json::json!({
                "status": "ok",
                "action": "coordinator_wake_scheduled",
            }))
        }
        IpcRequest::Pause => {
            logger.info("IPC Pause: pausing coordinator");
            daemon_cfg.paused = true;
            let mut coord_state = CoordinatorState::load_or_default(dir);
            coord_state.paused = true;
            coord_state.save(dir);
            IpcResponse::success(serde_json::json!({
                "status": "paused",
            }))
        }
        IpcRequest::Resume => {
            logger.info("IPC Resume: resuming coordinator");
            daemon_cfg.paused = false;
            let mut coord_state = CoordinatorState::load_or_default(dir);
            coord_state.paused = false;
            coord_state.save(dir);
            *wake_coordinator = true;
            IpcResponse::success(serde_json::json!({
                "status": "resumed",
            }))
        }
        IpcRequest::Reconfigure {
            max_agents,
            executor,
            poll_interval,
            model,
        } => {
            logger.info(&format!(
                "IPC Reconfigure: max_agents={:?}, executor={:?}, poll_interval={:?}, model={:?}",
                max_agents, executor, poll_interval, model
            ));
            handle_reconfigure(
                dir,
                daemon_cfg,
                max_agents,
                executor,
                poll_interval,
                model,
                logger,
            )
        }
        IpcRequest::AddTask {
            title,
            id,
            description,
            after,
            tags,
            skills,
            deliverables,
            model,
            verify,
            origin,
        } => {
            logger.info(&format!(
                "IPC AddTask: title='{}', origin={:?}",
                title, origin
            ));
            let resp = handle_add_task(
                dir,
                &title,
                id.as_deref(),
                description.as_deref(),
                &after,
                &tags,
                &skills,
                &deliverables,
                model.as_deref(),
                verify.as_deref(),
                origin.as_deref(),
            );
            if resp.ok {
                *wake_coordinator = true;
            }
            resp
        }
        IpcRequest::QueryTask { task_id } => {
            logger.info(&format!("IPC QueryTask: task_id={}", task_id));
            handle_query_task(dir, &task_id)
        }
        IpcRequest::SendMessage {
            task_id,
            body,
            sender,
            priority,
        } => {
            let sender = sender.as_deref().unwrap_or("coordinator");
            let priority = priority.as_deref().unwrap_or("normal");
            logger.info(&format!(
                "IPC SendMessage: task_id={}, sender={}, priority={}",
                task_id, sender, priority
            ));
            handle_send_message(dir, &task_id, &body, sender, priority)
        }
        IpcRequest::UserChat {
            message,
            request_id,
            attachments,
        } => {
            logger.info(&format!("IPC UserChat: request_id={}", request_id));
            match append_chat_inbox(dir, &message, &request_id, attachments) {
                Ok(msg_id) => {
                    // Signal urgent wake — bypasses settling delay entirely
                    *urgent_wake = true;
                    IpcResponse::success(serde_json::json!({
                        "status": "accepted",
                        "request_id": request_id,
                        "inbox_id": msg_id,
                    }))
                }
                Err(e) => IpcResponse::error(&format!("Failed to store chat message: {}", e)),
            }
        }
    }
}

/// Handle spawn request
fn handle_spawn(
    dir: &Path,
    task_id: &str,
    executor: &str,
    timeout: Option<&str>,
    model: Option<&str>,
) -> IpcResponse {
    // Use the spawn command implementation
    match crate::commands::spawn::spawn_agent(dir, task_id, executor, timeout, model) {
        Ok((agent_id, pid)) => IpcResponse::success(serde_json::json!({
            "agent_id": agent_id,
            "pid": pid,
            "task_id": task_id,
            "executor": executor,
            "model": model,
        })),
        Err(e) => IpcResponse::error(&e.to_string()),
    }
}

/// Handle agents list request
fn handle_agents(dir: &Path) -> IpcResponse {
    match AgentRegistry::load(dir) {
        Ok(registry) => {
            let agents: Vec<_> = registry
                .list_agents()
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "id": a.id,
                        "task_id": a.task_id,
                        "executor": a.executor,
                        "pid": a.pid,
                        "status": format!("{:?}", a.status).to_lowercase(),
                        "uptime": a.uptime_human(),
                        "started_at": a.started_at,
                        "last_heartbeat": a.last_heartbeat,
                    })
                })
                .collect();
            IpcResponse::success(serde_json::json!({ "agents": agents }))
        }
        Err(e) => IpcResponse::error(&e.to_string()),
    }
}

/// Handle kill request
fn handle_kill(dir: &Path, agent_id: &str, force: bool) -> IpcResponse {
    match crate::commands::kill::run(dir, agent_id, force, true) {
        Ok(()) => IpcResponse::success(serde_json::json!({
            "killed": agent_id,
            "force": force,
        })),
        Err(e) => IpcResponse::error(&e.to_string()),
    }
}

/// Handle heartbeat request
fn handle_heartbeat(dir: &Path, agent_id: &str) -> IpcResponse {
    match AgentRegistry::load_locked(dir) {
        Ok(mut locked) => {
            if locked.heartbeat(agent_id) {
                if let Err(e) = locked.save() {
                    return IpcResponse::error(&e.to_string());
                }
                IpcResponse::success(serde_json::json!({
                    "agent_id": agent_id,
                    "heartbeat": "recorded",
                }))
            } else {
                IpcResponse::error(&format!("Agent '{}' not found", agent_id))
            }
        }
        Err(e) => IpcResponse::error(&e.to_string()),
    }
}

/// Handle status request
fn handle_status(dir: &Path) -> IpcResponse {
    let state = match ServiceState::load(dir) {
        Ok(Some(s)) => s,
        Ok(None) => return IpcResponse::error("No service state found"),
        Err(e) => return IpcResponse::error(&e.to_string()),
    };

    let registry = AgentRegistry::load_or_warn(dir);
    let alive_count = registry.active_count();
    let idle_count = registry.idle_count();

    // Use persisted coordinator state (reflects effective config + runtime metrics)
    let coord = CoordinatorState::load_or_default(dir);

    IpcResponse::success(serde_json::json!({
        "status": "running",
        "pid": state.pid,
        "socket": state.socket_path,
        "started_at": state.started_at,
        "agents": {
            "alive": alive_count,
            "idle": idle_count,
            "total": registry.agents.len(),
        },
        "coordinator": {
            "enabled": coord.enabled,
            "paused": coord.paused,
            "max_agents": coord.max_agents,
            "poll_interval": coord.poll_interval,
            "executor": coord.executor,
            "model": coord.model,
            "ticks": coord.ticks,
            "last_tick": coord.last_tick,
            "agents_alive": coord.agents_alive,
            "tasks_ready": coord.tasks_ready,
            "agents_spawned_last_tick": coord.agents_spawned,
        }
    }))
}

/// Handle shutdown request
fn handle_shutdown(dir: &Path, kill_agents: bool, logger: &DaemonLogger) -> IpcResponse {
    if kill_agents {
        // Only kill agents if explicitly requested.
        // Agents are detached (setsid) and survive daemon stop by default.
        if let Err(e) = crate::commands::kill::run_all(dir, true, true) {
            logger.error(&format!("Error killing agents during shutdown: {}", e));
        }
    }

    IpcResponse::success(serde_json::json!({
        "status": "shutting_down",
        "kill_agents": kill_agents,
    }))
}

/// Handle reconfigure request: update daemon config at runtime.
/// If all fields are None, re-read config.toml from disk.
fn handle_reconfigure(
    dir: &Path,
    daemon_cfg: &mut DaemonConfig,
    max_agents: Option<usize>,
    executor: Option<String>,
    poll_interval: Option<u64>,
    model: Option<String>,
    logger: &DaemonLogger,
) -> IpcResponse {
    let has_overrides =
        max_agents.is_some() || executor.is_some() || poll_interval.is_some() || model.is_some();

    if has_overrides {
        // Apply individual overrides
        if let Some(n) = max_agents {
            daemon_cfg.max_agents = n;
        }
        if let Some(e) = executor {
            daemon_cfg.executor = e;
        }
        if let Some(i) = poll_interval {
            daemon_cfg.poll_interval = Duration::from_secs(i);
        }
        if let Some(m) = model {
            daemon_cfg.model = Some(m);
        }
    } else {
        // No flags: re-read config.toml from disk
        match Config::load(dir) {
            Ok(config) => {
                daemon_cfg.max_agents = config.coordinator.max_agents;
                daemon_cfg.executor = config.coordinator.executor;
                daemon_cfg.poll_interval = Duration::from_secs(config.coordinator.poll_interval);
                daemon_cfg.model = config.coordinator.model;
                daemon_cfg.settling_delay =
                    Duration::from_millis(config.coordinator.settling_delay_ms);
            }
            Err(e) => {
                logger.error(&format!("Failed to reload config.toml: {}", e));
                return IpcResponse::error(&format!("Failed to reload config.toml: {}", e));
            }
        }
    }

    // Update persisted coordinator state so `wg service status` reflects the change
    if let Some(mut coord_state) = CoordinatorState::load(dir) {
        coord_state.max_agents = daemon_cfg.max_agents;
        coord_state.executor = daemon_cfg.executor.clone();
        coord_state.poll_interval = daemon_cfg.poll_interval.as_secs();
        coord_state.model = daemon_cfg.model.clone();
        coord_state.save(dir);
    }

    logger.info(&format!(
        "Reconfigured: max_agents={}, executor={}, poll_interval={}s, model={}{}",
        daemon_cfg.max_agents,
        daemon_cfg.executor,
        daemon_cfg.poll_interval.as_secs(),
        daemon_cfg.model.as_deref().unwrap_or("default"),
        if has_overrides {
            ""
        } else {
            " (from config.toml)"
        },
    ));

    IpcResponse::success(serde_json::json!({
        "status": "reconfigured",
        "source": if has_overrides { "flags" } else { "config.toml" },
        "config": {
            "max_agents": daemon_cfg.max_agents,
            "executor": daemon_cfg.executor,
            "poll_interval": daemon_cfg.poll_interval.as_secs(),
            "model": daemon_cfg.model,
        }
    }))
}

/// Handle AddTask IPC request — create a task in this workgraph from a remote peer.
#[allow(clippy::too_many_arguments)]
fn handle_add_task(
    dir: &Path,
    title: &str,
    id: Option<&str>,
    description: Option<&str>,
    after: &[String],
    tags: &[String],
    skills: &[String],
    deliverables: &[String],
    model: Option<&str>,
    verify: Option<&str>,
    origin: Option<&str>,
) -> IpcResponse {
    let graph_path = graph_path(dir);
    let title_owned = title.to_string();
    let description_owned = description.map(String::from);
    let after_owned = after.to_vec();
    let tags_owned = tags.to_vec();
    let skills_owned = skills.to_vec();
    let deliverables_owned = deliverables.to_vec();
    let model_owned = model.map(String::from);
    let verify_owned = verify.map(String::from);
    let id_owned = id.map(String::from);

    let task_id = match workgraph::parser::mutate_graph(&graph_path, |graph| -> anyhow::Result<String> {
        // Generate or validate task ID
        let task_id = match id_owned {
            Some(ref id) => {
                if graph.get_node(id).is_some() {
                    anyhow::bail!("Task with ID '{}' already exists", id);
                }
                id.clone()
            }
            None => {
                let slug: String = title_owned
                    .to_lowercase()
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '-' })
                    .collect::<String>()
                    .split('-')
                    .filter(|s| !s.is_empty())
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("-");
                let base_id = if slug.is_empty() {
                    "task".to_string()
                } else {
                    slug
                };
                if graph.get_node(&base_id).is_none() {
                    base_id
                } else {
                    let mut found = None;
                    for i in 2..1000 {
                        let candidate = format!("{}-{}", base_id, i);
                        if graph.get_node(&candidate).is_none() {
                            found = Some(candidate);
                            break;
                        }
                    }
                    found.unwrap_or_else(|| {
                        format!(
                            "task-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0)
                        )
                    })
                }
            }
        };

        let task = Task {
            id: task_id.clone(),
            title: title_owned.clone(),
            description: description_owned,
            status: Status::Open,
            assigned: None,
            estimate: None,
            before: vec![],
            after: after_owned.clone(),
            requires: vec![],
            tags: tags_owned,
            skills: skills_owned,
            inputs: vec![],
            deliverables: deliverables_owned,
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: model_owned,
            provider: None,
            verify: verify_owned,
            agent: None,
            loop_iteration: 0,
            cycle_failure_restarts: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            cycle_config: None,
            token_usage: None,
            session_id: None,
            wait_condition: None,
            checkpoint: None,
            resurrection_count: 0,
            last_resurrected_at: None,
            exec_mode: None,
        };

        graph.add_node(Node::Task(task));

        // Maintain bidirectional after/blocks consistency
        for dep in &after_owned {
            if let Some(blocker) = graph.get_task_mut(dep)
                && !blocker.before.contains(&task_id)
            {
                blocker.before.push(task_id.clone());
            }
        }

        Ok(task_id)
    }) {
        Ok(id) => id,
        Err(e) => return IpcResponse::error(&format!("Failed to add task: {}", e)),
    };

    // Notify TUI to auto-focus on the new task
    crate::commands::notify_new_task_focus(dir, &task_id);

    // Record provenance
    let origin_str = origin.unwrap_or("unknown");
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "add_task",
        Some(&task_id),
        None,
        serde_json::json!({ "title": title, "origin": origin_str, "remote": true }),
        config.log.rotation_threshold,
    );

    IpcResponse::success(serde_json::json!({
        "task_id": task_id,
        "title": title,
    }))
}

/// Handle SendMessage IPC request — send a message to a task's queue.
fn handle_send_message(
    dir: &Path,
    task_id: &str,
    body: &str,
    sender: &str,
    priority: &str,
) -> IpcResponse {
    // Validate task exists
    let graph_path = graph_path(dir);
    let graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return IpcResponse::error(&format!("Failed to load graph: {}", e)),
    };
    if graph.get_task(task_id).is_none() {
        return IpcResponse::error(&format!("Task '{}' not found", task_id));
    }

    match workgraph::messages::send_message(dir, task_id, body, sender, priority) {
        Ok(msg_id) => IpcResponse::success(serde_json::json!({
            "task_id": task_id,
            "message_id": msg_id,
        })),
        Err(e) => IpcResponse::error(&format!("Failed to send message: {}", e)),
    }
}

/// Handle QueryTask IPC request — return a task's status for cross-repo dependency checking.
fn handle_query_task(dir: &Path, task_id: &str) -> IpcResponse {
    let graph_path = graph_path(dir);
    let graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return IpcResponse::error(&format!("Failed to load graph: {}", e)),
    };

    match graph.get_task(task_id) {
        Some(task) => IpcResponse::success(serde_json::json!({
            "task_id": task.id,
            "title": task.title,
            "status": format!("{:?}", task.status),
            "assigned": task.assigned,
            "started_at": task.started_at,
            "completed_at": task.completed_at,
            "failure_reason": task.failure_reason,
        })),
        None => IpcResponse::error(&format!("Task '{}' not found", task_id)),
    }
}

/// Append a user chat message to the inbox.
/// Delegates to workgraph::chat for the actual storage.
fn append_chat_inbox(
    dir: &Path,
    content: &str,
    request_id: &str,
    attachments: Vec<workgraph::chat::Attachment>,
) -> Result<u64> {
    if attachments.is_empty() {
        workgraph::chat::append_inbox(dir, content, request_id)
    } else {
        workgraph::chat::append_inbox_with_attachments(dir, content, request_id, attachments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_ipc_request_serialization() {
        let req = IpcRequest::Spawn {
            task_id: "task-1".to_string(),
            executor: "claude".to_string(),
            timeout: Some("30m".to_string()),
            model: Some("sonnet".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"spawn\""));
        assert!(json.contains("\"task_id\":\"task-1\""));
        assert!(json.contains("\"model\":\"sonnet\""));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::Spawn {
                task_id,
                executor,
                timeout,
                model,
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(executor, "claude");
                assert_eq!(timeout, Some("30m".to_string()));
                assert_eq!(model, Some("sonnet".to_string()));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_ipc_graph_changed_serialization() {
        let req = IpcRequest::GraphChanged;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"graph_changed\""));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, IpcRequest::GraphChanged));

        // Also test parsing from raw JSON
        let raw = r#"{"cmd":"graph_changed"}"#;
        let parsed: IpcRequest = serde_json::from_str(raw).unwrap();
        assert!(matches!(parsed, IpcRequest::GraphChanged));
    }

    #[test]
    fn test_ipc_response_success() {
        let resp = IpcResponse::success(serde_json::json!({"agent_id": "agent-1"}));
        assert!(resp.ok);
        assert!(resp.error.is_none());
        assert!(resp.data.is_some());
    }

    #[test]
    fn test_ipc_response_error() {
        let resp = IpcResponse::error("Something went wrong");
        assert!(!resp.ok);
        assert_eq!(resp.error, Some("Something went wrong".to_string()));
        assert!(resp.data.is_none());
    }

    #[test]
    fn test_ipc_reconfigure_serialization_with_flags() {
        let req = IpcRequest::Reconfigure {
            max_agents: Some(8),
            executor: Some("opencode".to_string()),
            poll_interval: Some(120),
            model: Some("sonnet".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"reconfigure\""));
        assert!(json.contains("\"max_agents\":8"));
        assert!(json.contains("\"executor\":\"opencode\""));
        assert!(json.contains("\"poll_interval\":120"));
        assert!(json.contains("\"model\":\"sonnet\""));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::Reconfigure {
                max_agents,
                executor,
                poll_interval,
                model,
            } => {
                assert_eq!(max_agents, Some(8));
                assert_eq!(executor, Some("opencode".to_string()));
                assert_eq!(poll_interval, Some(120));
                assert_eq!(model, Some("sonnet".to_string()));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_ipc_reconfigure_serialization_no_flags() {
        // No flags means re-read from disk
        let req = IpcRequest::Reconfigure {
            max_agents: None,
            executor: None,
            poll_interval: None,
            model: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"reconfigure\""));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::Reconfigure {
                max_agents,
                executor,
                poll_interval,
                model,
            } => {
                assert!(max_agents.is_none());
                assert!(executor.is_none());
                assert!(poll_interval.is_none());
                assert!(model.is_none());
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_handle_reconfigure_with_flags() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Create initial coordinator state on disk
        let coord = CoordinatorState {
            enabled: true,
            max_agents: 4,
            poll_interval: 60,
            executor: "claude".to_string(),
            ..Default::default()
        };
        fs::create_dir_all(dir.join("service")).unwrap();
        coord.save(dir);

        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            paused: false,
            settling_delay: Duration::from_millis(2000),
        };

        let logger = DaemonLogger::open(dir).unwrap();
        let resp = handle_reconfigure(
            dir,
            &mut cfg,
            Some(8),
            Some("opencode".to_string()),
            None,
            Some("haiku".to_string()),
            &logger,
        );
        assert!(resp.ok);
        assert_eq!(cfg.max_agents, 8);
        assert_eq!(cfg.executor, "opencode");
        assert_eq!(cfg.poll_interval, Duration::from_secs(60)); // unchanged
        assert_eq!(cfg.model, Some("haiku".to_string()));

        // Verify persisted state was updated
        let loaded = CoordinatorState::load(dir).unwrap();
        assert_eq!(loaded.max_agents, 8);
        assert_eq!(loaded.executor, "opencode");
    }

    #[test]
    fn test_handle_reconfigure_from_disk() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Write a config.toml
        let config_content = r#"
[coordinator]
max_agents = 10
executor = "shell"
poll_interval = 120
"#;
        fs::write(dir.join("config.toml"), config_content).unwrap();
        fs::create_dir_all(dir.join("service")).unwrap();

        let coord = CoordinatorState {
            enabled: true,
            max_agents: 4,
            poll_interval: 60,
            executor: "claude".to_string(),
            ..Default::default()
        };
        coord.save(dir);

        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            paused: false,
            settling_delay: Duration::from_millis(2000),
        };

        let logger = DaemonLogger::open(dir).unwrap();
        // No flags -> re-read from disk
        let resp = handle_reconfigure(dir, &mut cfg, None, None, None, None, &logger);
        assert!(resp.ok);
        assert_eq!(cfg.max_agents, 10);
        assert_eq!(cfg.executor, "shell");
        assert_eq!(cfg.poll_interval, Duration::from_secs(120));
        assert_eq!(cfg.model, None); // config.toml doesn't set model
    }

    #[test]
    fn test_ipc_user_chat_serialization() {
        let req = IpcRequest::UserChat {
            message: "help me plan the auth system".to_string(),
            request_id: "chat-123-abcd".to_string(),
            attachments: vec![],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"user_chat\""));
        assert!(json.contains("\"message\":\"help me plan the auth system\""));
        assert!(json.contains("\"request_id\":\"chat-123-abcd\""));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::UserChat {
                message,
                request_id,
                ..
            } => {
                assert_eq!(message, "help me plan the auth system");
                assert_eq!(request_id, "chat-123-abcd");
            }
            _ => panic!("Wrong request type"),
        }

        // Also test parsing from raw JSON
        let raw = r#"{"cmd":"user_chat","message":"hello","request_id":"req-1"}"#;
        let parsed: IpcRequest = serde_json::from_str(raw).unwrap();
        match parsed {
            IpcRequest::UserChat {
                message,
                request_id,
                ..
            } => {
                assert_eq!(message, "hello");
                assert_eq!(request_id, "req-1");
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_handle_user_chat_sets_urgent_wake() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Create required directories
        fs::create_dir_all(dir.join("service")).unwrap();

        let mut running = true;
        let mut wake_coordinator = false;
        let mut urgent_wake = false;
        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            paused: false,
            settling_delay: Duration::from_millis(2000),
        };
        let logger = DaemonLogger::open(dir).unwrap();

        let resp = handle_request(
            dir,
            IpcRequest::UserChat {
                message: "test message".to_string(),
                request_id: "req-test-1".to_string(),
                attachments: vec![],
            },
            &mut running,
            &mut wake_coordinator,
            &mut urgent_wake,
            &mut cfg,
            &logger,
        );

        // Verify response
        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data["status"], "accepted");
        assert_eq!(data["request_id"], "req-test-1");
        assert_eq!(data["inbox_id"], 1);

        // Verify urgent_wake was set (not wake_coordinator)
        assert!(urgent_wake, "urgent_wake should be true after UserChat");
        assert!(
            !wake_coordinator,
            "wake_coordinator should NOT be set by UserChat"
        );

        // Verify message was written to inbox
        let msgs = workgraph::chat::read_inbox(dir).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "test message");
        assert_eq!(msgs[0].request_id, "req-test-1");
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn test_graph_changed_sets_wake_not_urgent() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        let mut running = true;
        let mut wake_coordinator = false;
        let mut urgent_wake = false;
        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            paused: false,
            settling_delay: Duration::from_millis(2000),
        };
        let logger = DaemonLogger::open(dir).unwrap();

        handle_request(
            dir,
            IpcRequest::GraphChanged,
            &mut running,
            &mut wake_coordinator,
            &mut urgent_wake,
            &mut cfg,
            &logger,
        );

        // GraphChanged should set wake_coordinator, NOT urgent_wake
        assert!(
            wake_coordinator,
            "wake_coordinator should be true after GraphChanged"
        );
        assert!(
            !urgent_wake,
            "urgent_wake should NOT be set by GraphChanged"
        );
    }
}

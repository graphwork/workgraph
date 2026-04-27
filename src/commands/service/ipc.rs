//! IPC protocol: message types and request handlers for the service daemon.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use workgraph::config::Config;
use workgraph::cron::{calculate_next_fire, parse_cron_expression};
use workgraph::graph::{Node, PRIORITY_DEFAULT, PRIORITY_HIGH, Status, Task};
use workgraph::parser::{load_graph, modify_graph};
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
        #[serde(default)]
        redispatch: bool,
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
    },
    /// Notify that the graph has changed; triggers an immediate coordinator tick
    GraphChanged,
    /// Pause the coordinator (no new agent spawns, running agents unaffected)
    Pause,
    /// Resume the coordinator (triggers immediate tick)
    Resume,
    /// Freeze all running agents (SIGSTOP) and pause the coordinator
    Freeze,
    /// Thaw all frozen agents (SIGCONT) and resume the coordinator
    Thaw,
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
        #[serde(default)]
        verify_timeout: Option<String>,
        /// Who requested this (for provenance)
        #[serde(default)]
        origin: Option<String>,
        /// Cron schedule expression (6-field format: "sec min hour day month dow")
        #[serde(default)]
        cron: Option<String>,
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
    /// Send a chat message from the user to a chat agent.
    /// Unlike SendMessage (which targets a specific task's queue), UserChat
    /// targets the chat agent directly and expects a conversational response.
    UserChat {
        /// The user's message text
        message: String,
        /// Unique request ID for correlating this request with a response
        request_id: String,
        /// Optional file attachments
        #[serde(default)]
        attachments: Vec<workgraph::chat::Attachment>,
        /// Target chat agent (default: 0)
        #[serde(default, alias = "coordinator_id")]
        chat_id: Option<u32>,
    },
    /// Create a new chat agent instance.
    #[serde(alias = "create_coordinator")]
    CreateChat {
        /// Optional human-readable name for the chat agent.
        #[serde(default)]
        name: Option<String>,
        /// Per-chat model override (e.g., "openai:qwen3-coder-30b").
        #[serde(default)]
        model: Option<String>,
        /// Per-chat executor override (e.g., "native").
        #[serde(default)]
        executor: Option<String>,
    },
    /// Hot-swap a chat agent's executor and/or model. Persists
    /// the override in CoordinatorState, SIGTERMs the current
    /// handler, and lets the supervisor respawn via spawn-task
    /// with the new executor. Conversation continuity is preserved
    /// because chat/<ref>/*.jsonl is shared across handlers — the
    /// new handler replays prior turns on its first prompt.
    #[serde(alias = "set_coordinator_executor")]
    SetChatExecutor {
        #[serde(alias = "coordinator_id")]
        chat_id: u32,
        #[serde(default)]
        executor: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },
    /// Delete a chat agent instance.
    #[serde(alias = "delete_coordinator")]
    DeleteChat {
        #[serde(alias = "coordinator_id")]
        chat_id: u32,
    },
    /// Archive a chat agent instance (mark as Done).
    #[serde(alias = "archive_coordinator")]
    ArchiveChat {
        #[serde(alias = "coordinator_id")]
        chat_id: u32,
    },
    /// Stop a chat agent instance (kill agent, reset to Open).
    #[serde(alias = "stop_coordinator")]
    StopChat {
        #[serde(alias = "coordinator_id")]
        chat_id: u32,
    },
    /// Interrupt a chat agent's current generation (sends SIGINT, does NOT kill).
    /// The chat process stays alive and can accept new messages immediately.
    #[serde(alias = "interrupt_coordinator")]
    InterruptChat {
        #[serde(alias = "coordinator_id")]
        chat_id: u32,
    },
    /// List all active chat agents.
    #[serde(alias = "list_coordinators")]
    ListChats,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_connection(
    dir: &Path,
    stream: UnixStream,
    running: &mut bool,
    wake_coordinator: &mut bool,
    urgent_wake: &mut bool,
    pending_coordinator_ids: &mut Vec<u32>,
    delete_coordinator_ids: &mut Vec<u32>,
    interrupt_coordinator_ids: &mut Vec<u32>,
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
            pending_coordinator_ids,
            delete_coordinator_ids,
            interrupt_coordinator_ids,
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
#[allow(clippy::too_many_arguments)]
fn handle_request(
    dir: &Path,
    request: IpcRequest,
    running: &mut bool,
    wake_coordinator: &mut bool,
    urgent_wake: &mut bool,
    pending_coordinator_ids: &mut Vec<u32>,
    delete_coordinator_ids: &mut Vec<u32>,
    interrupt_coordinator_ids: &mut Vec<u32>,
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
                logger,
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
        IpcRequest::Kill {
            agent_id,
            force,
            redispatch,
        } => {
            logger.info(&format!(
                "IPC Kill: agent_id={}, force={}, redispatch={}",
                agent_id, force, redispatch
            ));
            handle_kill(dir, &agent_id, force, redispatch)
        }
        IpcRequest::Heartbeat { agent_id } => handle_heartbeat(dir, &agent_id),
        IpcRequest::Status => handle_status(dir),
        IpcRequest::Shutdown { force, kill_agents } => {
            logger.info(&format!(
                "IPC Shutdown: force={}, kill_agents={}",
                force, kill_agents
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
        IpcRequest::Freeze => {
            logger.info("IPC Freeze: sending SIGSTOP to all agents and pausing coordinator");
            handle_freeze(dir, daemon_cfg, logger)
        }
        IpcRequest::Thaw => {
            logger.info("IPC Thaw: sending SIGCONT to frozen agents and resuming coordinator");
            let resp = handle_thaw(dir, daemon_cfg, logger);
            if resp.ok {
                *wake_coordinator = true;
            }
            resp
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
            verify_timeout,
            origin,
            cron,
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
                verify_timeout.as_deref(),
                cron.as_deref(),
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
            chat_id,
        } => {
            let cid = chat_id.unwrap_or(0);
            logger.info(&format!(
                "IPC UserChat: request_id={}, chat_id={}",
                request_id, cid
            ));
            match append_chat_inbox(dir, cid, &message, &request_id, attachments) {
                Ok(msg_id) => {
                    // Signal urgent wake — bypasses settling delay entirely
                    *urgent_wake = true;
                    // Track which chat agent was targeted for lazy spawning
                    pending_coordinator_ids.push(cid);
                    IpcResponse::success(serde_json::json!({
                        "status": "accepted",
                        "request_id": request_id,
                        "inbox_id": msg_id,
                        "chat_id": cid,
                    }))
                }
                Err(e) => IpcResponse::error(&format!("Failed to store chat message: {}", e)),
            }
        }
        IpcRequest::CreateChat {
            name,
            model,
            executor,
        } => {
            logger.info(&format!(
                "IPC CreateChat: name={:?}, model={:?}, executor={:?}",
                name, model, executor
            ));
            handle_create_coordinator(dir, name.as_deref(), model.as_deref(), executor.as_deref())
        }
        IpcRequest::SetChatExecutor {
            chat_id,
            executor,
            model,
        } => {
            logger.info(&format!(
                "IPC SetChatExecutor: chat_id={}, executor={:?}, model={:?}",
                chat_id, executor, model
            ));
            handle_set_coordinator_executor(
                dir,
                chat_id,
                executor.as_deref(),
                model.as_deref(),
            )
        }
        IpcRequest::DeleteChat { chat_id } => {
            logger.info(&format!(
                "IPC DeleteChat: chat_id={}",
                chat_id
            ));
            let resp = handle_delete_coordinator(dir, chat_id);
            if resp.ok {
                delete_coordinator_ids.push(chat_id);
            }
            resp
        }
        IpcRequest::ArchiveChat { chat_id } => {
            logger.info(&format!(
                "IPC ArchiveChat: chat_id={}",
                chat_id
            ));
            let resp = handle_archive_coordinator(dir, chat_id);
            if resp.ok {
                delete_coordinator_ids.push(chat_id);
            }
            resp
        }
        IpcRequest::StopChat { chat_id } => {
            logger.info(&format!(
                "IPC StopChat: chat_id={}",
                chat_id
            ));
            let resp = handle_stop_coordinator(dir, chat_id);
            if resp.ok {
                delete_coordinator_ids.push(chat_id);
            }
            resp
        }
        IpcRequest::InterruptChat { chat_id } => {
            logger.info(&format!(
                "IPC InterruptChat: chat_id={}",
                chat_id
            ));
            // No graph changes — just signal the daemon to send SIGINT to the
            // chat agent's Claude CLI subprocess. The actual interrupt happens
            // in the daemon loop where coordinator_agents is accessible.
            interrupt_coordinator_ids.push(chat_id);
            IpcResponse::success(serde_json::json!({
                "chat_id": chat_id,
                "interrupted": true,
            }))
        }
        IpcRequest::ListChats => {
            logger.info("IPC ListChats");
            handle_list_coordinators(dir)
        }
    }
}

/// Handle spawn request.
///
/// Routes through `workgraph::dispatch::plan_spawn` so the IPC spawn entry
/// honors the same {executor, model, endpoint} precedence as the
/// dispatcher tick. The IPC-passed `executor` is treated as a manual hint
/// that plan_spawn consults at the `agent_executor` level (wins over
/// `[dispatcher].executor` but loses to `task.exec` / `task.exec_mode`).
fn handle_spawn(
    dir: &Path,
    task_id: &str,
    executor: &str,
    timeout: Option<&str>,
    model: Option<&str>,
    logger: &DaemonLogger,
) -> IpcResponse {
    let gp = graph_path(dir);
    let graph = match load_graph(&gp) {
        Ok(g) => g,
        Err(e) => return IpcResponse::error(&format!("Failed to load graph: {}", e)),
    };
    let task = match graph.get_task(task_id) {
        Some(t) => t.clone(),
        None => return IpcResponse::error(&format!("Task '{}' not found", task_id)),
    };

    let config = Config::load_or_default(dir);

    // Agency-derived executor wins over the IPC hint. If the task is bound
    // to an agent, use that agent's effective_executor; otherwise fall
    // back to the IPC-passed executor (`wg spawn --executor X`). Pass the
    // model so the agency can override claude → native when the model has
    // a non-Anthropic provider prefix (autohaiku regression fix).
    let agents_dir = dir.join("agency").join("cache/agents");
    let agent_entity = task
        .agent
        .as_ref()
        .and_then(|hash| workgraph::agency::find_agent_by_prefix(&agents_dir, hash).ok());
    let prospective_model = task
        .model
        .as_deref()
        .or(model)
        .or(config.coordinator.model.as_deref());
    let agency_executor = agent_entity
        .as_ref()
        .map(|a| a.effective_executor_for_model(prospective_model).to_string());
    let ipc_executor = if executor.is_empty() {
        None
    } else {
        Some(executor.to_string())
    };
    let agent_executor_owned = agency_executor.or(ipc_executor);

    // SINGLE SOURCE OF TRUTH: every spawn decision flows through plan_spawn.
    let plan = match workgraph::dispatch::plan_spawn(
        &task,
        &config,
        agent_executor_owned.as_deref(),
        model,
    ) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("plan_spawn for {}: {}", task_id, e);
            logger.error(&msg);
            return IpcResponse::error(&msg);
        }
    };

    // Provenance: every IPC-driven spawn emits one line tracing each
    // decision back to the config knob that produced it.
    logger.info(&format!(
        "[ipc] {}: {}",
        task_id,
        plan.provenance.log_line(&plan)
    ));

    let resolved_executor = plan.executor.as_str().to_string();
    let resolved_model = plan.model.raw.clone();

    match crate::commands::spawn::spawn_agent(
        dir,
        task_id,
        &resolved_executor,
        timeout,
        Some(&resolved_model),
    ) {
        Ok((agent_id, pid)) => IpcResponse::success(serde_json::json!({
            "agent_id": agent_id,
            "pid": pid,
            "task_id": task_id,
            "executor": resolved_executor,
            "model": resolved_model,
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
fn handle_kill(dir: &Path, agent_id: &str, force: bool, redispatch: bool) -> IpcResponse {
    match crate::commands::kill::run(dir, agent_id, force, redispatch, true) {
        Ok(()) => IpcResponse::success(serde_json::json!({
            "killed": agent_id,
            "force": force,
            "paused": !redispatch,
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
        if let Err(e) = crate::commands::kill::run_all(dir, true, true, true) {
            logger.error(&format!("Error killing agents during shutdown: {}", e));
        }
    }

    IpcResponse::success(serde_json::json!({
        "status": "shutting_down",
        "kill_agents": kill_agents,
    }))
}

/// Handle freeze: send SIGSTOP to all alive agent processes, pause coordinator,
/// and update registry + coordinator state.
#[cfg(unix)]
fn handle_freeze(dir: &Path, daemon_cfg: &mut DaemonConfig, logger: &DaemonLogger) -> IpcResponse {
    use workgraph::service::registry::AgentStatus;

    let mut coord_state = CoordinatorState::load_or_default(dir);
    if coord_state.frozen {
        return IpcResponse::success(serde_json::json!({
            "status": "already_frozen",
            "frozen_pids": coord_state.frozen_pids,
        }));
    }

    let mut locked_registry = match AgentRegistry::load_locked(dir) {
        Ok(r) => r,
        Err(e) => return IpcResponse::error(&format!("Failed to load registry: {}", e)),
    };

    let mut frozen_pids = Vec::new();
    let mut failed_pids = Vec::new();

    for agent in locked_registry.registry.agents.values_mut() {
        if !agent.is_alive() {
            continue;
        }
        let pid = agent.pid as i32;
        if unsafe { libc::kill(pid, libc::SIGSTOP) } == 0 {
            frozen_pids.push(agent.pid);
            agent.status = AgentStatus::Frozen;
            logger.info(&format!(
                "Sent SIGSTOP to agent {} (PID {})",
                agent.id, agent.pid
            ));
        } else {
            let err = std::io::Error::last_os_error();
            logger.warn(&format!(
                "Failed to SIGSTOP agent {} (PID {}): {}",
                agent.id, agent.pid, err
            ));
            failed_pids.push(agent.pid);
        }
    }

    if let Err(e) = locked_registry.save() {
        logger.error(&format!("Failed to save registry after freeze: {}", e));
    }

    // Pause coordinator so no new agents are spawned
    daemon_cfg.paused = true;
    coord_state.paused = true;
    coord_state.frozen = true;
    coord_state.frozen_pids = frozen_pids.clone();
    coord_state.save(dir);

    logger.info(&format!(
        "Freeze complete: {} agents frozen, {} failed",
        frozen_pids.len(),
        failed_pids.len()
    ));

    IpcResponse::success(serde_json::json!({
        "status": "frozen",
        "frozen_count": frozen_pids.len(),
        "frozen_pids": frozen_pids,
        "failed_pids": failed_pids,
    }))
}

#[cfg(not(unix))]
fn handle_freeze(
    _dir: &Path,
    _daemon_cfg: &mut DaemonConfig,
    _logger: &DaemonLogger,
) -> IpcResponse {
    IpcResponse::error("Freeze is only supported on Unix systems")
}

/// Handle thaw: send SIGCONT to all frozen agent processes, resume coordinator,
/// and update registry + coordinator state.
#[cfg(unix)]
fn handle_thaw(dir: &Path, daemon_cfg: &mut DaemonConfig, logger: &DaemonLogger) -> IpcResponse {
    use crate::commands::is_process_alive;
    use workgraph::service::registry::AgentStatus;

    let mut coord_state = CoordinatorState::load_or_default(dir);
    if !coord_state.frozen {
        return IpcResponse::success(serde_json::json!({
            "status": "not_frozen",
        }));
    }

    let mut locked_registry = match AgentRegistry::load_locked(dir) {
        Ok(r) => r,
        Err(e) => return IpcResponse::error(&format!("Failed to load registry: {}", e)),
    };

    let mut thawed_pids = Vec::new();
    let mut dead_pids = Vec::new();
    let mut failed_pids = Vec::new();

    for agent in locked_registry.registry.agents.values_mut() {
        if agent.status != AgentStatus::Frozen {
            continue;
        }

        if !is_process_alive(agent.pid) {
            // Agent died while frozen (e.g., OOM killed)
            agent.status = AgentStatus::Dead;
            dead_pids.push(agent.pid);
            logger.warn(&format!(
                "Agent {} (PID {}) died while frozen",
                agent.id, agent.pid
            ));
            continue;
        }

        let pid = agent.pid as i32;
        if unsafe { libc::kill(pid, libc::SIGCONT) } == 0 {
            agent.status = AgentStatus::Working;
            thawed_pids.push(agent.pid);
            logger.info(&format!(
                "Sent SIGCONT to agent {} (PID {})",
                agent.id, agent.pid
            ));
        } else {
            let err = std::io::Error::last_os_error();
            logger.warn(&format!(
                "Failed to SIGCONT agent {} (PID {}): {}",
                agent.id, agent.pid, err
            ));
            failed_pids.push(agent.pid);
        }
    }

    if let Err(e) = locked_registry.save() {
        logger.error(&format!("Failed to save registry after thaw: {}", e));
    }

    // Resume coordinator
    daemon_cfg.paused = false;
    coord_state.paused = false;
    coord_state.frozen = false;
    coord_state.frozen_pids.clear();
    coord_state.save(dir);

    logger.info(&format!(
        "Thaw complete: {} agents thawed, {} dead, {} failed",
        thawed_pids.len(),
        dead_pids.len(),
        failed_pids.len()
    ));

    IpcResponse::success(serde_json::json!({
        "status": "thawed",
        "thawed_count": thawed_pids.len(),
        "thawed_pids": thawed_pids,
        "dead_pids": dead_pids,
        "failed_pids": failed_pids,
    }))
}

#[cfg(not(unix))]
fn handle_thaw(_dir: &Path, _daemon_cfg: &mut DaemonConfig, _logger: &DaemonLogger) -> IpcResponse {
    IpcResponse::error("Thaw is only supported on Unix systems")
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
        match Config::load_merged(dir) {
            Ok(config) => {
                daemon_cfg.max_agents = config.coordinator.max_agents;
                daemon_cfg.executor = config.coordinator.effective_executor();
                daemon_cfg.poll_interval = Duration::from_secs(config.coordinator.poll_interval);
                daemon_cfg.model = config.coordinator.model;
                daemon_cfg.provider = config.coordinator.provider;
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
    verify_timeout: Option<&str>,
    cron: Option<&str>,
    origin: Option<&str>,
) -> IpcResponse {
    let graph_path = graph_path(dir);
    let graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return IpcResponse::error(&format!("Failed to load graph: {}", e)),
    };

    // Generate or validate task ID
    let task_id = match id {
        Some(id) => {
            if graph.get_node(id).is_some() {
                return IpcResponse::error(&format!("Task with ID '{}' already exists", id));
            }
            id.to_string()
        }
        None => {
            // Reuse the same slug generation logic as add.rs
            let slug: String = title
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

    // Handle cron scheduling
    let (cron_schedule, cron_enabled, next_cron_fire) = if let Some(cron_expr) = cron {
        // Validate the cron expression
        match parse_cron_expression(cron_expr) {
            Ok(schedule) => {
                // Calculate next fire time from now
                let next_fire = calculate_next_fire(&schedule, chrono::Utc::now());
                let next_fire_str = next_fire.map(|dt| dt.to_rfc3339());
                (Some(cron_expr.to_string()), true, next_fire_str)
            }
            Err(e) => {
                return IpcResponse::error(&format!(
                    "Invalid cron expression '{}': {}",
                    cron_expr, e
                ));
            }
        }
    } else {
        (None, false, None)
    };

    let task = Task {
        id: task_id.clone(),
        title: title.to_string(),
        description: description.map(String::from),
        status: Status::Open,
        priority: PRIORITY_DEFAULT,
        assigned: None,
        estimate: None,
        before: vec![],
        after: after.to_vec(),
        requires: vec![],
        tags: tags.to_vec(),
        skills: skills.to_vec(),
        inputs: vec![],
        deliverables: deliverables.to_vec(),
        artifacts: vec![],
        exec: None,
        timeout: None,
        not_before: None,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries: None,
        failure_reason: None,
        model: model.map(String::from),
        provider: None,
        endpoint: None,
        verify: verify.map(String::from),
        verify_timeout: verify_timeout.map(String::from),
        agent: None,
        loop_iteration: 0,
        last_iteration_completed_at: None,
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
        triage_count: 0,
        resurrection_count: 0,
        last_resurrected_at: None,
        validation: None,
        validation_commands: vec![],
        validator_agent: None,
        validator_model: None,
        gate_attempts: 0,
        test_required: false,
        rejection_count: 0,
        max_rejections: None,
        exec_mode: None,
        verify_failures: 0,
        rescue_count: 0,
        spawn_failures: 0,
        dispatch_count: 0,
        tier: None,
        no_tier_escalation: false,
        tried_models: vec![],
        superseded_by: vec![],
        supersedes: None,
        unplaced: false,
        place_near: vec![],
        place_before: vec![],
        independent: false,
        iteration_round: 0,
        iteration_anchor: None,
        iteration_parent: None,
        iteration_config: None,
        cron_schedule,
        cron_enabled,
        last_cron_fire: None,
        next_cron_fire,
    };

    // Save atomically via modify_graph
    let task_for_save = task.clone();
    let task_id_for_save = task_id.clone();
    let after_for_save: Vec<String> = after.iter().map(|s| s.to_string()).collect();
    match modify_graph(&graph_path, |graph| {
        graph.add_node(Node::Task(task_for_save.clone()));
        // Maintain bidirectional after/blocks consistency
        for dep in &after_for_save {
            if let Some(blocker) = graph.get_task_mut(dep)
                && !blocker.before.contains(&task_id_for_save)
            {
                blocker.before.push(task_id_for_save.clone());
            }
        }
        true
    }) {
        Ok(_) => {}
        Err(e) => return IpcResponse::error(&format!("Failed to save graph: {}", e)),
    }

    // Notify TUI to auto-focus on the new task (skip internal/system tasks)
    if !task_id.starts_with('.') {
        crate::commands::notify_new_task_focus(dir, &task_id);
    }

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

/// Append a user chat message to a coordinator's inbox.
/// Delegates to workgraph::chat for the actual storage.
fn append_chat_inbox(
    dir: &Path,
    coordinator_id: u32,
    content: &str,
    request_id: &str,
    attachments: Vec<workgraph::chat::Attachment>,
) -> Result<u64> {
    if attachments.is_empty() {
        workgraph::chat::append_inbox_for(dir, coordinator_id, content, request_id)
    } else {
        workgraph::chat::append_inbox_with_attachments_for(
            dir,
            coordinator_id,
            content,
            request_id,
            attachments,
        )
    }
}

/// Find the next fresh chat agent ID by scanning both existing chat tasks
/// (legacy `.coordinator-N` and new `.chat-N`) and existing chat history files.
/// Returns max(existing_ids) + 1 to ensure the new chat has never existed before
/// and has no chat history files.
fn find_next_fresh_coordinator_id(graph: &workgraph::graph::WorkGraph, dir: &Path) -> u32 {
    let mut max_id = None::<u32>;

    // Scan all existing chat tasks (both new .chat-N and legacy .coordinator-N)
    for task in graph.tasks() {
        if let Some(id) = workgraph::chat_id::parse_chat_task_id(&task.id) {
            max_id = Some(max_id.map_or(id, |current_max| current_max.max(id)));
        }
    }

    // Scan all existing chat history files
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            // Look for chat-history-{id}.jsonl files
            if name_str.starts_with("chat-history-") && name_str.ends_with(".jsonl") {
                let id_part = &name_str[13..name_str.len() - 6]; // Remove "chat-history-" and ".jsonl"
                if let Ok(id) = id_part.parse::<u32>() {
                    max_id = Some(max_id.map_or(id, |current_max| current_max.max(id)));
                }
            }
        }
    }

    // Scan sessions.json for all coordinator aliases (active + archived).
    // This replaces the old filesystem scan and catches archived
    // coordinators whose chat dirs have been moved to .archive/.
    if let Ok(reg) = workgraph::chat_sessions::load(dir) {
        for meta in reg.sessions.values() {
            if meta.kind != workgraph::chat_sessions::SessionKind::Coordinator {
                continue;
            }
            for alias in &meta.aliases {
                if let Some(suffix) = alias.strip_prefix("coordinator-")
                    && let Ok(id) = suffix.parse::<u32>()
                {
                    max_id = Some(max_id.map_or(id, |current_max| current_max.max(id)));
                }
            }
        }
    }

    // Return max_id + 1, or 0 if no coordinators exist yet
    max_id.map_or(0, |id| id + 1)
}

/// Handle CreateCoordinator IPC request.
fn handle_create_coordinator(
    dir: &Path,
    name: Option<&str>,
    model: Option<&str>,
    executor: Option<&str>,
) -> IpcResponse {
    let graph_path = crate::commands::graph_path(dir);
    let mut graph = match workgraph::parser::load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return IpcResponse::error(&format!("Failed to load graph: {}", e)),
    };

    let config = workgraph::config::Config::load_or_default(dir);
    let max = config.coordinator.max_coordinators;
    let alive = graph
        .tasks()
        .filter(|t| t.tags.iter().any(|tag| workgraph::chat_id::is_chat_loop_tag(tag)))
        .filter(|t| !matches!(t.status, workgraph::graph::Status::Abandoned))
        .filter(|t| !t.tags.iter().any(|tag| tag == "archived"))
        .count();
    if alive >= max {
        return IpcResponse::error(&format!(
            "Chat cap reached ({}/{})",
            alive, max
        ));
    }

    // Find the next available chat ID by scanning both existing tasks
    // and existing chat history files to ensure truly fresh chats.
    let next_id = find_next_fresh_coordinator_id(&graph, dir);

    // Create the chat task
    let title = name
        .map(|n| format!("Chat: {}", n))
        .unwrap_or_else(|| format!("Chat {}", next_id));

    let task = workgraph::graph::Task {
        id: workgraph::chat_id::format_chat_task_id(next_id),
        title,
        description: Some(format!(
            "Chat {} — persistent chat agent.",
            next_id
        )),
        status: workgraph::graph::Status::InProgress,
        priority: PRIORITY_HIGH,
        tags: vec![workgraph::chat_id::CHAT_LOOP_TAG.to_string()],
        cycle_config: Some(workgraph::graph::CycleConfig {
            max_iterations: 0,
            guard: None,
            delay: None,
            no_converge: true,
            restart_on_failure: true,
            max_failure_restarts: None,
        }),
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        log: vec![workgraph::graph::LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: Some("daemon".to_string()),
            user: Some(workgraph::current_user()),
            message: format!("Chat {} task created via IPC", next_id),
        }],
        ..Default::default()
    };

    graph.add_node(workgraph::graph::Node::Task(task));

    // Companion `.archive-N` and `.compact-N` tasks are no longer created.
    // Archival runs natively in the dispatcher (see `run_automatic_archival`);
    // graph-cycle compaction has been retired entirely.

    match workgraph::parser::modify_graph(&graph_path, |fresh| {
        // Re-apply all mutations to a fresh graph
        for node in graph.nodes() {
            if let workgraph::graph::Node::Task(t) = node {
                if let Some(ft) = fresh.get_task_mut(&t.id) {
                    *ft = t.clone();
                } else {
                    fresh.add_node(workgraph::graph::Node::Task(t.clone()));
                }
            }
        }
        true
    }) {
        Ok(_) => {}
        Err(e) => return IpcResponse::error(&format!("Failed to save graph: {}", e)),
    }

    // Record executor/model combo in launcher history
    {
        let exec = executor.unwrap_or("claude");
        let _ = workgraph::launcher_history::record_use(
            &workgraph::launcher_history::HistoryEntry::new(exec, model, None, "tui"),
        );
    }

    // Write per-coordinator state file with model/executor overrides if specified.
    if model.is_some() || executor.is_some() {
        let mut state = super::CoordinatorState::load_or_default_for(dir, next_id);
        state.model_override = model.map(String::from);
        state.executor_override = executor.map(String::from);
        state.save_for(dir, next_id);
    }

    IpcResponse::success(serde_json::json!({
        "coordinator_id": next_id,
        "chat_id": next_id,
        "task_id": workgraph::chat_id::format_chat_task_id(next_id),
        "name": name,
    }))
}

/// Handle DeleteCoordinator IPC request.
fn handle_delete_coordinator(dir: &Path, coordinator_id: u32) -> IpcResponse {
    let graph_path = crate::commands::graph_path(dir);
    let task_id = workgraph::chat_id::format_chat_task_id(coordinator_id);
    let legacy_task_id = format!(".coordinator-{}", coordinator_id);
    let mut result_msg: Option<String> = None;
    match workgraph::parser::modify_graph(&graph_path, |graph| {
        // Try .chat-N (new), then .coordinator-N (legacy), then .coordinator (very-legacy ID 0)
        let resolved_id = if graph.get_task(&task_id).is_some() {
            task_id.as_str()
        } else if graph.get_task(&legacy_task_id).is_some() {
            legacy_task_id.as_str()
        } else if coordinator_id == 0 && graph.get_task(".coordinator").is_some() {
            ".coordinator"
        } else {
            result_msg = Some(format!("Chat task '{}' not found", task_id));
            return false;
        };
        let task = graph.get_task_mut(resolved_id).unwrap();
        task.status = workgraph::graph::Status::Abandoned;
        task.log.push(workgraph::graph::LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: Some("daemon".to_string()),
            user: Some(workgraph::current_user()),
            message: format!("Chat {} deleted via IPC", coordinator_id),
        });
        true
    }) {
        Ok(_) => {}
        Err(e) => return IpcResponse::error(&format!("Failed to save graph: {}", e)),
    }
    if let Some(msg) = result_msg {
        return IpcResponse::error(&msg);
    }

    IpcResponse::success(serde_json::json!({
        "coordinator_id": coordinator_id,
        "task_id": task_id,
    }))
}

/// Handle ArchiveCoordinator IPC request.
/// Marks the chat task as Done, tags it "archived", and
/// archives the chat session (moves chat dir to `.archive/`, updates
/// sessions.json) so it won't be resurrected on restart.
fn handle_archive_coordinator(dir: &Path, coordinator_id: u32) -> IpcResponse {
    let graph_path = crate::commands::graph_path(dir);
    let task_id = workgraph::chat_id::format_chat_task_id(coordinator_id);
    let legacy_task_id = format!(".coordinator-{}", coordinator_id);
    let mut result_msg: Option<String> = None;
    match workgraph::parser::modify_graph(&graph_path, |graph| {
        // Try .chat-N (new), then .coordinator-N (legacy), then .coordinator (very-legacy ID 0)
        let resolved_id = if graph.get_task(&task_id).is_some() {
            task_id.as_str()
        } else if graph.get_task(&legacy_task_id).is_some() {
            legacy_task_id.as_str()
        } else if coordinator_id == 0 && graph.get_task(".coordinator").is_some() {
            ".coordinator"
        } else {
            result_msg = Some(format!("Chat task '{}' not found", task_id));
            return false;
        };
        let task = graph.get_task_mut(resolved_id).unwrap();
        task.status = workgraph::graph::Status::Done;
        task.tags
            .retain(|t| !workgraph::chat_id::is_chat_loop_tag(t));
        if !task.tags.contains(&"archived".to_string()) {
            task.tags.push("archived".to_string());
        }
        task.log.push(workgraph::graph::LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: Some("daemon".to_string()),
            user: Some(workgraph::current_user()),
            message: format!("Chat {} archived via IPC", coordinator_id),
        });
        true
    }) {
        Ok(_) => {}
        Err(e) => return IpcResponse::error(&format!("Failed to save graph: {}", e)),
    }
    if let Some(msg) = result_msg {
        return IpcResponse::error(&msg);
    }

    // Archive the chat session so the chat dir moves to .archive/
    // and won't be resurrected on daemon restart.
    let alias = format!("coordinator-{}", coordinator_id);
    if let Err(e) = workgraph::chat_sessions::archive_session(dir, &alias) {
        eprintln!(
            "[ipc] Warning: chat {} task archived but chat session archive failed: {}",
            coordinator_id, e
        );
    }

    IpcResponse::success(serde_json::json!({
        "coordinator_id": coordinator_id,
        "task_id": task_id,
    }))
}

/// Handle StopCoordinator IPC request.
/// Kills any running agent for this coordinator and resets the task to Open.
/// Hot-swap the executor / model for an existing coordinator.
///
/// Writes the override into `CoordinatorState` so future supervisor
/// restarts use the new executor, then SIGTERMs the live handler.
/// `subprocess_coordinator_loop`'s `child.wait()` returns as the
/// handler exits, the loop's restart branch fires, and spawn-task
/// reads `WG_EXECUTOR_TYPE=<new>` on the next cycle. Conversation
/// history lives in `chat/coordinator-<N>/{inbox,outbox}.jsonl` —
/// shared across handlers — so the new executor sees prior turns.
fn handle_set_coordinator_executor(
    dir: &Path,
    coordinator_id: u32,
    executor: Option<&str>,
    model: Option<&str>,
) -> IpcResponse {
    if executor.is_none() && model.is_none() {
        return IpcResponse::error("at least one of --executor or --model must be provided");
    }

    let mut state = super::CoordinatorState::load_or_default_for(dir, coordinator_id);
    if let Some(e) = executor {
        state.executor_override = Some(e.to_string());
    }
    if let Some(m) = model {
        state.model_override = Some(m.to_string());
    }
    state.save_for(dir, coordinator_id);

    // Signal the live handler to exit so the supervisor respawns
    // with the new executor_override in effect.
    let chat_dir = dir
        .join("chat")
        .join(format!("coordinator-{}", coordinator_id));
    let mut handler_pid: Option<u32> = None;
    if let Ok(Some(info)) = workgraph::session_lock::read_holder(&chat_dir)
        && info.alive
    {
        handler_pid = Some(info.pid);
        #[cfg(unix)]
        unsafe {
            libc::kill(info.pid as i32, libc::SIGTERM);
        }
    }

    IpcResponse::success(serde_json::json!({
        "coordinator_id": coordinator_id,
        "executor": executor,
        "model": model,
        "signaled_pid": handler_pid,
        "note": "supervisor will respawn the handler with the new settings",
    }))
}

fn handle_stop_coordinator(dir: &Path, coordinator_id: u32) -> IpcResponse {
    let graph_path = crate::commands::graph_path(dir);
    let task_id = workgraph::chat_id::format_chat_task_id(coordinator_id);
    let legacy_task_id = format!(".coordinator-{}", coordinator_id);

    // Resolve the actual task ID (.chat-N new, .coordinator-N legacy, or .coordinator very-legacy)
    let resolved_task_id = if let Ok(graph) = workgraph::parser::load_graph(&graph_path) {
        if graph.get_task(&task_id).is_some() {
            task_id.clone()
        } else if graph.get_task(&legacy_task_id).is_some() {
            legacy_task_id.clone()
        } else if coordinator_id == 0 && graph.get_task(".coordinator").is_some() {
            ".coordinator".to_string()
        } else {
            task_id.clone()
        }
    } else {
        task_id.clone()
    };

    // Kill any running agent (must happen before modify_graph to avoid holding lock)
    if let Ok(graph) = workgraph::parser::load_graph(&graph_path)
        && let Some(task) = graph.get_task(&resolved_task_id)
        && task.agent.is_some()
        && let Ok(registry) = AgentRegistry::load(dir)
    {
        for agent in registry.list_agents() {
            if agent.task_id == resolved_task_id {
                let _ = crate::commands::kill::run(dir, &agent.id, false, true, true);
                break;
            }
        }
    }

    let mut result_msg: Option<String> = None;
    match workgraph::parser::modify_graph(&graph_path, |graph| {
        // Try .chat-N (new), then .coordinator-N (legacy), then .coordinator (very-legacy ID 0)
        let actual_id = if graph.get_task(&task_id).is_some() {
            task_id.as_str()
        } else if graph.get_task(&legacy_task_id).is_some() {
            legacy_task_id.as_str()
        } else if coordinator_id == 0 && graph.get_task(".coordinator").is_some() {
            ".coordinator"
        } else {
            result_msg = Some(format!("Chat task '{}' not found", task_id));
            return false;
        };
        let task = graph.get_task_mut(actual_id).unwrap();
        task.status = workgraph::graph::Status::Open;
        task.assigned = None;
        task.log.push(workgraph::graph::LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: Some("daemon".to_string()),
            user: Some(workgraph::current_user()),
            message: format!("Chat {} stopped via IPC", coordinator_id),
        });
        true
    }) {
        Ok(_) => {}
        Err(e) => return IpcResponse::error(&format!("Failed to save graph: {}", e)),
    }
    if let Some(msg) = result_msg {
        return IpcResponse::error(&msg);
    }

    IpcResponse::success(serde_json::json!({
        "coordinator_id": coordinator_id,
        "task_id": task_id,
    }))
}

/// Handle ListCoordinators IPC request.
fn handle_list_coordinators(dir: &Path) -> IpcResponse {
    let graph_path = crate::commands::graph_path(dir);
    let graph = match workgraph::parser::load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return IpcResponse::error(&format!("Failed to load graph: {}", e)),
    };

    let mut coordinators = Vec::new();
    for task in graph.tasks() {
        if task.tags.iter().any(|t| t == "coordinator-loop") {
            // Skip abandoned or archived coordinators
            if matches!(task.status, workgraph::graph::Status::Abandoned) {
                continue;
            }
            if task.tags.iter().any(|t| t == "archived") {
                continue;
            }
            // Extract coordinator ID from task ID (.coordinator-N)
            let cid = task
                .id
                .strip_prefix(".coordinator-")
                .and_then(|s: &str| s.parse::<u32>().ok())
                .or_else(|| {
                    // Legacy .coordinator (no suffix) → ID 0
                    if task.id == ".coordinator" {
                        Some(0)
                    } else {
                        None
                    }
                });
            if let Some(id) = cid {
                coordinators.push(serde_json::json!({
                    "coordinator_id": id,
                    "task_id": task.id,
                    "title": task.title,
                    "status": format!("{:?}", task.status),
                    "loop_iteration": task.loop_iteration,
                }));
            }
        }
    }

    coordinators.sort_by_key(|c| c["coordinator_id"].as_u64().unwrap_or(0));

    IpcResponse::success(serde_json::json!({
        "coordinators": coordinators,
    }))
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

        // Create initial coordinator state on disk (per-ID file)
        let coord = CoordinatorState {
            enabled: true,
            max_agents: 4,
            poll_interval: 60,
            executor: "claude".to_string(),
            ..Default::default()
        };
        fs::create_dir_all(dir.join("service")).unwrap();
        coord.save_for(dir, 0);

        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            provider: None,
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
        let loaded = CoordinatorState::load_for(dir, 0).unwrap();
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
        coord.save_for(dir, 0);

        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            provider: None,
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
        // cfg.model may be None (no global config) or Some(...) (from global config merge).
        // The local config.toml in the test doesn't set a model, so any value here
        // comes from the host's global config — which is expected after the merge fix.
    }

    #[test]
    fn test_ipc_user_chat_serialization() {
        let req = IpcRequest::UserChat {
            message: "help me plan the auth system".to_string(),
            request_id: "chat-123-abcd".to_string(),
            attachments: vec![],
            chat_id: None,
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

        // Also test parsing from raw JSON (backward compat: no chat_id)
        let raw = r#"{"cmd":"user_chat","message":"hello","request_id":"req-1"}"#;
        let parsed: IpcRequest = serde_json::from_str(raw).unwrap();
        match parsed {
            IpcRequest::UserChat {
                message,
                request_id,
                chat_id,
                ..
            } => {
                assert_eq!(message, "hello");
                assert_eq!(request_id, "req-1");
                assert_eq!(chat_id, None); // defaults to None
            }
            _ => panic!("Wrong request type"),
        }

        // Test backward-compat: legacy field name `coordinator_id` is accepted
        let raw2 = r#"{"cmd":"user_chat","message":"hi","request_id":"req-2","coordinator_id":1}"#;
        let parsed2: IpcRequest = serde_json::from_str(raw2).unwrap();
        match parsed2 {
            IpcRequest::UserChat { chat_id, .. } => {
                assert_eq!(chat_id, Some(1));
            }
            _ => panic!("Wrong request type"),
        }

        // Test new field name `chat_id`
        let raw3 = r#"{"cmd":"user_chat","message":"hi","request_id":"req-3","chat_id":2}"#;
        let parsed3: IpcRequest = serde_json::from_str(raw3).unwrap();
        match parsed3 {
            IpcRequest::UserChat { chat_id, .. } => {
                assert_eq!(chat_id, Some(2));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_ipc_create_chat_serialization() {
        let req = IpcRequest::CreateChat {
            name: Some("Feature Work".to_string()),
            model: None,
            executor: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        // New canonical command name
        assert!(json.contains("\"cmd\":\"create_chat\""));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::CreateChat {
                name,
                model,
                executor,
            } => {
                assert_eq!(name, Some("Feature Work".to_string()));
                assert_eq!(model, None);
                assert_eq!(executor, None);
            }
            _ => panic!("Wrong request type"),
        }

        // Test with model and executor overrides
        let req2 = IpcRequest::CreateChat {
            name: Some("Local Model".to_string()),
            model: Some("openai:qwen3-coder-30b".to_string()),
            executor: Some("native".to_string()),
        };
        let json2 = serde_json::to_string(&req2).unwrap();
        let parsed2: IpcRequest = serde_json::from_str(&json2).unwrap();
        match parsed2 {
            IpcRequest::CreateChat {
                name,
                model,
                executor,
            } => {
                assert_eq!(name, Some("Local Model".to_string()));
                assert_eq!(model, Some("openai:qwen3-coder-30b".to_string()));
                assert_eq!(executor, Some("native".to_string()));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_ipc_legacy_create_coordinator_accepted_with_warning() {
        // Backward-compat: legacy `create_coordinator` command name still parses.
        let raw = r#"{"cmd":"create_coordinator","name":"Legacy"}"#;
        let parsed: IpcRequest = serde_json::from_str(raw).unwrap();
        match parsed {
            IpcRequest::CreateChat { name, .. } => {
                assert_eq!(name, Some("Legacy".to_string()));
            }
            _ => panic!("Legacy create_coordinator must parse to CreateChat"),
        }
    }

    #[test]
    fn test_ipc_delete_chat_serialization() {
        let req = IpcRequest::DeleteChat { chat_id: 2 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"delete_chat\""));
        assert!(json.contains("\"chat_id\":2"));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::DeleteChat { chat_id } => {
                assert_eq!(chat_id, 2);
            }
            _ => panic!("Wrong request type"),
        }

        // Backward-compat: legacy `delete_coordinator` + `coordinator_id` still parses.
        let raw = r#"{"cmd":"delete_coordinator","coordinator_id":7}"#;
        let parsed: IpcRequest = serde_json::from_str(raw).unwrap();
        match parsed {
            IpcRequest::DeleteChat { chat_id } => assert_eq!(chat_id, 7),
            _ => panic!("Legacy delete_coordinator must parse to DeleteChat"),
        }
    }

    #[test]
    fn test_ipc_list_chats_serialization() {
        let req = IpcRequest::ListChats;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"list_chats\""));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, IpcRequest::ListChats));

        // Backward-compat: legacy `list_coordinators` still parses.
        let raw = r#"{"cmd":"list_coordinators"}"#;
        let parsed: IpcRequest = serde_json::from_str(raw).unwrap();
        assert!(matches!(parsed, IpcRequest::ListChats));
    }

    #[test]
    fn test_ipc_archive_chat_serialization() {
        let req = IpcRequest::ArchiveChat { chat_id: 3 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"archive_chat\""));
        assert!(json.contains("\"chat_id\":3"));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::ArchiveChat { chat_id } => {
                assert_eq!(chat_id, 3);
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_ipc_stop_chat_serialization() {
        let req = IpcRequest::StopChat { chat_id: 1 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"stop_chat\""));
        assert!(json.contains("\"chat_id\":1"));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::StopChat { chat_id } => {
                assert_eq!(chat_id, 1);
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_ipc_interrupt_chat_serialization() {
        let req = IpcRequest::InterruptChat { chat_id: 2 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"interrupt_chat\""));
        assert!(json.contains("\"chat_id\":2"));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::InterruptChat { chat_id } => {
                assert_eq!(chat_id, 2);
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
        let mut pending_coordinator_ids = Vec::new();
        let mut delete_coordinator_ids = Vec::new();
        let mut interrupt_coordinator_ids = Vec::new();
        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            provider: None,
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
                chat_id: None,
            },
            &mut running,
            &mut wake_coordinator,
            &mut urgent_wake,
            &mut pending_coordinator_ids,
            &mut delete_coordinator_ids,
            &mut interrupt_coordinator_ids,
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

        // Verify pending_coordinator_ids was populated
        assert_eq!(pending_coordinator_ids, vec![0]);

        // Verify message was written to inbox (coordinator 0)
        let msgs = workgraph::chat::read_inbox(dir).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "test message");
        assert_eq!(msgs[0].request_id, "req-test-1");
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn test_handle_user_chat_with_coordinator_id() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        fs::create_dir_all(dir.join("service")).unwrap();

        let mut running = true;
        let mut wake_coordinator = false;
        let mut urgent_wake = false;
        let mut pending_coordinator_ids = Vec::new();
        let mut delete_coordinator_ids = Vec::new();
        let mut interrupt_coordinator_ids = Vec::new();
        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            provider: None,
            paused: false,
            settling_delay: Duration::from_millis(2000),
        };
        let logger = DaemonLogger::open(dir).unwrap();

        // Send to coordinator 1
        let resp = handle_request(
            dir,
            IpcRequest::UserChat {
                message: "message for coord 1".to_string(),
                request_id: "req-coord1".to_string(),
                attachments: vec![],
                chat_id: Some(1),
            },
            &mut running,
            &mut wake_coordinator,
            &mut urgent_wake,
            &mut pending_coordinator_ids,
            &mut delete_coordinator_ids,
            &mut interrupt_coordinator_ids,
            &mut cfg,
            &logger,
        );

        assert!(resp.ok);
        let data = resp.data.unwrap();
        assert_eq!(data["chat_id"], 1);

        // Verify pending_coordinator_ids tracks the targeted chat agent
        assert_eq!(pending_coordinator_ids, vec![1]);

        // Message should be in coordinator 1's inbox, not coordinator 0's
        let msgs0 = workgraph::chat::read_inbox(dir).unwrap();
        assert!(msgs0.is_empty());

        let msgs1 = workgraph::chat::read_inbox_for(dir, 1).unwrap();
        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs1[0].content, "message for coord 1");
    }

    #[test]
    fn test_graph_changed_sets_wake_not_urgent() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        let mut running = true;
        let mut wake_coordinator = false;
        let mut urgent_wake = false;
        let mut pending_coordinator_ids = Vec::new();
        let mut delete_coordinator_ids = Vec::new();
        let mut interrupt_coordinator_ids = Vec::new();
        let mut cfg = DaemonConfig {
            max_agents: 4,
            executor: "claude".to_string(),
            poll_interval: Duration::from_secs(60),
            model: None,
            provider: None,
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
            &mut pending_coordinator_ids,
            &mut delete_coordinator_ids,
            &mut interrupt_coordinator_ids,
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

    #[test]
    fn test_handle_add_task_internal_no_focus_steal() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Create an empty graph file
        fs::write(dir.join("graph.jsonl"), "").unwrap();

        let focus_path = dir.join(".new_task_focus");

        // Adding an internal (dot-prefixed) task should NOT create the focus marker
        let resp = handle_add_task(
            dir,
            "Internal eval task",
            Some(".evaluate-my-task"),
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None, // verify
            None, // verify_timeout
            None, // cron
            None, // origin
        );
        assert!(resp.ok, "Adding internal task should succeed");
        assert!(
            !focus_path.exists(),
            "Internal dot-prefixed task should NOT create .new_task_focus"
        );

        // Adding a regular task SHOULD create the focus marker
        let resp = handle_add_task(
            dir,
            "User task",
            Some("my-regular-task"),
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None, // verify
            None, // verify_timeout
            None, // cron
            None, // origin
        );
        assert!(resp.ok, "Adding regular task should succeed");
        assert!(
            focus_path.exists(),
            "Regular task should create .new_task_focus"
        );
        let focused_id = fs::read_to_string(&focus_path).unwrap();
        assert_eq!(focused_id, "my-regular-task");
    }

    #[test]
    fn test_handle_list_coordinators_excludes_abandoned_but_keeps_done() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Create coordinator tasks with various statuses
        let active = workgraph::graph::Task {
            id: ".coordinator-0".to_string(),
            title: "Active Coordinator".to_string(),
            status: workgraph::graph::Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        let abandoned = workgraph::graph::Task {
            id: ".coordinator-1".to_string(),
            title: "Abandoned Coordinator".to_string(),
            status: workgraph::graph::Status::Abandoned,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        let done = workgraph::graph::Task {
            id: ".coordinator-2".to_string(),
            title: "Done Coordinator".to_string(),
            status: workgraph::graph::Status::Done,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };

        // Write graph to disk
        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(workgraph::graph::Node::Task(active));
        graph.add_node(workgraph::graph::Node::Task(abandoned));
        graph.add_node(workgraph::graph::Node::Task(done));
        workgraph::parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

        let resp = handle_list_coordinators(dir);
        assert!(resp.ok);
        let data = resp.data.unwrap();
        let coordinators = data["coordinators"].as_array().unwrap();

        // Active and Done coordinators should be listed; Abandoned should be excluded
        assert_eq!(coordinators.len(), 2);
        let ids: Vec<_> = coordinators
            .iter()
            .map(|c| c["coordinator_id"].as_u64().unwrap())
            .collect();
        assert!(ids.contains(&0), "Active coordinator should be listed");
        assert!(ids.contains(&2), "Done coordinator should be listed");
        assert!(
            !ids.contains(&1),
            "Abandoned coordinator should not be listed"
        );
    }

    #[test]
    fn test_handle_archive_coordinator_adds_archived_tag_and_removes_coordinator_loop() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        let task = workgraph::graph::Task {
            id: ".coordinator-2".to_string(),
            title: "Coordinator 2".to_string(),
            status: workgraph::graph::Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };

        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(workgraph::graph::Node::Task(task));
        workgraph::parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

        let resp = handle_archive_coordinator(dir, 2);
        assert!(resp.ok);

        // Reload and check task state
        let graph = workgraph::parser::load_graph(&dir.join("graph.jsonl")).unwrap();
        let task = graph.get_task(".coordinator-2").unwrap();
        assert_eq!(task.status, workgraph::graph::Status::Done);
        assert!(task.tags.contains(&"archived".to_string()));
        assert!(!task.tags.contains(&"coordinator-loop".to_string()));
    }

    #[test]
    fn test_handle_list_coordinators_excludes_archived() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        let active = workgraph::graph::Task {
            id: ".coordinator-0".to_string(),
            title: "Active".to_string(),
            status: workgraph::graph::Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        let archived = workgraph::graph::Task {
            id: ".coordinator-1".to_string(),
            title: "Archived".to_string(),
            status: workgraph::graph::Status::Done,
            tags: vec!["coordinator-loop".to_string(), "archived".to_string()],
            ..Default::default()
        };

        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(workgraph::graph::Node::Task(active));
        graph.add_node(workgraph::graph::Node::Task(archived));
        workgraph::parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

        let resp = handle_list_coordinators(dir);
        assert!(resp.ok);
        let data = resp.data.unwrap();
        let coordinators = data["coordinators"].as_array().unwrap();

        assert_eq!(coordinators.len(), 1);
        assert_eq!(coordinators[0]["coordinator_id"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_per_user_coord_create_with_user_label() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Create empty graph
        let graph = workgraph::graph::WorkGraph::new();
        workgraph::parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

        // Create chat agent labeled "alice"
        let resp = handle_create_coordinator(dir, Some("alice"), None, None);
        assert!(resp.ok, "create_chat should succeed");

        // Verify the chat task was created with correct label and new prefix
        let graph = workgraph::parser::load_graph(&dir.join("graph.jsonl")).unwrap();
        let coord = graph
            .get_task(".chat-0")
            .expect("chat task should exist with new .chat-N prefix");
        assert_eq!(coord.title, "Chat: alice");
        assert!(coord.tags.contains(&"chat-loop".to_string()));

        // Create chat labeled "bob"
        let resp = handle_create_coordinator(dir, Some("bob"), None, None);
        assert!(resp.ok, "create_chat for bob should succeed");

        let graph = workgraph::parser::load_graph(&dir.join("graph.jsonl")).unwrap();
        let coord = graph
            .get_task(".chat-1")
            .expect("second chat should exist");
        assert_eq!(coord.title, "Chat: bob");

        // Both chats should coexist
        assert!(graph.get_task(".chat-0").is_some());
        assert!(graph.get_task(".chat-1").is_some());
    }

    #[test]
    fn test_per_user_coord_two_users_independent_state() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Create empty graph and two coordinators
        let graph = workgraph::graph::WorkGraph::new();
        workgraph::parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();

        handle_create_coordinator(dir, Some("alice"), None, None);
        handle_create_coordinator(dir, Some("bob"), None, None);

        // Write per-coordinator state files
        let alice_state = CoordinatorState {
            enabled: true,
            max_agents: 3,
            accumulated_tokens: 100,
            ..Default::default()
        };
        alice_state.save_for(dir, 0);

        let bob_state = CoordinatorState {
            enabled: true,
            max_agents: 5,
            accumulated_tokens: 200,
            ..Default::default()
        };
        bob_state.save_for(dir, 1);

        // Verify independent state
        let alice_loaded = CoordinatorState::load_for(dir, 0).unwrap();
        assert_eq!(alice_loaded.max_agents, 3);
        assert_eq!(alice_loaded.accumulated_tokens, 100);

        let bob_loaded = CoordinatorState::load_for(dir, 1).unwrap();
        assert_eq!(bob_loaded.max_agents, 5);
        assert_eq!(bob_loaded.accumulated_tokens, 200);

        // Updating alice doesn't affect bob
        let mut alice_updated = alice_loaded;
        alice_updated.accumulated_tokens = 999;
        alice_updated.save_for(dir, 0);

        let bob_check = CoordinatorState::load_for(dir, 1).unwrap();
        assert_eq!(
            bob_check.accumulated_tokens, 200,
            "bob's state should be untouched"
        );
    }
}

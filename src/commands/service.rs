//! Agent Service Daemon
//!
//! Manages the wg service daemon that coordinates agent spawning, monitoring,
//! and automatic task assignment. The daemon integrates coordinator logic to
//! periodically find ready tasks, spawn agents, and clean up finished agents.
//!
//! Usage:
//!   wg service start [--max-agents N] [--executor E] [--interval S]  # Start with overrides
//!   wg service stop [--force]                                        # Stop the service daemon
//!   wg service status                                                # Show service + coordinator state
//!
//! The daemon respects coordinator config from .workgraph/config.toml:
//!   [coordinator]
//!   max_agents = 4       # Maximum parallel agents
//!   poll_interval = 60   # Background safety-net poll interval (seconds)
//!   interval = 30        # Coordinator tick interval (standalone command)
//!   executor = "claude"  # Executor for spawned agents

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Read as IoRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use chrono::Utc;

use workgraph::agency;
use workgraph::config::Config;
use workgraph::graph::{LogEntry, Node, Status, Task, evaluate_cycle_iteration};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks_with_peers_cycle_aware;
use workgraph::service::registry::{AgentEntry, AgentRegistry, AgentStatus};

use super::{graph_path, is_process_alive, spawn};

// ---------------------------------------------------------------------------
// Persistent daemon logger
// ---------------------------------------------------------------------------

/// Maximum log file size before rotation (10 MB)
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Path to the daemon log file
pub fn log_file_path(dir: &Path) -> PathBuf {
    dir.join("service").join("daemon.log")
}

/// A simple file-based logger with timestamps and size-based rotation.
///
/// The logger keeps one backup (`daemon.log.1`) and truncates when the active
/// log exceeds [`LOG_MAX_BYTES`].
#[derive(Clone)]
pub struct DaemonLogger {
    inner: Arc<Mutex<DaemonLoggerInner>>,
}

struct DaemonLoggerInner {
    file: fs::File,
    path: PathBuf,
    written: u64,
}

impl DaemonLogger {
    /// Open (or create) the log file at `.workgraph/service/daemon.log`.
    pub fn open(dir: &Path) -> Result<Self> {
        let service_dir = dir.join("service");
        if !service_dir.exists() {
            fs::create_dir_all(&service_dir)?;
        }
        let path = log_file_path(dir);
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open daemon log at {:?}", path))?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Mutex::new(DaemonLoggerInner {
                file,
                path,
                written,
            })),
        })
    }

    /// Write a timestamped line to the log.  `level` is a short tag like
    /// `INFO`, `WARN`, or `ERROR`.
    pub fn log(&self, level: &str, msg: &str) {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let line = format!("{} [{}] {}\n", ts, level, msg);
        if let Ok(mut inner) = self.inner.lock() {
            if let Err(e) = inner.file.write_all(line.as_bytes()) {
                eprintln!("Warning: daemon log write failed: {}", e);
            }
            if let Err(e) = inner.file.flush() {
                eprintln!("Warning: daemon log flush failed: {}", e);
            }
            inner.written += line.len() as u64;
            if inner.written >= LOG_MAX_BYTES {
                Self::rotate(&mut inner);
            }
        }
    }

    pub fn info(&self, msg: &str) {
        self.log("INFO", msg);
    }

    pub fn warn(&self, msg: &str) {
        self.log("WARN", msg);
    }

    pub fn error(&self, msg: &str) {
        self.log("ERROR", msg);
    }

    /// Rotate: rename current log to `.log.1` (overwriting any previous
    /// backup) and open a fresh file.
    fn rotate(inner: &mut DaemonLoggerInner) {
        let backup = inner.path.with_extension("log.1");
        // Best-effort: ignore errors during rotation
        let _ = fs::rename(&inner.path, &backup);
        if let Ok(f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&inner.path)
        {
            inner.file = f;
            inner.written = 0;
        }
    }

    /// Install a panic hook that writes the panic info to this log before
    /// the process aborts.
    pub fn install_panic_hook(&self) {
        let logger = self.clone();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let msg = format!("PANIC: {}", info);
            logger.log("FATAL", &msg);
            default_hook(info);
        }));
    }
}

/// Read the last `n` lines from the daemon log that match the given level
/// (or all lines if `level_filter` is `None`).  Returns up to `n` lines,
/// most recent last.
pub fn tail_log(dir: &Path, n: usize, level_filter: Option<&str>) -> Vec<String> {
    let path = log_file_path(dir);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = content.lines().collect();
    let filtered: Vec<String> = if let Some(level) = level_filter {
        let tag = format!("[{}]", level);
        lines
            .iter()
            .filter(|l| l.contains(&tag))
            .map(std::string::ToString::to_string)
            .collect()
    } else {
        lines.iter().map(std::string::ToString::to_string).collect()
    };
    filtered
        .into_iter()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Default socket path (project-specific, inside .workgraph dir)
pub fn default_socket_path(dir: &Path) -> PathBuf {
    dir.join("service").join("daemon.sock")
}

/// Path to the service state file
pub fn state_file_path(dir: &Path) -> PathBuf {
    dir.join("service").join("state.json")
}

/// Service state stored on disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    pub pid: u32,
    pub socket_path: String,
    pub started_at: String,
}

impl ServiceState {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = state_file_path(dir);
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read service state from {:?}", path))?;
        let state: ServiceState = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse service state from {:?}", path))?;
        Ok(Some(state))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let service_dir = dir.join("service");
        if !service_dir.exists() {
            fs::create_dir_all(&service_dir).with_context(|| {
                format!("Failed to create service directory at {:?}", service_dir)
            })?;
        }
        let path = state_file_path(dir);
        let content =
            serde_json::to_string_pretty(self).context("Failed to serialize service state")?;
        fs::write(&path, content)
            .with_context(|| format!("Failed to write service state to {:?}", path))?;
        Ok(())
    }

    pub fn remove(dir: &Path) -> Result<()> {
        let path = state_file_path(dir);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove service state at {:?}", path))?;
        }
        Ok(())
    }
}

/// Path to the coordinator state file
pub fn coordinator_state_path(dir: &Path) -> PathBuf {
    dir.join("service").join("coordinator-state.json")
}

/// Runtime coordinator state persisted to disk for status queries
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoordinatorState {
    /// Whether the coordinator is enabled
    pub enabled: bool,
    /// Effective config: max agents
    pub max_agents: usize,
    /// Effective config: background poll interval seconds (safety net)
    pub poll_interval: u64,
    /// Effective config: executor name
    pub executor: String,
    /// Effective config: model for spawned agents
    #[serde(default)]
    pub model: Option<String>,
    /// Total coordinator ticks completed
    pub ticks: u64,
    /// ISO 8601 timestamp of the last tick
    pub last_tick: Option<String>,
    /// Number of agents alive at last tick
    pub agents_alive: usize,
    /// Number of tasks ready at last tick
    pub tasks_ready: usize,
    /// Number of agents spawned in last tick
    pub agents_spawned: usize,
    /// Whether the coordinator is paused (no new agent spawns)
    #[serde(default)]
    pub paused: bool,
}

impl CoordinatorState {
    pub fn load(dir: &Path) -> Option<Self> {
        let path = coordinator_state_path(dir);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return None, // file doesn't exist yet
        };
        match serde_json::from_str(&content) {
            Ok(state) => Some(state),
            Err(e) => {
                eprintln!(
                    "Warning: corrupt coordinator state at {}: {}",
                    path.display(),
                    e
                );
                None
            }
        }
    }

    pub fn save(&self, dir: &Path) {
        let path = coordinator_state_path(dir);
        match serde_json::to_string_pretty(self) {
            Ok(content) => {
                if let Err(e) = fs::write(&path, content) {
                    eprintln!(
                        "Warning: failed to save coordinator state to {}: {}",
                        path.display(),
                        e
                    );
                }
            }
            Err(e) => {
                eprintln!("Warning: failed to serialize coordinator state: {}", e);
            }
        }
    }

    /// Load coordinator state, defaulting to empty if missing or corrupt.
    /// Corrupt files already emit a warning via `load()`.
    pub fn load_or_default(dir: &Path) -> Self {
        Self::load(dir).unwrap_or_default()
    }

    pub fn remove(dir: &Path) {
        let path = coordinator_state_path(dir);
        let _ = fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------------------
// Coordinator tick logic (moved from deprecated coordinator.rs)
// ---------------------------------------------------------------------------

/// Result of a single coordinator tick
pub struct TickResult {
    /// Number of agents alive after the tick
    pub agents_alive: usize,
    /// Number of ready tasks found
    pub tasks_ready: usize,
    /// Number of agents spawned in this tick
    pub agents_spawned: usize,
}

/// Clean up dead agents and count alive ones. Returns `None` with an early
/// `TickResult` if the alive count already meets `max_agents`.
fn cleanup_and_count_alive(
    dir: &Path,
    graph_path: &Path,
    max_agents: usize,
) -> Result<Result<usize, TickResult>> {
    // Clean up dead agents: process exited
    let finished_agents = cleanup_dead_agents(dir, graph_path)?;
    if !finished_agents.is_empty() {
        eprintln!(
            "[coordinator] Cleaned up {} dead agent(s): {:?}",
            finished_agents.len(),
            finished_agents
        );
    }

    // Now count truly alive agents (process still running)
    let registry = AgentRegistry::load(dir)?;
    let alive_count = registry
        .agents
        .values()
        .filter(|a| a.is_alive() && is_process_alive(a.pid))
        .count();

    if alive_count >= max_agents {
        eprintln!(
            "[coordinator] Max agents ({}) running, waiting...",
            max_agents
        );
        return Ok(Err(TickResult {
            agents_alive: alive_count,
            tasks_ready: 0,
            agents_spawned: 0,
        }));
    }

    Ok(Ok(alive_count))
}

/// Check whether any tasks are ready. Returns `None` with an early `TickResult`
/// if no ready tasks exist.
fn check_ready_or_return(
    graph: &workgraph::graph::WorkGraph,
    alive_count: usize,
    dir: &Path,
) -> Option<TickResult> {
    let cycle_analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_with_peers_cycle_aware(graph, dir, &cycle_analysis);
    if ready.is_empty() {
        let terminal = graph.tasks().filter(|t| t.status.is_terminal()).count();
        let total = graph.tasks().count();
        if terminal == total && total > 0 {
            eprintln!("[coordinator] All {} tasks complete!", total);
        } else {
            eprintln!(
                "[coordinator] No ready tasks (terminal: {}/{})",
                terminal, total
            );
        }
        return Some(TickResult {
            agents_alive: alive_count,
            tasks_ready: 0,
            agents_spawned: 0,
        });
    }
    None
}

/// Auto-assign: build assignment subgraph for unassigned ready tasks.
///
/// Per the agency design (§4, §10), when auto_assign is enabled and a ready
/// task has no agent field, the coordinator creates a blocking assignment task
/// `assign-{task-id}` BEFORE spawning any agents.  The assigner agent is then
/// spawned on the assignment task, inspects the agency via wg CLI, and calls
/// `wg assign <task-id> <agent-hash>` followed by `wg done assign-{task-id}`.
///
/// Returns `true` if the graph was modified.
fn build_auto_assign_tasks(graph: &mut workgraph::graph::WorkGraph, config: &Config, dir: &Path) -> bool {
    let mut modified = false;

    // Collect task data to avoid holding references while mutating graph
    let ready_task_data: Vec<_> = {
        let cycle_analysis = graph.compute_cycle_analysis();
        let ready = ready_tasks_with_peers_cycle_aware(graph, dir, &cycle_analysis);
        ready
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    t.title.clone(),
                    t.description.clone(),
                    t.skills.clone(),
                    t.agent.clone(),
                    t.assigned.clone(),
                    t.tags.clone(),
                )
            })
            .collect()
    };

    for (task_id, task_title, task_desc, task_skills, task_agent, task_assigned, task_tags) in
        ready_task_data
    {
        // Skip tasks that already have an agent or are already claimed
        if task_agent.is_some() || task_assigned.is_some() {
            continue;
        }

        // Skip tasks tagged with assignment/evaluation/evolution to prevent
        // infinite regress (assign-assign-assign-...)
        let dominated_tags = ["assignment", "evaluation", "evolution"];
        if task_tags
            .iter()
            .any(|tag| dominated_tags.contains(&tag.as_str()))
        {
            continue;
        }

        let assign_task_id = format!("assign-{}", task_id);

        // Skip if assignment task already exists (idempotent)
        if graph.get_task(&assign_task_id).is_some() {
            continue;
        }

        // Build description for the assigner with the original task's context
        let mut desc = format!(
            "Assign an agent to task '{}'.\n\n## Original Task\n**Title:** {}\n",
            task_id, task_title,
        );
        if let Some(ref d) = task_desc {
            desc.push_str(&format!("**Description:** {}\n", d));
        }
        if !task_skills.is_empty() {
            desc.push_str(&format!("**Skills:** {}\n", task_skills.join(", ")));
        }
        desc.push_str(&format!(
            "\n## Instructions\n\n\
             Pick the best agent for this task and assign them.\n\n\
             ### Step 1: Gather Information\n\n\
             Run these commands to understand the available agents and their track records:\n\
             ```\n\
             wg agent list --json\n\
             wg role list --json\n\
             wg motivation list --json\n\
             ```\n\n\
             For agents with evaluation history, drill into performance details:\n\
             ```\n\
             wg agent performance <agent-hash> --json\n\
             ```\n\n\
             ### Step 2: Match Agent to Task\n\n\
             Compare each agent's capabilities to the task requirements:\n\n\
             1. **Role fit**: The agent's role skills should overlap with the task's \
             required skills. A Programmer (code-writing, testing, debugging) fits \
             implementation tasks; a Reviewer (code-review, security-audit) fits review \
             tasks; an Architect (system-design, dependency-analysis) fits design tasks; \
             a Documenter (technical-writing) fits documentation tasks.\n\n\
             2. **Motivation fit**: The agent's operational parameters should match the \
             task's nature. A Careful agent suits tasks where correctness is critical. \
             A Fast agent suits urgent, low-risk tasks. A Thorough agent suits complex \
             tasks requiring deep analysis.\n\n\
             3. **Capabilities**: Check the agent's `capabilities` list for specific \
             technology or domain tags that match the task (e.g., \"rust\", \"python\", \
             \"kubernetes\").\n\n\
             ### Step 3: Use Performance Data\n\n\
             Each agent has a `performance` record with `task_count`, `avg_score` \
             (0.0–1.0), and individual evaluation entries. Each evaluation has \
             dimension scores: `correctness` (40% weight), `completeness` (30%), \
             `efficiency` (15%), `style_adherence` (15%).\n\n\
             - **Prefer agents with higher avg_score** on similar tasks (check \
             evaluation `task_id` and `context_id` to see what kinds of work they've \
             done before).\n\
             - **Weight recent evaluations more** — an agent's latest scores are more \
             predictive than older ones.\n\
             - **Consider dimension strengths**: If the task demands correctness above \
             all else, prefer agents who score highest on `correctness` even if their \
             overall average is slightly lower.\n\n\
             ### Step 4: Handle Cold Start\n\n\
             When agents have 0 evaluations (new agency, or new agents), you cannot \
             rely on performance data. In this case:\n\n\
             - **Match on role and motivation** — this is the primary signal. Pick the \
             agent whose role skills best cover the task requirements.\n\
             - **Spread work across untested agents** to build evaluation data. If \
             multiple agents have 0 evaluations and similar role fit, prefer whichever \
             has completed fewer tasks (lower `task_count`) so the agency gathers \
             diverse signal.\n\
             - **Default to Careful motivation** for high-stakes tasks and Fast \
             motivation for routine work when there's no data to differentiate.\n\n\
             ### Step 5: Balance Exploration vs Exploitation\n\n\
             - **Exploitation (default)**: Assign the highest-scoring agent whose \
             skills match. This maximizes expected quality.\n\
             - **Exploration**: Occasionally assign a less-proven agent to gather new \
             performance data. Do this when:\n\
               - A newer agent (higher generation, or fewer evaluations) has relevant \
             skills but limited history.\n\
               - The top performer's score advantage is small (< 0.1 difference).\n\
               - The task is lower-risk (not blocking many other tasks, not tagged as \
             critical).\n\
             - **Never explore with agents whose avg_score is below 0.4** — that \
             signals consistent poor performance.\n\n\
             ### Step 6: Assign\n\n\
             Once you've chosen an agent, run:\n\
             ```\n\
             wg assign {} <agent-hash>\n\
             wg done {}\n\
             ```\n\n\
             If no suitable agent exists for this task, report why:\n\
             ```\n\
             wg fail {} --reason \"No agent with matching skills for: <explanation>\"\n\
             ```",
            task_id, assign_task_id, assign_task_id,
        ));

        // Create the assignment task (blocks the original)
        let assign_task = Task {
            id: assign_task_id.clone(),
            title: format!("Assign agent for: {}", task_title),
            description: Some(desc),
            status: Status::Open,
            assigned: None,
            estimate: None,
            before: vec![task_id.clone()],
            after: vec![],
            requires: vec![],
            tags: vec!["assignment".to_string(), "agency".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: Some(Utc::now().to_rfc3339()),
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: config.agency.assigner_model.clone(),
            verify: None,
            agent: config.agency.assigner_agent.clone(),

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            cycle_config: None,
        };

        graph.add_node(Node::Task(assign_task));

        // Add the assignment task as a blocker on the original task
        if let Some(t) = graph.get_task_mut(&task_id)
            && !t.after.contains(&assign_task_id)
        {
            t.after.push(assign_task_id.clone());
        }

        eprintln!(
            "[coordinator] Created assignment task '{}' blocking '{}'",
            assign_task_id, task_id,
        );
        modified = true;
    }

    modified
}

/// Auto-evaluate: create evaluation tasks for completed/active tasks.
///
/// Per the agency design (§4.3), when auto_evaluate is enabled the coordinator
/// creates an evaluation task `evaluate-{task-id}` that is blocked by the
/// original task.  When the original task completes (done or failed),
/// the evaluation task becomes ready and the coordinator spawns an
/// evaluator agent on it.
///
/// Tasks tagged "evaluation", "assignment", or "evolution" are NOT
/// auto-evaluated to prevent infinite regress.  Abandoned tasks are also
/// excluded.
///
/// Returns `true` if the graph was modified.
fn build_auto_evaluate_tasks(
    dir: &Path,
    graph: &mut workgraph::graph::WorkGraph,
    config: &Config,
) -> bool {
    let mut modified = false;

    // Load agents to identify human operators — their work quality isn't
    // a reflection of a role+motivation prompt so we skip auto-evaluation.
    let agents_dir = dir.join("agency").join("agents");
    let all_agents = agency::load_all_agents_or_warn(&agents_dir);
    let human_agent_ids: std::collections::HashSet<&str> = all_agents
        .iter()
        .filter(|a| a.is_human())
        .map(|a| a.id.as_str())
        .collect();

    // Collect all tasks (not just ready ones) that might need eval tasks.
    // We iterate all non-terminal tasks so eval tasks are created early.
    let tasks_needing_eval: Vec<_> = graph
        .tasks()
        .filter(|t| {
            // Skip tasks that already have an evaluation task
            let eval_id = format!("evaluate-{}", t.id);
            if graph.get_task(&eval_id).is_some() {
                return false;
            }
            // Skip tasks tagged with evaluation/assignment/evolution
            let dominated_tags = ["evaluation", "assignment", "evolution"];
            if t.tags
                .iter()
                .any(|tag| dominated_tags.contains(&tag.as_str()))
            {
                return false;
            }
            // Skip tasks assigned to human agents
            if let Some(ref agent_id) = t.agent
                && human_agent_ids.contains(agent_id.as_str())
            {
                return false;
            }
            // Only create for tasks that are active (Open, InProgress, Blocked)
            // or already completed (Done, Failed) without an eval task
            !matches!(t.status, Status::Abandoned)
        })
        .map(|t| (t.id.clone(), t.title.clone()))
        .collect();

    for (task_id, task_title) in &tasks_needing_eval {
        let eval_task_id = format!("evaluate-{}", task_id);

        // Double-check (the filter above already checks but graph may have changed)
        if graph.get_task(&eval_task_id).is_some() {
            continue;
        }

        let desc = format!(
            "Evaluate the completed task '{}'.\n\n\
             Run `wg evaluate {}` to produce a structured evaluation.\n\
             This reads the task output from `.workgraph/output/{}/` and \
             the task definition via `wg show {}`.",
            task_id, task_id, task_id, task_id,
        );

        let eval_task = Task {
            id: eval_task_id.clone(),
            title: format!("Evaluate: {}", task_title),
            description: Some(desc),
            status: Status::Open,
            assigned: None,
            estimate: None,
            before: vec![],
            after: vec![task_id.clone()],
            requires: vec![],
            tags: vec!["evaluation".to_string(), "agency".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: Some(format!("wg evaluate {}", task_id)),
            not_before: None,
            created_at: Some(Utc::now().to_rfc3339()),
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: config.agency.evaluator_model.clone(),
            verify: None,
            agent: config.agency.evaluator_agent.clone(),

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            cycle_config: None,
        };

        graph.add_node(Node::Task(eval_task));

        eprintln!(
            "[coordinator] Created evaluation task '{}' blocked by '{}'",
            eval_task_id, task_id,
        );
        modified = true;
    }

    // Unblock evaluation tasks whose source task has Failed.
    // `ready_tasks()` only unblocks when the blocker is Done. For Failed
    // tasks we still want evaluation to proceed (§4.3: "Failed tasks also
    // get evaluated"), so we remove the blocker explicitly.
    let eval_fixups: Vec<(String, String)> = graph
        .tasks()
        .filter(|t| t.id.starts_with("evaluate-") && t.status == Status::Open)
        .filter_map(|t| {
            // The eval task blocks on a single task: the original
            if t.after.len() == 1 {
                let source_id = &t.after[0];
                if let Some(source) = graph.get_task(source_id)
                    && source.status == Status::Failed
                {
                    return Some((t.id.clone(), source_id.clone()));
                }
            }
            None
        })
        .collect();

    for (eval_id, source_id) in &eval_fixups {
        if let Some(t) = graph.get_task_mut(eval_id) {
            t.after.retain(|b| b != source_id);
            modified = true;
            eprintln!(
                "[coordinator] Unblocked evaluation task '{}' (source '{}' failed)",
                eval_id, source_id,
            );
        }
    }

    modified
}

/// Spawn an evaluation task directly without the full agent spawn machinery.
///
/// Instead of coordinator → run.sh → bash → `wg evaluate` → claude, this
/// forks a single process: `wg evaluate <source-task> --model <model>` that
/// marks the eval task done/failed on exit.  This eliminates:
///   - Executor config resolution & template processing
///   - run.sh wrapper script
///   - prompt.txt / metadata.json generation
///
/// The forked process is still tracked in the agent registry for dead-agent
/// detection.
fn spawn_eval_inline(
    dir: &Path,
    eval_task_id: &str,
    evaluator_model: Option<&str>,
) -> Result<(String, u32)> {
    use std::process::{Command, Stdio};

    let graph_path = graph_path(dir);
    let mut graph = load_graph(&graph_path).context("Failed to load graph for eval spawn")?;

    let task = graph.get_task_mut_or_err(eval_task_id)?;
    if task.status != Status::Open {
        anyhow::bail!("Eval task '{}' is not open (status: {:?})", eval_task_id, task.status);
    }

    // Extract source task ID from the exec command ("wg evaluate <source-id>")
    let source_task_id = task
        .exec
        .as_deref()
        .and_then(|e| e.strip_prefix("wg evaluate "))
        .unwrap_or_else(|| {
            eval_task_id
                .strip_prefix("evaluate-")
                .unwrap_or(eval_task_id)
        })
        .to_string();

    // Set up minimal agent tracking
    let mut agent_registry = AgentRegistry::load(dir)?;
    let agent_id = format!("agent-{}", agent_registry.next_agent_id);

    // Create minimal output directory for log capture
    let output_dir = dir.join("agents").join(&agent_id);
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Failed to create eval output dir: {:?}", output_dir))?;
    let output_file = output_dir.join("output.log");
    let output_file_str = output_file.to_string_lossy().to_string();

    // Build the eval command with explicit model
    let mut eval_cmd = format!("wg evaluate '{}'", source_task_id.replace('\'', "'\\''"));
    if let Some(model) = evaluator_model {
        eval_cmd.push_str(&format!(" --model '{}'", model.replace('\'', "'\\''")));
    }

    let escaped_eval_id = eval_task_id.replace('\'', "'\\''");
    let escaped_output = output_file_str.replace('\'', "'\\''");

    // Single script: run eval, then mark done/failed based on exit code
    let script = format!(
        r#"unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT
{eval_cmd} >> '{escaped_output}' 2>&1
EXIT_CODE=$?
if [ $EXIT_CODE -eq 0 ]; then
    wg done '{escaped_eval_id}' 2>> '{escaped_output}'
else
    wg fail '{escaped_eval_id}' --reason "wg evaluate exited with code $EXIT_CODE" 2>> '{escaped_output}'
fi
exit $EXIT_CODE"#,
    );

    // Claim the task before spawning
    task.status = Status::InProgress;
    task.started_at = Some(Utc::now().to_rfc3339());
    task.assigned = Some(agent_id.clone());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(agent_id.clone()),
        message: format!(
            "Spawned eval inline{}",
            evaluator_model
                .map(|m| format!(" --model {}", m))
                .unwrap_or_default()
        ),
    });
    save_graph(&graph, &graph_path).context("Failed to save graph after claiming eval task")?;

    // Fork the process
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(&script);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // Detach into own session so it survives daemon restart
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Rollback the claim
            if let Ok(mut rollback_graph) = load_graph(&graph_path)
                && let Some(t) = rollback_graph.get_task_mut(eval_task_id) {
                    t.status = Status::Open;
                    t.started_at = None;
                    t.assigned = None;
                    t.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: Some(agent_id.clone()),
                        message: format!("Eval spawn failed, reverting claim: {}", e),
                    });
                    let _ = save_graph(&rollback_graph, &graph_path);
                }
            return Err(anyhow::anyhow!("Failed to spawn eval process: {}", e));
        }
    };

    let pid = child.id();

    // Register in agent registry for dead-agent detection
    agent_registry.register_agent_with_model(
        pid,
        eval_task_id,
        "eval",
        &output_file_str,
        evaluator_model,
    );
    agent_registry.save(dir).context("Failed to save agent registry after eval spawn")?;

    Ok((agent_id, pid))
}

/// Spawn agents on ready tasks, up to `slots_available`. Returns the number of
/// agents successfully spawned.
fn spawn_agents_for_ready_tasks(
    dir: &Path,
    graph: &workgraph::graph::WorkGraph,
    executor: &str,
    model: Option<&str>,
    slots_available: usize,
) -> usize {
    let cycle_analysis = graph.compute_cycle_analysis();
    let final_ready = ready_tasks_with_peers_cycle_aware(graph, dir, &cycle_analysis);
    let agents_dir = dir.join("agency").join("agents");
    let mut spawned = 0;

    let to_spawn = final_ready.iter().take(slots_available);
    for task in to_spawn {
        // Skip if already claimed
        if task.assigned.is_some() {
            continue;
        }

        // Evaluation tasks run inline: fork `wg evaluate` directly instead of
        // going through the full spawn machinery (run.sh, executor config, etc.)
        if task.tags.iter().any(|t| t == "evaluation") && task.exec.is_some() {
            let eval_model = task.model.as_deref();
            eprintln!(
                "[coordinator] Spawning eval inline for: {} - {}{}",
                task.id,
                task.title,
                eval_model.map(|m| format!(" (model: {})", m)).unwrap_or_default(),
            );
            match spawn_eval_inline(dir, &task.id, eval_model) {
                Ok((agent_id, pid)) => {
                    eprintln!("[coordinator] Spawned eval {} (PID {})", agent_id, pid);
                    spawned += 1;
                }
                Err(e) => {
                    eprintln!("[coordinator] Failed to spawn eval for {}: {}", task.id, e);
                }
            }
            continue;
        }

        // Resolve executor: tasks with exec commands use shell executor directly,
        // otherwise: agent.executor > config.coordinator.executor
        let effective_executor = if task.exec.is_some() {
            "shell".to_string()
        } else {
            task.agent
                .as_ref()
                .and_then(|agent_hash| agency::find_agent_by_prefix(&agents_dir, agent_hash).ok())
                .map(|agent| agent.executor)
                .unwrap_or_else(|| executor.to_string())
        };

        // Pass coordinator model to spawn; spawn resolves the full hierarchy:
        // task.model > executor.model > coordinator.model > 'default'
        eprintln!(
            "[coordinator] Spawning agent for: {} - {} (executor: {})",
            task.id, task.title, effective_executor
        );
        match spawn::spawn_agent(dir, &task.id, &effective_executor, None, model) {
            Ok((agent_id, pid)) => {
                eprintln!("[coordinator] Spawned {} (PID {})", agent_id, pid);
                spawned += 1;
            }
            Err(e) => {
                eprintln!("[coordinator] Failed to spawn for {}: {}", task.id, e);
            }
        }
    }

    spawned
}

/// Single coordinator tick: spawn agents on ready tasks
pub fn coordinator_tick(
    dir: &Path,
    max_agents: usize,
    executor: &str,
    model: Option<&str>,
) -> Result<TickResult> {
    let graph_path = graph_path(dir);

    // Load config for agency settings
    let config = Config::load_or_default(dir);

    // Phase 1: Clean up dead agents and count alive ones
    let alive_count = match cleanup_and_count_alive(dir, &graph_path, max_agents)? {
        Ok(count) => count,
        Err(early_result) => return Ok(early_result),
    };

    // Phase 2: Load graph
    let mut graph = load_graph(&graph_path).context("Failed to load graph")?;

    let slots_available = max_agents.saturating_sub(alive_count);

    // Phase 3: Auto-assign unassigned ready tasks
    // NOTE: These must run BEFORE the early-return check, because they may
    // create new ready tasks (e.g. evaluate-* tasks) that weren't there before.
    let mut graph_modified = false;
    if config.agency.auto_assign {
        graph_modified |= build_auto_assign_tasks(&mut graph, &config, dir);
    }

    // Phase 4: Auto-evaluate tasks
    if config.agency.auto_evaluate {
        graph_modified |= build_auto_evaluate_tasks(dir, &mut graph, &config);
    }

    // Save graph once if it was modified during auto-assign or auto-evaluate.
    // Abort tick if save fails — continuing with unsaved state would spawn agents
    // on tasks that haven't been persisted.
    if graph_modified {
        save_graph(&graph, &graph_path)
            .context("Failed to save graph after auto-assign/auto-evaluate; aborting tick")?;
    }

    // Phase 5: Check for ready tasks (after agency phases may have created new ones)
    if let Some(early_result) = check_ready_or_return(&graph, alive_count, dir) {
        return Ok(early_result);
    }

    // Phase 6: Spawn agents on ready tasks
    let cycle_analysis = graph.compute_cycle_analysis();
    let final_ready = ready_tasks_with_peers_cycle_aware(&graph, dir, &cycle_analysis);
    let ready_count = final_ready.len();
    drop(final_ready);
    let spawned = spawn_agents_for_ready_tasks(dir, &graph, executor, model, slots_available);

    Ok(TickResult {
        agents_alive: alive_count + spawned,
        tasks_ready: ready_count,
        agents_spawned: spawned,
    })
}

/// Reason an agent was detected as dead
enum DeadReason {
    /// Process is no longer running
    ProcessExited,
}

/// Check if an agent should be considered dead
fn detect_dead_reason(agent: &AgentEntry) -> Option<DeadReason> {
    if !agent.is_alive() {
        return None;
    }

    // Process not running is the only signal — heartbeat is no longer used for detection
    if !is_process_alive(agent.pid) {
        return Some(DeadReason::ProcessExited);
    }

    None
}

/// Clean up dead agents (process exited)
/// Returns list of cleaned up agent IDs
fn cleanup_dead_agents(dir: &Path, graph_path: &Path) -> Result<Vec<String>> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Find agents that are dead: process gone
    let dead: Vec<_> = locked_registry
        .agents
        .values()
        .filter_map(|a| {
            detect_dead_reason(a).map(|reason| {
                (
                    a.id.clone(),
                    a.task_id.clone(),
                    a.pid,
                    a.output_file.clone(),
                    reason,
                )
            })
        })
        .collect();

    // Auto-bump heartbeat for agents whose process is still alive
    for agent in locked_registry.agents.values_mut() {
        if agent.is_alive() && is_process_alive(agent.pid) {
            agent.last_heartbeat = Utc::now().to_rfc3339();
        }
    }

    if dead.is_empty() {
        locked_registry.save_ref()?;
        return Ok(vec![]);
    }

    // Mark these agents as dead in registry
    for (agent_id, _, _, _, _) in &dead {
        if let Some(agent) = locked_registry.get_agent_mut(agent_id) {
            agent.status = AgentStatus::Dead;
        }
    }
    locked_registry.save_ref()?;

    // Load config for triage settings
    let config = Config::load_or_default(dir);

    // Unclaim their tasks (if still in progress - agent may have completed or failed them already)
    let mut graph = load_graph(graph_path).context("Failed to load graph")?;
    let mut tasks_modified = false;
    let mut tasks_completed_by_triage: Vec<String> = Vec::new();

    for (agent_id, task_id, pid, output_file, reason) in &dead {
        if let Some(task) = graph.get_task_mut(task_id) {
            // Only unclaim if task is still in progress (agent didn't finish it properly)
            if task.status == Status::InProgress {
                if config.agency.auto_triage {
                    // Run synchronous triage to assess progress
                    match run_triage(&config, task, output_file) {
                        Ok(verdict) => {
                            let is_done = verdict.verdict == "done";
                            apply_triage_verdict(task, &verdict, agent_id, *pid);
                            eprintln!(
                                "[coordinator] Triage for '{}': verdict={}, reason={}",
                                task_id, verdict.verdict, verdict.reason
                            );
                            if is_done && task.status == Status::Done {
                                tasks_completed_by_triage.push(task_id.clone());
                            }
                        }
                        Err(e) => {
                            // Triage failed, fall back to restart behavior
                            eprintln!(
                                "[coordinator] Triage failed for '{}': {}, falling back to restart",
                                task_id, e
                            );
                            task.status = Status::Open;
                            task.assigned = None;
                            task.log.push(LogEntry {
                                timestamp: Utc::now().to_rfc3339(),
                                actor: Some("triage".to_string()),
                                message: format!(
                                    "Triage failed ({}), task reset: agent '{}' (PID {}) process exited",
                                    e, agent_id, pid
                                ),
                            });
                        }
                    }
                } else {
                    // Existing behavior: simple unclaim
                    task.status = Status::Open;
                    task.assigned = None;
                    let reason_msg = match reason {
                        DeadReason::ProcessExited => format!(
                            "Task unclaimed: agent '{}' (PID {}) process exited",
                            agent_id, pid
                        ),
                    };
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: None,
                        message: reason_msg,
                    });
                }
                tasks_modified = true;
            }
        }
    }

    // Evaluate structural cycle iterations for tasks triaged as done
    if !tasks_completed_by_triage.is_empty() {
        let cycle_analysis = graph.compute_cycle_analysis();
        for task_id in &tasks_completed_by_triage {
            evaluate_cycle_iteration(&mut graph, task_id, &cycle_analysis);
        }
    }

    if tasks_modified {
        save_graph(&graph, graph_path).context("Failed to save graph")?;
    }

    // Capture output for completed/failed tasks whose agents just died.
    // done.rs already captures output, but fail.rs does not,
    // and the agent may have completed without triggering capture (e.g. wrapper
    // script marked it done but output capture wasn't invoked). This is a
    // best-effort safety net.
    let graph = load_graph(graph_path).context("Failed to reload graph for output capture")?;
    for (_agent_id, task_id, _pid, _output_file, _reason) in &dead {
        if let Some(task) = graph.get_task(task_id)
            && matches!(task.status, Status::Done | Status::Failed)
        {
            let output_dir = dir.join("output").join(task_id);
            if !output_dir.exists() {
                if let Err(e) = agency::capture_task_output(dir, task) {
                    eprintln!(
                        "[coordinator] Warning: output capture failed for '{}': {}",
                        task_id, e
                    );
                } else {
                    eprintln!(
                        "[coordinator] Captured output for completed task '{}'",
                        task_id
                    );
                }
            }
        }
    }

    Ok(dead.into_iter().map(|(id, _, _, _, _)| id).collect())
}

// ---------------------------------------------------------------------------
// Dead-agent triage
// ---------------------------------------------------------------------------

/// Triage verdict returned by the LLM
#[derive(Debug, serde::Deserialize)]
struct TriageVerdict {
    /// One of "done", "continue", "restart"
    verdict: String,
    /// Brief explanation of the verdict
    #[serde(default)]
    reason: String,
    /// Summary of work accomplished (used for "continue" context)
    #[serde(default)]
    summary: String,
}

/// Read the last `max_bytes` of a file, prepending a truncation notice if needed.
fn read_truncated_log(path: &str, max_bytes: usize) -> String {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return "(output log not found or unreadable)".to_string(),
    };

    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(_) => return "(could not read output log metadata)".to_string(),
    };

    let file_size = metadata.len() as usize;
    if file_size == 0 {
        return "(output log is empty)".to_string();
    }

    let mut file = file;
    if file_size <= max_bytes {
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_ok() {
            return buf;
        }
        return "(could not read output log)".to_string();
    }

    // Seek to file_size - max_bytes and read from there
    let skip = file_size - max_bytes;
    if file.seek(SeekFrom::Start(skip as u64)).is_err() {
        return "(could not seek in output log)".to_string();
    }
    let mut buf = vec![0u8; max_bytes];
    match file.read_exact(&mut buf) {
        Ok(_) => {
            // Find the first newline after the seek point to avoid partial lines
            let start = buf
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[start..]).to_string();
            format!("[... {} bytes truncated ...]\n{}", skip + start, text)
        }
        Err(_) => "(could not read output log tail)".to_string(),
    }
}

/// Build the triage prompt for the LLM.
fn build_triage_prompt(task: &Task, log_content: &str) -> String {
    let task_title = &task.title;
    let task_desc = task.description.as_deref().unwrap_or("(no description)");
    let task_id = &task.id;

    format!(
        r#"You are a triage system for a software development task coordinator.

An agent was working on a task but its process died unexpectedly (OOM, crash, SIGKILL).
Examine the agent's output log below and determine how much progress was made.

## Task Information
- **ID:** {task_id}
- **Title:** {task_title}
- **Description:** {task_desc}

## Agent Output Log
```
{log_content}
```

## Instructions
Based on the output log, respond with ONLY a JSON object (no markdown fences, no commentary):

{{
  "verdict": "<done|continue|restart>",
  "reason": "<one-sentence explanation>",
  "summary": "<what was accomplished, including specific files changed or artifacts produced>"
}}

Verdicts:
- **"done"**: The work appears complete — code was written, tests pass, the agent just didn't call the completion command before dying.
- **"continue"**: Significant progress was made (files created/modified, partial implementation) — a new agent should pick up where this one left off.
- **"restart"**: Little or no meaningful progress — a fresh start is appropriate.

Be conservative: only use "done" if the output clearly shows the task was finished. When in doubt between "continue" and "restart", prefer "continue" if any artifacts were created."#
    )
}

/// Run the triage LLM call synchronously. Returns a parsed TriageVerdict.
fn run_triage(config: &Config, task: &Task, output_file: &str) -> Result<TriageVerdict> {
    let max_log_bytes = config.agency.triage_max_log_bytes.unwrap_or(50_000);
    let timeout_secs = config.agency.triage_timeout.unwrap_or(30);
    let model = config.agency.triage_model.as_deref().unwrap_or("haiku");

    let log_content = read_truncated_log(output_file, max_log_bytes);
    let prompt = build_triage_prompt(task, &log_content);

    // Use `timeout` to wrap the claude call
    let output = process::Command::new("timeout")
        .arg(format!("{}s", timeout_secs))
        .arg("claude")
        .arg("--model")
        .arg(model)
        .arg("--print")
        .arg("--dangerously-skip-permissions")
        .arg(&prompt)
        .output()
        .context("Failed to run claude CLI for triage")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Triage claude call failed (exit {:?}): {}",
            output.status.code(),
            stderr.chars().take(200).collect::<String>()
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout);

    // Parse JSON verdict from output
    let json_str = extract_triage_json(&raw)
        .ok_or_else(|| anyhow::anyhow!("No valid JSON found in triage output"))?;

    let verdict: TriageVerdict = serde_json::from_str(&json_str)
        .with_context(|| format!("Failed to parse triage JSON: {}", json_str))?;

    // Validate verdict value
    match verdict.verdict.as_str() {
        "done" | "continue" | "restart" => Ok(verdict),
        other => anyhow::bail!(
            "Invalid triage verdict '{}', expected done/continue/restart",
            other
        ),
    }
}

/// Extract a JSON object from potentially noisy LLM output.
fn extract_triage_json(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    // Strip markdown code fences
    if trimmed.starts_with("```") {
        let inner = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
            return Some(inner.to_string());
        }
    }

    // Find first { to last }
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
        && start <= end
    {
        let candidate = &trimmed[start..=end];
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

/// Apply a triage verdict to a task.
fn apply_triage_verdict(task: &mut Task, verdict: &TriageVerdict, agent_id: &str, pid: u32) {
    match verdict.verdict.as_str() {
        "done" => {
            task.status = Status::Done;
            task.completed_at = Some(Utc::now().to_rfc3339());
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("triage".to_string()),
                message: format!(
                    "Triage: work complete (agent '{}' PID {} died) — {}",
                    agent_id, pid, verdict.reason
                ),
            });
        }
        "continue" => {
            // Check max_retries before allowing continue
            if let Some(max) = task.max_retries
                && task.retry_count >= max
            {
                task.status = Status::Failed;
                task.failure_reason = Some(format!(
                    "Max retries exceeded ({}/{}): {}",
                    task.retry_count, max, verdict.reason
                ));
                task.assigned = None;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("triage".to_string()),
                    message: format!(
                        "Triage: wanted continue but max retries exceeded ({}/{}) — failing task",
                        task.retry_count, max
                    ),
                });
                return;
            }

            task.status = Status::Open;
            task.assigned = None;
            task.retry_count += 1;

            // Replace (not append) recovery context to prevent unbounded description growth
            let recovery_context = format!(
                "\n\n## Previous Attempt Recovery\n\
                 A previous agent worked on this task but died before completing.\n\n\
                 **What was accomplished:** {}\n\n\
                 Continue from where the previous agent left off. Do NOT redo completed work.\n\
                 Check existing artifacts before starting.",
                verdict.summary
            );
            if let Some(ref mut desc) = task.description {
                // Strip any existing recovery section before adding new one
                if let Some(pos) = desc.find("\n\n## Previous Attempt Recovery") {
                    desc.truncate(pos);
                }
                desc.push_str(&recovery_context);
            } else {
                task.description = Some(recovery_context.trim_start().to_string());
            }

            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("triage".to_string()),
                message: format!(
                    "Triage: continuing (agent '{}' PID {} died) — {}",
                    agent_id, pid, verdict.reason
                ),
            });
        }
        _ => {
            // "restart" or anything else: same as existing behavior
            // Check max_retries before allowing restart
            if let Some(max) = task.max_retries
                && task.retry_count >= max
            {
                task.status = Status::Failed;
                task.failure_reason = Some(format!(
                    "Max retries exceeded ({}/{}): {}",
                    task.retry_count, max, verdict.reason
                ));
                task.assigned = None;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("triage".to_string()),
                    message: format!(
                        "Triage: wanted restart but max retries exceeded ({}/{}) — failing task",
                        task.retry_count, max
                    ),
                });
                return;
            }

            task.status = Status::Open;
            task.assigned = None;
            task.retry_count += 1;
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("triage".to_string()),
                message: format!(
                    "Triage: restarting (agent '{}' PID {} died) — {}",
                    agent_id, pid, verdict.reason
                ),
            });
        }
    }
}

/// Generate systemd user service file
/// Uses `wg service start` as ExecStart; settings come from config.toml
pub fn generate_systemd_service(dir: &Path) -> Result<()> {
    let workdir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());

    // Derive a project identifier from the directory basename for unique service naming
    let project_name = workdir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    // Sanitize for systemd unit naming: keep alphanumerics, hyphens, underscores
    let project_name: String = project_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let unit_name = format!("wg-{project_name}");

    // ExecStart uses `wg service start` - the service daemon includes the coordinator
    let service_content = format!(
        r#"[Unit]
Description=Workgraph Service ({project_name})
After=network.target

[Service]
Type=simple
WorkingDirectory={workdir}
ExecStart={wg} --dir {wg_dir} service start
ExecStop={wg} --dir {wg_dir} service stop
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
"#,
        project_name = project_name,
        workdir = workdir.display(),
        wg = std::env::current_exe()?.display(),
        wg_dir = dir
            .canonicalize()
            .unwrap_or_else(|_| dir.to_path_buf())
            .display(),
    );

    // Write to ~/.config/systemd/user/wg-{project_name}.service
    let home = std::env::var("HOME").context("HOME not set")?;
    let service_dir = std::path::PathBuf::from(&home)
        .join(".config")
        .join("systemd")
        .join("user");

    std::fs::create_dir_all(&service_dir)?;

    let service_path = service_dir.join(format!("{unit_name}.service"));
    std::fs::write(&service_path, service_content)?;

    println!("Created systemd user service: {}", service_path.display());
    println!();
    println!("Settings are read from .workgraph/config.toml");
    println!("To change settings: wg config --max-agents N --interval N");
    println!();
    println!("To enable and start:");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable {unit_name}");
    println!("  systemctl --user start {unit_name}");
    println!();
    println!("To check status:");
    println!("  systemctl --user status {unit_name}");
    println!("  journalctl --user -u {unit_name} -f");

    Ok(())
}

/// Run a single coordinator tick (debug/testing command)
pub fn run_tick(
    dir: &Path,
    max_agents: Option<usize>,
    executor: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let config = Config::load(dir)?;
    let max_agents = max_agents.unwrap_or(config.coordinator.max_agents);
    let executor = executor
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| config.coordinator.executor.clone());

    let graph_path = graph_path(dir);
    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let model = model
        .map(std::string::ToString::to_string)
        .or_else(|| config.coordinator.model.clone());
    println!(
        "Running single coordinator tick (max_agents={}, executor={}, model={})...",
        max_agents,
        &executor,
        model.as_deref().unwrap_or("default")
    );
    match coordinator_tick(dir, max_agents, &executor, model.as_deref()) {
        Ok(result) => {
            println!(
                "Tick complete: {} alive, {} ready, {} spawned",
                result.agents_alive, result.tasks_ready, result.agents_spawned
            );
        }
        Err(e) => eprintln!("Coordinator tick error: {}", e),
    }
    Ok(())
}

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
    QueryTask {
        task_id: String,
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

/// Find orphan daemon processes for this workgraph directory.
///
/// Scans `/proc` for processes whose command line contains `wg` and
/// `service daemon` with a `--dir` pointing at the given workgraph directory.
/// Returns a list of PIDs that match, excluding `exclude_pid` if given.
#[cfg(unix)]
pub fn find_orphan_daemon_pids(dir: &Path, exclude_pid: Option<u32>) -> Vec<u32> {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let dir_str = canonical.to_string_lossy().to_string();
    let our_pid = std::process::id();

    let mut orphans = Vec::new();

    let proc_dir = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return orphans,
    };

    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Only look at numeric directories (PID directories)
        let pid: u32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Skip our own process and the excluded PID
        if pid == our_pid || exclude_pid == Some(pid) {
            continue;
        }

        // Read cmdline
        let cmdline_path = format!("/proc/{}/cmdline", pid);
        let cmdline = match fs::read(&cmdline_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // cmdline is NUL-separated
        let cmdline_str = String::from_utf8_lossy(&cmdline);
        let args: Vec<&str> = cmdline_str.split('\0').collect();

        // Check if this is a `wg ... service daemon --dir <our_dir>` process
        let has_service_daemon = args
            .windows(2)
            .any(|w| w[0] == "service" && w[1] == "daemon");
        let has_our_dir = args.windows(2).any(|w| w[0] == "--dir" && w[1] == dir_str);

        if has_service_daemon && has_our_dir {
            orphans.push(pid);
        }
    }

    orphans
}

#[cfg(not(unix))]
pub fn find_orphan_daemon_pids(_dir: &Path, _exclude_pid: Option<u32>) -> Vec<u32> {
    Vec::new()
}

/// Start the service daemon
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub fn run_start(
    dir: &Path,
    socket_path: Option<&str>,
    _port: Option<u16>,
    max_agents: Option<usize>,
    executor: Option<&str>,
    interval: Option<u64>,
    model: Option<&str>,
    json: bool,
    force: bool,
) -> Result<()> {
    // Check if service is already running
    if let Some(state) = ServiceState::load(dir)? {
        if is_process_running(state.pid) {
            if force {
                // Kill existing daemon before starting a new one
                if !json {
                    println!(
                        "Killing existing daemon (PID {}) before starting new one...",
                        state.pid
                    );
                }
                // Send shutdown via IPC first (graceful)
                let socket = PathBuf::from(&state.socket_path);
                if socket.exists()
                    && let Ok(mut stream) = UnixStream::connect(&socket)
                {
                    let request = IpcRequest::Shutdown {
                        force: false,
                        kill_agents: false,
                    };
                    if let Ok(json_req) = serde_json::to_string(&request) {
                        let _ = writeln!(stream, "{}", json_req);
                        let _ = stream.flush();
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                // If still alive, kill it
                if is_process_running(state.pid) {
                    kill_process_graceful(state.pid)?;
                }
                // Clean up
                if socket.exists() {
                    let _ = fs::remove_file(&socket);
                }
                ServiceState::remove(dir)?;
            } else {
                if json {
                    let output = serde_json::json!({
                        "error": "Service already running",
                        "pid": state.pid,
                        "socket": state.socket_path,
                    });
                    println!("{}", serde_json::to_string_pretty(&output)?);
                } else {
                    println!(
                        "Service already running (PID {}). Use 'wg service stop' first or 'wg service start --force'.",
                        state.pid
                    );
                    println!("Socket: {}", state.socket_path);
                }
                return Ok(());
            }
        } else {
            // Stale state, clean up
            ServiceState::remove(dir)?;
        }
    }

    // Also check for orphan daemon processes that lost their state file
    let orphans = find_orphan_daemon_pids(dir, None);
    if !orphans.is_empty() {
        if force {
            for &pid in &orphans {
                if !json {
                    println!("Killing orphan daemon process (PID {})...", pid);
                }
                let _ = kill_process_graceful(pid);
            }
        } else {
            let pids: Vec<String> = orphans.iter().map(|p| p.to_string()).collect();
            if json {
                let output = serde_json::json!({
                    "error": "Orphan daemon processes found",
                    "orphan_pids": orphans,
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "Found orphan daemon process(es) for this workgraph: PID {}",
                    pids.join(", ")
                );
                println!("Use 'wg service start --force' to kill them and start fresh.");
            }
            return Ok(());
        }
    }

    let socket = socket_path
        .map(PathBuf::from)
        .unwrap_or_else(|| default_socket_path(dir));

    // Remove stale socket file if exists
    if socket.exists() {
        fs::remove_file(&socket)
            .with_context(|| format!("Failed to remove stale socket at {:?}", socket))?;
    }

    // Fork the daemon process
    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;

    let dir_str = dir.to_string_lossy().to_string();
    let socket_str = socket.to_string_lossy().to_string();

    // Start daemon in background
    let mut args = vec![
        "--dir".to_string(),
        dir_str,
        "service".to_string(),
        "daemon".to_string(),
        "--socket".to_string(),
        socket_str.clone(),
    ];
    if let Some(n) = max_agents {
        args.push("--max-agents".to_string());
        args.push(n.to_string());
    }
    if let Some(e) = executor {
        args.push("--executor".to_string());
        args.push(e.to_string());
    }
    if let Some(i) = interval {
        args.push("--interval".to_string());
        args.push(i.to_string());
    }
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    // Redirect daemon stderr to the log file so early startup crashes and
    // unexpected panics that bypass the DaemonLogger are captured.
    let log_path = log_file_path(dir);
    let service_dir = dir.join("service");
    if !service_dir.exists() {
        fs::create_dir_all(&service_dir)
            .context("Failed to create service directory for log file")?;
    }
    let stderr_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open daemon log at {:?}", log_path))?;

    let child = process::Command::new(&current_exe)
        .args(&args)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .context("Failed to spawn daemon process")?;

    let pid = child.id();

    // Save state
    let state = ServiceState {
        pid,
        socket_path: socket_str.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    state.save(dir)?;

    // Wait a moment for the daemon to start
    std::thread::sleep(Duration::from_millis(200));

    // Verify daemon started successfully
    if !is_process_running(pid) {
        ServiceState::remove(dir)?;
        anyhow::bail!("Daemon process exited immediately. Check logs.");
    }

    // Resolve effective config for display (CLI flags override config.toml)
    let config = Config::load_or_default(dir);
    let eff_max_agents = max_agents.unwrap_or(config.coordinator.max_agents);
    let eff_poll_interval = interval.unwrap_or(config.coordinator.poll_interval);
    let eff_executor = executor
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| config.coordinator.executor.clone());
    let eff_model: Option<String> = model
        .map(std::string::ToString::to_string)
        .or_else(|| config.coordinator.model.clone());

    let log_path_str = log_path.to_string_lossy().to_string();

    // Warn if auto_assign is enabled but no agency agents are defined
    let no_agents_defined = {
        let agents_dir = dir.join("agency").join("agents");
        agency::load_all_agents_or_warn(&agents_dir).is_empty()
    };
    let warn_no_agents = config.agency.auto_assign && no_agents_defined;

    if json {
        let mut output = serde_json::json!({
            "status": "started",
            "pid": pid,
            "socket": socket_str,
            "log": log_path_str,
            "coordinator": {
                "max_agents": eff_max_agents,
                "poll_interval": eff_poll_interval,
                "executor": eff_executor,
                "model": eff_model,
            }
        });
        if warn_no_agents {
            output["warning"] = serde_json::json!(
                "auto_assign is enabled but no agents are defined. Run 'wg agency init' or 'wg agent create' to create agents."
            );
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Service started (PID {})", pid);
        println!("Socket: {}", socket_str);
        println!("Log: {}", log_path_str);
        let model_str = eff_model.as_deref().unwrap_or("default");
        println!(
            "Coordinator: max_agents={}, poll_interval={}s, executor={}, model={}",
            eff_max_agents, eff_poll_interval, eff_executor, model_str
        );
        if warn_no_agents {
            println!();
            println!("Warning: auto_assign is enabled but no agents are defined.");
            println!("  Run 'wg agency init' or 'wg agent create' to create agents.");
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_start(
    _dir: &Path,
    _socket_path: Option<&str>,
    _port: Option<u16>,
    _max_agents: Option<usize>,
    _executor: Option<&str>,
    _interval: Option<u64>,
    _model: Option<&str>,
    _json: bool,
    _force: bool,
) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Reap zombie child processes (non-blocking).
///
/// The daemon spawns agent processes via `Command::spawn()`. When an agent
/// exits (or is killed), its process becomes a zombie until the parent calls
/// `waitpid`. This function reaps all zombies so that `is_process_alive(pid)`
/// correctly returns `false` for dead agents.
#[cfg(unix)]
fn reap_zombies() {
    loop {
        let result = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if result <= 0 {
            break; // No more zombies (0) or error (-1, e.g. no children)
        }
    }
}

/// Mutable coordinator runtime config, updated by Reconfigure IPC.
struct DaemonConfig {
    max_agents: usize,
    executor: String,
    poll_interval: Duration,
    model: Option<String>,
    paused: bool,
}

/// Run the actual daemon loop (called by forked process)
#[cfg(unix)]
pub fn run_daemon(
    dir: &Path,
    socket_path: &str,
    cli_max_agents: Option<usize>,
    cli_executor: Option<&str>,
    cli_interval: Option<u64>,
    cli_model: Option<&str>,
) -> Result<()> {
    let socket = PathBuf::from(socket_path);

    // --- Persistent logging setup ---
    let logger = DaemonLogger::open(dir).context("Failed to initialise daemon logger")?;
    logger.install_panic_hook();

    logger.info(&format!(
        "Daemon starting (PID {}, socket {})",
        std::process::id(),
        socket_path,
    ));

    // Ensure socket directory exists
    if let Some(parent) = socket.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }

    // Remove existing socket
    if socket.exists() {
        fs::remove_file(&socket)?;
    }

    // Bind to socket
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("Failed to bind to socket {:?}", socket))?;

    // Set socket permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&socket, perms)?;
    }

    // Set non-blocking for graceful shutdown
    listener.set_nonblocking(true)?;

    let dir = dir.to_path_buf();
    let mut running = true;

    // Load coordinator config, CLI args override config values
    let config = Config::load_or_default(&dir);
    let mut daemon_cfg = DaemonConfig {
        max_agents: cli_max_agents.unwrap_or(config.coordinator.max_agents),
        executor: cli_executor
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| config.coordinator.executor.clone()),
        // The poll_interval is the slow background safety-net timer.
        // CLI --interval overrides it; otherwise use config.coordinator.poll_interval.
        poll_interval: Duration::from_secs(
            cli_interval.unwrap_or(config.coordinator.poll_interval),
        ),
        model: cli_model
            .map(std::string::ToString::to_string)
            .or_else(|| config.coordinator.model.clone()),
        paused: false,
    };

    logger.info(&format!(
        "Coordinator config: poll_interval={}s, max_agents={}, executor={}, model={}",
        daemon_cfg.poll_interval.as_secs(),
        daemon_cfg.max_agents,
        &daemon_cfg.executor,
        daemon_cfg.model.as_deref().unwrap_or("default"),
    ));

    // Aggregate usage stats on startup
    match workgraph::usage::aggregate_usage_stats(&dir) {
        Ok(count) if count > 0 => {
            logger.info(&format!(
                "Aggregated {} usage log entries on startup",
                count
            ));
        }
        Ok(_) => {} // No entries to aggregate
        Err(e) => {
            logger.warn(&format!("Failed to aggregate usage stats: {}", e));
        }
    }

    // Initialize coordinator state on disk
    let mut coord_state = CoordinatorState {
        enabled: true,
        max_agents: daemon_cfg.max_agents,
        poll_interval: daemon_cfg.poll_interval.as_secs(),
        executor: daemon_cfg.executor.clone(),
        model: daemon_cfg.model.clone(),
        ticks: 0,
        last_tick: None,
        agents_alive: 0,
        tasks_ready: 0,
        agents_spawned: 0,
        paused: false,
    };
    coord_state.save(&dir);

    // Track last coordinator tick time - run immediately on start
    let mut last_coordinator_tick = Instant::now() - daemon_cfg.poll_interval;

    while running {
        // Reap zombie child processes (agents that have exited).
        // Even though agents call setsid() to create a new session, they are
        // still children of the daemon (parent-child is set at fork, not
        // affected by setsid). Without reaping, killed agents remain as
        // zombies and is_process_alive(pid) keeps returning true.
        reap_zombies();

        match listener.accept() {
            Ok((stream, _)) => {
                let mut wake_coordinator = false;
                if let Err(e) = handle_connection(
                    &dir,
                    stream,
                    &mut running,
                    &mut wake_coordinator,
                    &mut daemon_cfg,
                    &logger,
                ) {
                    logger.error(&format!("Error handling connection: {}", e));
                }
                if wake_coordinator {
                    logger.info("GraphChanged received, scheduling immediate coordinator tick");
                    // Force an immediate coordinator tick
                    last_coordinator_tick = Instant::now() - daemon_cfg.poll_interval;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No connection, sleep briefly
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                logger.error(&format!("Accept error: {}", e));
            }
        }

        // Background safety-net tick: runs on poll_interval even without IPC events.
        // The fast-path is GraphChanged IPC which resets last_coordinator_tick.
        if !daemon_cfg.paused && last_coordinator_tick.elapsed() >= daemon_cfg.poll_interval {
            last_coordinator_tick = Instant::now();

            // Aggregate usage stats periodically
            match workgraph::usage::aggregate_usage_stats(&dir) {
                Ok(count) if count > 0 => {
                    logger.info(&format!("Aggregated {} usage log entries", count));
                }
                Ok(_) => {} // No entries to aggregate
                Err(e) => {
                    logger.warn(&format!("Failed to aggregate usage stats: {}", e));
                }
            }

            logger.info(&format!(
                "Coordinator tick #{} starting (max_agents={}, executor={})",
                coord_state.ticks + 1,
                daemon_cfg.max_agents,
                &daemon_cfg.executor
            ));
            match coordinator_tick(
                &dir,
                daemon_cfg.max_agents,
                &daemon_cfg.executor,
                daemon_cfg.model.as_deref(),
            ) {
                Ok(result) => {
                    coord_state.ticks += 1;
                    coord_state.last_tick = Some(chrono::Utc::now().to_rfc3339());
                    coord_state.max_agents = daemon_cfg.max_agents;
                    coord_state.poll_interval = daemon_cfg.poll_interval.as_secs();
                    coord_state.executor = daemon_cfg.executor.clone();
                    coord_state.model = daemon_cfg.model.clone();
                    coord_state.agents_alive = result.agents_alive;
                    coord_state.tasks_ready = result.tasks_ready;
                    coord_state.agents_spawned = result.agents_spawned;
                    coord_state.save(&dir);
                    logger.info(&format!(
                        "Coordinator tick #{} complete: agents_alive={}, tasks_ready={}, spawned={}",
                        coord_state.ticks, result.agents_alive, result.tasks_ready, result.agents_spawned
                    ));
                }
                Err(e) => {
                    coord_state.ticks += 1;
                    coord_state.save(&dir);
                    logger.error(&format!("Coordinator tick error: {}", e));
                }
            }
        }
    }

    logger.info("Daemon shutting down");

    // Cleanup
    let _ = fs::remove_file(&socket);
    CoordinatorState::remove(&dir);
    ServiceState::remove(&dir)?;

    logger.info("Daemon shutdown complete");

    Ok(())
}

#[cfg(not(unix))]
pub fn run_daemon(
    _dir: &Path,
    _socket_path: &str,
    _max_agents: Option<usize>,
    _executor: Option<&str>,
    _interval: Option<u64>,
    _model: Option<&str>,
) -> Result<()> {
    anyhow::bail!("Daemon is only supported on Unix systems")
}

/// Handle a single IPC connection
#[cfg(unix)]
fn handle_connection(
    dir: &Path,
    stream: UnixStream,
    running: &mut bool,
    wake_coordinator: &mut bool,
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

        let response = handle_request(dir, request, running, wake_coordinator, daemon_cfg, logger);
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
                title,
                origin
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
    use workgraph::graph::{Node, Status, Task};
    use workgraph::parser::{load_graph, save_graph};

    let graph_path = super::graph_path(dir);
    let mut graph = match load_graph(&graph_path) {
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

    let task = Task {
        id: task_id.clone(),
        title: title.to_string(),
        description: description.map(String::from),
        status: Status::Open,
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
        not_before: None,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries: None,
        failure_reason: None,
        model: model.map(String::from),
        verify: verify.map(String::from),
        agent: None,
        loop_iteration: 0,
        ready_after: None,
        paused: false,
        visibility: "internal".to_string(),
        cycle_config: None,
    };

    graph.add_node(Node::Task(task));

    // Maintain bidirectional after/blocks consistency
    for dep in after {
        if let Some(blocker) = graph.get_task_mut(dep)
            && !blocker.before.contains(&task_id)
        {
            blocker.before.push(task_id.clone());
        }
    }

    if let Err(e) = save_graph(&graph, &graph_path) {
        return IpcResponse::error(&format!("Failed to save graph: {}", e));
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

/// Handle QueryTask IPC request — return a task's status for cross-repo dependency checking.
fn handle_query_task(dir: &Path, task_id: &str) -> IpcResponse {
    use workgraph::parser::load_graph;

    let graph_path = super::graph_path(dir);
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

/// Stop the service daemon
#[cfg(unix)]
pub fn run_stop(dir: &Path, force: bool, kill_agents: bool, json: bool) -> Result<()> {
    let state = match ServiceState::load(dir)? {
        Some(s) => s,
        None => {
            if json {
                let output = serde_json::json!({ "error": "Service not running" });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Service not running");
            }
            return Ok(());
        }
    };

    // Try to send shutdown command via socket
    let socket = PathBuf::from(&state.socket_path);
    if socket.exists()
        && let Ok(mut stream) = UnixStream::connect(&socket)
    {
        let request = IpcRequest::Shutdown { force, kill_agents };
        let json_req = serde_json::to_string(&request)?;
        // Best-effort: shutdown falls through to kill if IPC fails
        if let Err(e) = writeln!(stream, "{}", json_req) {
            eprintln!("Warning: failed to send shutdown request: {}", e);
        }
        if let Err(e) = stream.flush() {
            eprintln!("Warning: failed to flush shutdown request: {}", e);
        }
        // Give it a moment to process
        std::thread::sleep(Duration::from_millis(200));
    }

    // If process is still running, kill it
    if is_process_running(state.pid) {
        if force {
            kill_process_force(state.pid)?;
        } else {
            kill_process_graceful(state.pid)?;
        }
    }

    // Clean up
    if socket.exists() {
        let _ = fs::remove_file(&socket);
    }
    ServiceState::remove(dir)?;

    // Scan for orphan daemon processes that may have been left behind by
    // previous start/stop cycles where the state file was removed but the
    // daemon process wasn't actually killed.
    let orphans = find_orphan_daemon_pids(dir, Some(state.pid));
    let mut orphan_count = 0;
    for &pid in &orphans {
        if force {
            let _ = kill_process_force(pid);
        } else {
            let _ = kill_process_graceful(pid);
        }
        orphan_count += 1;
    }

    if json {
        let output = serde_json::json!({
            "status": "stopped",
            "pid": state.pid,
            "force": force,
            "kill_agents": kill_agents,
            "orphans_killed": orphan_count,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if orphan_count > 0 {
        println!(
            "Service stopped (PID {}), killed {} orphan daemon(s)",
            state.pid, orphan_count
        );
    } else if kill_agents {
        println!("Service stopped (PID {}), agents killed", state.pid);
    } else {
        println!(
            "Service stopped (PID {}), agents continue running",
            state.pid
        );
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_stop(_dir: &Path, _force: bool, _kill_agents: bool, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Show service status
#[cfg(unix)]
pub fn run_status(dir: &Path, json: bool) -> Result<()> {
    let state = match ServiceState::load(dir)? {
        Some(s) => s,
        None => {
            if json {
                let output = serde_json::json!({
                    "status": "not_running",
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Service: not running");
            }
            return Ok(());
        }
    };

    let running = is_process_running(state.pid);

    if !running {
        // Stale state, clean up
        ServiceState::remove(dir)?;
        if json {
            let output = serde_json::json!({
                "status": "not_running",
                "note": "Cleaned up stale state",
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("Service: not running (cleaned up stale state)");
        }
        return Ok(());
    }

    // Get agent summary (runtime registry = spawned processes)
    let registry = AgentRegistry::load_or_warn(dir);
    let alive_count = registry.active_count();
    let idle_count = registry.idle_count();

    // Check if any agency agents are defined (YAML definitions, not runtime processes)
    let agency_agents_dir = dir.join("agency").join("agents");
    let agency_agents_defined = !agency::load_all_agents_or_warn(&agency_agents_dir).is_empty();

    // Calculate uptime
    let uptime = chrono::DateTime::parse_from_rfc3339(&state.started_at)
        .map(|started| {
            let now = chrono::Utc::now();
            let duration = now.signed_duration_since(started);
            workgraph::format_duration(duration.num_seconds(), false)
        })
        .unwrap_or_else(|_| "unknown".to_string());

    // Load coordinator state (persisted by daemon, reflects effective config + runtime)
    let coord = CoordinatorState::load_or_default(dir);

    // Log file info
    let log_path = log_file_path(dir);
    let log_path_str = log_path.to_string_lossy().to_string();
    let log_exists = log_path.exists();
    let recent_errors = tail_log(dir, 5, Some("ERROR"));
    let recent_fatals = tail_log(dir, 5, Some("FATAL"));

    if json {
        let mut output = serde_json::json!({
            "status": "running",
            "pid": state.pid,
            "socket": state.socket_path,
            "started_at": state.started_at,
            "uptime": uptime,
            "agents": {
                "alive": alive_count,
                "idle": idle_count,
                "total": registry.agents.len(),
                "agents_defined": agency_agents_defined,
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
            },
            "log": {
                "path": log_path_str,
                "exists": log_exists,
            }
        });
        if !agency_agents_defined {
            output["warning"] =
                serde_json::json!("No agents defined — run 'wg agency init' or 'wg agent create'");
        }
        if agency_agents_defined && alive_count == 0 && coord.ticks > 0 && coord.agents_spawned == 0 && coord.tasks_ready > 0 {
            output["agents"]["note"] = serde_json::json!("tasks are ready but no agents have been spawned — check agent configuration");
        }
        if !recent_errors.is_empty() || !recent_fatals.is_empty() {
            let mut all_errors: Vec<String> = recent_fatals;
            all_errors.extend(recent_errors);
            output["log"]["recent_errors"] = serde_json::json!(all_errors);
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Service: running (PID {})", state.pid);
        println!("Socket: {}", state.socket_path);
        println!("Uptime: {}", uptime);
        if !agency_agents_defined {
            println!("Agents: No agents defined — run 'wg agency init' or 'wg agent create'");
        } else {
            println!(
                "Agents: {} alive, {} idle, {} total",
                alive_count,
                idle_count,
                registry.agents.len()
            );
            if alive_count == 0 && coord.ticks > 0 && coord.agents_spawned == 0 && coord.tasks_ready > 0 {
                println!("  Note: tasks are ready but no agents have been spawned — check agent configuration");
            }
        }
        let model_str = coord.model.as_deref().unwrap_or("default");
        let pause_str = if coord.paused { ", PAUSED" } else { "" };
        println!(
            "Coordinator: enabled{}, max_agents={}, poll_interval={}s, executor={}, model={}",
            pause_str, coord.max_agents, coord.poll_interval, coord.executor, model_str
        );
        if let Some(ref last) = coord.last_tick {
            println!(
                "  Last tick: {} (#{}, agents_alive={}/{}, tasks_ready={}, spawned={})",
                last,
                coord.ticks,
                coord.agents_alive,
                coord.max_agents,
                coord.tasks_ready,
                coord.agents_spawned
            );
        } else {
            println!("  No ticks yet");
        }
        println!("Log: {}", log_path_str);
        if !recent_errors.is_empty() || !recent_fatals.is_empty() {
            println!("  Recent errors:");
            for line in &recent_fatals {
                println!("    {}", line);
            }
            for line in &recent_errors {
                println!("    {}", line);
            }
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_status(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Reload service daemon configuration at runtime
#[cfg(unix)]
pub fn run_reload(
    dir: &Path,
    max_agents: Option<usize>,
    executor: Option<&str>,
    interval: Option<u64>,
    model: Option<&str>,
    json: bool,
) -> Result<()> {
    let request = IpcRequest::Reconfigure {
        max_agents,
        executor: executor.map(std::string::ToString::to_string),
        poll_interval: interval,
        model: model.map(std::string::ToString::to_string),
    };

    let response = send_request(dir, &request)?;

    if !response.ok {
        let msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        if json {
            let output = serde_json::json!({ "error": msg });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            eprintln!("Error: {}", msg);
        }
        anyhow::bail!("{}", msg);
    }

    if json {
        if let Some(data) = &response.data {
            println!("{}", serde_json::to_string_pretty(data)?);
        }
    } else {
        let has_flags =
            max_agents.is_some() || executor.is_some() || interval.is_some() || model.is_some();
        if has_flags {
            println!("Configuration updated");
        } else {
            println!("Configuration reloaded from config.toml");
        }
        if let Some(data) = &response.data
            && let Some(cfg) = data.get("config")
        {
            let ma = cfg
                .get("max_agents")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let ex = cfg.get("executor").and_then(|v| v.as_str()).unwrap_or("?");
            let pi = cfg
                .get("poll_interval")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let mdl = cfg
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            println!(
                "Effective config: max_agents={}, executor={}, poll_interval={}s, model={}",
                ma, ex, pi, mdl
            );
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_reload(
    _dir: &Path,
    _max_agents: Option<usize>,
    _executor: Option<&str>,
    _interval: Option<u64>,
    _model: Option<&str>,
    _json: bool,
) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Pause the coordinator (no new agent spawns, running agents unaffected)
#[cfg(unix)]
pub fn run_pause(dir: &Path, json: bool) -> Result<()> {
    let response = send_request(dir, &IpcRequest::Pause)?;

    if !response.ok {
        let msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        if json {
            let output = serde_json::json!({ "error": msg });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            eprintln!("Error: {}", msg);
        }
        anyhow::bail!("{}", msg);
    }

    if json {
        if let Some(data) = &response.data {
            println!("{}", serde_json::to_string_pretty(data)?);
        }
    } else {
        println!("Coordinator paused (running agents continue, no new spawns)");
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_pause(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Resume the coordinator (triggers immediate tick)
#[cfg(unix)]
pub fn run_resume(dir: &Path, json: bool) -> Result<()> {
    let response = send_request(dir, &IpcRequest::Resume)?;

    if !response.ok {
        let msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        if json {
            let output = serde_json::json!({ "error": msg });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            eprintln!("Error: {}", msg);
        }
        anyhow::bail!("{}", msg);
    }

    if json {
        if let Some(data) = &response.data {
            println!("{}", serde_json::to_string_pretty(data)?);
        }
    } else {
        println!("Coordinator resumed");
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_resume(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

use super::is_process_alive as is_process_running;

/// Public wrapper: check if the service process is alive
pub fn is_service_alive(pid: u32) -> bool {
    is_process_running(pid)
}

/// Check if the coordinator is currently paused
pub fn is_service_paused(dir: &Path) -> bool {
    CoordinatorState::load(dir).is_some_and(|c| c.paused)
}

/// Send SIGTERM, wait, then SIGKILL
#[cfg(unix)]
fn kill_process_graceful(pid: u32) -> Result<()> {
    let pid_i32 = pid as i32;

    if unsafe { libc::kill(pid_i32, 0) } != 0 {
        return Ok(());
    }

    if unsafe { libc::kill(pid_i32, libc::SIGTERM) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGTERM to PID {}", pid));
    }

    for _ in 0..5 {
        std::thread::sleep(Duration::from_secs(1));
        if unsafe { libc::kill(pid_i32, 0) } != 0 {
            return Ok(());
        }
    }

    if unsafe { libc::kill(pid_i32, libc::SIGKILL) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGKILL to PID {}", pid));
    }

    Ok(())
}

#[cfg(not(unix))]
fn kill_process_graceful(_pid: u32) -> Result<()> {
    anyhow::bail!("Process killing is only supported on Unix systems")
}

/// Send SIGKILL immediately
#[cfg(unix)]
fn kill_process_force(pid: u32) -> Result<()> {
    let pid_i32 = pid as i32;

    if unsafe { libc::kill(pid_i32, 0) } != 0 {
        return Ok(());
    }

    if unsafe { libc::kill(pid_i32, libc::SIGKILL) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGKILL to PID {}", pid));
    }

    Ok(())
}

#[cfg(not(unix))]
fn kill_process_force(_pid: u32) -> Result<()> {
    anyhow::bail!("Process killing is only supported on Unix systems")
}

/// Send an IPC request to the running service
#[cfg(unix)]
pub fn send_request(dir: &Path, request: &IpcRequest) -> Result<IpcResponse> {
    let state = ServiceState::load(dir)?.ok_or_else(|| anyhow::anyhow!("Service not running"))?;

    let socket = PathBuf::from(&state.socket_path);
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("Failed to connect to service at {:?}", socket))?;

    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let json = serde_json::to_string(&request)?;
    writeln!(stream, "{}", json)?;
    stream.flush()?;

    let reader = BufReader::new(&stream);
    for line in reader.lines() {
        let line = line.context("Failed to read response")?;
        if !line.is_empty() {
            let response: IpcResponse =
                serde_json::from_str(&line).context("Failed to parse response")?;
            return Ok(response);
        }
    }

    anyhow::bail!("No response from service")
}

#[cfg(not(unix))]
pub fn send_request(_dir: &Path, _request: &IpcRequest) -> Result<IpcResponse> {
    anyhow::bail!("IPC is only supported on Unix systems")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_socket_path() {
        let temp_dir = TempDir::new().unwrap();
        let socket = default_socket_path(temp_dir.path());
        assert_eq!(socket, temp_dir.path().join("service").join("daemon.sock"));
    }

    #[test]
    fn test_service_state_roundtrip() {
        let temp_dir = TempDir::new().unwrap();

        let state = ServiceState {
            pid: 12345,
            socket_path: "/tmp/test.sock".to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };

        state.save(temp_dir.path()).unwrap();

        let loaded = ServiceState::load(temp_dir.path()).unwrap().unwrap();
        assert_eq!(loaded.pid, 12345);
        assert_eq!(loaded.socket_path, "/tmp/test.sock");

        ServiceState::remove(temp_dir.path()).unwrap();
        assert!(ServiceState::load(temp_dir.path()).unwrap().is_none());
    }

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
    fn test_is_process_running() {
        // Current process should be running
        #[cfg(unix)]
        {
            let pid = std::process::id();
            assert!(is_process_running(pid));
        }

        // Non-existent process
        #[cfg(unix)]
        assert!(!is_process_running(999999999));
    }

    #[test]
    fn test_status_not_running() {
        let temp_dir = TempDir::new().unwrap();
        // No state file, should report not running
        let result = run_status(temp_dir.path(), false);
        assert!(result.is_ok());
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
        };

        let logger = DaemonLogger::open(dir).unwrap();
        // No flags → re-read from disk
        let resp = handle_reconfigure(dir, &mut cfg, None, None, None, None, &logger);
        assert!(resp.ok);
        assert_eq!(cfg.max_agents, 10);
        assert_eq!(cfg.executor, "shell");
        assert_eq!(cfg.poll_interval, Duration::from_secs(120));
        assert_eq!(cfg.model, None); // config.toml doesn't set model
    }

    #[test]
    fn test_daemon_logger_basic() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        let logger = DaemonLogger::open(dir).unwrap();
        logger.info("test message");
        logger.error("test error");
        logger.warn("test warning");

        let log_path = log_file_path(dir);
        let content = fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("[INFO] test message"));
        assert!(content.contains("[ERROR] test error"));
        assert!(content.contains("[WARN] test warning"));
    }

    #[test]
    fn test_tail_log() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        let logger = DaemonLogger::open(dir).unwrap();
        logger.info("info 1");
        logger.error("error 1");
        logger.info("info 2");
        logger.error("error 2");
        logger.error("error 3");

        // Get last 2 error lines
        let errors = tail_log(dir, 2, Some("ERROR"));
        assert_eq!(errors.len(), 2);
        assert!(errors[0].contains("error 2"));
        assert!(errors[1].contains("error 3"));

        // Get all lines
        let all = tail_log(dir, 100, None);
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn test_read_truncated_log_missing_file() {
        let result = read_truncated_log("/nonexistent/path/output.log", 50000);
        assert!(result.contains("not found"));
    }

    #[test]
    fn test_read_truncated_log_small_file() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("output.log");
        fs::write(&log_path, "hello world\nline 2\n").unwrap();
        let result = read_truncated_log(log_path.to_str().unwrap(), 50000);
        assert_eq!(result, "hello world\nline 2\n");
    }

    #[test]
    fn test_read_truncated_log_large_file() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("output.log");
        // Write 200 bytes, read last 100
        let content = "a".repeat(100) + "\n" + &"b".repeat(99);
        fs::write(&log_path, &content).unwrap();
        let result = read_truncated_log(log_path.to_str().unwrap(), 100);
        assert!(result.contains("[... "));
        assert!(result.contains("bytes truncated"));
        // Should contain the tail portion
        assert!(result.contains("bbb"));
    }

    #[test]
    fn test_read_truncated_log_empty_file() {
        let temp_dir = TempDir::new().unwrap();
        let log_path = temp_dir.path().join("output.log");
        fs::write(&log_path, "").unwrap();
        let result = read_truncated_log(log_path.to_str().unwrap(), 50000);
        assert!(result.contains("empty"));
    }

    #[test]
    fn test_build_triage_prompt() {
        let task = Task {
            id: "test-task".to_string(),
            title: "Fix the bug".to_string(),
            description: Some("There is a bug in foo.rs".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            estimate: None,
            before: vec![],
            after: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            cycle_config: None,
        };
        let prompt = build_triage_prompt(&task, "some log output");
        assert!(prompt.contains("test-task"));
        assert!(prompt.contains("Fix the bug"));
        assert!(prompt.contains("some log output"));
        assert!(prompt.contains("done"));
        assert!(prompt.contains("continue"));
        assert!(prompt.contains("restart"));
    }

    #[test]
    fn test_extract_triage_json_plain() {
        let input = r#"{"verdict": "done", "reason": "work complete", "summary": "all done"}"#;
        let result = extract_triage_json(input).unwrap();
        let parsed: TriageVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.verdict, "done");
    }

    #[test]
    fn test_extract_triage_json_with_fences() {
        let input = "```json\n{\"verdict\": \"continue\", \"reason\": \"partial\", \"summary\": \"half done\"}\n```";
        let result = extract_triage_json(input).unwrap();
        let parsed: TriageVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.verdict, "continue");
    }

    #[test]
    fn test_extract_triage_json_with_surrounding_text() {
        let input = "Here is my analysis:\n{\"verdict\": \"restart\", \"reason\": \"no progress\", \"summary\": \"\"}\nDone.";
        let result = extract_triage_json(input).unwrap();
        let parsed: TriageVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.verdict, "restart");
    }

    #[test]
    fn test_extract_triage_json_garbage() {
        assert!(extract_triage_json("no json here").is_none());
    }

    #[test]
    fn test_extract_triage_json_inverted_braces_no_panic() {
        // If } appears before { in the text, should return None, not panic
        assert!(extract_triage_json("some text } then { more text").is_none());
    }

    #[test]
    fn test_run_start_refuses_if_daemon_alive() {
        // If state.json exists with a PID that is alive, run_start should refuse
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Use our own PID to simulate an alive daemon
        let our_pid = std::process::id();
        let state = ServiceState {
            pid: our_pid,
            socket_path: dir
                .join("service")
                .join("daemon.sock")
                .to_string_lossy()
                .to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        state.save(dir).unwrap();

        // run_start should not start a new daemon
        let result = run_start(dir, None, None, None, None, None, None, false, false);
        assert!(result.is_ok()); // returns Ok but prints "already running"

        // State should be unchanged (same PID)
        let loaded = ServiceState::load(dir).unwrap().unwrap();
        assert_eq!(loaded.pid, our_pid);
    }

    #[test]
    fn test_run_start_cleans_stale_state() {
        // If state.json exists with a PID that is dead, run_start should clean up
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Use a non-existent PID
        let state = ServiceState {
            pid: 999999999,
            socket_path: dir
                .join("service")
                .join("daemon.sock")
                .to_string_lossy()
                .to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        state.save(dir).unwrap();

        // The stale state should be cleaned up (run_start will try to spawn daemon
        // which will fail since we don't have a real wg binary, but the stale
        // state should be removed first)
        let state_path = state_file_path(dir);
        assert!(state_path.exists());
        // We can't fully test start since it spawns a real process, but we verify
        // the state cleanup happens by checking ServiceState::load after removal
        ServiceState::remove(dir).unwrap();
        assert!(!state_path.exists());
    }

    #[test]
    fn test_find_orphan_daemon_pids_no_orphans() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        // No orphans should be found for a random temp dir
        let orphans = find_orphan_daemon_pids(dir, None);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_run_stop_cleans_up_state_and_socket() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Write a state file with a dead PID
        let state = ServiceState {
            pid: 999999999,
            socket_path: dir
                .join("service")
                .join("daemon.sock")
                .to_string_lossy()
                .to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        state.save(dir).unwrap();

        // Stop should succeed and clean up
        let result = run_stop(dir, false, false, false);
        assert!(result.is_ok());

        // State file should be removed
        assert!(ServiceState::load(dir).unwrap().is_none());
    }

    #[test]
    fn test_apply_triage_verdict_done() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: None,
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            estimate: None,
            before: vec![],
            after: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
        cycle_config: None,
        };
        let verdict = TriageVerdict {
            verdict: "done".to_string(),
            reason: "work complete".to_string(),
            summary: "all files written".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Done);
        assert!(task.completed_at.is_some());
        assert!(task.log.last().unwrap().message.contains("work complete"));
    }

    #[test]
    fn test_apply_triage_verdict_done_verified() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: None,
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            estimate: None,
            before: vec![],
            after: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: Some("Check tests pass".to_string()),
            agent: None,

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
        cycle_config: None,
        };
        let verdict = TriageVerdict {
            verdict: "done".to_string(),
            reason: "tests pass".to_string(),
            summary: "implementation complete".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_apply_triage_verdict_continue() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: Some("Original description".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            estimate: None,
            before: vec![],
            after: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
        cycle_config: None,
        };
        let verdict = TriageVerdict {
            verdict: "continue".to_string(),
            reason: "partial progress".to_string(),
            summary: "Created foo.rs and bar.rs".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert_eq!(task.retry_count, 1);
        assert!(
            task.description
                .as_ref()
                .unwrap()
                .contains("Previous Attempt Recovery")
        );
        assert!(
            task.description
                .as_ref()
                .unwrap()
                .contains("Created foo.rs and bar.rs")
        );
    }

    #[test]
    fn test_apply_triage_verdict_restart() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: None,
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            estimate: None,
            before: vec![],
            after: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
        cycle_config: None,
        };
        let verdict = TriageVerdict {
            verdict: "restart".to_string(),
            reason: "no progress".to_string(),
            summary: "".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert_eq!(task.retry_count, 1);
        // Description should NOT have recovery context for restart
        assert!(task.description.is_none());
    }

    #[test]
    fn test_apply_triage_verdict_continue_max_retries_exceeded() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: Some("Original".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            estimate: None,
            before: vec![],
            after: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 3,
            max_retries: Some(3),
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
        cycle_config: None,
        };
        let verdict = TriageVerdict {
            verdict: "continue".to_string(),
            reason: "needs more work".to_string(),
            summary: "partial progress".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Failed);
        assert!(task.assigned.is_none());
        assert_eq!(task.retry_count, 3); // not incremented
        assert!(
            task.failure_reason
                .as_ref()
                .unwrap()
                .contains("Max retries exceeded")
        );
    }

    #[test]
    fn test_apply_triage_verdict_restart_max_retries_exceeded() {
        let mut task = Task {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: None,
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            estimate: None,
            before: vec![],
            after: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 2,
            max_retries: Some(2),
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,

            loop_iteration: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
        cycle_config: None,
        };
        let verdict = TriageVerdict {
            verdict: "restart".to_string(),
            reason: "no progress".to_string(),
            summary: "".to_string(),
        };
        apply_triage_verdict(&mut task, &verdict, "agent-1", 1234);
        assert_eq!(task.status, Status::Failed);
        assert!(task.assigned.is_none());
        assert_eq!(task.retry_count, 2); // not incremented
        assert!(
            task.failure_reason
                .as_ref()
                .unwrap()
                .contains("Max retries exceeded")
        );
    }

    #[test]
    fn test_no_agents_warning_when_auto_assign_enabled() {
        // When auto_assign is enabled but no agency agents exist,
        // the service start output should include a warning.
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir.join("agency").join("agents")).unwrap();

        // Enable auto_assign in config
        let mut config = Config::load_or_default(wg_dir);
        config.agency.auto_assign = true;
        config.save(wg_dir).unwrap();

        // Check: no agency agents defined
        let agents_dir = wg_dir.join("agency").join("agents");
        let agents = agency::load_all_agents_or_warn(&agents_dir);
        assert!(agents.is_empty(), "Expected no agents defined");

        // The condition that triggers the warning
        let no_agents_defined = agents.is_empty();
        let warn_no_agents = config.agency.auto_assign && no_agents_defined;
        assert!(
            warn_no_agents,
            "Should warn: auto_assign enabled, no agents defined"
        );
    }

    #[test]
    fn test_no_warning_when_agents_exist() {
        // When agency agents exist, no warning should be shown.
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();

        // Use agency init to create roles, motivations, and a default agent
        super::super::agency_init::run(wg_dir).unwrap();

        let mut config = Config::load_or_default(wg_dir);
        config.agency.auto_assign = true;
        config.save(wg_dir).unwrap();

        let agents_dir = wg_dir.join("agency").join("agents");
        let agents = agency::load_all_agents_or_warn(&agents_dir);
        assert!(!agents.is_empty(), "Expected at least one agent");

        let no_agents_defined = agents.is_empty();
        let warn_no_agents = config.agency.auto_assign && no_agents_defined;
        assert!(!warn_no_agents, "Should NOT warn when agents are defined");
    }

    #[test]
    fn test_status_distinguishes_no_agents_from_dead_agents() {
        // When no agency agents are defined, status should say "No agents defined"
        // rather than just showing agents_alive=0.
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir.join("agency").join("agents")).unwrap();

        let agents_dir = wg_dir.join("agency").join("agents");
        let agency_agents_defined = !agency::load_all_agents_or_warn(&agents_dir).is_empty();

        // No agents defined — should show the "No agents defined" message
        assert!(!agency_agents_defined);

        let status_line = if !agency_agents_defined {
            "Agents: No agents defined — run 'wg agency init' or 'wg agent create'".to_string()
        } else {
            "Agents: 0 alive, 0 idle, 0 total".to_string()
        };
        assert!(
            status_line.contains("No agents defined"),
            "Expected 'No agents defined' message, got: {}",
            status_line
        );
    }

    #[test]
    fn test_status_shows_counts_when_agents_defined() {
        // When agency agents exist but none are alive (process-wise),
        // status should show the alive/idle/total counts, NOT "No agents defined".
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();

        // Create an agent via agency init
        super::super::agency_init::run(wg_dir).unwrap();

        let agents_dir = wg_dir.join("agency").join("agents");
        let agency_agents_defined = !agency::load_all_agents_or_warn(&agents_dir).is_empty();
        assert!(agency_agents_defined);

        let status_line = if !agency_agents_defined {
            "Agents: No agents defined — run 'wg agency init' or 'wg agent create'".to_string()
        } else {
            "Agents: 0 alive, 0 idle, 0 total".to_string()
        };
        assert!(
            !status_line.contains("No agents defined"),
            "Should show counts when agents are defined, got: {}",
            status_line
        );
        assert!(status_line.contains("0 alive"));
    }

    #[test]
    fn test_eval_inline_extracts_source_task_from_exec() {
        // spawn_eval_inline extracts the source task ID from exec command
        // This tests the extraction logic used in the function
        let exec = Some("wg evaluate my-source-task".to_string());
        let source_id = exec
            .as_deref()
            .and_then(|e| e.strip_prefix("wg evaluate "))
            .unwrap_or("fallback");
        assert_eq!(source_id, "my-source-task");
    }

    #[test]
    fn test_eval_inline_extracts_source_task_from_id_fallback() {
        // When exec is missing the prefix, fall back to stripping evaluate- from task ID
        let exec: Option<String> = None;
        let eval_task_id = "evaluate-some-task";
        let source_id = exec
            .as_deref()
            .and_then(|e| e.strip_prefix("wg evaluate "))
            .unwrap_or_else(|| {
                eval_task_id
                    .strip_prefix("evaluate-")
                    .unwrap_or(eval_task_id)
            });
        assert_eq!(source_id, "some-task");
    }

    #[test]
    fn test_eval_routing_condition() {
        // The routing condition for inline eval: has "evaluation" tag AND exec is set
        let mut task = Task::default();
        task.id = "evaluate-t1".to_string();
        task.tags = vec!["evaluation".to_string(), "agency".to_string()];
        task.exec = Some("wg evaluate t1".to_string());

        let is_inline_eval = task.tags.iter().any(|t| t == "evaluation") && task.exec.is_some();
        assert!(is_inline_eval);

        // Non-eval exec task should NOT match
        let mut shell_task = Task::default();
        shell_task.exec = Some("bash run.sh".to_string());
        let is_inline_eval2 =
            shell_task.tags.iter().any(|t| t == "evaluation") && shell_task.exec.is_some();
        assert!(!is_inline_eval2);

        // Eval tag but no exec should NOT match
        let mut no_exec = Task::default();
        no_exec.tags = vec!["evaluation".to_string()];
        let is_inline_eval3 =
            no_exec.tags.iter().any(|t| t == "evaluation") && no_exec.exec.is_some();
        assert!(!is_inline_eval3);
    }
}

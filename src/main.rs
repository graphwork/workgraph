#![allow(
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::manual_clamp,
    clippy::vec_init_then_push,
    clippy::if_same_then_else
)]

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

mod commands;
mod tui;

#[derive(Parser)]
#[command(name = "wg")]
#[command(about = "Workgraph - A lightweight work coordination graph")]
#[command(version)]
#[command(disable_help_flag = true)]
#[command(disable_help_subcommand = true)]
struct Cli {
    /// Path to the workgraph directory (default: .workgraph in current dir)
    #[arg(long, global = true)]
    dir: Option<PathBuf>,

    /// Output as JSON for machine consumption
    #[arg(long, global = true)]
    json: bool,

    /// Show help (use --help-all for full command list)
    #[arg(long, short = 'h', global = true)]
    help: bool,

    /// Show all commands in help output
    #[arg(long, global = true)]
    help_all: bool,

    /// Sort help output alphabetically
    #[arg(long, short = 'a', global = true)]
    alphabetical: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new workgraph in the current directory
    Init,

    /// Add a new task
    Add {
        /// Task title
        title: String,

        /// Task ID (auto-generated if not provided)
        #[arg(long)]
        id: Option<String>,

        /// Detailed description (body, acceptance criteria, etc.)
        #[arg(long, short = 'd', alias = "desc")]
        description: Option<String>,

        /// This task is blocked by another task (can specify multiple)
        #[arg(long = "blocked-by", value_delimiter = ',', num_args = 1..)]
        blocked_by: Vec<String>,

        /// Assign to an actor
        #[arg(long)]
        assign: Option<String>,

        /// Estimated hours
        #[arg(long)]
        hours: Option<f64>,

        /// Estimated cost
        #[arg(long)]
        cost: Option<f64>,

        /// Tags
        #[arg(long, short)]
        tag: Vec<String>,

        /// Required skills/capabilities for this task
        #[arg(long)]
        skill: Vec<String>,

        /// Input files/context paths needed for this task
        #[arg(long)]
        input: Vec<String>,

        /// Expected output paths/artifacts
        #[arg(long)]
        deliverable: Vec<String>,

        /// Maximum number of retries allowed for this task
        #[arg(long)]
        max_retries: Option<u32>,

        /// Preferred model for this task (haiku, sonnet, opus)
        #[arg(long)]
        model: Option<String>,

        /// Verification criteria - task requires review before done
        #[arg(long)]
        verify: Option<String>,

        /// Create a loop edge back to target task (re-activates on completion)
        #[arg(long = "loops-to")]
        loops_to: Option<String>,

        /// Maximum loop iterations (required with --loops-to)
        #[arg(long = "loop-max")]
        loop_max: Option<u32>,

        /// Guard condition for loop: 'task:<id>=<status>' or 'always'
        #[arg(long = "loop-guard")]
        loop_guard: Option<String>,

        /// Delay between loop iterations (e.g., 30s, 5m, 1h, 24h, 7d)
        #[arg(long = "loop-delay")]
        loop_delay: Option<String>,
    },

    /// Edit an existing task
    Edit {
        /// Task ID to edit
        id: String,

        /// Update task title
        #[arg(long)]
        title: Option<String>,

        /// Update task description
        #[arg(long, short = 'd')]
        description: Option<String>,

        /// Add a blocked-by dependency
        #[arg(long = "add-blocked-by")]
        add_blocked_by: Vec<String>,

        /// Remove a blocked-by dependency
        #[arg(long = "remove-blocked-by")]
        remove_blocked_by: Vec<String>,

        /// Add a tag
        #[arg(long = "add-tag")]
        add_tag: Vec<String>,

        /// Remove a tag
        #[arg(long = "remove-tag")]
        remove_tag: Vec<String>,

        /// Update preferred model
        #[arg(long)]
        model: Option<String>,

        /// Add a required skill
        #[arg(long = "add-skill")]
        add_skill: Vec<String>,

        /// Remove a required skill
        #[arg(long = "remove-skill")]
        remove_skill: Vec<String>,

        /// Add a loop edge back to target task (re-activates on completion)
        #[arg(long = "add-loops-to")]
        add_loops_to: Option<String>,

        /// Maximum loop iterations (used with --add-loops-to)
        #[arg(long = "loop-max")]
        loop_max: Option<u32>,

        /// Guard condition for loop: 'task:<id>=<status>' or 'always'
        #[arg(long = "loop-guard")]
        loop_guard: Option<String>,

        /// Delay between loop iterations (e.g., 30s, 5m, 1h, 24h, 7d)
        #[arg(long = "loop-delay")]
        loop_delay: Option<String>,

        /// Remove a loop edge to target task
        #[arg(long = "remove-loops-to")]
        remove_loops_to: Option<String>,

        /// Manually override the loop iteration counter on this task
        #[arg(long = "loop-iteration")]
        loop_iteration: Option<u32>,
    },

    /// Mark a task as done (fails for verified tasks - use submit instead)
    Done {
        /// Task ID to mark as done
        id: String,
    },

    /// Submit work for review (for verified tasks)
    Submit {
        /// Task ID to submit
        id: String,

        /// Actor submitting the work
        #[arg(long)]
        actor: Option<String>,
    },

    /// Approve a pending-review task (marks as done)
    Approve {
        /// Task ID to approve
        id: String,

        /// Actor approving the work
        #[arg(long)]
        actor: Option<String>,
    },

    /// Reject a pending-review task (returns to open for rework)
    Reject {
        /// Task ID to reject
        id: String,

        /// Reason for rejection
        #[arg(long)]
        reason: Option<String>,

        /// Actor rejecting the work
        #[arg(long)]
        actor: Option<String>,
    },

    /// Mark a task as failed (can be retried)
    Fail {
        /// Task ID to mark as failed
        id: String,

        /// Reason for failure
        #[arg(long)]
        reason: Option<String>,
    },

    /// Mark a task as abandoned (will not be retried)
    Abandon {
        /// Task ID to abandon
        id: String,

        /// Reason for abandonment
        #[arg(long)]
        reason: Option<String>,
    },

    /// Retry a failed task (resets to open status)
    Retry {
        /// Task ID to retry
        id: String,
    },

    /// Claim a task for work (sets status to InProgress)
    Claim {
        /// Task ID to claim
        id: String,

        /// Assign to a specific actor
        #[arg(long)]
        actor: Option<String>,
    },

    /// Release a claimed task (sets status back to Open)
    Unclaim {
        /// Task ID to unclaim
        id: String,
    },

    /// Reclaim a task from a dead/unresponsive agent
    Reclaim {
        /// Task ID to reclaim
        id: String,

        /// The actor currently holding the task
        #[arg(long)]
        from: String,

        /// The new actor to assign the task to
        #[arg(long)]
        to: String,
    },

    /// List tasks that are ready to work on
    Ready,

    /// Show what's blocking a task
    Blocked {
        /// Task ID
        id: String,
    },

    /// Show the full transitive chain explaining why a task is blocked
    WhyBlocked {
        /// Task ID
        id: String,
    },

    /// Check the graph for issues (cycles, orphan references)
    Check,

    /// List all tasks
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
    },

    /// Visualize the dependency graph (ASCII tree by default)
    Viz {
        /// Include done tasks (default: only open tasks)
        #[arg(long)]
        all: bool,

        /// Filter by status (open, in-progress, done, blocked)
        #[arg(long)]
        status: Option<String>,

        /// Highlight the critical path in red
        #[arg(long)]
        critical_path: bool,

        /// Output Graphviz DOT format
        #[arg(long, conflicts_with_all = ["mermaid"])]
        dot: bool,

        /// Output Mermaid diagram format
        #[arg(long, conflicts_with_all = ["dot"])]
        mermaid: bool,

        /// Render directly to file (requires dot installed)
        #[arg(long, short)]
        output: Option<String>,
    },

    /// Output the full graph data (DOT format with archive support)
    #[command(hide = true)]
    GraphExport {
        /// Include archived tasks
        #[arg(long)]
        archive: bool,

        /// Only show tasks completed/archived after this date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,

        /// Only show tasks completed/archived before this date (YYYY-MM-DD)
        #[arg(long)]
        until: Option<String>,
    },

    /// Calculate cost of a task including dependencies
    Cost {
        /// Task ID
        id: String,
    },

    /// Show coordination status and ready tasks for parallel execution
    Coordinate {
        /// Maximum number of parallel tasks to show
        #[arg(long)]
        max_parallel: Option<usize>,
    },

    /// Plan what can be accomplished with given resources
    Plan {
        /// Available budget (dollars)
        #[arg(long)]
        budget: Option<f64>,

        /// Available hours
        #[arg(long)]
        hours: Option<f64>,
    },

    /// Reschedule a task (set not_before timestamp)
    Reschedule {
        /// Task ID
        id: String,

        /// Hours from now until task is ready (e.g., 24 for tomorrow)
        #[arg(long)]
        after: Option<f64>,

        /// Specific timestamp when task becomes ready (ISO 8601)
        #[arg(long)]
        at: Option<String>,
    },

    /// Show impact analysis - what tasks depend on this one
    Impact {
        /// Task ID
        id: String,
    },

    /// Analyze cycles in the graph with classification
    Loops,

    /// Analyze graph structure - entry points, dead ends, high-impact roots
    Structure,

    /// Find tasks blocking the most work (bottleneck analysis)
    Bottlenecks,

    /// Show task completion velocity over time
    Velocity {
        /// Number of weeks to show (default: 4)
        #[arg(long)]
        weeks: Option<usize>,
    },

    /// Show task age distribution - how long tasks have been open
    Aging,

    /// Show project completion forecast based on velocity and remaining work
    Forecast,

    /// Show agent workload balance and assignment distribution
    Workload,

    /// Show resource utilization - committed vs available capacity
    Resources,

    /// Show the critical path (longest dependency chain)
    CriticalPath,

    /// Comprehensive health report combining all analyses
    Analyze,

    /// Archive completed tasks to a separate file
    Archive {
        /// Show what would be archived without actually archiving
        #[arg(long)]
        dry_run: bool,

        /// Only archive tasks completed more than this duration ago (e.g., 30d, 7d, 1w)
        #[arg(long)]
        older: Option<String>,

        /// List archived tasks instead of archiving
        #[arg(long)]
        list: bool,
    },

    /// Show detailed information about a single task
    Show {
        /// Task ID
        id: String,
    },

    /// Add progress log/notes to a task
    Log {
        /// Task ID
        id: String,

        /// Log message (if not provided, lists log entries)
        message: Option<String>,

        /// Actor adding the log entry
        #[arg(long)]
        actor: Option<String>,

        /// List log entries instead of adding
        #[arg(long)]
        list: bool,
    },

    /// Manage resources
    Resource {
        #[command(subcommand)]
        command: ResourceCommands,
    },

    /// Manage skills (Claude Code skill installation, task skill queries)
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },

    /// Manage the agency (roles + motivations)
    Agency {
        #[command(subcommand)]
        command: AgencyCommands,
    },

    /// Manage agency roles (what an agent does)
    Role {
        #[command(subcommand)]
        command: RoleCommands,
    },

    /// Manage agency motivations (why an agent acts)
    Motivation {
        #[command(subcommand)]
        command: MotivationCommands,
    },

    /// Alias for 'motivation'
    #[command(hide = true)]
    Mot {
        #[command(subcommand)]
        command: MotivationCommands,
    },

    /// Assign an agent to a task
    Assign {
        /// Task ID to assign agent to
        task: String,

        /// Agent hash (or prefix) to assign
        agent_hash: Option<String>,

        /// Clear the agent assignment from the task
        #[arg(long)]
        clear: bool,
    },

    /// Find agents capable of performing a task
    Match {
        /// Task ID to match agents against
        task: String,
    },

    /// Record agent heartbeat or check for stale agents
    Heartbeat {
        /// Agent ID to record heartbeat for (omit to check status)
        /// Agent IDs start with "agent-" (e.g., agent-1, agent-7)
        actor: Option<String>,

        /// Check for stale agents (no heartbeat within threshold)
        #[arg(long)]
        check: bool,

        /// Check for stale agents instead of actors
        #[arg(long)]
        agents: bool,

        /// Minutes without heartbeat before agent is considered stale (default: 5)
        #[arg(long, default_value = "5")]
        threshold: u64,
    },

    /// Manage task artifacts (produced outputs)
    Artifact {
        /// Task ID
        task: String,

        /// Artifact path to add (omit to list)
        path: Option<String>,

        /// Remove an artifact instead of adding
        #[arg(long)]
        remove: bool,
    },

    /// Show available context for a task from its dependencies
    Context {
        /// Task ID
        task: String,

        /// Show tasks that depend on this task's outputs
        #[arg(long)]
        dependents: bool,
    },

    /// Find the best next task for an agent (agent work loop)
    Next {
        /// Agent ID to find tasks for
        #[arg(long)]
        actor: String,
    },

    /// Show context-efficient task trajectory (claim order for minimal context switching)
    Trajectory {
        /// Starting task ID
        task: String,

        /// Suggest trajectories for an actor based on capabilities
        #[arg(long)]
        actor: Option<String>,
    },

    /// Execute a task's shell command (claim + run + done/fail)
    Exec {
        /// Task ID to execute
        task: String,

        /// Actor performing the execution
        #[arg(long)]
        actor: Option<String>,

        /// Show what would be executed without running
        #[arg(long)]
        dry_run: bool,

        /// Set the exec command for a task (instead of running)
        #[arg(long)]
        set: Option<String>,

        /// Clear the exec command for a task
        #[arg(long)]
        clear: bool,
    },

    /// Manage agents (role+motivation pairings) and run agent loops
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },

    /// Spawn an agent to work on a specific task
    Spawn {
        /// Task ID to spawn an agent for
        task: String,

        /// Executor to use (claude, shell, or custom config name)
        #[arg(long)]
        executor: String,

        /// Timeout duration (e.g., 30m, 1h, 90s)
        #[arg(long)]
        timeout: Option<String>,

        /// Model to use (haiku, sonnet, opus) - overrides task/executor defaults
        #[arg(long)]
        model: Option<String>,
    },

    /// Trigger evaluation of a completed task
    Evaluate {
        /// Task ID to evaluate
        task: String,

        /// Model to use for the evaluator (overrides config and task defaults)
        #[arg(long)]
        evaluator_model: Option<String>,

        /// Show what would be evaluated without spawning the evaluator agent
        #[arg(long)]
        dry_run: bool,
    },

    /// Trigger an evolution cycle on agency roles and motivations
    Evolve {
        /// Show proposed changes without applying them
        #[arg(long)]
        dry_run: bool,

        /// Evolution strategy: mutation, crossover, gap-analysis, retirement, motivation-tuning, all (default: all)
        #[arg(long)]
        strategy: Option<String>,

        /// Maximum number of operations to apply
        #[arg(long)]
        budget: Option<u32>,

        /// Model to use for the evolver agent
        #[arg(long)]
        model: Option<String>,
    },

    /// View or modify project configuration
    Config {
        /// Show current configuration
        #[arg(long)]
        show: bool,

        /// Initialize default config file
        #[arg(long)]
        init: bool,

        /// Set executor (claude, opencode, codex)
        #[arg(long)]
        executor: Option<String>,

        /// Set model (opus-4-5, sonnet, haiku)
        #[arg(long)]
        model: Option<String>,

        /// Set default interval in seconds
        #[arg(long)]
        set_interval: Option<u64>,

        /// Set coordinator max agents
        #[arg(long)]
        max_agents: Option<usize>,

        /// Set coordinator poll interval in seconds
        #[arg(long)]
        coordinator_interval: Option<u64>,

        /// Set service daemon background poll interval in seconds (safety net)
        #[arg(long)]
        poll_interval: Option<u64>,

        /// Set coordinator executor
        #[arg(long)]
        coordinator_executor: Option<String>,

        /// Matrix configuration subcommand
        #[arg(long)]
        matrix: bool,

        /// Set Matrix homeserver URL
        #[arg(long)]
        homeserver: Option<String>,

        /// Set Matrix username
        #[arg(long)]
        username: Option<String>,

        /// Set Matrix password
        #[arg(long)]
        password: Option<String>,

        /// Set Matrix access token
        #[arg(long)]
        access_token: Option<String>,

        /// Set Matrix default room
        #[arg(long)]
        room: Option<String>,

        /// Enable/disable automatic evaluation on task completion
        #[arg(long)]
        auto_evaluate: Option<bool>,

        /// Enable/disable automatic identity assignment when spawning agents
        #[arg(long)]
        auto_assign: Option<bool>,

        /// Set model for assigner agents
        #[arg(long)]
        assigner_model: Option<String>,

        /// Set model for evaluator agents
        #[arg(long)]
        evaluator_model: Option<String>,

        /// Set model for evolver agents
        #[arg(long)]
        evolver_model: Option<String>,

        /// Set assigner agent (content-hash)
        #[arg(long)]
        assigner_agent: Option<String>,

        /// Set evaluator agent (content-hash)
        #[arg(long)]
        evaluator_agent: Option<String>,

        /// Set evolver agent (content-hash)
        #[arg(long)]
        evolver_agent: Option<String>,

        /// Set retention heuristics (prose policy for evolver)
        #[arg(long)]
        retention_heuristics: Option<String>,

        /// Enable/disable automatic triage of dead agents
        #[arg(long)]
        auto_triage: Option<bool>,

        /// Set model for triage (default: haiku)
        #[arg(long)]
        triage_model: Option<String>,

        /// Set timeout in seconds for triage calls (default: 30)
        #[arg(long)]
        triage_timeout: Option<u64>,

        /// Set max bytes to read from agent output log for triage (default: 50000)
        #[arg(long)]
        triage_max_log_bytes: Option<usize>,
    },

    /// Detect and clean up dead agents
    DeadAgents {
        /// Check for dead agents without modifying
        #[arg(long)]
        check: bool,

        /// Mark dead agents and unclaim their tasks
        #[arg(long)]
        cleanup: bool,

        /// Remove dead agents from registry
        #[arg(long)]
        remove: bool,

        /// Check if agent processes are still running
        #[arg(long)]
        processes: bool,

        /// Override heartbeat timeout threshold (minutes)
        #[arg(long)]
        threshold: Option<u64>,
    },

    /// List running agents
    Agents {
        /// Only show alive agents (starting, working, idle)
        #[arg(long)]
        alive: bool,

        /// Only show dead agents
        #[arg(long)]
        dead: bool,

        /// Only show working agents
        #[arg(long)]
        working: bool,

        /// Only show idle agents
        #[arg(long)]
        idle: bool,
    },

    /// Kill running agent(s)
    Kill {
        /// Agent ID to kill (e.g., agent-1)
        agent: Option<String>,

        /// Force kill (SIGKILL immediately instead of graceful SIGTERM)
        #[arg(long)]
        force: bool,

        /// Kill all running agents
        #[arg(long)]
        all: bool,
    },

    /// Manage the agent service daemon
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },

    /// Launch interactive TUI dashboard
    Tui {
        /// Data refresh rate in milliseconds (default: 2000)
        #[arg(long, default_value = "2000")]
        refresh_rate: u64,
    },

    /// Print a concise cheat sheet for agent onboarding
    Quickstart,

    /// Quick one-screen status overview
    Status,

    /// Send task notification to Matrix room
    #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
    Notify {
        /// Task ID to notify about
        task: String,

        /// Target Matrix room (uses default_room from config if not specified)
        #[arg(long)]
        room: Option<String>,

        /// Custom message to include with the notification
        #[arg(long, short)]
        message: Option<String>,
    },

    /// Matrix integration commands
    #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
    Matrix {
        #[command(subcommand)]
        command: MatrixCommands,
    },
}

#[derive(Subcommand)]
enum ResourceCommands {
    /// Add a new resource
    Add {
        /// Resource ID
        id: String,

        /// Display name
        #[arg(long)]
        name: Option<String>,

        /// Resource type (money, compute, time, etc.)
        #[arg(long = "type")]
        resource_type: Option<String>,

        /// Available amount
        #[arg(long)]
        available: Option<f64>,

        /// Unit (usd, hours, gpu-hours, etc.)
        #[arg(long)]
        unit: Option<String>,
    },

    /// List all resources
    List,
}

#[derive(Subcommand)]
enum SkillCommands {
    /// List all skills used across tasks
    List,

    /// Show skills for a specific task
    Task {
        /// Task ID
        id: String,
    },

    /// Find tasks requiring a specific skill
    Find {
        /// Skill name to search for
        skill: String,
    },

    /// Install the wg Claude Code skill to ~/.claude/skills/wg/
    Install,
}

#[derive(Subcommand)]
enum AgencyCommands {
    /// Seed agency with starter roles and motivations
    Init,

    /// Show agency performance analytics
    Stats {
        /// Minimum evaluations to consider a pair "explored" (default: 3)
        #[arg(long, default_value = "3")]
        min_evals: u32,
    },
}

#[derive(Subcommand)]
enum RoleCommands {
    /// Create a new role
    Add {
        /// Role name
        name: String,

        /// Desired outcome for this role
        #[arg(long)]
        outcome: String,

        /// Skills (name, name:file:///path, name:https://url, name:inline:content)
        #[arg(long)]
        skill: Vec<String>,

        /// Role description
        #[arg(long, short = 'd')]
        description: Option<String>,
    },

    /// List all roles
    List,

    /// Show full role details
    Show {
        /// Role ID
        id: String,
    },

    /// Open role YAML in EDITOR for manual editing
    Edit {
        /// Role ID
        id: String,
    },

    /// Remove a role
    Rm {
        /// Role ID
        id: String,
    },

    /// Show evolutionary lineage/ancestry tree for a role
    Lineage {
        /// Role ID
        id: String,
    },
}

#[derive(Subcommand)]
enum MotivationCommands {
    /// Create a new motivation
    Add {
        /// Motivation name
        name: String,

        /// Acceptable tradeoffs (can be repeated)
        #[arg(long)]
        accept: Vec<String>,

        /// Unacceptable tradeoffs (can be repeated)
        #[arg(long)]
        reject: Vec<String>,

        /// Motivation description
        #[arg(long, short = 'd')]
        description: Option<String>,
    },

    /// List all motivations
    List,

    /// Show full motivation details
    Show {
        /// Motivation ID
        id: String,
    },

    /// Open motivation YAML in EDITOR for manual editing
    Edit {
        /// Motivation ID
        id: String,
    },

    /// Remove a motivation
    Rm {
        /// Motivation ID
        id: String,
    },

    /// Show evolutionary lineage/ancestry tree for a motivation
    Lineage {
        /// Motivation ID
        id: String,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// Create a new agent (role + motivation pairing)
    Create {
        /// Agent name
        name: String,

        /// Role ID (or prefix) — optional for human agents
        #[arg(long)]
        role: Option<String>,

        /// Motivation ID (or prefix) — optional for human agents
        #[arg(long)]
        motivation: Option<String>,

        /// Skills/capabilities (comma-separated or repeated)
        #[arg(long, value_delimiter = ',')]
        capabilities: Vec<String>,

        /// Hourly rate for cost tracking
        #[arg(long)]
        rate: Option<f64>,

        /// Maximum concurrent task capacity
        #[arg(long)]
        capacity: Option<f64>,

        /// Trust level (verified, provisional, unknown)
        #[arg(long)]
        trust_level: Option<String>,

        /// Contact info (email, matrix ID, etc.)
        #[arg(long)]
        contact: Option<String>,

        /// Executor backend (claude, matrix, email, shell)
        #[arg(long, default_value = "claude")]
        executor: String,
    },

    /// List all agents
    List,

    /// Show full agent details including resolved role/motivation
    Show {
        /// Agent ID (or prefix)
        id: String,
    },

    /// Remove an agent
    Rm {
        /// Agent ID (or prefix)
        id: String,
    },

    /// Show ancestry (lineage of constituent role and motivation)
    Lineage {
        /// Agent ID (or prefix)
        id: String,
    },

    /// Show evaluation history for an agent
    Performance {
        /// Agent ID (or prefix)
        id: String,
    },

    /// Run autonomous agent loop (wake/check/work/sleep cycle)
    Run {
        /// Actor ID for this agent
        #[arg(long)]
        actor: String,

        /// Run only one iteration then exit
        #[arg(long)]
        once: bool,

        /// Seconds to sleep between iterations (default from config, fallback: 10)
        #[arg(long)]
        interval: Option<u64>,

        /// Maximum number of tasks to complete before stopping
        #[arg(long)]
        max_tasks: Option<u32>,

        /// Reset agent state (discard saved statistics and task history)
        #[arg(long)]
        reset_state: bool,
    },
}

#[derive(Subcommand)]
enum ServiceCommands {
    /// Start the agent service daemon
    Start {
        /// Port to listen on (optional, for HTTP API)
        #[arg(long)]
        port: Option<u16>,

        /// Unix socket path (default: .workgraph/service/daemon.sock)
        #[arg(long)]
        socket: Option<String>,

        /// Maximum number of parallel agents (overrides config.toml)
        #[arg(long)]
        max_agents: Option<usize>,

        /// Executor to use for spawned agents (overrides config.toml)
        #[arg(long)]
        executor: Option<String>,

        /// Background poll interval in seconds (overrides config.toml coordinator.poll_interval)
        #[arg(long)]
        interval: Option<u64>,

        /// Model to use for spawned agents (overrides config.toml coordinator.model)
        #[arg(long)]
        model: Option<String>,
    },

    /// Stop the agent service daemon
    Stop {
        /// Force stop (SIGKILL the daemon immediately)
        #[arg(long)]
        force: bool,

        /// Also kill running agents (by default, detached agents continue running)
        #[arg(long)]
        kill_agents: bool,
    },

    /// Show service status
    Status,

    /// Reload daemon configuration without restarting
    ///
    /// With flags: applies the specified overrides to the running daemon.
    /// Without flags: re-reads config.toml from disk.
    Reload {
        /// Maximum number of parallel agents
        #[arg(long)]
        max_agents: Option<usize>,

        /// Executor to use for spawned agents
        #[arg(long)]
        executor: Option<String>,

        /// Background poll interval in seconds
        #[arg(long)]
        interval: Option<u64>,

        /// Model to use for spawned agents
        #[arg(long)]
        model: Option<String>,
    },

    /// Pause the coordinator (running agents continue, no new spawns)
    Pause,

    /// Resume the coordinator
    Resume,

    /// Generate a systemd user service file for the wg service daemon
    Install,

    /// Run a single coordinator tick and exit (debug mode)
    Tick {
        /// Maximum number of parallel agents (overrides config.toml)
        #[arg(long)]
        max_agents: Option<usize>,

        /// Executor to use for spawned agents (overrides config.toml)
        #[arg(long)]
        executor: Option<String>,

        /// Model to use for spawned agents (overrides config.toml)
        #[arg(long)]
        model: Option<String>,
    },

    /// Run the daemon (internal, called by start)
    #[command(hide = true)]
    Daemon {
        /// Unix socket path
        #[arg(long)]
        socket: String,

        /// Maximum number of parallel agents (overrides config.toml)
        #[arg(long)]
        max_agents: Option<usize>,

        /// Executor to use for spawned agents (overrides config.toml)
        #[arg(long)]
        executor: Option<String>,

        /// Background poll interval in seconds (overrides config.toml coordinator.poll_interval)
        #[arg(long)]
        interval: Option<u64>,

        /// Model to use for spawned agents (overrides config.toml coordinator.model)
        #[arg(long)]
        model: Option<String>,
    },
}

#[cfg(any(feature = "matrix", feature = "matrix-lite"))]
#[derive(Subcommand)]
enum MatrixCommands {
    /// Start the Matrix message listener
    ///
    /// Listens to configured Matrix room(s) for commands like:
    /// - claim <task> - Claim a task for work
    /// - done <task> - Mark a task as done
    /// - fail <task> [reason] - Mark a task as failed
    /// - input <task> <text> - Add input/log entry to a task
    Listen {
        /// Matrix room to listen in (uses default_room from config if not specified)
        #[arg(long)]
        room: Option<String>,
    },

    /// Send a message to a Matrix room
    Send {
        /// Message to send
        message: String,

        /// Target Matrix room (uses default_room from config if not specified)
        #[arg(long)]
        room: Option<String>,
    },

    /// Show Matrix connection status
    Status,

    /// Login with password (caches access token)
    Login,

    /// Logout and clear cached credentials
    Logout,
}

/// Print custom help output with usage-based ordering
fn print_help(dir: &PathBuf, show_all: bool, alphabetical: bool) {
    use workgraph::config::Config;
    use workgraph::usage::{self, MAX_HELP_COMMANDS};

    // Get subcommand definitions from clap
    let cmd = Cli::command();
    let subcommands: Vec<_> = cmd
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| {
            let name = c.get_name().to_string();
            let about = c.get_about().map(|s| s.to_string()).unwrap_or_default();
            (name, about)
        })
        .collect();

    // Load config for ordering preference
    let config = Config::load(dir).unwrap_or_default();
    let use_alphabetical = alphabetical || config.help.ordering == "alphabetical";

    println!("wg - workgraph task management\n");

    if use_alphabetical {
        // Simple alphabetical listing
        let mut sorted = subcommands.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let to_show = if show_all {
            sorted.len()
        } else {
            MAX_HELP_COMMANDS.min(sorted.len())
        };
        println!("Commands:");
        for (name, about) in sorted.iter().take(to_show) {
            println!("  {:15} {}", name, about);
        }
        if !show_all && sorted.len() > MAX_HELP_COMMANDS {
            println!(
                "  ... and {} more (--help-all)",
                sorted.len() - MAX_HELP_COMMANDS
            );
        }
    } else if config.help.ordering == "curated" || usage::load_command_order(dir).is_none() {
        // Use curated default ordering
        let mut shown = std::collections::HashSet::new();
        let to_show = if show_all {
            subcommands.len()
        } else {
            MAX_HELP_COMMANDS.min(subcommands.len())
        };

        println!("Commands:");
        let mut count = 0;

        // First show commands in curated order
        for &default_cmd in usage::DEFAULT_ORDER {
            if count >= to_show {
                break;
            }
            if let Some((name, about)) = subcommands.iter().find(|(n, _)| n == default_cmd) {
                println!("  {:15} {}", name, about);
                shown.insert(name.clone());
                count += 1;
            }
        }

        // Then show remaining alphabetically
        let mut remaining: Vec<_> = subcommands
            .iter()
            .filter(|(n, _)| !shown.contains(n))
            .collect();
        remaining.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, about) in remaining {
            if count >= to_show {
                break;
            }
            println!("  {:15} {}", name, about);
            count += 1;
        }

        if !show_all && subcommands.len() > MAX_HELP_COMMANDS {
            println!(
                "  ... and {} more (--help-all)",
                subcommands.len() - MAX_HELP_COMMANDS
            );
        }
    } else {
        // Use personalized usage-based ordering with tiers
        let usage_data = usage::load_command_order(dir).unwrap();
        let (frequent, occasional, rare) = usage::group_by_tier(&usage_data);

        let mut shown = 0;
        let max_show = if show_all {
            subcommands.len()
        } else {
            MAX_HELP_COMMANDS
        };

        // Helper to print commands in a tier
        let mut print_tier = |title: &str, tier_cmds: &[&str]| {
            if tier_cmds.is_empty() || shown >= max_show {
                return;
            }
            println!("{}:", title);
            for &cmd_name in tier_cmds {
                if shown >= max_show {
                    break;
                }
                if let Some((_, about)) = subcommands.iter().find(|(n, _)| n == cmd_name) {
                    println!("  {:15} {}", cmd_name, about);
                    shown += 1;
                }
            }
            println!();
        };

        print_tier("Your most-used", &frequent);
        print_tier("Also used", &occasional);

        if show_all {
            print_tier("Less common", &rare);
        } else if shown < max_show && !rare.is_empty() {
            let remaining = max_show - shown;
            let to_show: Vec<&str> = rare.iter().take(remaining).copied().collect();
            if !to_show.is_empty() {
                println!("More commands:");
                for &cmd_name in &to_show {
                    if let Some((_, about)) = subcommands.iter().find(|(n, _)| n == cmd_name) {
                        println!("  {:15} {}", cmd_name, about);
                    }
                }
            }
        }

        let total_cmds = frequent.len() + occasional.len() + rare.len();
        if !show_all && total_cmds > MAX_HELP_COMMANDS {
            // Count commands we didn't show
            let unshown: usize = subcommands
                .iter()
                .filter(|(n, _)| {
                    !frequent.contains(&n.as_str())
                        && !occasional.contains(&n.as_str())
                        && !rare
                            .iter()
                            .take(max_show - frequent.len() - occasional.len())
                            .any(|&r| r == n.as_str())
                })
                .count();
            if unshown > 0 {
                println!("  ... and {} more (--help-all)", unshown);
            }
        }
    }

    println!("\nOptions:");
    println!("  -d, --dir <PATH>    Workgraph directory [default: .workgraph]");
    println!("  -h, --help          Print help (--help-all for all commands)");
    println!("      --alphabetical  Sort commands alphabetically");
    println!("      --json          Output as JSON");
    println!("  -V, --version       Print version");
}

/// Get the command name from a Commands enum variant for usage tracking
fn command_name(cmd: &Commands) -> &'static str {
    match cmd {
        Commands::Init => "init",
        Commands::Add { .. } => "add",
        Commands::Edit { .. } => "edit",
        Commands::Done { .. } => "done",
        Commands::Submit { .. } => "submit",
        Commands::Approve { .. } => "approve",
        Commands::Reject { .. } => "reject",
        Commands::Fail { .. } => "fail",
        Commands::Abandon { .. } => "abandon",
        Commands::Retry { .. } => "retry",
        Commands::Claim { .. } => "claim",
        Commands::Unclaim { .. } => "unclaim",
        Commands::Reclaim { .. } => "reclaim",
        Commands::Ready => "ready",
        Commands::Blocked { .. } => "blocked",
        Commands::WhyBlocked { .. } => "why-blocked",
        Commands::Check => "check",
        Commands::List { .. } => "list",
        Commands::Viz { .. } => "viz",
        Commands::GraphExport { .. } => "graph-export",
        Commands::Cost { .. } => "cost",
        Commands::Coordinate { .. } => "coordinate",
        Commands::Plan { .. } => "plan",
        Commands::Reschedule { .. } => "reschedule",
        Commands::Impact { .. } => "impact",
        Commands::Loops => "loops",
        Commands::Structure => "structure",
        Commands::Bottlenecks => "bottlenecks",
        Commands::Velocity { .. } => "velocity",
        Commands::Aging => "aging",
        Commands::Forecast => "forecast",
        Commands::Workload => "workload",
        Commands::Resources => "resources",
        Commands::CriticalPath => "critical-path",
        Commands::Analyze => "analyze",
        Commands::Archive { .. } => "archive",
        Commands::Show { .. } => "show",
        Commands::Log { .. } => "log",
        Commands::Resource { .. } => "resource",
        Commands::Skill { .. } => "skill",
        Commands::Agency { .. } => "agency",
        Commands::Role { .. } => "role",
        Commands::Motivation { .. } => "motivation",
        Commands::Mot { .. } => "motivation",
        Commands::Assign { .. } => "assign",
        Commands::Match { .. } => "match",
        Commands::Heartbeat { .. } => "heartbeat",
        Commands::Artifact { .. } => "artifact",
        Commands::Context { .. } => "context",
        Commands::Next { .. } => "next",
        Commands::Trajectory { .. } => "trajectory",
        Commands::Exec { .. } => "exec",
        Commands::Agent { .. } => "agent",
        Commands::Spawn { .. } => "spawn",
        Commands::Evaluate { .. } => "evaluate",
        Commands::Evolve { .. } => "evolve",
        Commands::Config { .. } => "config",
        Commands::DeadAgents { .. } => "dead-agents",
        Commands::Agents { .. } => "agents",
        Commands::Kill { .. } => "kill",
        Commands::Service { .. } => "service",
        Commands::Tui { .. } => "tui",
        Commands::Quickstart => "quickstart",
        Commands::Status => "status",
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Notify { .. } => "notify",
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Matrix { .. } => "matrix",
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let workgraph_dir = cli.dir.unwrap_or_else(|| PathBuf::from(".workgraph"));

    // Handle help flags
    if cli.help || cli.help_all || cli.command.is_none() {
        print_help(&workgraph_dir, cli.help_all, cli.alphabetical);
        return Ok(());
    }

    let command = cli.command.unwrap();

    // Track command usage (fire-and-forget, ignores errors)
    workgraph::usage::append_usage_log(&workgraph_dir, command_name(&command));

    match command {
        Commands::Init => commands::init::run(&workgraph_dir),
        Commands::Add {
            title,
            id,
            description,
            blocked_by,
            assign,
            hours,
            cost,
            tag,
            skill,
            input,
            deliverable,
            max_retries,
            model,
            verify,
            loops_to,
            loop_max,
            loop_guard,
            loop_delay,
        } => commands::add::run(
            &workgraph_dir,
            &title,
            id.as_deref(),
            description.as_deref(),
            &blocked_by,
            assign.as_deref(),
            hours,
            cost,
            &tag,
            &skill,
            &input,
            &deliverable,
            max_retries,
            model.as_deref(),
            verify.as_deref(),
            loops_to.as_deref(),
            loop_max,
            loop_guard.as_deref(),
            loop_delay.as_deref(),
        ),
        Commands::Edit {
            id,
            title,
            description,
            add_blocked_by,
            remove_blocked_by,
            add_tag,
            remove_tag,
            model,
            add_skill,
            remove_skill,
            add_loops_to,
            loop_max,
            loop_guard,
            loop_delay,
            remove_loops_to,
            loop_iteration,
        } => commands::edit::run(
            &workgraph_dir,
            &id,
            title.as_deref(),
            description.as_deref(),
            &add_blocked_by,
            &remove_blocked_by,
            &add_tag,
            &remove_tag,
            model.as_deref(),
            &add_skill,
            &remove_skill,
            add_loops_to.as_deref(),
            loop_max,
            loop_guard.as_deref(),
            loop_delay.as_deref(),
            remove_loops_to.as_deref(),
            loop_iteration,
        ),
        Commands::Done { id } => commands::done::run(&workgraph_dir, &id),
        Commands::Submit { id, actor } => {
            commands::submit::run(&workgraph_dir, &id, actor.as_deref())
        }
        Commands::Approve { id, actor } => {
            commands::approve::run(&workgraph_dir, &id, actor.as_deref())
        }
        Commands::Reject { id, reason, actor } => {
            commands::reject::run(&workgraph_dir, &id, reason.as_deref(), actor.as_deref())
        }
        Commands::Fail { id, reason } => {
            commands::fail::run(&workgraph_dir, &id, reason.as_deref())
        }
        Commands::Abandon { id, reason } => {
            commands::abandon::run(&workgraph_dir, &id, reason.as_deref())
        }
        Commands::Retry { id } => commands::retry::run(&workgraph_dir, &id),
        Commands::Claim { id, actor } => {
            commands::claim::claim(&workgraph_dir, &id, actor.as_deref())
        }
        Commands::Unclaim { id } => commands::claim::unclaim(&workgraph_dir, &id),
        Commands::Reclaim { id, from, to } => {
            commands::reclaim::run(&workgraph_dir, &id, &from, &to)
        }
        Commands::Ready => commands::ready::run(&workgraph_dir, cli.json),
        Commands::Blocked { id } => commands::blocked::run(&workgraph_dir, &id, cli.json),
        Commands::WhyBlocked { id } => commands::why_blocked::run(&workgraph_dir, &id, cli.json),
        Commands::Check => commands::check::run(&workgraph_dir),
        Commands::List { status } => {
            commands::list::run(&workgraph_dir, status.as_deref(), cli.json)
        }
        Commands::Viz {
            all,
            status,
            critical_path,
            dot,
            mermaid,
            output,
        } => {
            let fmt = if dot {
                commands::viz::OutputFormat::Dot
            } else if mermaid {
                commands::viz::OutputFormat::Mermaid
            } else {
                commands::viz::OutputFormat::Ascii
            };
            let options = commands::viz::VizOptions {
                all,
                status,
                critical_path,
                format: fmt,
                output,
            };
            commands::viz::run(&workgraph_dir, options)
        }
        Commands::GraphExport {
            archive,
            since,
            until,
        } => commands::graph::run(&workgraph_dir, archive, since.as_deref(), until.as_deref()),
        Commands::Cost { id } => commands::cost::run(&workgraph_dir, &id),
        Commands::Coordinate { max_parallel } => {
            commands::coordinate::run(&workgraph_dir, cli.json, max_parallel)
        }
        Commands::Plan { budget, hours } => {
            commands::plan::run(&workgraph_dir, budget, hours, cli.json)
        }
        Commands::Reschedule { id, after, at } => {
            commands::reschedule::run(&workgraph_dir, &id, after, at.as_deref())
        }
        Commands::Impact { id } => commands::impact::run(&workgraph_dir, &id, cli.json),
        Commands::Loops => commands::loops::run(&workgraph_dir, cli.json),
        Commands::Structure => commands::structure::run(&workgraph_dir, cli.json),
        Commands::Bottlenecks => commands::bottlenecks::run(&workgraph_dir, cli.json),
        Commands::Velocity { weeks } => commands::velocity::run(&workgraph_dir, cli.json, weeks),
        Commands::Aging => commands::aging::run(&workgraph_dir, cli.json),
        Commands::Forecast => commands::forecast::run(&workgraph_dir, cli.json),
        Commands::Workload => commands::workload::run(&workgraph_dir, cli.json),
        Commands::Resources => commands::resources::run(&workgraph_dir, cli.json),
        Commands::CriticalPath => commands::critical_path::run(&workgraph_dir, cli.json),
        Commands::Analyze => commands::analyze::run(&workgraph_dir, cli.json),
        Commands::Archive {
            dry_run,
            older,
            list,
        } => commands::archive::run(&workgraph_dir, dry_run, older.as_deref(), list),
        Commands::Show { id } => commands::show::run(&workgraph_dir, &id, cli.json),
        Commands::Log {
            id,
            message,
            actor,
            list,
        } => {
            if list || message.is_none() {
                commands::log::run_list(&workgraph_dir, &id, cli.json)
            } else {
                commands::log::run_add(
                    &workgraph_dir,
                    &id,
                    message.as_deref().unwrap(),
                    actor.as_deref(),
                )
            }
        }
        Commands::Resource { command } => match command {
            ResourceCommands::Add {
                id,
                name,
                resource_type,
                available,
                unit,
            } => commands::resource::run_add(
                &workgraph_dir,
                &id,
                name.as_deref(),
                resource_type.as_deref(),
                available,
                unit.as_deref(),
            ),
            ResourceCommands::List => commands::resource::run_list(&workgraph_dir, cli.json),
        },
        Commands::Skill { command } => match command {
            SkillCommands::List => commands::skills::run_list(&workgraph_dir, cli.json),
            SkillCommands::Task { id } => commands::skills::run_task(&workgraph_dir, &id, cli.json),
            SkillCommands::Find { skill } => {
                commands::skills::run_find(&workgraph_dir, &skill, cli.json)
            }
            SkillCommands::Install => commands::skills::run_install(),
        },
        Commands::Agency { command } => match command {
            AgencyCommands::Init => {
                let agency_dir = workgraph_dir.join("agency");
                let (roles, motivations) = workgraph::agency::seed_starters(&agency_dir)
                    .context("Failed to seed agency starters")?;
                if roles == 0 && motivations == 0 {
                    println!("Agency already initialized (all starters present).");
                } else {
                    println!(
                        "Seeded agency with {} roles and {} motivations.",
                        roles, motivations
                    );
                }
                Ok(())
            }
            AgencyCommands::Stats { min_evals } => {
                commands::agency_stats::run(&workgraph_dir, cli.json, min_evals)
            }
        },
        Commands::Role { command } => match command {
            RoleCommands::Add {
                name,
                outcome,
                skill,
                description,
            } => commands::role::run_add(
                &workgraph_dir,
                &name,
                &outcome,
                &skill,
                description.as_deref(),
            ),
            RoleCommands::List => commands::role::run_list(&workgraph_dir, cli.json),
            RoleCommands::Show { id } => commands::role::run_show(&workgraph_dir, &id, cli.json),
            RoleCommands::Edit { id } => commands::role::run_edit(&workgraph_dir, &id),
            RoleCommands::Rm { id } => commands::role::run_rm(&workgraph_dir, &id),
            RoleCommands::Lineage { id } => {
                commands::role::run_lineage(&workgraph_dir, &id, cli.json)
            }
        },
        Commands::Motivation { command } | Commands::Mot { command } => match command {
            MotivationCommands::Add {
                name,
                accept,
                reject,
                description,
            } => commands::motivation::run_add(
                &workgraph_dir,
                &name,
                &accept,
                &reject,
                description.as_deref(),
            ),
            MotivationCommands::List => commands::motivation::run_list(&workgraph_dir, cli.json),
            MotivationCommands::Show { id } => {
                commands::motivation::run_show(&workgraph_dir, &id, cli.json)
            }
            MotivationCommands::Edit { id } => commands::motivation::run_edit(&workgraph_dir, &id),
            MotivationCommands::Rm { id } => commands::motivation::run_rm(&workgraph_dir, &id),
            MotivationCommands::Lineage { id } => {
                commands::motivation::run_lineage(&workgraph_dir, &id, cli.json)
            }
        },
        Commands::Assign {
            task,
            agent_hash,
            clear,
        } => commands::assign::run(&workgraph_dir, &task, agent_hash.as_deref(), clear),
        Commands::Match { task } => commands::match_cmd::run(&workgraph_dir, &task, cli.json),
        Commands::Heartbeat {
            actor,
            check,
            agents,
            threshold,
        } => {
            if check || actor.is_none() {
                if agents {
                    commands::heartbeat::run_check_agents(&workgraph_dir, threshold, cli.json)
                } else {
                    commands::heartbeat::run_check(&workgraph_dir, threshold, cli.json)
                }
            } else {
                // Use run_auto to automatically detect agent vs actor
                commands::heartbeat::run_auto(&workgraph_dir, actor.as_deref().unwrap())
            }
        }
        Commands::Artifact { task, path, remove } => {
            if let Some(artifact_path) = path {
                if remove {
                    commands::artifact::run_remove(&workgraph_dir, &task, &artifact_path)
                } else {
                    commands::artifact::run_add(&workgraph_dir, &task, &artifact_path)
                }
            } else {
                commands::artifact::run_list(&workgraph_dir, &task, cli.json)
            }
        }
        Commands::Context { task, dependents } => {
            if dependents {
                commands::context::run_dependents(&workgraph_dir, &task, cli.json)
            } else {
                commands::context::run(&workgraph_dir, &task, cli.json)
            }
        }
        Commands::Next { actor } => commands::next::run(&workgraph_dir, &actor, cli.json),
        Commands::Trajectory { task, actor } => {
            if let Some(actor_id) = actor {
                commands::trajectory::suggest_for_actor(&workgraph_dir, &actor_id, cli.json)
            } else {
                commands::trajectory::run(&workgraph_dir, &task, cli.json)
            }
        }
        Commands::Exec {
            task,
            actor,
            dry_run,
            set,
            clear,
        } => {
            if let Some(cmd) = set {
                commands::exec::set_exec(&workgraph_dir, &task, &cmd)
            } else if clear {
                commands::exec::clear_exec(&workgraph_dir, &task)
            } else {
                commands::exec::run(&workgraph_dir, &task, actor.as_deref(), dry_run)
            }
        }
        Commands::Agent { command } => match command {
            AgentCommands::Create {
                name,
                role,
                motivation,
                capabilities,
                rate,
                capacity,
                trust_level,
                contact,
                executor,
            } => commands::agent_crud::run_create(
                &workgraph_dir,
                &name,
                role.as_deref(),
                motivation.as_deref(),
                &capabilities,
                rate,
                capacity,
                trust_level.as_deref(),
                contact.as_deref(),
                &executor,
            ),
            AgentCommands::List => commands::agent_crud::run_list(&workgraph_dir, cli.json),
            AgentCommands::Show { id } => {
                commands::agent_crud::run_show(&workgraph_dir, &id, cli.json)
            }
            AgentCommands::Rm { id } => commands::agent_crud::run_rm(&workgraph_dir, &id),
            AgentCommands::Lineage { id } => {
                commands::agent_crud::run_lineage(&workgraph_dir, &id, cli.json)
            }
            AgentCommands::Performance { id } => {
                commands::agent_crud::run_performance(&workgraph_dir, &id, cli.json)
            }
            AgentCommands::Run {
                actor,
                once,
                interval,
                max_tasks,
                reset_state,
            } => commands::agent::run(
                &workgraph_dir,
                &actor,
                once,
                interval,
                max_tasks,
                reset_state,
                cli.json,
            ),
        },
        Commands::Spawn {
            task,
            executor,
            timeout,
            model,
        } => commands::spawn::run(
            &workgraph_dir,
            &task,
            &executor,
            timeout.as_deref(),
            model.as_deref(),
            cli.json,
        ),
        Commands::Evaluate {
            task,
            evaluator_model,
            dry_run,
        } => commands::evaluate::run(
            &workgraph_dir,
            &task,
            evaluator_model.as_deref(),
            dry_run,
            cli.json,
        ),
        Commands::Evolve {
            dry_run,
            strategy,
            budget,
            model,
        } => commands::evolve::run(
            &workgraph_dir,
            dry_run,
            strategy.as_deref(),
            budget,
            model.as_deref(),
            cli.json,
        ),
        Commands::Config {
            show,
            init,
            executor,
            model,
            set_interval,
            max_agents,
            coordinator_interval,
            poll_interval,
            coordinator_executor,
            matrix,
            homeserver,
            username,
            password,
            access_token,
            room,
            auto_evaluate,
            auto_assign,
            assigner_model,
            evaluator_model,
            evolver_model,
            assigner_agent,
            evaluator_agent,
            evolver_agent,
            retention_heuristics,
            auto_triage,
            triage_model,
            triage_timeout,
            triage_max_log_bytes,
        } => {
            // Handle Matrix configuration
            if matrix
                || homeserver.is_some()
                || username.is_some()
                || password.is_some()
                || access_token.is_some()
                || room.is_some()
            {
                let has_matrix_updates = homeserver.is_some()
                    || username.is_some()
                    || password.is_some()
                    || access_token.is_some()
                    || room.is_some();

                if has_matrix_updates {
                    commands::config_cmd::update_matrix(
                        homeserver.as_deref(),
                        username.as_deref(),
                        password.as_deref(),
                        access_token.as_deref(),
                        room.as_deref(),
                    )
                } else {
                    commands::config_cmd::show_matrix(cli.json)
                }
            } else if init {
                commands::config_cmd::init(&workgraph_dir)
            } else if show
                || (executor.is_none()
                    && model.is_none()
                    && set_interval.is_none()
                    && max_agents.is_none()
                    && coordinator_interval.is_none()
                    && poll_interval.is_none()
                    && coordinator_executor.is_none()
                    && auto_evaluate.is_none()
                    && auto_assign.is_none()
                    && assigner_model.is_none()
                    && evaluator_model.is_none()
                    && evolver_model.is_none()
                    && assigner_agent.is_none()
                    && evaluator_agent.is_none()
                    && evolver_agent.is_none()
                    && retention_heuristics.is_none()
                    && auto_triage.is_none()
                    && triage_model.is_none()
                    && triage_timeout.is_none()
                    && triage_max_log_bytes.is_none())
            {
                commands::config_cmd::show(&workgraph_dir, cli.json)
            } else {
                commands::config_cmd::update(
                    &workgraph_dir,
                    executor.as_deref(),
                    model.as_deref(),
                    set_interval,
                    max_agents,
                    coordinator_interval,
                    poll_interval,
                    coordinator_executor.as_deref(),
                    auto_evaluate,
                    auto_assign,
                    assigner_model.as_deref(),
                    evaluator_model.as_deref(),
                    evolver_model.as_deref(),
                    assigner_agent.as_deref(),
                    evaluator_agent.as_deref(),
                    evolver_agent.as_deref(),
                    retention_heuristics.as_deref(),
                    auto_triage,
                    triage_model.as_deref(),
                    triage_timeout,
                    triage_max_log_bytes,
                )
            }
        }
        Commands::DeadAgents {
            check: _,
            cleanup,
            remove,
            processes,
            threshold,
        } => {
            if processes {
                commands::dead_agents::run_check_processes(&workgraph_dir, cli.json)
            } else if remove {
                commands::dead_agents::run_remove_dead(&workgraph_dir, cli.json).map(|_| ())
            } else if cleanup {
                commands::dead_agents::run_cleanup(&workgraph_dir, threshold, cli.json).map(|_| ())
            } else {
                // Default to check
                commands::dead_agents::run_check(&workgraph_dir, threshold, cli.json)
            }
        }
        Commands::Agents {
            alive,
            dead,
            working,
            idle,
        } => {
            let filter = if alive {
                Some(commands::agents::AgentFilter::Alive)
            } else if dead {
                Some(commands::agents::AgentFilter::Dead)
            } else if working {
                Some(commands::agents::AgentFilter::Working)
            } else if idle {
                Some(commands::agents::AgentFilter::Idle)
            } else {
                None
            };
            commands::agents::run(&workgraph_dir, filter, cli.json)
        }
        Commands::Kill { agent, force, all } => {
            if all {
                commands::kill::run_all(&workgraph_dir, force, cli.json)
            } else if let Some(agent_id) = agent {
                commands::kill::run(&workgraph_dir, &agent_id, force, cli.json)
            } else {
                anyhow::bail!("Must specify an agent ID or use --all")
            }
        }
        Commands::Service { command } => match command {
            ServiceCommands::Start {
                port,
                socket,
                max_agents,
                executor,
                interval,
                model,
            } => commands::service::run_start(
                &workgraph_dir,
                socket.as_deref(),
                port,
                max_agents,
                executor.as_deref(),
                interval,
                model.as_deref(),
                cli.json,
            ),
            ServiceCommands::Stop { force, kill_agents } => {
                commands::service::run_stop(&workgraph_dir, force, kill_agents, cli.json)
            }
            ServiceCommands::Status => commands::service::run_status(&workgraph_dir, cli.json),
            ServiceCommands::Reload {
                max_agents,
                executor,
                interval,
                model,
            } => commands::service::run_reload(
                &workgraph_dir,
                max_agents,
                executor.as_deref(),
                interval,
                model.as_deref(),
                cli.json,
            ),
            ServiceCommands::Pause => commands::service::run_pause(&workgraph_dir, cli.json),
            ServiceCommands::Resume => commands::service::run_resume(&workgraph_dir, cli.json),
            ServiceCommands::Install => commands::service::generate_systemd_service(&workgraph_dir),
            ServiceCommands::Tick {
                max_agents,
                executor,
                model,
            } => commands::service::run_tick(
                &workgraph_dir,
                max_agents,
                executor.as_deref(),
                model.as_deref(),
            ),
            ServiceCommands::Daemon {
                socket,
                max_agents,
                executor,
                interval,
                model,
            } => commands::service::run_daemon(
                &workgraph_dir,
                &socket,
                max_agents,
                executor.as_deref(),
                interval,
                model.as_deref(),
            ),
        },
        Commands::Tui { refresh_rate } => tui::run(workgraph_dir, refresh_rate),
        Commands::Quickstart => commands::quickstart::run(cli.json),
        Commands::Status => commands::status::run(&workgraph_dir, cli.json),
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Notify {
            task,
            room,
            message,
        } => commands::notify::run(
            &workgraph_dir,
            &task,
            room.as_deref(),
            message.as_deref(),
            cli.json,
        ),
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Matrix { command } => match command {
            MatrixCommands::Listen { room } => {
                commands::matrix::run_listen(&workgraph_dir, room.as_deref())
            }
            MatrixCommands::Send { message, room } => {
                commands::matrix::run_send(&workgraph_dir, room.as_deref(), &message)
            }
            MatrixCommands::Status => commands::matrix::run_status(&workgraph_dir, cli.json),
            MatrixCommands::Login => commands::matrix::run_login(&workgraph_dir),
            MatrixCommands::Logout => commands::matrix::run_logout(&workgraph_dir),
        },
    }
}

#![warn(clippy::redundant_closure)]

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::path::{Path, PathBuf};

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

        /// Create the task in a peer workgraph (by name or path)
        #[arg(long)]
        repo: Option<String>,

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

    /// Mark a task as done
    Done {
        /// Task ID to mark as done
        id: String,

        /// Signal that the task's iterative loop has converged (stops loop edges from firing)
        #[arg(long)]
        converged: bool,
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

    /// Pause a task (coordinator will skip it until resumed)
    Pause {
        /// Task ID to pause
        id: String,
    },

    /// Resume a paused task
    Resume {
        /// Task ID to resume
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

        /// Only show paused tasks
        #[arg(long)]
        paused: bool,
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
        #[arg(long, conflicts_with_all = ["mermaid", "graph"])]
        dot: bool,

        /// Output Mermaid diagram format
        #[arg(long, conflicts_with_all = ["dot", "graph"])]
        mermaid: bool,

        /// Output 2D spatial graph with box-drawing characters
        #[arg(long, conflicts_with_all = ["dot", "mermaid"])]
        graph: bool,

        /// Render directly to file (requires dot installed)
        #[arg(long, short)]
        output: Option<String>,

        /// Show internal tasks (assign-*, evaluate-*) normally hidden
        #[arg(long)]
        show_internal: bool,
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

    /// Show coordination status: ready tasks, in-progress tasks, and opportunities
    /// for parallel execution. Useful for sprint planning or standup reviews.
    Coordinate {
        /// Maximum number of parallel tasks to show
        #[arg(long)]
        max_parallel: Option<usize>,
    },

    /// Plan what work fits within a budget or hour constraint. Lists tasks by
    /// priority that can be accomplished with the given resources.
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

    /// Analyze graph structure: entry points (no dependencies), dead ends
    /// (nothing depends on them), fan-out (tasks blocking many others),
    /// and high-impact root tasks.
    Structure,

    /// Find tasks blocking the most downstream work. Ranks tasks by how
    /// many other tasks are transitively waiting on them.
    Bottlenecks,

    /// Show task completion velocity: tasks completed per week over a
    /// rolling window. Helps gauge team throughput and trends.
    Velocity {
        /// Number of weeks to show (default: 4)
        #[arg(long)]
        weeks: Option<usize>,
    },

    /// Show task age distribution: how long open/in-progress tasks have
    /// been waiting. Highlights stale work that may need attention.
    Aging,

    /// Forecast project completion date based on recent velocity and
    /// remaining open tasks. Uses linear extrapolation.
    Forecast,

    /// Show agent workload balance: how many tasks each agent has claimed
    /// or completed, to identify over/under-utilization.
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

    /// Garbage collect terminal tasks (failed, abandoned) from the graph
    Gc {
        /// Show what would be removed without actually removing
        #[arg(long)]
        dry_run: bool,

        /// Also remove done tasks (by default only failed+abandoned)
        #[arg(long)]
        include_done: bool,
    },

    /// Show detailed information about a single task
    Show {
        /// Task ID
        id: String,
    },

    /// Trace commands: execution history and trace functions
    Trace {
        #[command(subcommand)]
        command: TraceCommands,
    },

    /// Replay tasks: snapshot graph, selectively reset tasks, re-execute with a different model
    Replay {
        /// Model to use for replayed tasks
        #[arg(long)]
        model: Option<String>,

        /// Only reset Failed/Abandoned tasks
        #[arg(long)]
        failed_only: bool,

        /// Only reset tasks with evaluation score below this threshold
        #[arg(long)]
        below_score: Option<f64>,

        /// Reset specific tasks (comma-separated) plus their transitive dependents
        #[arg(long, value_delimiter = ',')]
        tasks: Vec<String>,

        /// Preserve Done tasks scoring above this threshold (default: 0.9)
        #[arg(long)]
        keep_done: Option<f64>,

        /// Dry run: show what would be reset without making changes
        #[arg(long)]
        plan_only: bool,

        /// Only replay tasks in this subgraph (rooted at given task)
        #[arg(long)]
        subgraph: Option<String>,
    },

    /// Manage run snapshots (list, show, restore, diff)
    Runs {
        #[command(subcommand)]
        command: RunsCommands,
    },

    /// Add progress log/notes to a task
    Log {
        /// Task ID (not required with --operations)
        id: Option<String>,

        /// Log message (if not provided, lists log entries)
        message: Option<String>,

        /// Actor adding the log entry
        #[arg(long)]
        actor: Option<String>,

        /// List log entries instead of adding
        #[arg(long)]
        list: bool,

        /// Show archived agent prompts and outputs for a task
        #[arg(long)]
        agent: bool,

        /// Show the operations log (reads current and rotated files)
        #[arg(long)]
        operations: bool,
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

    /// Manage peer workgraph instances for cross-repo communication
    Peer {
        #[command(subcommand)]
        command: PeerCommands,
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
        agent: Option<String>,

        /// Check for stale agents (no heartbeat within threshold)
        #[arg(long)]
        check: bool,

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

        /// Executor to use (claude, amplifier, shell, or custom config name)
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

        /// Target global config (~/.workgraph/config.toml) instead of local
        #[arg(long, conflicts_with = "local")]
        global: bool,

        /// Explicitly target local config (default for writes)
        #[arg(long, conflicts_with = "global")]
        local: bool,

        /// Show merged config with source annotations (global/local/default)
        #[arg(long)]
        list: bool,

        /// Set executor (claude, amplifier, shell, or custom config name)
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
        /// Mark dead agents and unclaim their tasks
        #[arg(long)]
        cleanup: bool,

        /// Remove dead agents from registry
        #[arg(long)]
        remove: bool,

        /// Check if agent processes are still running
        #[arg(long)]
        processes: bool,

        /// Purge dead/done/failed agents from registry (and optionally delete dirs)
        #[arg(long)]
        purge: bool,

        /// Also delete agent work directories (.workgraph/agents/<id>/) when purging
        #[arg(long, requires = "purge")]
        delete_dirs: bool,

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

    /// Interactive configuration wizard for first-time setup
    Setup,

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
enum TraceCommands {
    /// Show the execution history of a task
    Show {
        /// Task ID to trace
        id: String,

        /// Show complete agent conversation output
        #[arg(long)]
        full: bool,

        /// Show only provenance log entries for this task
        #[arg(long)]
        ops_only: bool,

        /// Show the full recursive execution tree (all descendant tasks)
        #[arg(long)]
        recursive: bool,

        /// Show chronological timeline with parallel execution lanes (requires --recursive)
        #[arg(long)]
        timeline: bool,

        /// Render the trace subgraph as a 2D box layout
        #[arg(long)]
        graph: bool,

        /// Animate the trace: replay graph evolution over time in the terminal
        #[arg(long)]
        animate: bool,

        /// Playback speed multiplier for --animate (default: 10)
        #[arg(long, default_value = "10.0")]
        speed: f64,
    },

    /// List available trace functions
    #[command(name = "list-functions")]
    ListFunctions {
        /// Show input parameters and task templates
        #[arg(long)]
        verbose: bool,

        /// Include functions from federated peer workgraphs
        #[arg(long)]
        include_peers: bool,
    },

    /// Show details of a trace function
    #[command(name = "show-function")]
    ShowFunction {
        /// Function ID (prefix match supported)
        id: String,
    },

    /// Extract a trace function from a completed task
    Extract {
        /// Task ID to extract from
        task_id: String,

        /// Function name/ID (default: derived from task ID)
        #[arg(long)]
        name: Option<String>,

        /// Include all subtasks (tasks blocked by this one) in the function
        #[arg(long)]
        subgraph: bool,

        /// Recursively extract the entire spawned subgraph with dependency structure,
        /// human intervention tracking, and parameterized templates
        #[arg(long)]
        recursive: bool,

        /// Use LLM to generalize descriptions (not yet wired)
        #[arg(long)]
        generalize: bool,

        /// Write to specific path instead of .workgraph/functions/<name>.yaml
        #[arg(long)]
        output: Option<String>,

        /// Overwrite existing function with same name
        #[arg(long)]
        force: bool,
    },

    /// Create tasks from a trace function with provided inputs
    Instantiate {
        /// Function ID (prefix match supported)
        function_id: String,

        /// Load function from a peer workgraph (peer:function-id) or file path
        #[arg(long)]
        from: Option<String>,

        /// Set an input parameter (repeatable, format: key=value)
        #[arg(long = "input", num_args = 1)]
        inputs: Vec<String>,

        /// Read inputs from a YAML/JSON file
        #[arg(long = "input-file")]
        input_file: Option<String>,

        /// Override the task ID prefix (default: from feature_name input)
        #[arg(long)]
        prefix: Option<String>,

        /// Show what tasks would be created without creating them
        #[arg(long)]
        dry_run: bool,

        /// Make all root tasks depend on this task (repeatable)
        #[arg(long = "blocked-by")]
        blocked_by: Vec<String>,

        /// Set model for all created tasks
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum RunsCommands {
    /// List all run snapshots
    List,

    /// Show details of a specific run
    Show {
        /// Run ID (e.g., run-001)
        id: String,
    },

    /// Restore graph from a run snapshot
    Restore {
        /// Run ID to restore from
        id: String,
    },

    /// Diff current graph against a run snapshot
    Diff {
        /// Run ID to diff against
        id: String,
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

        /// Group stats by model (shows per-model score breakdown)
        #[arg(long)]
        by_model: bool,
    },

    /// Scan filesystem for agency stores
    Scan {
        /// Root directory to scan
        root: String,

        /// Maximum recursion depth
        #[arg(long, default_value = "10")]
        max_depth: usize,
    },

    /// Pull entities from another agency store into local
    Pull {
        /// Source store (path, named remote, or directory)
        source: String,

        /// Only pull specific entity IDs (prefix match)
        #[arg(long = "entity", value_delimiter = ',')]
        entity_ids: Vec<String>,

        /// Only pull entities of this type (role, motivation, agent)
        #[arg(long = "type")]
        entity_type: Option<String>,

        /// Show what would be pulled without writing
        #[arg(long)]
        dry_run: bool,

        /// Skip merging performance data (copy definitions only)
        #[arg(long)]
        no_performance: bool,

        /// Skip copying evaluation JSON files
        #[arg(long)]
        no_evaluations: bool,

        /// Overwrite local metadata instead of merging
        #[arg(long)]
        force: bool,

        /// Pull into ~/.workgraph/agency/ instead of local project
        #[arg(long)]
        global: bool,
    },

    /// Merge entities from multiple agency stores
    Merge {
        /// Source stores (paths, named remotes, or directories)
        sources: Vec<String>,

        /// Merge into a specific target path instead of local project
        #[arg(long)]
        into: Option<String>,

        /// Show what would be merged without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Manage named references to other agency stores
    Remote {
        #[command(subcommand)]
        command: RemoteCommands,
    },

    /// Push local entities to another agency store
    Push {
        /// Target store (path, named remote, or directory)
        target: String,

        /// Only push specific entity IDs
        #[arg(long = "entity", value_delimiter = ',')]
        entity_ids: Vec<String>,

        /// Only push entities of this type (role, motivation, agent)
        #[arg(long = "type")]
        entity_type: Option<String>,

        /// Show what would be pushed without writing
        #[arg(long)]
        dry_run: bool,

        /// Skip merging performance data (copy definitions only)
        #[arg(long)]
        no_performance: bool,

        /// Skip copying evaluation JSON files
        #[arg(long)]
        no_evaluations: bool,

        /// Overwrite target metadata instead of merging
        #[arg(long)]
        force: bool,

        /// Push from ~/.workgraph/agency/ instead of local project
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand)]
enum RemoteCommands {
    /// Add a named remote agency store
    Add {
        /// Remote name
        name: String,

        /// Path to the agency store
        path: String,

        /// Description of this remote
        #[arg(long, short = 'd')]
        description: Option<String>,
    },

    /// Remove a named remote
    Remove {
        /// Remote name to remove
        name: String,
    },

    /// List all configured remotes
    List,

    /// Show details of a remote including entity counts
    Show {
        /// Remote name
        name: String,
    },
}

#[derive(Subcommand)]
enum PeerCommands {
    /// Register a peer workgraph instance
    Add {
        /// Peer name (used as shorthand reference)
        name: String,

        /// Path to the peer project (containing .workgraph/)
        path: String,

        /// Description of this peer
        #[arg(long, short = 'd')]
        description: Option<String>,
    },

    /// Remove a registered peer
    Remove {
        /// Peer name to remove
        name: String,
    },

    /// List all configured peers with service status
    List,

    /// Show detailed info about a peer
    Show {
        /// Peer name
        name: String,
    },

    /// Quick health check of all peers
    Status,
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

        /// Kill existing daemon before starting (prevents stacked daemons)
        #[arg(long)]
        force: bool,
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
fn print_help(dir: &Path, show_all: bool, alphabetical: bool) {
    use workgraph::config::Config;
    use workgraph::usage::{self, MAX_HELP_COMMANDS};

    // Get subcommand definitions from clap
    let cmd = Cli::command();
    let subcommands: Vec<_> = cmd
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| {
            let name = c.get_name().to_string();
            let about = c
                .get_about()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            (name, about)
        })
        .collect();

    // Load config for ordering preference
    let config = Config::load_or_default(dir);
    let use_alphabetical = alphabetical || config.help.ordering == "alphabetical";

    println!("wg - workgraph task management\n");

    if use_alphabetical {
        // Simple alphabetical listing
        let mut sorted = subcommands;
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
    } else if config.help.ordering == "curated" {
        print_curated_help(&subcommands, show_all);
    } else if let Some(usage_data) = usage::load_command_order(dir) {
        // Use personalized usage-based ordering with tiers
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
    } else {
        // No usage data and not curated — fall back to curated ordering
        print_curated_help(&subcommands, show_all);
    }

    println!("\nOptions:");
    println!("  -d, --dir <PATH>    Workgraph directory [default: .workgraph]");
    println!("  -h, --help          Print help (--help-all for all commands)");
    println!("      --alphabetical  Sort commands alphabetically");
    println!("      --json          Output as JSON");
    println!("  -V, --version       Print version");
}

/// Print commands using the curated default ordering, with remaining commands shown alphabetically.
fn print_curated_help(subcommands: &[(String, String)], show_all: bool) {
    use workgraph::usage::{self, MAX_HELP_COMMANDS};

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
}

/// Get the command name from a Commands enum variant for usage tracking
fn command_name(cmd: &Commands) -> &'static str {
    match cmd {
        Commands::Init => "init",
        Commands::Add { .. } => "add",
        Commands::Edit { .. } => "edit",
        Commands::Done { .. } => "done",
        Commands::Fail { .. } => "fail",
        Commands::Abandon { .. } => "abandon",
        Commands::Retry { .. } => "retry",
        Commands::Claim { .. } => "claim",
        Commands::Unclaim { .. } => "unclaim",
        Commands::Pause { .. } => "pause",
        Commands::Resume { .. } => "resume",
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
        Commands::Gc { .. } => "gc",
        Commands::Show { .. } => "show",
        Commands::Trace { .. } => "trace",
        Commands::Replay { .. } => "replay",
        Commands::Runs { .. } => "runs",
        Commands::Log { .. } => "log",
        Commands::Resource { .. } => "resource",
        Commands::Skill { .. } => "skill",
        Commands::Agency { .. } => "agency",
        Commands::Peer { .. } => "peer",
        Commands::Role { .. } => "role",
        Commands::Motivation { .. } => "motivation",
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
        Commands::Setup => "setup",
        Commands::Quickstart => "quickstart",
        Commands::Status => "status",
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Notify { .. } => "notify",
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Matrix { .. } => "matrix",
    }
}

/// Returns true if the command supports `--json` output.
fn supports_json(cmd: &Commands) -> bool {
    matches!(
        cmd,
        Commands::Ready
            | Commands::Blocked { .. }
            | Commands::WhyBlocked { .. }
            | Commands::List { .. }
            | Commands::Coordinate { .. }
            | Commands::Plan { .. }
            | Commands::Impact { .. }
            | Commands::Loops
            | Commands::Structure
            | Commands::Bottlenecks
            | Commands::Velocity { .. }
            | Commands::Aging
            | Commands::Forecast
            | Commands::Workload
            | Commands::Resources
            | Commands::CriticalPath
            | Commands::Analyze
            | Commands::Archive { .. }
            | Commands::Gc { .. }
            | Commands::Show { .. }
            | Commands::Trace { .. }
            | Commands::Replay { .. }
            | Commands::Runs { .. }
            | Commands::Log { .. }
            | Commands::Resource { .. }
            | Commands::Skill { .. }
            | Commands::Agency { .. }
            | Commands::Peer { .. }
            | Commands::Role { .. }
            | Commands::Motivation { .. }
            | Commands::Match { .. }
            | Commands::Heartbeat { .. }
            | Commands::Artifact { .. }
            | Commands::Context { .. }
            | Commands::Next { .. }
            | Commands::Trajectory { .. }
            | Commands::Agent { .. }
            | Commands::Evaluate { .. }
            | Commands::Evolve { .. }
            | Commands::Config { .. }
            | Commands::DeadAgents { .. }
            | Commands::Agents { .. }
            | Commands::Kill { .. }
            | Commands::Service { .. }
            | Commands::Cost { .. }
            | Commands::Check
            | Commands::Quickstart
            | Commands::Status
    ) || {
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        {
            matches!(cmd, Commands::Notify { .. } | Commands::Matrix { .. })
        }
        #[cfg(not(any(feature = "matrix", feature = "matrix-lite")))]
        {
            false
        }
    }
}

/// Check if the user is requesting help for a specific subcommand (e.g., `wg show --help`).
///
/// Because we use `disable_help_flag = true` for the custom top-level help system,
/// clap doesn't intercept `--help` at the subcommand level. This function pre-scans
/// raw args and, if a subcommand + help flag is detected, prints clap's native help
/// for that subcommand.
fn maybe_print_subcommand_help() -> bool {
    let args: Vec<String> = std::env::args().collect();

    // Check if --help or -h appears alongside a subcommand
    let has_help = args.iter().any(|a| a == "--help" || a == "-h");
    if !has_help {
        return false;
    }

    // Build the clap command to get subcommand names
    let cmd = Cli::command();
    let subcmd_names: Vec<String> = cmd
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();

    // Find which subcommand is being referenced (skip argv[0], skip flags)
    let subcmd = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-') && subcmd_names.contains(a));

    if let Some(subcmd_name) = subcmd {
        // Extract the subcommand from clap and print its help directly
        let cmd = Cli::command();
        if let Some(sub) = cmd.get_subcommands().find(|c| c.get_name() == subcmd_name) {
            let mut sub = sub.clone().disable_help_flag(false);
            sub.print_help().ok();
            println!();
            std::process::exit(0);
        }
    }

    false
}

fn main() -> Result<()> {
    // Handle subcommand-level help before clap parses (since we disable_help_flag globally)
    maybe_print_subcommand_help();

    let cli = Cli::parse();

    let workgraph_dir = cli.dir.unwrap_or_else(|| PathBuf::from(".workgraph"));

    // Handle help flags (top-level custom help with usage-based ordering)
    if cli.help || cli.help_all || cli.command.is_none() {
        print_help(&workgraph_dir, cli.help_all, cli.alphabetical);
        return Ok(());
    }

    let command = match cli.command {
        Some(c) => c,
        None => return Ok(()),
    };

    // Warn if --json is passed to a command that doesn't support it
    if cli.json && !supports_json(&command) {
        eprintln!(
            "Warning: --json flag is not supported by 'wg {}' and will be ignored",
            command_name(&command)
        );
    }

    // Track command usage (fire-and-forget, ignores errors)
    workgraph::usage::append_usage_log(&workgraph_dir, command_name(&command));

    match command {
        Commands::Init => commands::init::run(&workgraph_dir),
        Commands::Add {
            title,
            id,
            description,
            repo,
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
        } => {
            if let Some(ref peer_ref) = repo {
                commands::add::run_remote(
                    &workgraph_dir,
                    peer_ref,
                    &title,
                    id.as_deref(),
                    description.as_deref(),
                    &blocked_by,
                    &tag,
                    &skill,
                    &deliverable,
                    model.as_deref(),
                    verify.as_deref(),
                )
            } else {
                commands::add::run(
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
                )
            }
        }
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
        Commands::Done { id, converged } => commands::done::run(&workgraph_dir, &id, converged),
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
        Commands::Pause { id } => commands::pause::run(&workgraph_dir, &id),
        Commands::Resume { id } => commands::resume::run(&workgraph_dir, &id),
        Commands::Reclaim { id, from, to } => {
            commands::reclaim::run(&workgraph_dir, &id, &from, &to)
        }
        Commands::Ready => commands::ready::run(&workgraph_dir, cli.json),
        Commands::Blocked { id } => commands::blocked::run(&workgraph_dir, &id, cli.json),
        Commands::WhyBlocked { id } => commands::why_blocked::run(&workgraph_dir, &id, cli.json),
        Commands::Check => commands::check::run(&workgraph_dir, cli.json),
        Commands::List { status, paused } => {
            commands::list::run(&workgraph_dir, status.as_deref(), paused, cli.json)
        }
        Commands::Viz {
            all,
            status,
            critical_path,
            dot,
            mermaid,
            graph,
            output,
            show_internal,
        } => {
            let fmt = if dot {
                commands::viz::OutputFormat::Dot
            } else if mermaid {
                commands::viz::OutputFormat::Mermaid
            } else if graph {
                commands::viz::OutputFormat::Graph
            } else {
                commands::viz::OutputFormat::Ascii
            };
            let options = commands::viz::VizOptions {
                all,
                status,
                critical_path,
                format: fmt,
                output,
                show_internal,
            };
            commands::viz::run(&workgraph_dir, &options)
        }
        Commands::GraphExport {
            archive,
            since,
            until,
        } => commands::graph::run(&workgraph_dir, archive, since.as_deref(), until.as_deref()),
        Commands::Cost { id } => commands::cost::run(&workgraph_dir, &id, cli.json),
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
        } => commands::archive::run(&workgraph_dir, dry_run, older.as_deref(), list, cli.json),
        Commands::Gc {
            dry_run,
            include_done,
        } => commands::gc::run(&workgraph_dir, dry_run, include_done),
        Commands::Show { id } => commands::show::run(&workgraph_dir, &id, cli.json),
        Commands::Trace { command } => match command {
            TraceCommands::Show { id, full, ops_only, recursive, timeline, graph, animate, speed } => {
                if animate {
                    commands::trace_animate::run(&workgraph_dir, &id, speed)
                } else if graph {
                    commands::trace::run_graph(&workgraph_dir, &id)
                } else if recursive || timeline {
                    commands::trace::run_recursive(&workgraph_dir, &id, timeline, cli.json)
                } else {
                    let mode = if cli.json {
                        commands::trace::TraceMode::Json
                    } else if full {
                        commands::trace::TraceMode::Full
                    } else if ops_only {
                        commands::trace::TraceMode::OpsOnly
                    } else {
                        commands::trace::TraceMode::Summary
                    };
                    commands::trace::run(&workgraph_dir, &id, mode)
                }
            }
            TraceCommands::ListFunctions { verbose, include_peers } => {
                commands::trace_function_cmd::run_list(&workgraph_dir, cli.json, verbose, include_peers)
            }
            TraceCommands::ShowFunction { id } => {
                commands::trace_function_cmd::run_show(&workgraph_dir, &id, cli.json)
            }
            TraceCommands::Extract {
                task_id,
                name,
                subgraph,
                recursive,
                generalize,
                output,
                force,
            } => commands::trace_extract::run(
                &workgraph_dir,
                &task_id,
                name.as_deref(),
                subgraph || recursive,
                generalize,
                output.as_deref(),
                force,
            ),
            TraceCommands::Instantiate {
                function_id,
                from,
                inputs,
                input_file,
                prefix,
                dry_run,
                blocked_by,
                model,
            } => commands::trace_instantiate::run(
                &workgraph_dir,
                &function_id,
                from.as_deref(),
                &inputs,
                input_file.as_deref(),
                prefix.as_deref(),
                dry_run,
                &blocked_by,
                model.as_deref(),
                cli.json,
            ),
        },
        Commands::Replay {
            model,
            failed_only,
            below_score,
            tasks,
            keep_done,
            plan_only,
            subgraph,
        } => {
            let opts = commands::replay::ReplayOptions {
                model,
                failed_only,
                below_score,
                tasks,
                keep_done,
                plan_only,
                subgraph,
            };
            commands::replay::run(&workgraph_dir, &opts, cli.json)
        }
        Commands::Runs { command } => match command {
            RunsCommands::List => commands::runs_cmd::run_list(&workgraph_dir, cli.json),
            RunsCommands::Show { id } => {
                commands::runs_cmd::run_show(&workgraph_dir, &id, cli.json)
            }
            RunsCommands::Restore { id } => {
                commands::runs_cmd::run_restore(&workgraph_dir, &id, cli.json)
            }
            RunsCommands::Diff { id } => {
                commands::runs_cmd::run_diff(&workgraph_dir, &id, cli.json)
            }
        },
        Commands::Log {
            id,
            message,
            actor,
            list,
            agent,
            operations,
        } => {
            if operations {
                commands::log::run_operations(&workgraph_dir, cli.json)
            } else {
                let id = id.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Task ID is required (use --operations to view the operations log)"
                    )
                })?;
                if agent {
                    commands::log::run_agent(&workgraph_dir, id, cli.json)
                } else if let (false, Some(msg)) = (list, &message) {
                    commands::log::run_add(&workgraph_dir, id, msg, actor.as_deref())
                } else {
                    commands::log::run_list(&workgraph_dir, id, cli.json)
                }
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
            AgencyCommands::Init => commands::agency_init::run(&workgraph_dir),
            AgencyCommands::Stats {
                min_evals,
                by_model,
            } => commands::agency_stats::run(&workgraph_dir, cli.json, min_evals, by_model),
            AgencyCommands::Scan { root, max_depth } => {
                let root_path = std::path::PathBuf::from(&root);
                commands::agency_scan::run(&root_path, cli.json, max_depth)
            }
            AgencyCommands::Pull {
                source,
                entity_ids,
                entity_type,
                dry_run,
                no_performance,
                no_evaluations,
                force,
                global,
            } => {
                let opts = commands::agency_pull::PullOptions {
                    source,
                    dry_run,
                    no_performance,
                    no_evaluations,
                    force,
                    global,
                    entity_ids,
                    entity_type,
                    json: cli.json,
                };
                commands::agency_pull::run(&workgraph_dir, &opts)
            }
            AgencyCommands::Merge {
                sources,
                into,
                dry_run,
            } => {
                let opts = commands::agency_merge::MergeOptions {
                    sources,
                    into,
                    dry_run,
                    json: cli.json,
                };
                commands::agency_merge::run(&workgraph_dir, &opts)
            }
            AgencyCommands::Remote { command } => match command {
                RemoteCommands::Add {
                    name,
                    path,
                    description,
                } => commands::agency_remote::run_add(
                    &workgraph_dir,
                    &name,
                    &path,
                    description.as_deref(),
                ),
                RemoteCommands::Remove { name } => {
                    commands::agency_remote::run_remove(&workgraph_dir, &name)
                }
                RemoteCommands::List => commands::agency_remote::run_list(&workgraph_dir, cli.json),
                RemoteCommands::Show { name } => {
                    commands::agency_remote::run_show(&workgraph_dir, &name, cli.json)
                }
            },
            AgencyCommands::Push {
                target,
                entity_ids,
                entity_type,
                dry_run,
                no_performance,
                no_evaluations,
                force,
                global,
            } => commands::agency_push::run(
                &workgraph_dir,
                &commands::agency_push::PushOptions {
                    target: &target,
                    dry_run,
                    no_performance,
                    no_evaluations,
                    force,
                    global,
                    entity_ids: &entity_ids,
                    entity_type: entity_type.as_deref(),
                    json: cli.json,
                },
            ),
        },
        Commands::Peer { command } => match command {
            PeerCommands::Add {
                name,
                path,
                description,
            } => commands::peer::run_add(
                &workgraph_dir,
                &name,
                &path,
                description.as_deref(),
            ),
            PeerCommands::Remove { name } => {
                commands::peer::run_remove(&workgraph_dir, &name)
            }
            PeerCommands::List => commands::peer::run_list(&workgraph_dir, cli.json),
            PeerCommands::Show { name } => {
                commands::peer::run_show(&workgraph_dir, &name, cli.json)
            }
            PeerCommands::Status => commands::peer::run_status(&workgraph_dir, cli.json),
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
        Commands::Motivation { command } => match command {
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
            agent,
            check,
            threshold,
            ..
        } => {
            if let (false, Some(a)) = (check, &agent) {
                commands::heartbeat::run_auto(&workgraph_dir, a)
            } else {
                commands::heartbeat::run_check_agents(&workgraph_dir, threshold, cli.json)
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
            global,
            local,
            list,
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
            // Derive scope from --global/--local flags
            let scope = if global {
                Some(commands::config_cmd::ConfigScope::Global)
            } else if local {
                Some(commands::config_cmd::ConfigScope::Local)
            } else {
                None
            };

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
            } else if list {
                commands::config_cmd::list(&workgraph_dir, cli.json)
            } else if init {
                commands::config_cmd::init(&workgraph_dir, scope)
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
                commands::config_cmd::show(&workgraph_dir, scope, cli.json)
            } else {
                // Default scope for writes = Local (like git)
                let write_scope =
                    scope.unwrap_or(commands::config_cmd::ConfigScope::Local);
                commands::config_cmd::update(
                    &workgraph_dir,
                    write_scope,
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
            cleanup,
            remove,
            processes,
            purge,
            delete_dirs,
            threshold,
        } => {
            if purge {
                commands::dead_agents::run_purge(&workgraph_dir, delete_dirs, cli.json).map(|_| ())
            } else if processes {
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
                force,
            } => commands::service::run_start(
                &workgraph_dir,
                socket.as_deref(),
                port,
                max_agents,
                executor.as_deref(),
                interval,
                model.as_deref(),
                cli.json,
                force,
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
        Commands::Setup => commands::setup::run(),
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
            MatrixCommands::Logout => {
                commands::matrix::run_logout(&workgraph_dir);
                Ok(())
            }
        },
    }
}

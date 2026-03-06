use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "wg")]
#[command(about = "Workgraph - A lightweight work coordination graph")]
#[command(version)]
#[command(disable_help_flag = true)]
#[command(disable_help_subcommand = true)]
pub struct Cli {
    /// Path to the workgraph directory (default: .workgraph in current dir)
    #[arg(long, global = true)]
    pub dir: Option<PathBuf>,

    /// Output as JSON for machine consumption
    #[arg(long, global = true)]
    pub json: bool,

    /// Show help (use --help-all for full command list)
    #[arg(long, short = 'h', global = true)]
    pub help: bool,

    /// Show all commands in help output
    #[arg(long, global = true)]
    pub help_all: bool,

    /// Sort help output alphabetically
    #[arg(long, short = 'a', global = true)]
    pub alphabetical: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// Initialize a new workgraph in the current directory
    Init {
        /// Skip agency initialization (roles, agents, auto-assign config)
        #[arg(long)]
        no_agency: bool,
    },

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

        /// This task comes after another task (can specify multiple)
        #[arg(long = "after", alias = "blocked-by", value_delimiter = ',', num_args = 1..)]
        after: Vec<String>,

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

        /// Provider for this task (anthropic, openai, openrouter, local)
        #[arg(long)]
        provider: Option<String>,

        /// Verification criteria - task requires review before done
        #[arg(long)]
        verify: Option<String>,

        /// Maximum iterations for structural cycle (sets cycle_config on this task as cycle header)
        #[arg(long = "max-iterations")]
        max_iterations: Option<u32>,

        /// Guard condition for cycle iteration: 'task:<id>=<status>' or 'always'
        #[arg(long = "cycle-guard")]
        cycle_guard: Option<String>,

        /// Delay between cycle iterations (e.g., 30s, 5m, 1h)
        #[arg(long = "cycle-delay")]
        cycle_delay: Option<String>,

        /// Force all cycle iterations to run (agents cannot signal convergence)
        #[arg(long = "no-converge")]
        no_converge: bool,

        /// Disable automatic cycle restart on failure (restart is on by default)
        #[arg(long = "no-restart-on-failure")]
        no_restart_on_failure: bool,

        /// Maximum failure-triggered cycle restarts (default: 3)
        #[arg(long = "max-failure-restarts")]
        max_failure_restarts: Option<u32>,

        /// Task visibility zone for trace exports (internal, public, peer)
        #[arg(long, default_value = "internal")]
        visibility: String,

        /// Context scope for prompt assembly (clean, task, graph, full)
        #[arg(long = "context-scope")]
        context_scope: Option<String>,

        /// Execution weight: full (default), light (read-only tools), bare (wg CLI only), shell (no LLM)
        #[arg(long = "exec-mode")]
        exec_mode: Option<String>,

        /// Create the task in paused state (default for interactive use)
        #[arg(long)]
        paused: bool,

        /// Skip draft mode and make task immediately available for dispatch
        #[arg(long, alias = "ready")]
        immediate: bool,

        /// Delay before task becomes ready (e.g., 30s, 5m, 1h, 1d)
        #[arg(long)]
        delay: Option<String>,

        /// Absolute timestamp before which task won't be dispatched (ISO 8601)
        #[arg(long = "not-before")]
        not_before: Option<String>,
    },

    /// Edit an existing task
    Edit {
        /// Task ID to edit
        #[arg(value_name = "TASK")]
        id: String,

        /// Update task title
        #[arg(long)]
        title: Option<String>,

        /// Update task description
        #[arg(long, short = 'd')]
        description: Option<String>,

        /// Add an after dependency
        #[arg(long = "add-after", alias = "add-blocked-by", value_delimiter = ',')]
        add_after: Vec<String>,

        /// Remove an after dependency
        #[arg(
            long = "remove-after",
            alias = "remove-blocked-by",
            value_delimiter = ','
        )]
        remove_after: Vec<String>,

        /// Add a tag
        #[arg(long = "add-tag")]
        add_tag: Vec<String>,

        /// Remove a tag
        #[arg(long = "remove-tag")]
        remove_tag: Vec<String>,

        /// Update preferred model
        #[arg(long)]
        model: Option<String>,

        /// Update provider for this task
        #[arg(long)]
        provider: Option<String>,

        /// Add a required skill
        #[arg(long = "add-skill")]
        add_skill: Vec<String>,

        /// Remove a required skill
        #[arg(long = "remove-skill")]
        remove_skill: Vec<String>,

        /// Set maximum iterations for structural cycle (sets cycle_config)
        #[arg(long = "max-iterations")]
        max_iterations: Option<u32>,

        /// Set guard condition for cycle iteration: 'task:<id>=<status>' or 'always'
        #[arg(long = "cycle-guard")]
        cycle_guard: Option<String>,

        /// Set delay between cycle iterations (e.g., 30s, 5m, 1h)
        #[arg(long = "cycle-delay")]
        cycle_delay: Option<String>,

        /// Force all cycle iterations to run (agents cannot signal convergence)
        #[arg(long = "no-converge")]
        no_converge: bool,

        /// Disable automatic cycle restart on failure
        #[arg(long = "no-restart-on-failure")]
        no_restart_on_failure: bool,

        /// Maximum failure-triggered cycle restarts (default: 3)
        #[arg(long = "max-failure-restarts")]
        max_failure_restarts: Option<u32>,

        /// Set task visibility zone (internal, public, peer)
        #[arg(long)]
        visibility: Option<String>,

        /// Set context scope for prompt assembly (clean, task, graph, full)
        #[arg(long = "context-scope")]
        context_scope: Option<String>,

        /// Set execution weight: full (default), light (read-only tools), bare (wg CLI only), shell (no LLM)
        #[arg(long = "exec-mode")]
        exec_mode: Option<String>,

        /// Delay before task becomes ready (e.g., 30s, 5m, 1h, 1d)
        #[arg(long)]
        delay: Option<String>,

        /// Absolute timestamp before which task won't be dispatched (ISO 8601)
        #[arg(long = "not-before")]
        not_before: Option<String>,
    },

    /// Mark a task as done
    Done {
        /// Task ID to mark as done
        #[arg(value_name = "TASK")]
        id: String,

        /// Signal that the task's iterative loop has converged (stops loop edges from firing)
        #[arg(long)]
        converged: bool,

        /// Skip the verify command gate (human escape hatch, blocked when WG_AGENT_ID is set)
        #[arg(long)]
        skip_verify: bool,
    },

    /// Mark a task as failed (can be retried)
    Fail {
        /// Task ID to mark as failed
        #[arg(value_name = "TASK")]
        id: String,

        /// Reason for failure
        #[arg(long)]
        reason: Option<String>,

        /// Reject a done task via evaluation gate. Allows failing a task that
        /// is already Done because the evaluator determined the work is
        /// unacceptable. The task transitions to Failed and its dependents
        /// become blocked.
        #[arg(long)]
        eval_reject: bool,
    },

    /// Mark a task as abandoned (will not be retried)
    Abandon {
        /// Task ID to abandon
        #[arg(value_name = "TASK")]
        id: String,

        /// Reason for abandonment
        #[arg(long)]
        reason: Option<String>,
    },

    /// Retry a failed task (resets to open status)
    Retry {
        /// Task ID to retry
        #[arg(value_name = "TASK")]
        id: String,
    },

    /// Claim a task for work (sets status to InProgress)
    Claim {
        /// Task ID to claim
        #[arg(value_name = "TASK")]
        id: String,

        /// Assign to a specific actor
        #[arg(long)]
        actor: Option<String>,
    },

    /// Release a claimed task (sets status back to Open)
    Unclaim {
        /// Task ID to unclaim
        #[arg(value_name = "TASK")]
        id: String,
    },

    /// Pause a task (coordinator will skip it until resumed)
    Pause {
        /// Task ID to pause
        #[arg(value_name = "TASK")]
        id: String,
    },

    /// Resume a paused task (propagates to downstream subgraph by default)
    Resume {
        /// Task ID to resume
        #[arg(value_name = "TASK")]
        id: String,
        /// Only resume this single task (skip subgraph propagation)
        #[arg(long)]
        only: bool,
    },

    /// Publish a draft task (validates dependencies, then resumes entire subgraph)
    Publish {
        /// Task ID to publish
        #[arg(value_name = "TASK")]
        id: String,
        /// Only publish this single task (skip subgraph propagation)
        #[arg(long)]
        only: bool,
    },

    /// Park a task and exit — sets status to Waiting until condition is met
    Wait {
        /// Task ID to park
        #[arg(value_name = "TASK")]
        id: String,

        /// Condition to wait for (e.g. "task:dep-a=done", "timer:5m", "message")
        #[arg(long)]
        until: String,

        /// Checkpoint summary of progress so far
        #[arg(long)]
        checkpoint: Option<String>,
    },

    /// Add a dependency: task depends on (waits for) dependency
    #[command(name = "add-dep", alias = "add-after")]
    AddDep {
        /// The task that will depend on the dependency
        #[arg(value_name = "TASK")]
        task: String,

        /// The dependency (blocker) task
        #[arg(value_name = "DEPENDENCY")]
        dependency: String,
    },

    /// Remove a dependency edge between two tasks
    #[command(name = "rm-dep")]
    RmDep {
        /// The task to remove the dependency from
        #[arg(value_name = "TASK")]
        task: String,

        /// The dependency to remove
        #[arg(value_name = "DEPENDENCY")]
        dependency: String,
    },

    /// Reclaim a task from a dead/unresponsive agent
    Reclaim {
        /// Task ID to reclaim
        #[arg(value_name = "TASK")]
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

    /// Show recently completed tasks and their artifacts (stigmergic discovery)
    Discover {
        /// Time window (e.g. "24h", "7d", "30m"). Default: 24h
        #[arg(long, default_value = "24h")]
        since: String,

        /// Include artifact paths in output
        #[arg(long)]
        with_artifacts: bool,
    },

    /// Show what's blocking a task
    Blocked {
        /// Task ID
        #[arg(value_name = "TASK")]
        id: String,
    },

    /// Show the full transitive chain explaining why a task is blocked
    WhyBlocked {
        /// Task ID
        #[arg(value_name = "TASK")]
        id: String,
    },

    /// Check the graph for issues (cycles, orphan references)
    Check,

    /// Analyze structural cycles in after edges (Tarjan's SCC)
    Cycles,

    /// List all tasks
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,

        /// Only show paused tasks
        #[arg(long)]
        paused: bool,

        /// Filter by tag (multiple --tag flags use AND semantics)
        #[arg(long = "tag")]
        tags: Vec<String>,
    },

    /// Visualize the dependency graph (ASCII tree by default)
    Viz {
        /// Task IDs to focus on — shows only their containing subgraphs
        #[arg(value_name = "TASK_ID")]
        focus: Vec<String>,

        /// Show all tasks including fully-done trees (default: active trees only)
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

        /// Launch interactive TUI mode instead of static output
        #[arg(long, conflicts_with_all = ["dot", "mermaid", "graph", "output", "no_tui"])]
        tui: bool,

        /// Force static output even when stdout is an interactive terminal
        #[arg(long, alias = "static", conflicts_with = "tui")]
        no_tui: bool,

        /// Disable mouse capture in TUI mode (useful in tmux)
        #[arg(long)]
        no_mouse: bool,

        /// Layout strategy: 'diamond' (default) places fan-in nodes under their
        /// common ancestor with arcs flowing down; 'tree' uses classic DFS order
        #[arg(long, default_value = "diamond")]
        layout: String,

        /// Filter by tag (multiple --tag flags use AND semantics)
        #[arg(long = "tag")]
        tags: Vec<String>,

        /// Edge color style: 'gray' (default), 'white', or 'mixed' (tree=white, arcs=gray)
        #[arg(long)]
        edge_color: Option<String>,
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
        #[arg(value_name = "TASK")]
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
        #[arg(value_name = "TASK")]
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
        #[arg(value_name = "TASK")]
        id: String,
    },

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

        #[command(subcommand)]
        command: Option<ArchiveCommands>,
    },

    /// Garbage collect terminal tasks (failed, abandoned) from the graph
    Gc {
        /// Show what would be removed without actually removing
        #[arg(long)]
        dry_run: bool,

        /// Also remove done tasks (by default only failed+abandoned)
        #[arg(long)]
        include_done: bool,

        /// Only remove tasks older than this duration (e.g., 30d, 7d, 1w, 24h)
        #[arg(long)]
        older: Option<String>,
    },

    /// Show detailed information about a single task
    Show {
        /// Task ID
        #[arg(value_name = "TASK")]
        id: String,
    },

    /// Trace commands: execution history, export, import
    Trace {
        #[command(subcommand)]
        command: TraceCommands,
    },

    /// Function management: extract, apply, list, show, bootstrap
    Func {
        #[command(subcommand)]
        command: FuncCommands,
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
        #[arg(value_name = "TASK")]
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

    /// Send and receive messages to/from tasks and agents
    Msg {
        #[command(subcommand)]
        command: MsgCommands,
    },

    /// Save a checkpoint for context preservation during long-running tasks
    Checkpoint {
        /// Task ID
        #[arg(value_name = "TASK")]
        task: String,

        /// Summary of progress (~500 tokens)
        #[arg(long, short = 's')]
        summary: String,

        /// Agent ID (default: WG_AGENT_ID env var or task assignee)
        #[arg(long)]
        agent: Option<String>,

        /// Files modified since last checkpoint
        #[arg(long = "file", short = 'f')]
        files: Vec<String>,

        /// Stream byte offset
        #[arg(long)]
        stream_offset: Option<u64>,

        /// Conversation turn count
        #[arg(long)]
        turn_count: Option<u64>,

        /// Input tokens used
        #[arg(long)]
        token_input: Option<u64>,

        /// Output tokens used
        #[arg(long)]
        token_output: Option<u64>,

        /// Checkpoint type: explicit (default) or auto
        #[arg(long, default_value = "explicit")]
        checkpoint_type: String,

        /// List checkpoints instead of creating one
        #[arg(long)]
        list: bool,
    },

    /// Chat with the coordinator agent
    Chat {
        /// Message to send (omit for interactive mode)
        message: Option<String>,

        /// Interactive REPL mode
        #[arg(long, short = 'i')]
        interactive: bool,

        /// Show chat history
        #[arg(long)]
        history: bool,

        /// Clear chat history
        #[arg(long)]
        clear: bool,

        /// Timeout in seconds waiting for response (default: 120)
        #[arg(long)]
        timeout: Option<u64>,

        /// Attach a file (copied to .workgraph/attachments/)
        #[arg(long)]
        attachment: Vec<String>,
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

    /// Manage the agency (roles + tradeoffs)
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

    /// Manage agency tradeoffs (acceptable/unacceptable constraints)
    #[command(alias = "motivation")]
    Tradeoff {
        #[command(subcommand)]
        command: TradeoffCommands,
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

    /// Manage agent definitions (identity: role + tradeoff pairings)
    #[command(
        after_help = "This command manages agent identity entities stored in .workgraph/agency/.\nEach agent definition pairs a role with a tradeoff profile.\n\nSee also: 'wg agents' to list running agent processes (service workers)."
    )]
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

    /// Evaluate tasks: auto-evaluate, record external scores, view history
    Evaluate {
        #[command(subcommand)]
        command: EvaluateCommands,
    },

    /// Trigger an evolution cycle, or review deferred operations
    Evolve {
        #[command(subcommand)]
        command: EvolveCommands,
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

        /// Set creator agent (content-hash)
        #[arg(long)]
        creator_agent: Option<String>,

        /// Set model for creator agents
        #[arg(long)]
        creator_model: Option<String>,

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

        /// Max tasks a single agent can create per execution (default: 10)
        #[arg(long)]
        max_child_tasks: Option<u32>,

        /// Max depth of task dependency chains from root (default: 8)
        #[arg(long)]
        max_task_depth: Option<u32>,

        /// Viz edge color style: 'gray' (default), 'white', or 'mixed'
        #[arg(long, name = "viz-edge-color")]
        viz_edge_color: Option<String>,

        /// Set the evaluation gate threshold (0.0–1.0). Evaluations below this
        /// score will reject (fail) the original task. Only applies to tasks
        /// tagged 'eval-gate' unless --eval-gate-all is set.
        #[arg(long, name = "eval-gate-threshold")]
        eval_gate_threshold: Option<f64>,

        /// Apply eval gate to ALL evaluated tasks, not just those tagged 'eval-gate'
        #[arg(long, name = "eval-gate-all")]
        eval_gate_all: Option<bool>,

        /// Enable or disable FLIP (roundtrip intent fidelity) evaluation
        #[arg(long, name = "flip-enabled")]
        flip_enabled: Option<bool>,

        /// Model for FLIP inference phase (reconstructing prompt from output)
        #[arg(long, name = "flip-inference-model")]
        flip_inference_model: Option<String>,

        /// Model for FLIP comparison phase (scoring similarity)
        #[arg(long, name = "flip-comparison-model")]
        flip_comparison_model: Option<String>,

        /// FLIP score threshold for triggering Opus verification (default: 0.7)
        #[arg(long, name = "flip-verification-threshold")]
        flip_verification_threshold: Option<f64>,

        /// Model for FLIP-triggered verification agents (default: opus)
        #[arg(long, name = "flip-verification-model")]
        flip_verification_model: Option<String>,

        /// Enable/disable chat history persistence across TUI restarts
        #[arg(long, name = "chat-history")]
        chat_history: Option<bool>,

        /// Maximum number of chat messages to persist (default: 1000)
        #[arg(long, name = "chat-history-max")]
        chat_history_max: Option<usize>,

        /// Show all model routing assignments (per-role model+provider)
        #[arg(long = "models")]
        show_models: bool,

        /// Set model for a dispatch role: --set-model <role> <model>
        /// Roles: default, task_agent, evaluator, flip_inference, flip_comparison,
        /// assigner, evolver, verification, triage, creator
        #[arg(long = "set-model", num_args = 2, value_names = ["ROLE", "MODEL"])]
        set_model: Option<Vec<String>>,

        /// Set provider for a dispatch role: --set-provider <role> <provider>
        #[arg(long = "set-provider", num_args = 2, value_names = ["ROLE", "PROVIDER"])]
        set_provider: Option<Vec<String>>,

        /// Set model for a dispatch role: --role-model <role>=<model>
        /// Equivalent to --set-model but uses key=value syntax.
        #[arg(long = "role-model", value_name = "ROLE=MODEL")]
        role_model: Option<String>,

        /// Set provider for a dispatch role: --role-provider <role>=<provider>
        /// Equivalent to --set-provider but uses key=value syntax.
        #[arg(long = "role-provider", value_name = "ROLE=PROVIDER")]
        role_provider: Option<String>,
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

    /// List running agent processes (service workers)
    #[command(
        after_help = "This command shows agent processes spawned by the service coordinator.\nThese are runtime workers, not agent identity definitions.\n\nSee also: 'wg agent' to manage agent definitions (role + tradeoff pairings)."
    )]
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

    /// Launch interactive TUI dashboard (same as `wg viz --all --tui`)
    Tui {
        /// Disable mouse capture (useful in tmux)
        #[arg(long)]
        no_mouse: bool,
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

    /// Stream workgraph events as JSON lines
    Watch {
        /// Filter events by type (repeatable). Types: task_state, evaluation, agent, all.
        #[arg(long = "event", default_value = "all")]
        event_types: Vec<String>,
        /// Filter events to a specific task ID (prefix match)
        #[arg(long)]
        task: Option<String>,
        /// Include N most recent historical events before streaming (default: 0)
        #[arg(long, default_value = "0")]
        replay: usize,
    },

    /// Matrix integration commands
    #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
    Matrix {
        #[command(subcommand)]
        command: MatrixCommands,
    },

    /// Telegram integration commands
    Telegram {
        #[command(subcommand)]
        command: TelegramCommands,
    },

    /// Run the native executor agent loop (internal, called by spawn)
    #[command(name = "native-exec", hide = true)]
    NativeExec {
        /// Path to the prompt file
        #[arg(long)]
        prompt_file: String,

        /// Exec mode for bundle resolution (bare/light/full)
        #[arg(long, default_value = "full")]
        exec_mode: String,

        /// Task ID being worked on
        #[arg(long)]
        task_id: String,

        /// Model to use (e.g., claude-sonnet-4-5-20250514)
        #[arg(long)]
        model: Option<String>,

        /// Maximum agent turns before stopping
        #[arg(long, default_value = "100")]
        max_turns: usize,
    },
}

#[derive(Subcommand)]
pub enum MsgCommands {
    /// Send a message to a task/agent
    Send {
        /// Task ID
        task_id: String,

        /// Message body
        message: Option<String>,

        /// Sender identifier (default: "user")
        #[arg(long, default_value = "user")]
        from: String,

        /// Message priority: normal or urgent
        #[arg(long, default_value = "normal")]
        priority: String,

        /// Read message body from stdin
        #[arg(long)]
        stdin: bool,
    },

    /// List all messages for a task
    List {
        /// Task ID
        task_id: String,
    },

    /// Read unread messages (marks as read, advances cursor)
    Read {
        /// Task ID
        task_id: String,

        /// Agent ID (default: from WG_AGENT_ID env var, or "user")
        #[arg(long)]
        agent: Option<String>,
    },

    /// Poll for new messages (exit code 0 = new messages, 1 = none)
    Poll {
        /// Task ID
        task_id: String,

        /// Agent ID (default: from WG_AGENT_ID env var, or "user")
        #[arg(long)]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum EvaluateCommands {
    /// Trigger LLM-based evaluation of a completed task
    Run {
        /// Task ID to evaluate
        task: String,
        /// Model to use for the evaluator
        #[arg(long)]
        evaluator_model: Option<String>,
        /// Show what would be evaluated without spawning the evaluator
        #[arg(long)]
        dry_run: bool,
        /// Run FLIP (roundtrip intent fidelity) evaluation instead of direct evaluation
        #[arg(long)]
        flip: bool,
    },

    /// Record an evaluation from an external source
    Record {
        /// Task ID
        #[arg(long)]
        task: String,
        /// Overall score (0.0-1.0)
        #[arg(long)]
        score: f64,
        /// Source identifier (e.g. "outcome:sharpe", "vx:peer-abc", "manual")
        #[arg(long)]
        source: String,
        /// Optional notes
        #[arg(long)]
        notes: Option<String>,
        /// Optional dimensional scores (repeatable, format: dimension=score)
        #[arg(long = "dim", num_args = 1)]
        dimensions: Vec<String>,
    },

    /// Show evaluation history (or both task-level and org-level scores for a specific task)
    Show {
        /// Show both task-level and org-level scores side by side for this task
        #[arg(value_name = "TASK")]
        task_detail: Option<String>,
        /// Filter by task ID (prefix match, when no TASK positional arg)
        #[arg(long)]
        task: Option<String>,
        /// Filter by agent ID (prefix match)
        #[arg(long)]
        agent: Option<String>,
        /// Filter by source (exact match or glob, e.g. "outcome:*")
        #[arg(long)]
        source: Option<String>,
        /// Show only the N most recent evaluations
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
pub enum EvolveCommands {
    /// Trigger an evolution cycle on agency roles and tradeoffs
    Run {
        /// Show proposed changes without applying them
        #[arg(long)]
        dry_run: bool,

        /// Evolution strategy: mutation, crossover, gap-analysis, retirement, tradeoff-tuning, all (default: all)
        #[arg(long)]
        strategy: Option<String>,

        /// Maximum number of operations to apply
        #[arg(long)]
        budget: Option<u32>,

        /// Model to use for the evolver agent
        #[arg(long)]
        model: Option<String>,
    },

    /// Review deferred evolver operations (list, approve, reject)
    Review {
        #[command(subcommand)]
        command: EvolveReviewCommands,
    },
}

#[derive(Subcommand)]
pub enum EvolveReviewCommands {
    /// List pending deferred operations awaiting human review
    List,

    /// Approve a deferred evolver operation and apply it
    Approve {
        /// Deferred operation ID
        id: String,

        /// Optional note explaining approval
        #[arg(long, short = 'n')]
        note: Option<String>,
    },

    /// Reject a deferred evolver operation
    Reject {
        /// Deferred operation ID
        id: String,

        /// Optional note explaining rejection
        #[arg(long, short = 'n')]
        note: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ArchiveCommands {
    /// Search archived tasks by title, description, and tags
    Search {
        /// Search query (case-insensitive substring match)
        query: String,

        /// Maximum number of results to show
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Restore an archived task back into the active graph
    Restore {
        /// Task ID to restore
        #[arg(value_name = "TASK")]
        task_id: String,

        /// Reopen the task (set status to 'open' instead of 'done')
        #[arg(long)]
        reopen: bool,
    },
}

#[derive(Subcommand)]
pub enum TraceCommands {
    /// Show the execution history of a task
    Show {
        /// Task ID to trace
        #[arg(value_name = "TASK")]
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

    /// Export trace data filtered by visibility zone
    Export {
        /// Root task ID (exports this task and all descendants)
        #[arg(long)]
        root: Option<String>,
        /// Visibility zone filter: "internal" (everything), "public" (sanitized),
        /// "peer" (richer for credentialed peers). Default: "internal".
        #[arg(long, default_value = "internal")]
        visibility: String,
        /// Output file path (default: stdout)
        #[arg(long, short = 'o')]
        output: Option<String>,
    },

    /// Import a trace export file as read-only context
    Import {
        /// Path to the trace export JSON file
        file: String,
        /// Source tag for imported data (e.g. "peer:alice", "team:platform")
        #[arg(long)]
        source: Option<String>,
        /// Show what would be imported without making changes
        #[arg(long)]
        dry_run: bool,
    },

    // Hidden aliases for backward compatibility (wg trace <cmd> → wg func <cmd>)
    #[command(name = "extract", hide = true)]
    ExtractAlias {
        #[arg(required = true, num_args = 1..)]
        task_ids: Vec<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        subgraph: bool,
        #[arg(long)]
        recursive: bool,
        #[arg(long)]
        generalize: bool,
        #[arg(long)]
        generative: bool,
        #[arg(long)]
        output: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        include_evaluations: bool,
    },

    #[command(name = "instantiate", hide = true)]
    InstantiateAlias {
        function_id: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long = "input", num_args = 1)]
        inputs: Vec<String>,
        #[arg(long = "input-file")]
        input_file: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long = "after", alias = "blocked-by", value_delimiter = ',')]
        after: Vec<String>,
        #[arg(long)]
        model: Option<String>,
    },

    #[command(name = "list-functions", hide = true)]
    ListFunctionsAlias {
        #[arg(long)]
        verbose: bool,
        #[arg(long)]
        include_peers: bool,
        #[arg(long)]
        visibility: Option<String>,
    },

    #[command(name = "show-function", hide = true)]
    ShowFunctionAlias { id: String },

    #[command(name = "bootstrap", hide = true)]
    BootstrapAlias {
        #[arg(long)]
        force: bool,
    },

    #[command(name = "make-adaptive", hide = true)]
    MakeAdaptiveAlias {
        function_id: String,
        #[arg(long, default_value = "10")]
        max_runs: u32,
    },
}

#[derive(Subcommand)]
pub enum FuncCommands {
    /// List available functions
    List {
        /// Show input parameters and task templates
        #[arg(long)]
        verbose: bool,

        /// Include functions from federated peer workgraphs
        #[arg(long)]
        include_peers: bool,

        /// Filter by visibility level (internal, peer, public)
        #[arg(long)]
        visibility: Option<String>,
    },

    /// Show details of a function
    Show {
        /// Function ID (prefix match supported)
        id: String,
    },

    /// Extract a function from completed task(s)
    Extract {
        /// Task ID(s) to extract from (multiple IDs with --generative)
        #[arg(required = true, num_args = 1..)]
        task_ids: Vec<String>,

        /// Function name/ID (default: derived from task ID)
        #[arg(long)]
        name: Option<String>,

        /// Include all subtasks (tasks blocked by this one) in the function
        #[arg(long)]
        subgraph: bool,

        /// Recursively extract the entire spawned subgraph with dependency structure
        #[arg(long)]
        recursive: bool,

        /// Use LLM to generalize descriptions
        #[arg(long)]
        generalize: bool,

        /// Multi-trace extraction: compare multiple traces to produce a generative function
        #[arg(long)]
        generative: bool,

        /// Write to specific path instead of .workgraph/functions/<name>.yaml
        #[arg(long)]
        output: Option<String>,

        /// Overwrite existing function with same name
        #[arg(long)]
        force: bool,

        /// Include coordinator-generated evaluation and assignment tasks
        /// (evaluate-*, assign-*) that are normally filtered out
        #[arg(long)]
        include_evaluations: bool,
    },

    /// Create tasks from a function with provided inputs
    Apply {
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
        #[arg(long = "after", alias = "blocked-by", value_delimiter = ',')]
        after: Vec<String>,

        /// Set model for all created tasks
        #[arg(long)]
        model: Option<String>,
    },

    /// Bootstrap the extract-function meta-function
    Bootstrap {
        /// Overwrite if already exists
        #[arg(long)]
        force: bool,
    },

    /// Upgrade a generative function to adaptive (adds run memory)
    #[command(name = "make-adaptive")]
    MakeAdaptive {
        /// Function ID (prefix match supported)
        function_id: String,

        /// Maximum number of past runs to include in memory
        #[arg(long, default_value = "10")]
        max_runs: u32,
    },
}

#[derive(Subcommand)]
pub enum RunsCommands {
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
pub enum ResourceCommands {
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
pub enum SkillCommands {
    /// List all skills used across tasks
    List,

    /// Show skills for a specific task
    Task {
        /// Task ID
        #[arg(value_name = "TASK")]
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
pub enum AgencyCommands {
    /// Seed agency with starter roles and tradeoffs
    Init,

    /// Migrate old-format agency store (roles/, motivations/, agents/) to primitive+cache format
    Migrate {
        /// Show what would be migrated without writing
        #[arg(long)]
        dry_run: bool,
    },

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

        /// Only pull entities of this type (role, tradeoff, agent)
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

    /// List pending deferred evolver operations awaiting human review
    Deferred,

    /// Approve a deferred evolver operation
    Approve {
        /// Deferred operation ID
        id: String,

        /// Optional note explaining approval
        #[arg(long, short = 'n')]
        note: Option<String>,
    },

    /// Reject a deferred evolver operation
    Reject {
        /// Deferred operation ID
        id: String,

        /// Optional note explaining rejection
        #[arg(long, short = 'n')]
        note: Option<String>,
    },

    /// Invoke the creator agent to discover and add new primitives
    Create {
        /// Model to use for the creator agent
        #[arg(long)]
        model: Option<String>,

        /// Show what would be created without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Push local entities to another agency store
    Push {
        /// Target store (path, named remote, or directory)
        target: String,

        /// Only push specific entity IDs
        #[arg(long = "entity", value_delimiter = ',')]
        entity_ids: Vec<String>,

        /// Only push entities of this type (role, tradeoff, agent)
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
pub enum RemoteCommands {
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
pub enum PeerCommands {
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
pub enum RoleCommands {
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
pub enum TradeoffCommands {
    /// Create a new tradeoff
    Add {
        /// Tradeoff name
        name: String,

        /// Acceptable tradeoffs (can be repeated)
        #[arg(long)]
        accept: Vec<String>,

        /// Unacceptable tradeoffs (can be repeated)
        #[arg(long)]
        reject: Vec<String>,

        /// Tradeoff description
        #[arg(long, short = 'd')]
        description: Option<String>,
    },

    /// List all tradeoffs
    List,

    /// Show full tradeoff details
    Show {
        /// Tradeoff ID
        id: String,
    },

    /// Open tradeoff YAML in EDITOR for manual editing
    Edit {
        /// Tradeoff ID
        id: String,
    },

    /// Remove a tradeoff
    Rm {
        /// Tradeoff ID
        id: String,
    },

    /// Show evolutionary lineage/ancestry tree for a tradeoff
    Lineage {
        /// Tradeoff ID
        id: String,
    },
}

#[derive(Subcommand)]
pub enum AgentCommands {
    /// Create a new agent definition (role + tradeoff pairing)
    Create {
        /// Agent name
        name: String,

        /// Role ID (or prefix) — optional for human agents
        #[arg(long)]
        role: Option<String>,

        /// Tradeoff ID (or prefix) — optional for human agents
        #[arg(long, alias = "motivation")]
        tradeoff: Option<String>,

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

    /// List all agent definitions
    List,

    /// Show agent definition details including resolved role/tradeoff
    Show {
        /// Agent ID (or prefix)
        id: String,
    },

    /// Remove an agent definition
    Rm {
        /// Agent ID (or prefix)
        id: String,
    },

    /// Show ancestry (lineage of constituent role and tradeoff)
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
pub enum ServiceCommands {
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

        /// Disable the persistent coordinator agent (LLM chat session)
        #[arg(long)]
        no_coordinator_agent: bool,
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

    /// Restart the service daemon (atomic stop + start)
    ///
    /// Stops the current daemon, waits for clean shutdown, then starts a new
    /// daemon preserving the current config (max_agents, executor, model).
    Restart {
        /// Force stop (SIGKILL the daemon immediately)
        #[arg(long)]
        force: bool,

        /// Also kill running agents during stop
        #[arg(long)]
        kill_agents: bool,
    },

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

        /// Disable the persistent coordinator agent (LLM chat session)
        #[arg(long)]
        no_coordinator_agent: bool,
    },
}

#[cfg(any(feature = "matrix", feature = "matrix-lite"))]
#[derive(Subcommand)]
pub enum MatrixCommands {
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

#[derive(Subcommand)]
pub enum TelegramCommands {
    /// Start the Telegram bot listener
    ///
    /// Polls the Telegram Bot API for messages and dispatches workgraph
    /// commands like: claim, done, fail, input, status, ready, help
    Listen {
        /// Telegram chat ID to listen in (uses configured chat_id if not specified)
        #[arg(long)]
        chat_id: Option<String>,
    },

    /// Send a message to the configured Telegram chat
    Send {
        /// Message to send
        message: String,

        /// Target chat ID (uses configured chat_id if not specified)
        #[arg(long)]
        chat_id: Option<String>,
    },

    /// Show Telegram configuration status
    Status,
}

/// Get the command name from a Commands enum variant for usage tracking
pub fn command_name(cmd: &Commands) -> &'static str {
    match cmd {
        Commands::Init { .. } => "init",
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
        Commands::Publish { .. } => "publish",
        Commands::Wait { .. } => "wait",
        Commands::AddDep { .. } => "add-dep",
        Commands::RmDep { .. } => "rm-dep",
        Commands::Reclaim { .. } => "reclaim",
        Commands::Ready => "ready",
        Commands::Discover { .. } => "discover",
        Commands::Blocked { .. } => "blocked",
        Commands::WhyBlocked { .. } => "why-blocked",
        Commands::Check => "check",
        Commands::Cycles => "cycles",
        Commands::List { .. } => "list",
        Commands::Viz { .. } => "viz",
        Commands::GraphExport { .. } => "graph-export",
        Commands::Cost { .. } => "cost",
        Commands::Coordinate { .. } => "coordinate",
        Commands::Plan { .. } => "plan",
        Commands::Reschedule { .. } => "reschedule",
        Commands::Impact { .. } => "impact",
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
        Commands::Func { .. } => "func",
        Commands::Replay { .. } => "replay",
        Commands::Runs { .. } => "runs",
        Commands::Log { .. } => "log",
        Commands::Msg { .. } => "msg",
        Commands::Resource { .. } => "resource",
        Commands::Skill { .. } => "skill",
        Commands::Agency { .. } => "agency",
        Commands::Peer { .. } => "peer",
        Commands::Role { .. } => "role",
        Commands::Tradeoff { .. } => "tradeoff",
        Commands::Assign { .. } => "assign",
        Commands::Match { .. } => "match",
        Commands::Heartbeat { .. } => "heartbeat",
        Commands::Checkpoint { .. } => "checkpoint",
        Commands::Artifact { .. } => "artifact",
        Commands::Context { .. } => "context",
        Commands::Next { .. } => "next",
        Commands::Trajectory { .. } => "trajectory",
        Commands::Exec { .. } => "exec",
        Commands::Agent { .. } => "agent",
        Commands::Spawn { .. } => "spawn",
        Commands::Evaluate { .. } => "evaluate",
        Commands::Watch { .. } => "watch",
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
        Commands::Telegram { .. } => "telegram",
        Commands::Chat { .. } => "chat",
        Commands::NativeExec { .. } => "native-exec",
    }
}

/// Returns true if the command supports `--json` output.
pub fn supports_json(cmd: &Commands) -> bool {
    matches!(
        cmd,
        Commands::Ready
            | Commands::Discover { .. }
            | Commands::Blocked { .. }
            | Commands::WhyBlocked { .. }
            | Commands::List { .. }
            | Commands::Coordinate { .. }
            | Commands::Plan { .. }
            | Commands::Impact { .. }
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
            | Commands::Func { .. }
            | Commands::Replay { .. }
            | Commands::Runs { .. }
            | Commands::Log { .. }
            | Commands::Msg { .. }
            | Commands::Resource { .. }
            | Commands::Skill { .. }
            | Commands::Agency { .. }
            | Commands::Peer { .. }
            | Commands::Role { .. }
            | Commands::Tradeoff { .. }
            | Commands::Match { .. }
            | Commands::Heartbeat { .. }
            | Commands::Checkpoint { .. }
            | Commands::Artifact { .. }
            | Commands::Context { .. }
            | Commands::Next { .. }
            | Commands::Trajectory { .. }
            | Commands::Agent { .. }
            | Commands::Evaluate { .. }
            | Commands::Watch { .. }
            | Commands::Evolve { .. }
            | Commands::Config { .. }
            | Commands::DeadAgents { .. }
            | Commands::Agents { .. }
            | Commands::Kill { .. }
            | Commands::Service { .. }
            | Commands::Cost { .. }
            | Commands::Check
            | Commands::Cycles
            | Commands::Quickstart
            | Commands::Status
            | Commands::Chat { .. }
            | Commands::Telegram { .. }
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

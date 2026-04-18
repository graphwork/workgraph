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

        /// Initialize the GLOBAL workgraph at `~/.workgraph` instead of
        /// the current directory. Useful for `wg nex`-style interactive
        /// usage from arbitrary directories without littering `.workgraph`
        /// dirs everywhere. Resolver precedence: --dir > $WG_DIR >
        /// project discovery > ~/.workgraph > ./.workgraph
        #[arg(long)]
        global: bool,
    },

    /// Bulk-reset a subgraph: given one or more seed tasks, close the
    /// reachable set in the chosen direction and reset each task to
    /// Open (clearing status, failure_reason, retry_count). With
    /// --also-strip-meta, also delete all dot-prefixed system tasks
    /// (.flip-*, .evaluate-*, .verify-*, .assign-*, .place-*, ...)
    /// attached to the closure, so the coordinator can regenerate
    /// fresh ones instead of reviving stale done ones.
    Reset {
        /// First (required) seed task.
        seed: String,

        /// Additional seed tasks (comma-separated or repeated --seeds).
        #[arg(long = "seeds", value_delimiter = ',', num_args = 0..)]
        seeds: Vec<String>,

        /// Traversal direction: `forward` (downstream, default),
        /// `backward` (upstream), or `both`.
        #[arg(long, default_value = "forward")]
        direction: String,

        /// Also delete dot-prefixed system tasks (.flip-*, .evaluate-*,
        /// .verify-*, .assign-*, .place-*, .verify-deferred-*)
        /// attached to any closure member.
        #[arg(long = "also-strip-meta")]
        also_strip_meta: bool,

        /// Show what would be reset/stripped without mutating.
        #[arg(long = "dry-run")]
        dry_run: bool,

        /// Confirm destructive execution when affecting more than one task.
        #[arg(long)]
        yes: bool,
    },

    /// Rescue a failed task by inserting a first-class replacement at
    /// its graph slot. Successors are rewired to unblock from the
    /// rescue instead of the failed target; the target stays in the
    /// graph for history with `superseded_by` log entries.
    ///
    /// Primary caller: the `.evaluate-*` agent, when it judges a task
    /// failed and can describe a concrete fix. The description becomes
    /// the rescue task's brief — be specific about what to change.
    Rescue {
        /// The failed task's ID to rescue.
        target: String,

        /// What the rescue task needs to do differently. Becomes the
        /// rescue task's description — treat this as the next agent's
        /// assignment brief.
        #[arg(long, short = 'd', alias = "desc")]
        description: String,

        /// Optional title override (default: `Rescue: <target>`).
        #[arg(long)]
        title: Option<String>,

        /// Explicit ID for the rescue task (auto-derived from title otherwise).
        #[arg(long)]
        id: Option<String>,

        /// The ID of the eval task that concluded the failure. Recorded
        /// in the rescue task's description and in the operations log.
        #[arg(long = "from-eval")]
        from_eval: Option<String>,
    },

    /// Insert a new task at a position relative to an existing target
    /// (before / after / parallel). Graph-surgery primitive; used as
    /// the foundation for `wg rescue`.
    Insert {
        /// Where to insert: `before`, `after`, or `parallel`.
        position: String,

        /// The existing task's ID that anchors the insertion.
        target: String,

        /// Title for the new task (required).
        #[arg(long)]
        title: String,

        /// Detailed description for the new task.
        #[arg(long, short = 'd', alias = "desc")]
        description: Option<String>,

        /// Explicit ID for the new task (auto-derived from title if absent).
        #[arg(long)]
        id: Option<String>,

        /// For `before` / `after`: rewire target's old predecessor/successor
        /// edges through the new node exclusively (remove the direct old
        /// edge). No effect in `parallel` mode.
        #[arg(long)]
        splice: bool,

        /// For `parallel`: remove target from its successors' dependency
        /// lists so they unblock from the new node ONLY (rescue semantics).
        /// No effect in `before` / `after` mode.
        #[arg(long = "replace-edges")]
        replace_edges: bool,
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

        /// [DEPRECATED] Provider for this task — use provider:model format in --model instead
        #[arg(long)]
        provider: Option<String>,

        /// Verification criteria - task requires review before done
        #[arg(long)]
        verify: Option<String>,

        /// Verification timeout (e.g., '15m', '900s'). Overrides global WG_VERIFY_TIMEOUT
        #[arg(long = "verify-timeout")]
        verify_timeout: Option<String>,

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

        /// Shell command to execute for this task (auto-sets exec_mode=shell)
        #[arg(long)]
        exec: Option<String>,

        /// Per-task timeout (e.g., 30s, 5m, 1h, 4h, 1d)
        #[arg(long)]
        timeout: Option<String>,

        /// Execution weight: full (default), light (read-only tools), bare (wg CLI only), shell (no LLM)
        #[arg(long = "exec-mode")]
        exec_mode: Option<String>,

        /// Create the task in paused state (default for interactive use)
        #[arg(long)]
        paused: bool,

        /// Skip automatic placement — make task immediately available for dispatch
        #[arg(long = "no-place", alias = "immediate", alias = "ready")]
        no_place: bool,

        /// Placement hint: place near these tasks (comma-separated IDs)
        #[arg(long = "place-near", value_delimiter = ',')]
        place_near: Vec<String>,

        /// Placement hint: place before these tasks (comma-separated IDs)
        #[arg(long = "place-before", value_delimiter = ',')]
        place_before: Vec<String>,

        /// Delay before task becomes ready (e.g., 30s, 5m, 1h, 1d)
        #[arg(long)]
        delay: Option<String>,

        /// Absolute timestamp before which task won't be dispatched (ISO 8601)
        #[arg(long = "not-before")]
        not_before: Option<String>,

        /// Allow phantom (forward-reference) dependencies without error
        #[arg(long = "allow-phantom")]
        allow_phantom: bool,

        /// Suppress implicit --after dependency on the creating task (alias: --no-after)
        #[arg(long = "independent", alias = "no-after")]
        independent: bool,

        /// Retry propagation policy: conservative, aggressive, or conditional:<float>
        #[arg(long = "propagation")]
        propagation: Option<String>,

        /// Retry strategy: same-model, upgrade-model, or escalate-to-human
        #[arg(long = "retry-strategy")]
        retry_strategy: Option<String>,

        /// Cron schedule expression (6-field format: "sec min hour day month dow")
        #[arg(long)]
        cron: Option<String>,

        /// Create as a blocking subtask: child is created, parent waits for child to complete
        #[arg(long)]
        subtask: bool,
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

        /// [DEPRECATED] Update provider — use provider:model format in --model instead
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

        /// Set or update the verify command (shell command that must pass before done)
        #[arg(long)]
        verify: Option<String>,

        /// Set or clear cron schedule (empty string "" clears; 6-field: "sec min hour day month dow")
        #[arg(long)]
        cron: Option<String>,

        /// Allow phantom (forward-reference) dependencies without error
        #[arg(long = "allow-phantom")]
        allow_phantom: bool,

        /// Allow cycle creation without CycleConfig (overrides cycle detection guard)
        #[arg(long = "allow-cycle")]
        allow_cycle: bool,
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

        /// Task IDs that supersede/replace this task (comma-separated)
        #[arg(long, value_delimiter = ',')]
        superseded_by: Vec<String>,
    },

    /// Retry a failed task (resets to open status)
    Retry {
        /// Task ID to retry
        #[arg(value_name = "TASK")]
        id: String,
    },

    /// Requeue an in-progress task for failed-dependency triage (resets to open)
    Requeue {
        /// Task ID to requeue
        #[arg(value_name = "TASK")]
        id: String,

        /// Reason for requeue (what fix tasks were created)
        #[arg(long)]
        reason: String,
    },

    /// Approve a task pending validation (transitions to Done)
    Approve {
        /// Task ID to approve
        #[arg(value_name = "TASK")]
        id: String,
    },

    /// Reject a task pending validation (reopens with feedback, or fails after max rejections)
    Reject {
        /// Task ID to reject
        #[arg(value_name = "TASK")]
        id: String,

        /// Reason for rejection
        #[arg(long)]
        reason: String,
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

    /// Manual cleanup commands for edge case recovery
    Cleanup {
        #[command(subcommand)]
        subcmd: crate::commands::cleanup::CleanupSubcommand,
    },

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

        /// Only show cron-scheduled tasks
        #[arg(long)]
        cron: bool,
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

        /// Force a specific output width in columns (default: auto-detect terminal width)
        #[arg(long)]
        columns: Option<u16>,
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

    /// Change a task's priority level (critical, high, normal, low, idle)
    Reprioritize {
        /// Task ID
        #[arg(value_name = "TASK")]
        id: String,

        /// New priority level: critical, high, normal, low, idle
        #[arg(value_name = "PRIORITY")]
        priority: String,
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

    /// Manage agent worktrees (list, archive, inspect)
    #[command(subcommand, name = "worktree")]
    Worktree(WorktreeCommand),

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

        /// Skip confirmation prompt for bulk archive operations
        #[arg(long, short = 'y')]
        yes: bool,

        /// Undo the last archive operation (restore all tasks from the last batch)
        #[arg(long)]
        undo: bool,

        /// Specific task IDs to archive
        #[arg(value_name = "IDS")]
        ids: Vec<String>,

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

    /// Set or accumulate token usage on a task
    #[command(hide = true)]
    Tokens {
        /// Task ID
        id: String,

        /// Token usage JSON (e.g. '{"cost_usd":0.1,"input_tokens":500,"output_tokens":200}')
        json: String,
    },

    /// Show token usage and estimated cost summaries
    Spend {
        /// Show only today's spend
        #[arg(long, short = 't')]
        today: bool,

        /// Output as JSON
        #[arg(long, short = 'j')]
        json: bool,
    },

    /// OpenRouter cost monitoring and management
    Openrouter {
        #[command(subcommand)]
        command: OpenRouterCommands,
    },

    /// Send and receive messages to/from tasks and agents
    Msg {
        #[command(subcommand)]
        command: MsgCommands,
    },

    /// Manage per-user conversation boards (.user-NAME)
    User {
        #[command(subcommand)]
        command: UserCommands,
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

    /// Compact: distill graph state into context.md
    Compact,

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

        /// Target coordinator ID (default: 0)
        #[arg(long, default_value = "0")]
        coordinator: u32,

        /// Show only the last N messages (with --history) or load only the last N
        /// messages in interactive mode.
        #[arg(long, value_name = "N")]
        history_depth: Option<usize>,

        /// Start with no history loaded. History is still persisted — this only
        /// affects the initial display.
        #[arg(long)]
        no_history: bool,

        /// Rotate chat files to archive (force-rotate regardless of thresholds)
        #[arg(long)]
        rotate: bool,

        /// Clean up archived files older than the retention period
        #[arg(long)]
        cleanup: bool,

        /// Compact chat history into a context summary
        #[arg(long)]
        compact: bool,

        /// Share context from another coordinator into this one.
        /// Copies the source coordinator's compacted summary as imported context.
        /// Use with --coordinator to specify the target (default: 0).
        #[arg(long, value_name = "FROM_ID")]
        share_from: Option<u32>,
    },

    /// Manage resources
    Resource {
        #[command(subcommand)]
        command: ResourceCommands,
    },

    /// Manage nex chat sessions (list, attach, alias).
    ///
    /// Every `wg nex` session — interactive, coordinator,
    /// task-agent — lives under `chat/<uuid>/` and is addressable by
    /// UUID, UUID prefix, or alias. These subcommands are the UX for
    /// inspecting and attaching to them.
    Session {
        #[command(subcommand)]
        command: SessionCommands,
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

        /// Automatically select an agent using LLM
        #[arg(long)]
        auto: bool,
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

    /// Drop into an interactive agent session for a task (or run its shell command)
    Exec {
        /// Task ID to execute
        task: String,

        /// Actor performing the execution
        #[arg(long)]
        actor: Option<String>,

        /// Show assembled context and env vars without launching anything
        #[arg(long)]
        dry_run: bool,

        /// Set the exec command for a task (instead of running)
        #[arg(long)]
        set: Option<String>,

        /// Clear the exec command for a task
        #[arg(long)]
        clear: bool,

        /// Run the task's shell exec command (legacy behavior) instead of interactive session
        #[arg(long)]
        shell: bool,

        /// Create an isolated git worktree (like real agents get)
        #[arg(long, conflicts_with = "no_worktree")]
        worktree: bool,

        /// Work in-place without worktree isolation (default)
        #[arg(long, conflicts_with = "worktree")]
        no_worktree: bool,

        /// Model to use for the executor (e.g., opus, sonnet, haiku)
        #[arg(long)]
        model: Option<String>,
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

    /// Manage provider profiles (model tier presets)
    Profile {
        #[command(subcommand)]
        command: ProfileCommands,
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

        /// Set max concurrent coordinator agents (LLM sessions). Default: 4.
        #[arg(long)]
        max_coordinators: Option<usize>,

        /// Set coordinator poll interval in seconds
        #[arg(long)]
        coordinator_interval: Option<u64>,

        /// Set service daemon background poll interval in seconds (safety net)
        #[arg(long)]
        poll_interval: Option<u64>,

        /// Set autonomous heartbeat interval in seconds (0 to disable).
        /// When enabled, the coordinator agent receives periodic synthetic
        /// prompts to review graph state and take action without human input.
        #[arg(long)]
        heartbeat_interval: Option<u64>,

        /// Set coordinator executor
        #[arg(long)]
        coordinator_executor: Option<String>,

        /// Set coordinator model (e.g., opus, sonnet, haiku)
        #[arg(long)]
        coordinator_model: Option<String>,

        /// [DEPRECATED] Set coordinator provider — use provider:model in --coordinator-model instead
        #[arg(long)]
        coordinator_provider: Option<String>,

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

        /// Set retention heuristics (prose policy for evolver)
        #[arg(long)]
        retention_heuristics: Option<String>,

        /// Enable/disable automatic triage of dead agents
        #[arg(long)]
        auto_triage: Option<bool>,

        /// Enable/disable automatic placement analysis on new tasks
        #[arg(long)]
        auto_place: Option<bool>,

        /// Enable/disable automatic creator agent invocation
        #[arg(long)]
        auto_create: Option<bool>,

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

        /// Set FLIP inference model (Phase 1: prompt reconstruction). Shorthand for --set-model flip_inference <model>
        #[arg(long, name = "flip-inference-model")]
        flip_inference_model: Option<String>,

        /// Set FLIP comparison model (Phase 2: similarity scoring). Shorthand for --set-model flip_comparison <model>
        #[arg(long, name = "flip-comparison-model")]
        flip_comparison_model: Option<String>,

        /// Set both FLIP inference and comparison models to the same value
        #[arg(long, name = "flip-model")]
        flip_model: Option<String>,

        /// FLIP score threshold for triggering Opus verification (default: 0.7)
        #[arg(long, name = "flip-verification-threshold")]
        flip_verification_threshold: Option<f64>,

        /// Enable/disable chat history persistence across TUI restarts
        #[arg(long, name = "chat-history")]
        chat_history: Option<bool>,

        /// Maximum number of chat messages to persist (default: 1000)
        #[arg(long, name = "chat-history-max")]
        chat_history_max: Option<usize>,

        /// TUI time counters (comma-separated: uptime,cumulative,active,session)
        #[arg(long, name = "tui-counters")]
        tui_counters: Option<String>,

        /// Show all model registry entries (built-in + user-defined)
        #[arg(long = "registry")]
        show_registry: bool,

        /// Add a new model to the registry (use with --id, --provider, --reg-model, --reg-tier)
        #[arg(long = "registry-add")]
        registry_add: bool,

        /// Remove a model from the registry by ID
        #[arg(long = "registry-remove", value_name = "ID")]
        registry_remove: Option<String>,

        /// Show current tier→model assignments
        #[arg(long = "tiers")]
        show_tiers: bool,

        /// Set which model a tier uses (e.g., --tier standard=gpt-4o)
        #[arg(long = "tier", value_name = "TIER=MODEL_ID")]
        set_tier: Option<String>,

        /// Registry entry short ID (for --registry-add)
        #[arg(long = "id", requires = "registry_add")]
        reg_id: Option<String>,

        /// Provider name (for --registry-add, e.g., openai, anthropic)
        #[arg(long = "provider", requires = "registry_add")]
        reg_provider: Option<String>,

        /// Full API model identifier (for --registry-add, e.g., gpt-4o)
        #[arg(long = "reg-model", requires = "registry_add")]
        reg_model: Option<String>,

        /// Quality tier for registry entry (for --registry-add: fast, standard, premium)
        #[arg(long = "reg-tier", requires = "registry_add")]
        reg_tier: Option<String>,

        /// API endpoint URL (for --registry-add)
        #[arg(long = "endpoint", requires = "registry_add")]
        reg_endpoint: Option<String>,

        /// Context window in tokens (for --registry-add)
        #[arg(long = "context-window", requires = "registry_add")]
        reg_context_window: Option<u64>,

        /// Cost per million input tokens in USD (for --registry-add)
        #[arg(long = "cost-input", requires = "registry_add")]
        cost_input: Option<f64>,

        /// Cost per million output tokens in USD (for --registry-add)
        #[arg(long = "cost-output", requires = "registry_add")]
        cost_output: Option<f64>,

        /// Show all model routing assignments (per-role model+provider)
        #[arg(long = "models")]
        show_models: bool,

        /// Set model for a dispatch role: --set-model <role> <model>
        /// Roles: default, task_agent, evaluator, flip_inference, flip_comparison,
        /// assigner, evolver, verification, triage, creator
        #[arg(long = "set-model", num_args = 2, value_names = ["ROLE", "MODEL"])]
        set_model: Option<Vec<String>>,

        /// [DEPRECATED] Set provider for a dispatch role — use provider:model in --set-model instead
        #[arg(long = "set-provider", num_args = 2, value_names = ["ROLE", "PROVIDER"])]
        set_provider: Option<Vec<String>>,

        /// Set endpoint for a dispatch role: --set-endpoint <role> <endpoint-name>
        /// Binds a named endpoint (from `wg endpoints list`) to a dispatch role.
        #[arg(long = "set-endpoint", num_args = 2, value_names = ["ROLE", "ENDPOINT"])]
        set_endpoint: Option<Vec<String>>,

        /// Set model for a dispatch role: --role-model <role>=<model>
        /// Equivalent to --set-model but uses key=value syntax.
        #[arg(long = "role-model", value_name = "ROLE=MODEL")]
        role_model: Option<String>,

        /// [DEPRECATED] Set provider for a dispatch role — use provider:model in --set-model instead
        /// Equivalent to --set-provider but uses key=value syntax.
        #[arg(long = "role-provider", value_name = "ROLE=PROVIDER")]
        role_provider: Option<String>,

        /// Max tokens of previous-attempt context to inject on retry (default: 2000, 0 = disabled)
        #[arg(long, name = "retry-context-tokens")]
        retry_context_tokens: Option<u32>,

        /// Set API key file for a provider: --set-key <provider> --file <path>
        #[arg(long = "set-key", value_name = "PROVIDER")]
        set_key: Option<String>,

        /// File path for --set-key (the key file to reference)
        #[arg(long = "file", requires = "set_key", value_name = "PATH")]
        key_file: Option<String>,

        /// Check OpenRouter API key validity and credit status
        #[arg(long, name = "check-key")]
        check_key: bool,

        /// Install project config as global default (~/.workgraph/config.toml)
        #[arg(long, name = "install-global")]
        install_global: bool,

        /// Skip confirmation when overwriting existing global config
        #[arg(long)]
        force: bool,
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

    /// Detect and recover orphaned in-progress tasks with dead agents
    #[command(
        after_help = "Sweep detects in-progress tasks whose assigned agent has died,\nbeen marked Dead, or is missing from the registry. It resets them\nto Open so the coordinator can re-dispatch.\n\nThis is safe to run anytime — it is idempotent."
    )]
    Sweep {
        /// Only report orphaned tasks, don't fix them
        #[arg(long)]
        dry_run: bool,
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
        /// Agent ID to kill, or task ID when using --tree
        agent: Option<String>,

        /// Force kill (SIGKILL immediately instead of graceful SIGTERM)
        #[arg(long)]
        force: bool,

        /// Kill all running agents
        #[arg(long)]
        all: bool,

        /// Kill agent for task + all downstream tasks (cascade kill)
        #[arg(long)]
        tree: bool,

        /// Show what would be killed/abandoned without doing it
        #[arg(long)]
        dry_run: bool,

        /// Kill agents but don't abandon tasks (allows respawn)
        #[arg(long)]
        no_abandon: bool,
    },

    /// Reap dead/done/failed agents from the registry
    Reap {
        /// Show what would be reaped without removing
        #[arg(long)]
        dry_run: bool,

        /// Only reap agents dead/done/failed for longer than this duration (e.g., 1h, 30m, 7d)
        #[arg(long)]
        older_than: Option<String>,
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

        /// Recording mode: disable mouse capture and keyboard enhancement
        /// queries for clean asciinema/terminal recording. Auto-enabled when
        /// ASCIINEMA_REC is set.
        #[arg(long)]
        recording: bool,

        /// Record all input events to a JSONL file for replay-based screencasts.
        #[arg(long, value_name = "FILE")]
        trace: Option<std::path::PathBuf>,

        /// Show key press feedback overlay (useful for screencasts/demos).
        /// Also enabled by tui.show_keys config.
        #[arg(long)]
        show_keys: bool,

        /// Load only the last N chat messages on startup (overrides default pagination window).
        /// User can still scroll up to load more.
        #[arg(long, value_name = "N")]
        history_depth: Option<usize>,

        /// Start with a clean chat view (no history loaded). History is still
        /// persisted — this only affects the initial display. Prevents scrollback
        /// for this session.
        #[arg(long)]
        no_history: bool,
    },

    /// Dump the current TUI screen contents (requires a running `wg tui`)
    #[command(name = "tui-dump")]
    TuiDump {},

    /// Render TUI event traces into asciinema screencasts
    Screencast {
        #[command(subcommand)]
        command: ScreencastCommands,
    },

    /// Multi-user server setup automation
    Server {
        #[command(subcommand)]
        command: ServerCommands,
    },

    /// Interactive configuration wizard for first-time setup
    Setup {
        /// Provider (anthropic, openrouter, openai, local, custom) — enables non-interactive mode
        #[arg(long)]
        provider: Option<String>,
        /// Path to API key file
        #[arg(long)]
        api_key_file: Option<String>,
        /// Environment variable name for API key
        #[arg(long)]
        api_key_env: Option<String>,
        /// API endpoint URL
        #[arg(long)]
        url: Option<String>,
        /// Default model ID
        #[arg(long)]
        model: Option<String>,
        /// Skip API key validation
        #[arg(long)]
        skip_validation: bool,
    },

    /// Print a concise cheat sheet for agent onboarding
    Quickstart,

    /// Quick one-screen status overview
    Status,

    /// Show time counters and agent statistics
    Stats,

    /// Display cleanup and monitoring metrics
    Metrics {
        /// Output as JSON instead of formatted text
        #[arg(long)]
        json: bool,
    },

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

    /// Manage LLM endpoints (add, remove, list, test)
    Endpoints {
        #[command(subcommand)]
        command: EndpointsCommands,
    },

    /// Manage LLM endpoints (singular alias for 'endpoints')
    #[command(hide = true)]
    Endpoint {
        #[command(subcommand)]
        command: EndpointsCommands,
    },

    /// Browse and search available models from OpenRouter
    Models {
        #[command(subcommand)]
        command: ModelsCommands,
    },

    /// Model registry and routing management
    Model {
        #[command(subcommand)]
        command: ModelCommands,
    },

    /// Manage API keys for LLM providers
    Key {
        #[command(subcommand)]
        command: KeyCommands,
    },

    /// Interactive agentic REPL — coding assistant powered by any model
    Nex {
        /// Model to use (e.g., openrouter:qwen/qwen3-coder, ollama:llama3.2, sonnet)
        #[arg(long, short = 'm')]
        model: Option<String>,

        /// Named endpoint from config (e.g., openrouter, local)
        #[arg(long, short = 'e')]
        endpoint: Option<String>,

        /// Custom system prompt
        #[arg(long)]
        system_prompt: Option<String>,

        /// Initial message (skip the first prompt)
        message: Option<String>,

        /// Maximum conversation turns
        #[arg(long, default_value = "200")]
        max_turns: usize,

        /// Chatty mode: echo the full tool output content under each
        /// tool-call line, exactly as the model sees it (capped at
        /// 20 lines / 1600 bytes per call). Default shows only a
        /// one-line summary per call. Useful when actively following
        /// an agent's actions.
        #[arg(long, short = 'c')]
        chatty: bool,

        /// Verbose console output: implies `--chatty` and also emits
        /// compaction diagnostics, token accounting, and the
        /// session-log path banner. Useful for debugging the REPL
        /// itself. The on-disk NDJSON session log is always complete
        /// regardless of this flag.
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Read-only safety mode: only expose tools that cannot modify
        /// state (read_file, grep, web_search, web_fetch, etc.). Tools
        /// like write_file, edit_file, and bash (which can run arbitrary
        /// commands) are removed from the registry. Use this when you
        /// want to browse, research, or explore without risk of the
        /// agent modifying any files.
        #[arg(long, short = 'r')]
        read_only: bool,

        /// Resume a previous nex session. Three shapes:
        ///
        ///   `wg nex --resume`              — interactive picker
        ///                                   over all sessions,
        ///                                   most-recent first.
        ///   `wg nex --resume <pattern>`    — pattern-match the
        ///                                   most-recent session
        ///                                   whose alias / uuid
        ///                                   prefix / kind
        ///                                   contains `<pattern>`.
        ///   `wg nex --chat <uuid|alias>`   — address a specific
        ///                                   session directly
        ///                                   (works without
        ///                                   `--resume`).
        ///
        /// Bare `wg nex` (no flags) starts a FRESH session every
        /// time — no auto-resume.
        #[arg(long, value_name = "PATTERN", num_args = 0..=1, default_missing_value = "")]
        resume: Option<String>,

        /// Load an agency role/skill by name to augment the session.
        /// Searches `.workgraph/agency/primitives/components/` for a
        /// matching component and appends its content to the system
        /// prompt. Use "coordinator" to enable workgraph management
        /// tools (wg_add, wg_done) which are otherwise stripped in
        /// interactive mode.
        #[arg(long)]
        role: Option<String>,

        /// Run as a chat-tethered agent: read user turns from
        /// `<workgraph>/chat/<id>/inbox.jsonl`, write streaming tokens
        /// to `<workgraph>/chat/<id>/streaming`, append finalized
        /// assistant turns to `<workgraph>/chat/<id>/outbox.jsonl`.
        /// Bypasses stdin/stderr. When set, the journal is stored at
        /// `<workgraph>/chat/<id>/conversation.jsonl` so `--resume`
        /// picks up the right session automatically.
        ///
        /// Primary use case: this is how `wg nex` serves as the
        /// coordinator (spawned by the service / a graph task with a
        /// chat tether to the TUI). Pair with `--role coordinator`
        /// for the wg_* mutation tools.
        #[arg(long = "chat-id")]
        chat_id: Option<u32>,

        /// Bind this nex session to a chat dir by reference. Accepts
        /// a UUID, a UUID prefix (≥4 chars), or an alias like
        /// `coordinator-0` / `task-<id>` / a user-chosen handle.
        /// If the reference doesn't yet resolve to a session, a new
        /// session is created under that alias. Same effect as
        /// `--chat-id` except not limited to numeric ids.
        #[arg(long = "chat")]
        chat_ref: Option<String>,

        /// Run in autonomous mode — EndTurn exits the loop instead
        /// of prompting for next input. Used when a task-agent
        /// spawns `wg nex` as a one-shot executor.
        #[arg(long = "autonomous")]
        autonomous: bool,

        /// Skip the MCP server spawn/discover step at startup. Use
        /// this when MCP tooling is misconfigured or when you want a
        /// deterministic, minimal tool surface for debugging.
        #[arg(long = "no-mcp")]
        no_mcp: bool,
    },

    /// Interactive agentic TUI — ratatui-based nex (two-pane with streaming + Ctrl-C cancel)
    #[command(name = "tui-nex")]
    TuiNex {
        /// Model to use (e.g., openrouter:qwen/qwen3-coder, ollama:llama3.2, sonnet)
        #[arg(long, short = 'm')]
        model: Option<String>,

        /// Named endpoint from config
        #[arg(long, short = 'e')]
        endpoint: Option<String>,
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

        /// Model to use (e.g., anthropic/claude-sonnet-4-6)
        #[arg(long)]
        model: Option<String>,

        /// LLM provider (e.g., anthropic, openai)
        #[arg(long)]
        provider: Option<String>,

        /// Named endpoint from config (e.g., openrouter, anthropic-prod)
        #[arg(long)]
        endpoint_name: Option<String>,

        /// Endpoint URL override
        #[arg(long)]
        endpoint_url: Option<String>,

        /// Pre-resolved API key (avoids re-resolution from config/files)
        #[arg(long)]
        api_key: Option<String>,

        /// Maximum agent turns before stopping
        #[arg(long, default_value = "100")]
        max_turns: usize,

        /// Disable resume from existing conversation journal (start fresh)
        #[arg(long, default_value = "false")]
        no_resume: bool,
    },

    /// Apply placement agent output (internal, called by wrapper script)
    #[command(name = "apply-placement", hide = true)]
    ApplyPlacement {
        /// Path to the agent output directory (contains raw_stream.jsonl)
        output_dir: String,

        /// Source task ID (the task being placed)
        source_task_id: String,
    },
}

#[derive(Subcommand)]
pub enum WorktreeCommand {
    /// List all agent worktrees with size, age, and uncommitted-changes status
    List,

    /// Archive an agent's worktree: auto-commit uncommitted work, optionally remove
    Archive {
        /// Agent ID (e.g., agent-16803)
        agent_id: String,

        /// Remove the worktree directory after committing.
        /// Without this flag, the directory is preserved on disk.
        #[arg(long)]
        remove: bool,
    },

    /// Garbage-collect stale worktrees. Dry-run by default — use --execute
    /// to actually remove. Filters (at least one recommended) narrow which
    /// worktrees qualify; with no filters, nothing is removed.
    Gc {
        /// Actually perform the removal. Without this flag, prints what
        /// would be removed and exits.
        #[arg(long)]
        execute: bool,

        /// Only consider worktrees older than this duration (e.g. "7d", "24h").
        /// Age is the last-modification time of the worktree directory.
        #[arg(long)]
        older: Option<String>,

        /// Only consider worktrees whose owning agent is no longer alive
        /// (process gone, registry status dead, or no registry entry).
        #[arg(long)]
        dead_only: bool,
    },
}

#[derive(Subcommand)]
pub enum EndpointsCommands {
    /// List all configured endpoints
    List,

    /// Add a new endpoint
    Add {
        /// Endpoint name (e.g., "openrouter", "anthropic-prod")
        name: String,

        /// Provider type: anthropic, openai, openrouter, local
        #[arg(long)]
        provider: Option<String>,

        /// API endpoint URL (defaults based on provider)
        #[arg(long)]
        url: Option<String>,

        /// Default model for this endpoint
        #[arg(long)]
        model: Option<String>,

        /// API key (prefer --api-key-file for security)
        #[arg(long)]
        api_key: Option<String>,

        /// Path to a file containing the API key
        #[arg(long)]
        api_key_file: Option<String>,

        /// Environment variable name to read the API key from
        #[arg(long)]
        key_env: Option<String>,

        /// Set as the default endpoint
        #[arg(long)]
        default: bool,

        /// Target global config (~/.workgraph/config.toml)
        #[arg(long)]
        global: bool,
    },

    /// Update an existing endpoint (only specified fields are changed)
    Update {
        /// Endpoint name to update
        name: String,

        /// Provider type: anthropic, openai, openrouter, local
        #[arg(long)]
        provider: Option<String>,

        /// API endpoint URL (defaults based on provider)
        #[arg(long)]
        url: Option<String>,

        /// Default model for this endpoint
        #[arg(long)]
        model: Option<String>,

        /// API key (prefer --api-key-file for security)
        #[arg(long)]
        api_key: Option<String>,

        /// Path to a file containing the API key
        #[arg(long)]
        api_key_file: Option<String>,

        /// Environment variable name to read the API key from
        #[arg(long)]
        key_env: Option<String>,

        /// Set as the default endpoint
        #[arg(long)]
        default: bool,

        /// Target global config (~/.workgraph/config.toml)
        #[arg(long)]
        global: bool,
    },

    /// Remove an endpoint by name
    Remove {
        /// Endpoint name to remove
        name: String,

        /// Target global config (~/.workgraph/config.toml)
        #[arg(long)]
        global: bool,
    },

    /// Set an endpoint as the default
    SetDefault {
        /// Endpoint name to set as default
        name: String,

        /// Target global config (~/.workgraph/config.toml)
        #[arg(long)]
        global: bool,
    },

    /// Test endpoint connectivity (hits /models API)
    Test {
        /// Endpoint name to test
        name: String,
    },
}

#[derive(Subcommand)]
pub enum ModelsCommands {
    /// List models from the local registry
    List {
        /// Filter by tier (frontier, mid, budget)
        #[arg(long)]
        tier: Option<String>,
    },

    /// Search models from OpenRouter by name, ID, or description
    Search {
        /// Search query (matches against model ID, name, and description)
        query: String,

        /// Only show models that support tool use (function calling)
        #[arg(long)]
        tools: bool,

        /// Skip the local cache and fetch fresh data from the API
        #[arg(long)]
        no_cache: bool,

        /// Maximum number of results to show (default: 50)
        #[arg(long, default_value = "50")]
        limit: usize,
    },

    /// List all models available on OpenRouter (remote API)
    Remote {
        /// Only show models that support tool use (function calling)
        #[arg(long)]
        tools: bool,

        /// Skip the local cache and fetch fresh data from the API
        #[arg(long)]
        no_cache: bool,

        /// Maximum number of results to show (default: 100)
        #[arg(long, default_value = "100")]
        limit: usize,
    },

    /// Add a custom model to the local registry
    Add {
        /// Model ID (e.g. "anthropic/claude-opus-4-6")
        id: String,

        /// Provider name
        #[arg(long)]
        provider: Option<String>,

        /// Cost per 1M input tokens (USD)
        #[arg(long, name = "cost-in")]
        cost_in: f64,

        /// Cost per 1M output tokens (USD)
        #[arg(long, name = "cost-out")]
        cost_out: f64,

        /// Context window size in tokens
        #[arg(long)]
        context_window: Option<u64>,

        /// Capability tags (e.g. coding, analysis, tool_use)
        #[arg(long, short)]
        capability: Vec<String>,

        /// Tier classification (frontier, mid, budget)
        #[arg(long, default_value = "mid")]
        tier: String,
    },

    /// Set the default model
    SetDefault {
        /// Model ID to set as default
        id: String,
    },

    /// Initialize the models.yaml with defaults
    Init,

    /// Fetch model data from OpenRouter and build the benchmark registry
    Fetch {
        /// Skip the local cache and fetch fresh data from the API
        #[arg(long)]
        no_cache: bool,
    },

    /// Show the benchmark registry with fitness scores and tier classification
    Benchmarks {
        /// Filter by tier (frontier, mid, budget)
        #[arg(long)]
        tier: Option<String>,

        /// Maximum number of models to display
        #[arg(long, default_value = "50")]
        limit: usize,
    },
}

#[derive(Subcommand)]
pub enum ModelCommands {
    /// Show all models in the registry (built-in + user-defined)
    List {
        /// Filter by tier (fast, standard, premium)
        #[arg(long)]
        tier: Option<String>,
    },

    /// Add or update a model in the config registry
    Add {
        /// Short alias for the model (e.g., "gpt-4o", "claude-via-openrouter")
        alias: String,

        /// Provider: anthropic, openai, openrouter, local
        #[arg(long)]
        provider: String,

        /// Full API model identifier (defaults to alias if omitted)
        #[arg(long)]
        model_id: Option<String>,

        /// Quality tier: fast, standard, premium
        #[arg(long, default_value = "standard")]
        tier: String,

        /// Named endpoint to use for this model
        #[arg(long)]
        endpoint: Option<String>,

        /// Context window in tokens
        #[arg(long)]
        context_window: Option<u64>,

        /// Cost per million input tokens (USD)
        #[arg(long)]
        cost_in: Option<f64>,

        /// Cost per million output tokens (USD)
        #[arg(long)]
        cost_out: Option<f64>,

        /// Write to global config (~/.workgraph/config.toml)
        #[arg(long)]
        global: bool,
    },

    /// Remove a model from the config registry
    Remove {
        /// Model alias to remove
        alias: String,

        /// Skip confirmation for entries referenced by roles
        #[arg(long)]
        force: bool,

        /// Write to global config
        #[arg(long)]
        global: bool,
    },

    /// Set the default model for agent dispatch
    SetDefault {
        /// Model alias (must exist in registry)
        alias: String,

        /// Write to global config
        #[arg(long)]
        global: bool,
    },

    /// Show per-role model routing configuration
    Routing,

    /// Set the model for a specific dispatch role
    Set {
        /// Role name (e.g., default, evaluator, triage, compactor)
        role: String,

        /// Model alias or ID
        model: String,

        /// Also set provider for this role
        #[arg(long)]
        provider: Option<String>,

        /// Also set endpoint for this role
        #[arg(long)]
        endpoint: Option<String>,

        /// Set tier override instead of direct model
        #[arg(long)]
        tier: Option<String>,

        /// Write to global config
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand)]
pub enum KeyCommands {
    /// Configure an API key for a provider
    Set {
        /// Provider name (e.g., openrouter, anthropic, openai)
        provider: String,

        /// Reference an environment variable by name
        #[arg(long)]
        env: Option<String>,

        /// Path to a file containing the key
        #[arg(long)]
        file: Option<String>,

        /// Store key value directly (written to ~/.workgraph/keys/<provider>.key, NOT to config)
        #[arg(long)]
        value: Option<String>,

        /// Apply to global config (~/.workgraph/config.toml)
        #[arg(long)]
        global: bool,
    },

    /// Validate API key availability and status
    Check {
        /// Provider name (omit to check all)
        provider: Option<String>,
    },

    /// Show key configuration status for all providers
    List,
}

#[derive(Subcommand)]
pub enum OpenRouterCommands {
    /// Show OpenRouter API key status and usage
    Status,
    /// Show session cost summary
    Session,
    /// Set cost cap limits
    SetLimit {
        /// Global cost cap in USD
        #[arg(long)]
        global: Option<f64>,
        /// Session cost cap in USD
        #[arg(long)]
        session: Option<f64>,
        /// Task cost cap in USD
        #[arg(long)]
        task: Option<f64>,
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
pub enum UserCommands {
    /// Create a user board (defaults to current user)
    Init {
        /// User handle (default: $WG_USER or $USER)
        name: Option<String>,
    },

    /// List all user boards (active + archived)
    List,

    /// Archive the active board and create a successor
    Archive {
        /// User handle (default: $WG_USER or $USER)
        name: Option<String>,
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
pub enum ProfileCommands {
    /// Set the active provider profile
    Set {
        /// Profile name (e.g., anthropic, openrouter, openai)
        name: String,

        /// Pin the fast tier to a specific model (e.g., openrouter:qwen/qwen3-coder)
        #[arg(long)]
        fast: Option<String>,

        /// Pin the standard tier to a specific model (e.g., openrouter:deepseek/deepseek-r1)
        #[arg(long)]
        standard: Option<String>,

        /// Pin the premium tier to a specific model (e.g., openrouter:qwen/qwen3-max)
        #[arg(long)]
        premium: Option<String>,
    },
    /// Show current profile and resolved model mappings
    Show {
        /// Show raw metrics (pricing, context length, benchmark scores) per model
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// List available profiles
    List,
    /// Refresh model data from OpenRouter and recompute rankings
    Refresh,
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

        /// Enable autopoietic cycle mode (back-edge from evaluate to partition)
        #[arg(long, alias = "cycle")]
        autopoietic: bool,

        /// Max cycle iterations (default: 3, requires --autopoietic)
        #[arg(long)]
        max_iterations: Option<u32>,

        /// Seconds between cycle iterations (default: 3600, requires --autopoietic)
        #[arg(long)]
        cycle_delay: Option<u64>,

        /// Force fan-out mode even with <50 evaluations
        #[arg(long)]
        force_fanout: bool,

        /// Force legacy single-shot mode even with ≥50 evaluations
        #[arg(long, conflicts_with = "force_fanout")]
        single_shot: bool,
    },

    /// Apply a synthesis-result.json from a fan-out evolution run
    Apply {
        /// Path to synthesis-result.json
        synthesis_file: std::path::PathBuf,

        /// Output path for apply-results.json (default: auto-derived from synthesis file path)
        #[arg(long, short = 'o')]
        output: Option<std::path::PathBuf>,
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

        /// Group stats by task type (research, implementation, fix, design, test, docs, refactor)
        #[arg(long)]
        by_task_type: bool,
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

    /// Import Agency's starter.csv primitives into WorkGraph
    Import {
        /// Path to the CSV file to import (omit when using --url or --upstream)
        csv_path: Option<String>,

        /// Fetch CSV from a remote URL instead of local file
        #[arg(long)]
        url: Option<String>,

        /// Fetch from the configured upstream URL (agency.upstream_url in config)
        #[arg(long)]
        upstream: bool,

        /// Show what would be imported without writing files
        #[arg(long)]
        dry_run: bool,

        /// Provenance tag (default: agency-import)
        #[arg(long)]
        tag: Option<String>,

        /// Re-import even if manifest hash matches (skip change detection)
        #[arg(long)]
        force: bool,

        /// Only check if upstream has changed (exit 0 = changed, exit 1 = same)
        #[arg(long)]
        check: bool,
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
pub enum SessionCommands {
    /// List every nex session in this workgraph.
    List {
        /// Print UUIDs + aliases as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Open a live view of an existing session. Tails `.streaming`
    /// and `outbox.jsonl` — new tokens appear as they're emitted by
    /// whichever process owns the session.
    Attach {
        /// Session reference: UUID, prefix, or alias.
        session: String,
    },

    /// Register a new, empty session with a chosen alias. Useful
    /// for pre-allocating a handle so something else (e.g. a
    /// spawned `wg nex --chat <alias>`) can pick it up.
    New {
        /// Alias (human handle) for the new session.
        alias: String,

        /// Optional longer descriptive label.
        #[arg(long)]
        label: Option<String>,
    },

    /// Manage aliases on an existing session.
    Alias {
        #[command(subcommand)]
        command: SessionAliasCommands,
    },

    /// Delete a session (registry entry + chat dir + all aliases).
    Rm {
        /// Session reference: UUID, prefix, or alias.
        session: String,
    },
}

#[derive(Subcommand)]
pub enum SessionAliasCommands {
    /// Add an alias to an existing session.
    Add {
        /// Session reference: UUID, prefix, or existing alias.
        session: String,
        /// New alias to install.
        alias: String,
    },
    /// Remove an alias from a session. The session itself stays.
    Rm {
        /// Alias to remove.
        alias: String,
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

        /// Preferred model (e.g., opus, sonnet, haiku, or full model ID)
        #[arg(long)]
        model: Option<String>,

        /// Preferred provider (e.g., anthropic, openrouter)
        #[arg(long)]
        provider: Option<String>,
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
pub enum ScreencastCommands {
    /// Render a TUI event trace into an asciinema .cast file
    Render {
        /// Path to the trace JSONL file produced by `wg tui --trace`
        #[arg(long)]
        trace: std::path::PathBuf,

        /// Output .cast file path
        #[arg(long)]
        output: std::path::PathBuf,

        /// Idle compression ratio as threshold:target (e.g. 5:2 compresses gaps >5s to 2s)
        #[arg(long, default_value = "5:2")]
        compress_idle: String,

        /// Target total recording duration in seconds (optional)
        #[arg(long)]
        target_duration: Option<f64>,

        /// Terminal width for the recording
        #[arg(long, default_value = "120")]
        width: u16,

        /// Terminal height for the recording
        #[arg(long, default_value = "36")]
        height: u16,
    },

    /// Launch an autopilot that drives the TUI for screencast recording
    Autopilot {
        /// Output .cast file path
        #[arg(long, default_value = "screencast.cast")]
        output: std::path::PathBuf,

        /// Terminal width
        #[arg(long, default_value = "80")]
        cols: u16,

        /// Terminal height
        #[arg(long, default_value = "24")]
        rows: u16,

        /// Maximum recording duration in seconds
        #[arg(long, default_value = "60")]
        duration: f64,
    },
}

#[derive(Subcommand)]
pub enum ServerCommands {
    /// Initialize multi-user server setup (dry-run by default)
    Init {
        /// Actually apply changes (default is dry-run)
        #[arg(long)]
        apply: bool,

        /// Unix group name (default: wg-<project>)
        #[arg(long)]
        group: Option<String>,

        /// Users to add to the project group (repeatable)
        #[arg(long = "user")]
        users: Vec<String>,

        /// Generate ttyd configuration for web terminal access
        #[arg(long)]
        ttyd: bool,

        /// Generate Caddy reverse-proxy configuration
        #[arg(long)]
        caddy: bool,

        /// Port for ttyd web terminal (default: 7681)
        #[arg(long, default_value = "7681")]
        ttyd_port: u16,
    },

    /// Create or attach to a user's tmux session
    Connect {
        /// User name (defaults to $WG_USER)
        #[arg(long)]
        user: Option<String>,
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

    /// Restart the service daemon (graceful stop then start)
    ///
    /// Stops the running daemon without killing agents, then starts a new one
    /// with the same configuration. Running agents continue independently.
    Restart,

    /// Pause the coordinator (running agents continue, no new spawns)
    Pause,

    /// Resume the coordinator
    Resume,

    /// Freeze all agents (SIGSTOP) and pause the service
    ///
    /// Sends SIGSTOP to all running agent processes, stopping them immediately
    /// while keeping all state in memory. Also pauses the coordinator so no new
    /// agents are spawned. Use `wg service thaw` to resume.
    ///
    /// Note: TCP connections may time out if frozen too long (~30-60s).
    /// Agents/executors should handle reconnection on resume.
    Freeze,

    /// Thaw frozen agents (SIGCONT) and resume the service
    ///
    /// Sends SIGCONT to all previously frozen agent processes, resuming them
    /// exactly where they left off. Also resumes the coordinator.
    Thaw,

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

    /// Create a new coordinator session
    CreateCoordinator {
        /// Optional name for the coordinator
        #[arg(long)]
        name: Option<String>,
        /// Model for this coordinator (e.g., "openai:qwen3-coder-30b")
        #[arg(long)]
        model: Option<String>,
        /// Executor for this coordinator (e.g., "native", "claude")
        #[arg(long)]
        executor: Option<String>,
    },

    /// Delete a coordinator session
    DeleteCoordinator {
        /// Coordinator ID to delete
        id: u32,
    },

    /// Archive a coordinator session (mark as Done)
    ArchiveCoordinator {
        /// Coordinator ID to archive
        id: u32,
    },

    /// Stop a coordinator session (kill agent, reset to Open)
    StopCoordinator {
        /// Coordinator ID to stop
        id: u32,
    },

    /// Interrupt a coordinator's current generation (sends SIGINT, preserves context)
    InterruptCoordinator {
        /// Coordinator ID to interrupt
        id: u32,
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

    /// Poll for replies from the configured Telegram chat
    ///
    /// Calls the Telegram Bot API getUpdates endpoint and filters for messages
    /// from the configured chat_id. Returns the reply text or empty/timeout.
    Poll {
        /// Maximum time to wait for a reply in seconds (default: 120)
        #[arg(long, default_value = "120")]
        timeout: u64,

        /// Target chat ID (uses configured chat_id if not specified)
        #[arg(long)]
        chat_id: Option<String>,
    },

    /// Send a message and wait for reply
    ///
    /// Sends the message and polls for reply at intervals. Times out after
    /// configurable max wait. Includes task ID context in sent messages.
    Ask {
        /// Message to send and wait for reply to
        message: String,

        /// Maximum time to wait for a reply in seconds (default: 600)
        #[arg(long, default_value = "600")]
        timeout: u64,

        /// Polling interval in seconds (default: 30)
        #[arg(long, default_value = "30")]
        interval: u64,

        /// Target chat ID (uses configured chat_id if not specified)
        #[arg(long)]
        chat_id: Option<String>,

        /// Task ID to include in message context (optional)
        #[arg(long)]
        task_id: Option<String>,
    },
}

/// Get the command name from a Commands enum variant for usage tracking
pub fn command_name(cmd: &Commands) -> &'static str {
    match cmd {
        Commands::Init { .. } => "init",
        Commands::Insert { .. } => "insert",
        Commands::Rescue { .. } => "rescue",
        Commands::Reset { .. } => "reset",
        Commands::Add { .. } => "add",
        Commands::Edit { .. } => "edit",
        Commands::Done { .. } => "done",
        Commands::Fail { .. } => "fail",
        Commands::Abandon { .. } => "abandon",
        Commands::Retry { .. } => "retry",
        Commands::Requeue { .. } => "requeue",
        Commands::Approve { .. } => "approve",
        Commands::Reject { .. } => "reject",
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
        Commands::Cleanup { .. } => "cleanup",
        Commands::Cycles => "cycles",
        Commands::List { .. } => "list",
        Commands::Viz { .. } => "viz",
        Commands::GraphExport { .. } => "graph-export",
        Commands::Cost { .. } => "cost",
        Commands::Coordinate { .. } => "coordinate",
        Commands::Plan { .. } => "plan",
        Commands::Reschedule { .. } => "reschedule",
        Commands::Reprioritize { .. } => "reprioritize",
        Commands::Impact { .. } => "impact",
        Commands::Structure => "structure",
        Commands::Bottlenecks => "bottlenecks",
        Commands::Velocity { .. } => "velocity",
        Commands::Aging => "aging",
        Commands::Forecast => "forecast",
        Commands::Workload => "workload",
        Commands::Worktree(_) => "worktree",
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
        Commands::Tokens { .. } => "tokens",
        Commands::Msg { .. } => "msg",
        Commands::User { .. } => "user",
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
        Commands::Compact => "compact",
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
        Commands::Profile { .. } => "profile",
        Commands::Config { .. } => "config",
        Commands::DeadAgents { .. } => "dead-agents",
        Commands::Sweep { .. } => "sweep",
        Commands::Agents { .. } => "agents",
        Commands::Kill { .. } => "kill",
        Commands::Reap { .. } => "reap",
        Commands::Server { .. } => "server",
        Commands::Service { .. } => "service",
        Commands::Screencast { .. } => "screencast",
        Commands::Tui { .. } => "tui",
        Commands::TuiDump { .. } => "tui-dump",
        Commands::Setup { .. } => "setup",
        Commands::Quickstart => "quickstart",
        Commands::Status => "status",
        Commands::Stats => "stats",
        Commands::Metrics { .. } => "metrics",
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Notify { .. } => "notify",
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Matrix { .. } => "matrix",
        Commands::Telegram { .. } => "telegram",
        Commands::Chat { .. } => "chat",
        Commands::Endpoints { .. } | Commands::Endpoint { .. } => "endpoints",
        Commands::Models { .. } => "models",
        Commands::Model { .. } => "model",
        Commands::Key { .. } => "key",
        Commands::Nex { .. } => "nex",
        Commands::TuiNex { .. } => "tui-nex",
        Commands::NativeExec { .. } => "native-exec",
        Commands::Spend { .. } => "spend",
        Commands::Openrouter { .. } => "openrouter",
        Commands::ApplyPlacement { .. } => "apply-placement",
        Commands::Session { .. } => "session",
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
            | Commands::Worktree(_)
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
            | Commands::Tokens { .. }
            | Commands::Msg { .. }
            | Commands::User { .. }
            | Commands::Resource { .. }
            | Commands::Skill { .. }
            | Commands::Agency { .. }
            | Commands::Peer { .. }
            | Commands::Role { .. }
            | Commands::Tradeoff { .. }
            | Commands::Match { .. }
            | Commands::Heartbeat { .. }
            | Commands::Checkpoint { .. }
            | Commands::Compact
            | Commands::Artifact { .. }
            | Commands::Context { .. }
            | Commands::Next { .. }
            | Commands::Trajectory { .. }
            | Commands::Agent { .. }
            | Commands::Evaluate { .. }
            | Commands::Watch { .. }
            | Commands::Evolve { .. }
            | Commands::Profile { .. }
            | Commands::Config { .. }
            | Commands::DeadAgents { .. }
            | Commands::Sweep { .. }
            | Commands::Agents { .. }
            | Commands::Kill { .. }
            | Commands::Reap { .. }
            | Commands::Service { .. }
            | Commands::Screencast { .. }
            | Commands::Cost { .. }
            | Commands::Check
            | Commands::Cleanup { .. }
            | Commands::Cycles
            | Commands::Quickstart
            | Commands::Status
            | Commands::Stats
            | Commands::Metrics { .. }
            | Commands::Chat { .. }
            | Commands::Telegram { .. }
            | Commands::Endpoints { .. }
            | Commands::Endpoint { .. }
            | Commands::Models { .. }
            | Commands::Model { .. }
            | Commands::Key { .. }
            | Commands::TuiDump { .. }
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

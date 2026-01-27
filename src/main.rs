use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod commands;

#[derive(Parser)]
#[command(name = "wg")]
#[command(about = "Workgraph - A lightweight work coordination graph")]
#[command(version)]
struct Cli {
    /// Path to the workgraph directory (default: .workgraph in current dir)
    #[arg(long, global = true)]
    dir: Option<PathBuf>,

    /// Output as JSON for machine consumption
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
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
        #[arg(long, short = 'd')]
        description: Option<String>,

        /// This task is blocked by another task
        #[arg(long = "blocked-by")]
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
    },

    /// Mark a task as done
    Done {
        /// Task ID to mark as done
        id: String,
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

    /// Show the full graph (DOT format for Graphviz)
    Graph {
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

    /// Show actor workload balance and assignment distribution
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

    /// Visualize the graph with filtering options
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

        /// Output format (dot, mermaid)
        #[arg(long, default_value = "dot")]
        format: String,

        /// Render directly to file (requires dot installed)
        #[arg(long, short)]
        output: Option<String>,
    },

    /// Manage resources
    Resource {
        #[command(subcommand)]
        command: ResourceCommands,
    },

    /// Manage actors
    Actor {
        #[command(subcommand)]
        command: ActorCommands,
    },

    /// List and find skills across tasks
    Skills {
        /// Show skills for a specific task
        #[arg(long)]
        task: Option<String>,

        /// Find tasks requiring a specific skill
        #[arg(long)]
        find: Option<String>,
    },

    /// Find actors capable of performing a task
    Match {
        /// Task ID to match actors against
        task: String,
    },

    /// Record actor/agent heartbeat or check for stale actors/agents
    Heartbeat {
        /// Actor or agent ID to record heartbeat for (omit to check status)
        /// Agent IDs start with "agent-" (e.g., agent-1, agent-7)
        actor: Option<String>,

        /// Check for stale actors (no heartbeat within threshold)
        #[arg(long)]
        check: bool,

        /// Check for stale agents instead of actors
        #[arg(long)]
        agents: bool,

        /// Minutes without heartbeat before actor/agent is considered stale (default: 5)
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

    /// Find the best next task for an actor (agent work loop)
    Next {
        /// Actor ID to find tasks for
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

    /// Run autonomous agent loop (wake/check/work/sleep cycle)
    Agent {
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
enum ActorCommands {
    /// Add a new actor
    Add {
        /// Actor ID
        id: String,

        /// Display name
        #[arg(long)]
        name: Option<String>,

        /// Role (engineer, pm, agent, etc.)
        #[arg(long)]
        role: Option<String>,

        /// Hourly rate
        #[arg(long)]
        rate: Option<f64>,

        /// Available hours (capacity)
        #[arg(long)]
        capacity: Option<f64>,

        /// Capabilities/skills this actor has (can be repeated)
        #[arg(long = "capability", short = 'c')]
        capabilities: Vec<String>,

        /// Maximum context size in tokens
        #[arg(long)]
        context_limit: Option<u64>,

        /// Trust level: verified, provisional, unknown
        #[arg(long)]
        trust_level: Option<String>,
    },

    /// List all actors
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let workgraph_dir = cli.dir.unwrap_or_else(|| PathBuf::from(".workgraph"));

    match cli.command {
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
        ),
        Commands::Done { id } => commands::done::run(&workgraph_dir, &id),
        Commands::Fail { id, reason } => commands::fail::run(&workgraph_dir, &id, reason.as_deref()),
        Commands::Abandon { id, reason } => commands::abandon::run(&workgraph_dir, &id, reason.as_deref()),
        Commands::Retry { id } => commands::retry::run(&workgraph_dir, &id),
        Commands::Claim { id, actor } => commands::claim::claim(&workgraph_dir, &id, actor.as_deref()),
        Commands::Unclaim { id } => commands::claim::unclaim(&workgraph_dir, &id),
        Commands::Reclaim { id, from, to } => commands::reclaim::run(&workgraph_dir, &id, &from, &to),
        Commands::Ready => commands::ready::run(&workgraph_dir, cli.json),
        Commands::Blocked { id } => commands::blocked::run(&workgraph_dir, &id, cli.json),
        Commands::WhyBlocked { id } => commands::why_blocked::run(&workgraph_dir, &id, cli.json),
        Commands::Check => commands::check::run(&workgraph_dir),
        Commands::List { status } => commands::list::run(&workgraph_dir, status.as_deref(), cli.json),
        Commands::Graph { archive, since, until } => {
            commands::graph::run(&workgraph_dir, archive, since.as_deref(), until.as_deref())
        }
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
                commands::log::run_add(&workgraph_dir, &id, message.as_deref().unwrap(), actor.as_deref())
            }
        }
        Commands::Viz {
            all,
            status,
            critical_path,
            format,
            output,
        } => {
            let fmt = format.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let options = commands::viz::VizOptions {
                all,
                status,
                critical_path,
                format: fmt,
                output,
            };
            commands::viz::run(&workgraph_dir, options)
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
        Commands::Actor { command } => match command {
            ActorCommands::Add {
                id,
                name,
                role,
                rate,
                capacity,
                capabilities,
                context_limit,
                trust_level,
            } => commands::actor::run_add(
                &workgraph_dir,
                &id,
                name.as_deref(),
                role.as_deref(),
                rate,
                capacity,
                &capabilities,
                context_limit,
                trust_level.as_deref(),
            ),
            ActorCommands::List => commands::actor::run_list(&workgraph_dir, cli.json),
        },
        Commands::Skills { task, find } => {
            if let Some(task_id) = task {
                commands::skills::run_task(&workgraph_dir, &task_id, cli.json)
            } else if let Some(skill) = find {
                commands::skills::run_find(&workgraph_dir, &skill, cli.json)
            } else {
                commands::skills::run_list(&workgraph_dir, cli.json)
            }
        }
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
        Commands::Agent {
            actor,
            once,
            interval,
            max_tasks,
            reset_state,
        } => commands::agent::run(&workgraph_dir, &actor, once, interval, max_tasks, reset_state, cli.json),
        Commands::Spawn {
            task,
            executor,
            timeout,
        } => commands::spawn::run(&workgraph_dir, &task, &executor, timeout.as_deref(), cli.json),
        Commands::Config {
            show,
            init,
            executor,
            model,
            set_interval,
        } => {
            if init {
                commands::config_cmd::init(&workgraph_dir)
            } else if show || (executor.is_none() && model.is_none() && set_interval.is_none()) {
                commands::config_cmd::show(&workgraph_dir, cli.json)
            } else {
                commands::config_cmd::update(
                    &workgraph_dir,
                    executor.as_deref(),
                    model.as_deref(),
                    set_interval,
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
    }
}

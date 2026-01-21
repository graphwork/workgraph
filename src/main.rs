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
    },

    /// Mark a task as done
    Done {
        /// Task ID to mark as done
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

    /// Show the full graph
    Graph,

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
        ),
        Commands::Done { id } => commands::done::run(&workgraph_dir, &id),
        Commands::Claim { id, actor } => commands::claim::claim(&workgraph_dir, &id, actor.as_deref()),
        Commands::Unclaim { id } => commands::claim::unclaim(&workgraph_dir, &id),
        Commands::Ready => commands::ready::run(&workgraph_dir, cli.json),
        Commands::Blocked { id } => commands::blocked::run(&workgraph_dir, &id, cli.json),
        Commands::WhyBlocked { id } => commands::why_blocked::run(&workgraph_dir, &id, cli.json),
        Commands::Check => commands::check::run(&workgraph_dir),
        Commands::List { status } => commands::list::run(&workgraph_dir, status.as_deref(), cli.json),
        Commands::Graph => commands::graph::run(&workgraph_dir),
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
            } => commands::actor::run_add(
                &workgraph_dir,
                &id,
                name.as_deref(),
                role.as_deref(),
                rate,
                capacity,
            ),
            ActorCommands::List => commands::actor::run_list(&workgraph_dir, cli.json),
        },
    }
}

use anyhow::Result;

const QUICKSTART_TEXT: &str = r#"
╔══════════════════════════════════════════════════════════════════════════════╗
║                         WORKGRAPH AGENT QUICKSTART                           ║
╚══════════════════════════════════════════════════════════════════════════════╝

GETTING STARTED
─────────────────────────────────────────
  wg init                     # Create a .workgraph directory
  wg setup                    # Interactive config wizard (executor, model, agency)
  wg agency init              # Bootstrap roles, tradeoffs, and a default agent
  wg service start            # Start the coordinator
  wg add "My first task"      # Add work — the service dispatches automatically

SKILL & BUNDLE SETUP (required for agents to use wg)
─────────────────────────────────────────
  Spawned agents need to know how to use workgraph. Without the right
  skill or bundle installed, agents won't know wg commands exist.

  Claude Code executor:
    wg skill install             # Installs ~/.claude/skills/wg/SKILL.md
                                 # This is injected into every Claude Code session

  Amplifier executor:
    cd ~/amplifier-bundle-workgraph && ./setup.sh
                                 # Installs the workgraph bundle and executor
                                 # Then use: amplifier run -B workgraph

  Custom executor:
    Ensure your executor's agent prompt includes wg CLI instructions.
    See 'wg quickstart' (this text) for the command reference.

  ⚠ If agents are spawned without the skill/bundle, they will not know
    how to call wg log, wg done, wg artifact, etc. — and tasks will fail.

AGENCY SETUP
─────────────────────────────────────────
  'wg agency init' creates sensible defaults so the service can auto-assign
  agents to tasks immediately. It sets up:

  • Roles     — what agents do (Programmer, Reviewer, Documenter, Architect)
  • Tradeoffs — constraints on how (Careful, Fast, Thorough, Balanced)
  • Agent     — a role+tradeoff pairing (default: Careful Programmer)
  • Config    — enables auto_assign and auto_evaluate

  Additional agency toggles:
    wg config --auto-place true      # Auto-place new tasks in the graph
    wg config --auto-create true     # Auto-invoke creator agent for new primitives

  Placement: when auto_place is enabled, the coordinator creates .place-* tasks
  for newly added tasks to determine optimal graph wiring (dependencies, context).

  Model registry: each dispatch role has a tier-based model default. View and
  configure per-role models:
    wg config --models                # Show all role→model assignments
    wg config --set-model <role> <m>  # Set model for a role (e.g., evolver opus)
    wg config --registry              # Show registered models and tiers
    wg config --tier <tier>=<model>   # Change which model a tier uses

  You can also set up manually:
    wg role add "Name" --outcome "What it produces" --skill skill-name
    wg tradeoff add "Name" --accept "Slow" --reject "Untested"
    wg agent create "Name" --role <hash> --tradeoff <hash>
    wg config --auto-assign true --auto-evaluate true

⚠ COORDINATOR SERVICE REMINDER ⚠
─────────────────────────────────────────
  Check if the coordinator is running:  wg service status

  If it IS running, your job is to DEFINE work, not DISPATCH it.
  Add tasks and dependencies — the coordinator handles the rest.
  Never manually 'wg spawn' or 'wg claim' while the service is running;
  you'll collide with the coordinator and get 'already claimed' errors.

  If it is NOT running, choose a mode below.

SERVICE MODE (recommended for parallel work)
─────────────────────────────────────────
  wg service start --max-agents 5  # Start coordinator with parallelism limit

  The coordinator automatically spawns agents on ready tasks. Just add tasks:

  wg add "Do the thing" --after prerequisite-task

  Monitor with wg agents and wg list. Do NOT manually wg spawn or wg claim —
  the coordinator handles this.

  wg service status           # Check if running, see last tick
  wg agents                   # Who's working on what
  wg list                     # What's done, what's pending
  wg tui                      # Interactive dashboard

MANUAL MODE (no service running)
─────────────────────────────────────────
  wg ready                    # See tasks available to work on
  wg claim <task-id>          # Claim a task (sets status to in-progress)
  wg log <task-id> "message"  # Log progress as you work
  wg done <task-id>           # Mark task complete

DISCOVERING & ADDING WORK
─────────────────────────────────────────
  wg list                     # List all tasks
  wg list --status open       # Filter by status (open, in-progress, done, etc.)
  wg show <task-id>           # View task details and context
  wg add "Title" -d "Desc"    # Add new task
  wg add "X" --after Y        # Add task blocked by another

  Context scopes control how much context the coordinator injects into the
  agent's prompt when dispatching a task:

  wg add "X" --context-scope clean  # Minimal: just the task description
  wg add "X" --context-scope task   # Standard default: task + predecessor context
  wg add "X" --context-scope graph  # Task + transitive dependency chain
  wg add "X" --context-scope full   # Everything: full graph state

TASK STATE COMMANDS
─────────────────────────────────────────
  wg done <task-id>           # Mark task complete (loop fires if present)
  wg done <task-id> --converged  # Complete and STOP the loop
  wg fail <task-id> --reason  # Mark failed (can be retried)
  wg abandon <task-id>        # Give up permanently

CONTEXT & ARTIFACTS
─────────────────────────────────────────
  wg context <task-id>        # See context from dependencies
  wg artifact <task-id> path  # Record output file/artifact
  wg log <task-id> --list     # View task's progress log

CYCLES (repeating workflows)
─────────────────────────────────────────
  Workgraph is a directed graph, NOT a DAG. It supports cycles natively.
  Use cycles instead of duplicating tasks (e.g., don't create "pass 1",
  "pass 2", "pass 3" — create one cycle that iterates).

  A cycle is formed when task A depends on task C, and task C (transitively)
  depends back on task A. The --after flag creates the back-edge, and
  --max-iterations caps how many times the cycle runs.

  CREATING A CYCLE — step by step:

    # 1. Create the first task in the loop
    wg add "Cleanup code"

    # 2. Chain the next steps
    wg add "Commit changes" --after cleanup-code
    wg add "Verify build" --after commit-changes

    # 3. Close the loop with a back-edge + iteration cap
    wg add "Cleanup code" --after verify-build --max-iterations 5

    This creates: cleanup → commit → verify → cleanup (up to 5 iterations)

  ANOTHER EXAMPLE — write/review cycle:

    wg add "Write draft"
    wg add "Review draft" --after write-draft
    wg add "Write draft" --after review-draft --max-iterations 3

  INSPECTING CYCLES:

    wg cycles                   # List detected cycles
    wg show <task-id>           # See loop_iteration to know which pass you're on

  IMPORTANT — Signaling convergence:
  Agents in a cycle MUST check whether the work has converged (i.e., no
  further changes are needed). When converged:

    wg done <task-id> --converged

  This stops the loop/cycle. Using plain 'wg done' causes the cycle to
  iterate again. Only use plain 'wg done' if you want the next iteration.

  KEY RULES:
  • One cycle, not N copies of tasks — let the iteration mechanism repeat
  • Use --max-iterations to prevent runaway loops (always set a cap)
  • Each agent in the cycle sees its loop_iteration count via wg show
  • Check for convergence: if nothing changed, use --converged to stop

HOUSEKEEPING
─────────────────────────────────────────
  wg archive                    # Archive completed tasks to a separate file
  wg archive --older 7d         # Only archive tasks completed more than 7 days ago
  wg archive --list             # List previously archived tasks
  wg gc                         # Garbage collect failed/abandoned tasks
  wg gc --dry-run               # Preview what would be removed
  wg gc --include-done          # Also remove done tasks (default: only failed+abandoned)
  wg gc --older 7d              # Only gc tasks older than 7 days

GROWING THE GRAPH
─────────────────────────────────────────
  The graph is a shared medium. Artifacts you write are read by other agents.
  Tasks you create get dispatched to other agents. You are not isolated —
  you are part of a living system.

  Your job is not just to complete your task. It is to leave the system
  better than you found it:

  Found a bug while implementing?
    wg add "Fix: edge case in parser" --after my-current-task -d "Found during impl"

  Documentation wrong or missing?
    wg add "Fix docs for X" -d "Spotted while reading auth.rs"

  Follow-up verification needed?
    wg add "Verify fix works end-to-end" --after my-current-task

  The loop: spec → implement → verify → improve → spec.
  You may be any node. Use 'wg context' to see what came before.
  Use 'wg add' to create what comes next.

  The coordinator dispatches anything you add. You don't need permission.
  Use judgment on size — if a fix takes 5 minutes, just do it inline.
  Create tasks for work that benefits from separate focus.

TIPS
─────────────────────────────────────────
• If the coordinator is running: add tasks → it dispatches automatically
• If no coordinator: ready → claim → work → done
• Run 'wg log' BEFORE starting work to track progress
• Use 'wg context' to understand what dependencies produced
• Check 'wg blocked <task-id>' if a task isn't appearing in ready list

EXECUTORS & MODELS
─────────────────────────────────────────
  The coordinator spawns agents using an executor (default: claude).
  Switch to amplifier for OpenRouter-backed models:

  wg config --coordinator-executor amplifier

  Set a default model for all agents:

  wg service start --model anthropic/claude-sonnet-4   # CLI override
  # Or in .workgraph/config.toml under [coordinator]: model = "anthropic/claude-sonnet-4"

  Per-task model selection (overrides the default):

  wg add "Fast task" --model google/gemini-2.5-flash
  wg add "Heavy task" --model anthropic/claude-opus-4

  Model hierarchy: task --model > executor model > coordinator model > 'default'

REUSABLE FUNCTIONS
─────────────────────────────────────────
  Functions capture proven workflow patterns for reuse:

  wg func list                        # Discover available functions
  wg func show <id>                   # View function details and inputs
  wg func apply <id> --input k=v      # Instantiate a function into tasks
  wg func extract a b c               # Extract a pattern from completed tasks

VISUALIZATION
─────────────────────────────────────────
  wg viz                              # ASCII tree of active subgraphs
  wg viz --all                        # Include fully-done trees
  wg viz <task-id>...                 # Focus on specific task subgraphs
  wg viz --critical-path              # Highlight longest dependency chain
  wg viz --dot                        # Output Graphviz DOT format
  wg viz --mermaid                    # Output Mermaid diagram format
  wg viz --show-internal              # Show assign-*/evaluate-* tasks
  wg viz --no-tui                     # Force static output (skip interactive TUI)
  wg tui                              # Interactive TUI dashboard

CONFIGURATION
─────────────────────────────────────────
  wg config --show                    # Show current configuration
  wg config --list                    # Show merged config with source annotations
  wg config --global --model opus     # Write to global ~/.workgraph/config.toml
  wg config --local --model sonnet    # Write to local .workgraph/config.toml
  wg config --creator-agent <hash>    # Set agent used for task creation
  wg config --creator-model <model>   # Set model used for task creation

TRACE, RUNS & REPLAY
─────────────────────────────────────────
  wg trace show <task-id>             # Show execution history of a task
  wg trace export --visibility public # Export trace data filtered by zone
  wg trace import <file>              # Import a trace export as read-only context

  wg runs list                        # List all run snapshots
  wg runs show <run>                  # Show details of a specific run
  wg runs diff <run>                  # Diff current graph against a snapshot
  wg runs restore <run>               # Restore graph from a snapshot

  wg replay --failed-only             # Re-execute only failed tasks
  wg replay --model <model>           # Replay with a different model
  wg replay --below-score 0.7         # Replay tasks scoring below threshold
  wg replay --subgraph <task-id>      # Replay only within a subgraph
  wg replay --keep-done 0.9           # Preserve high-scoring done tasks

ANALYSIS
─────────────────────────────────────────
  wg analyze                          # Comprehensive health report
  wg structure                        # Entry points, dead ends, fan-out
  wg bottlenecks                      # Tasks blocking the most downstream work
  wg critical-path                    # Longest dependency chain
  wg forecast                         # Completion date estimate from velocity

DEAD AGENT DETECTION
─────────────────────────────────────────
  wg dead-agents                      # List dead/unresponsive agents
  wg dead-agents --cleanup            # Mark dead agents and unclaim their tasks
  wg dead-agents --purge              # Purge dead/done/failed agents from registry
  wg dead-agents --purge --delete-dirs  # Also delete agent work directories
  wg dead-agents --threshold 30       # Override heartbeat timeout (minutes)

PEER WORKGRAPHS
─────────────────────────────────────────
  Cross-repo communication between workgraph instances:

  wg peer add <name> <path>           # Register a peer workgraph
  wg peer list                        # List all peers with service status
  wg peer status                      # Quick health check of all peers
  wg add "Task" --repo <peer>         # Create a task in a peer workgraph

EVALUATION & MONITORING
─────────────────────────────────────────
  wg evaluate run <task-id>           # Trigger LLM evaluation of a completed task
  wg evaluate show                    # View evaluation history
  wg watch                            # Stream workgraph events as JSON lines
  wg watch --task <id>                # Stream events for a specific task
"#;

fn json_output() -> serde_json::Value {
    serde_json::json!({
        "getting_started": [
            "wg init",
            "wg setup",
            "wg agency init",
            "wg service start",
            "wg add \"My first task\""
        ],
        "skill_bundle_setup": {
            "description": "Spawned agents need the right skill or bundle installed to understand wg commands.",
            "claude": {
                "install": "wg skill install",
                "location": "~/.claude/skills/wg/SKILL.md",
                "note": "Injected into every Claude Code session automatically"
            },
            "amplifier": {
                "install": "cd ~/amplifier-bundle-workgraph && ./setup.sh",
                "alternative": "amplifier bundle add git+https://github.com/graphwork/amplifier-bundle-workgraph",
                "usage": "amplifier run -B workgraph"
            },
            "custom": "Ensure your executor's agent prompt includes wg CLI instructions"
        },
        "agency": {
            "description": "Agency gives the service agents to assign to tasks.",
            "quick_setup": "wg agency init",
            "concepts": {
                "roles": "What agents do (skills + desired outcome)",
                "tradeoffs": "Constraints on how agents work (acceptable/unacceptable trade-offs)",
                "agents": "A role + tradeoff pairing that gets assigned to tasks"
            },
            "manual_setup": [
                "wg role add \"Name\" --outcome \"...\" --skill name",
                "wg tradeoff add \"Name\" --accept \"...\" --reject \"...\"",
                "wg agent create \"Name\" --role <hash> --tradeoff <hash>",
                "wg config --auto-assign true --auto-evaluate true"
            ],
            "placement": {
                "description": "When auto_place is enabled, the coordinator creates .place-* tasks for newly added tasks to determine optimal graph wiring.",
                "enable": "wg config --auto-place true"
            },
            "auto_create": {
                "description": "When auto_create is enabled, the coordinator invokes the creator agent to discover and add new primitives when the store needs expansion.",
                "enable": "wg config --auto-create true"
            },
            "model_registry": {
                "show_models": "wg config --models",
                "set_model": "wg config --set-model <role> <model>",
                "show_registry": "wg config --registry",
                "set_tier": "wg config --tier <tier>=<model>"
            }
        },
        "modes": {
            "service": {
                "description": "Recommended for parallel work. Coordinator dispatches automatically.",
                "start": "wg service start --max-agents 5",
                "workflow": "Add tasks with dependencies → coordinator spawns agents on ready tasks",
                "warning": "Do NOT manually wg spawn or wg claim while the service is running",
                "monitor": ["wg service status", "wg agents", "wg list", "wg tui"]
            },
            "manual": {
                "description": "For when no service is running. You claim and work tasks yourself.",
                "workflow": ["wg ready", "wg claim <task-id>", "wg log <task-id> \"msg\"", "wg done <task-id>"]
            }
        },
        "commands": {
            "discovery": {
                "list": "List all tasks",
                "show": "View task details and context",
                "add": "Add a new task (supports --context-scope clean/task/graph/full)",
                "ready": "See tasks available to work on (manual mode)"
            },
            "work": {
                "claim": "Claim a task for work (manual mode only)",
                "log": "Log progress as you work",
                "context": "See context from dependencies",
                "artifact": "Record output file/artifact"
            },
            "completion": {
                "done": "Mark task complete",
                "done_converged": "Complete task and stop loop (wg done <id> --converged)",
                "fail": "Mark failed (can be retried)",
                "abandon": "Give up permanently"
            }
        },
        "cycles": {
            "description": "Structural cycles model repeating workflows via after back-edges with CycleConfig.",
            "create": "wg add \"Write\" --after review --max-iterations 3",
            "inspect": ["wg show <task-id>", "wg cycles"],
            "convergence": "IMPORTANT: Use 'wg done <task-id> --converged' to stop a cycle when work is complete. Plain 'wg done' causes the cycle to iterate again."
        },
        "growing_the_graph": {
            "ethos": "The graph is a shared medium. You are not isolated — you are part of a living system. Your job is not just to complete your task, but to leave the system better than you found it.",
            "the_loop": "spec → implement → verify → improve → spec. Use 'wg context' to see what came before. Use 'wg add' to create what comes next.",
            "examples": {
                "found_bug": "wg add \"Fix: ...\" --after <task-id> -d \"Found while working on <task-id>\"",
                "docs_wrong": "wg add \"Fix docs for X\" -d \"Spotted while reading ...\"",
                "followup": "wg add \"Verify: ...\" --after <task-id>"
            },
            "guidance": "The coordinator dispatches anything you add. If a fix takes 5 minutes, do it inline. Create tasks for work that benefits from separate focus."
        },
        "tips": [
            "If the coordinator is running: add tasks with dependencies, it dispatches automatically",
            "If no coordinator: ready → claim → work → done",
            "Run 'wg log' BEFORE starting work to track progress",
            "Use 'wg context' to understand what dependencies produced",
            "Check 'wg blocked <task-id>' if a task isn't appearing in ready list"
        ],
        "executors_and_models": {
            "switch_executor": "wg config --coordinator-executor amplifier",
            "set_model_cli": "wg service start --model anthropic/claude-sonnet-4",
            "set_model_config": "[coordinator] model = \"anthropic/claude-sonnet-4\"",
            "per_task_model": "wg add \"task\" --model google/gemini-2.5-flash",
            "hierarchy": "task --model > executor model > coordinator model > 'default'"
        },
        "housekeeping": {
            "archive": "wg archive",
            "archive_older": "wg archive --older 7d",
            "archive_list": "wg archive --list",
            "gc": "wg gc",
            "gc_dry_run": "wg gc --dry-run",
            "gc_include_done": "wg gc --include-done",
            "gc_older": "wg gc --older 7d"
        },
        "functions": {
            "description": "Reusable workflow patterns extracted from completed tasks.",
            "commands": {
                "list": "wg func list",
                "show": "wg func show <id>",
                "apply": "wg func apply <id> --input k=v",
                "extract": "wg func extract a b c"
            }
        },
        "visualization": {
            "viz": "wg viz",
            "viz_all": "wg viz --all",
            "viz_focus": "wg viz <task-id>...",
            "viz_critical_path": "wg viz --critical-path",
            "viz_dot": "wg viz --dot",
            "viz_mermaid": "wg viz --mermaid",
            "viz_show_internal": "wg viz --show-internal",
            "viz_no_tui": "wg viz --no-tui",
            "tui": "wg tui"
        },
        "configuration": {
            "show": "wg config --show",
            "list": "wg config --list (merged config with source annotations)",
            "global": "wg config --global (target ~/.workgraph/config.toml)",
            "local": "wg config --local (target .workgraph/config.toml)",
            "creator_agent": "wg config --creator-agent <hash>",
            "creator_model": "wg config --creator-model <model>"
        },
        "context_scopes": {
            "description": "Control how much context the coordinator injects into agent prompts.",
            "levels": {
                "clean": "Minimal: just the task description",
                "task": "Standard default: task + predecessor context",
                "graph": "Task + transitive dependency chain",
                "full": "Everything: full graph state"
            },
            "usage": "wg add \"task\" --context-scope <level>"
        },
        "trace_runs_replay": {
            "trace": {
                "show": "wg trace show <task-id>",
                "export": "wg trace export --visibility public",
                "import": "wg trace import <file>"
            },
            "runs": {
                "list": "wg runs list",
                "show": "wg runs show <run>",
                "diff": "wg runs diff <run>",
                "restore": "wg runs restore <run>"
            },
            "replay": {
                "failed_only": "wg replay --failed-only",
                "with_model": "wg replay --model <model>",
                "below_score": "wg replay --below-score 0.7",
                "subgraph": "wg replay --subgraph <task-id>",
                "keep_done": "wg replay --keep-done 0.9"
            }
        },
        "analysis": {
            "analyze": "wg analyze (comprehensive health report)",
            "structure": "wg structure (entry points, dead ends, fan-out)",
            "bottlenecks": "wg bottlenecks (tasks blocking the most downstream work)",
            "critical_path": "wg critical-path (longest dependency chain)",
            "forecast": "wg forecast (completion date from velocity)"
        },
        "dead_agents": {
            "detect": "wg dead-agents",
            "cleanup": "wg dead-agents --cleanup",
            "purge": "wg dead-agents --purge",
            "purge_with_dirs": "wg dead-agents --purge --delete-dirs",
            "threshold": "wg dead-agents --threshold <minutes>"
        },
        "peer_workgraphs": {
            "add": "wg peer add <name> <path>",
            "list": "wg peer list",
            "status": "wg peer status",
            "cross_repo_task": "wg add \"task\" --repo <peer>"
        },
        "evaluation_and_monitoring": {
            "evaluate_run": "wg evaluate run <task-id>",
            "evaluate_show": "wg evaluate show",
            "watch": "wg watch",
            "watch_task": "wg watch --task <id>"
        }
    })
}

pub fn run(json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&json_output())?);
    } else {
        println!("{}", QUICKSTART_TEXT.trim());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quickstart_text_contains_service_mode() {
        assert!(QUICKSTART_TEXT.contains("SERVICE MODE"));
    }

    #[test]
    fn test_quickstart_text_contains_manual_mode() {
        assert!(QUICKSTART_TEXT.contains("MANUAL MODE"));
    }

    #[test]
    fn test_quickstart_text_contains_discovering_work() {
        assert!(QUICKSTART_TEXT.contains("DISCOVERING & ADDING WORK"));
    }

    #[test]
    fn test_quickstart_text_contains_task_state_commands() {
        assert!(QUICKSTART_TEXT.contains("TASK STATE COMMANDS"));
    }

    #[test]
    fn test_quickstart_text_contains_context_artifacts() {
        assert!(QUICKSTART_TEXT.contains("CONTEXT & ARTIFACTS"));
    }

    #[test]
    fn test_quickstart_text_contains_cycles() {
        assert!(QUICKSTART_TEXT.contains("CYCLES"));
    }

    #[test]
    fn test_quickstart_text_contains_tips() {
        assert!(QUICKSTART_TEXT.contains("TIPS"));
    }

    #[test]
    fn test_quickstart_text_contains_coordinator_reminder() {
        assert!(QUICKSTART_TEXT.contains("COORDINATOR SERVICE REMINDER"));
    }

    #[test]
    fn test_quickstart_text_contains_getting_started() {
        assert!(QUICKSTART_TEXT.contains("GETTING STARTED"));
        assert!(QUICKSTART_TEXT.contains("wg agency init"));
    }

    #[test]
    fn test_quickstart_text_contains_agency_setup() {
        assert!(QUICKSTART_TEXT.contains("AGENCY SETUP"));
        assert!(QUICKSTART_TEXT.contains("Roles"));
        assert!(QUICKSTART_TEXT.contains("Tradeoffs"));
    }

    #[test]
    fn test_run_text_mode_succeeds() {
        assert!(run(false).is_ok());
    }

    #[test]
    fn test_run_json_mode_succeeds() {
        assert!(run(true).is_ok());
    }

    #[test]
    fn test_json_output_has_expected_fields() {
        let output = json_output();

        // Check top-level keys
        assert!(output.get("getting_started").is_some());
        assert!(output.get("agency").is_some());
        assert!(output.get("modes").is_some());
        assert!(output.get("commands").is_some());
        assert!(output.get("cycles").is_some());
        assert!(output.get("tips").is_some());

        // Check getting_started is an array
        let gs = output.get("getting_started").unwrap().as_array().unwrap();
        assert!(gs.len() >= 3);

        // Check agency fields
        let agency = output.get("agency").unwrap();
        assert!(agency.get("quick_setup").is_some());
        assert!(agency.get("concepts").is_some());

        // Check modes
        let modes = output.get("modes").unwrap();
        assert!(modes.get("service").is_some());
        assert!(modes.get("manual").is_some());

        // Check commands sub-sections
        let commands = output.get("commands").unwrap();
        assert!(commands.get("discovery").is_some());
        assert!(commands.get("work").is_some());
        assert!(commands.get("completion").is_some());

        // Check cycles fields
        let cycles = output.get("cycles").unwrap();
        assert!(cycles.get("description").is_some());
        assert!(cycles.get("create").is_some());
        assert!(cycles.get("inspect").is_some());

        // Check growing_the_graph section
        let gtg = output.get("growing_the_graph").unwrap();
        assert!(gtg.get("ethos").is_some());
        assert!(gtg.get("the_loop").is_some());
        assert!(gtg.get("examples").is_some());

        // Check tips is an array with entries
        let tips = output.get("tips").unwrap().as_array().unwrap();
        assert!(!tips.is_empty());
        assert!(tips.len() >= 5);

        // Check executors_and_models section
        let em = output.get("executors_and_models").unwrap();
        assert!(em.get("switch_executor").is_some());
        assert!(em.get("per_task_model").is_some());
        assert!(em.get("hierarchy").is_some());

        // Check housekeeping section
        let hk = output.get("housekeeping").unwrap();
        assert!(hk.get("archive").is_some());
        assert!(hk.get("gc").is_some());

        // Check functions section
        let funcs = output.get("functions").unwrap();
        assert!(funcs.get("commands").is_some());

        // Check evaluation_and_monitoring section
        let eval = output.get("evaluation_and_monitoring").unwrap();
        assert!(eval.get("evaluate_run").is_some());
        assert!(eval.get("watch").is_some());
    }

    #[test]
    fn test_quickstart_text_contains_executors_and_models() {
        assert!(QUICKSTART_TEXT.contains("EXECUTORS & MODELS"));
        assert!(QUICKSTART_TEXT.contains("--coordinator-executor amplifier"));
        assert!(QUICKSTART_TEXT.contains("--model"));
    }

    #[test]
    fn test_quickstart_text_contains_functions() {
        assert!(QUICKSTART_TEXT.contains("REUSABLE FUNCTIONS"));
        assert!(QUICKSTART_TEXT.contains("wg func list"));
        assert!(QUICKSTART_TEXT.contains("wg func apply"));
    }

    #[test]
    fn test_quickstart_text_contains_housekeeping() {
        assert!(QUICKSTART_TEXT.contains("HOUSEKEEPING"));
        assert!(QUICKSTART_TEXT.contains("wg archive"));
        assert!(QUICKSTART_TEXT.contains("wg gc"));
    }

    #[test]
    fn test_quickstart_text_contains_evaluation_and_monitoring() {
        assert!(QUICKSTART_TEXT.contains("EVALUATION & MONITORING"));
        assert!(QUICKSTART_TEXT.contains("wg evaluate run"));
        assert!(QUICKSTART_TEXT.contains("wg watch"));
    }

    #[test]
    fn test_quickstart_text_contains_skill_bundle_setup() {
        assert!(QUICKSTART_TEXT.contains("SKILL & BUNDLE SETUP"));
        assert!(QUICKSTART_TEXT.contains("wg skill install"));
        assert!(QUICKSTART_TEXT.contains("amplifier run -B workgraph"));
    }

    #[test]
    fn test_json_output_has_skill_bundle_setup() {
        let output = json_output();
        let sbs = output
            .get("skill_bundle_setup")
            .expect("missing skill_bundle_setup");
        assert!(sbs.get("claude").is_some());
        assert!(sbs.get("amplifier").is_some());
        assert!(sbs.get("custom").is_some());
        assert!(
            sbs["claude"]["install"]
                .as_str()
                .unwrap()
                .contains("wg skill install")
        );
    }

    #[test]
    fn test_quickstart_converged_prominent() {
        // The CYCLES section must contain IMPORTANT and --converged prominently
        assert!(
            QUICKSTART_TEXT.contains("IMPORTANT — Signaling convergence:"),
            "Cycles section should have IMPORTANT heading for convergence"
        );
        assert!(
            QUICKSTART_TEXT.contains("wg done <task-id> --converged"),
            "Cycles section should show --converged command"
        );
        // The task state commands should also mention --converged
        assert!(
            QUICKSTART_TEXT.contains("wg done <task-id> --converged  # Complete and STOP the loop"),
            "Task state commands should include --converged variant"
        );
    }

    #[test]
    fn test_quickstart_text_contains_visualization() {
        assert!(QUICKSTART_TEXT.contains("VISUALIZATION"));
        assert!(QUICKSTART_TEXT.contains("wg viz"));
        assert!(QUICKSTART_TEXT.contains("--show-internal"));
        assert!(QUICKSTART_TEXT.contains("--no-tui"));
    }

    #[test]
    fn test_quickstart_text_contains_configuration() {
        assert!(QUICKSTART_TEXT.contains("CONFIGURATION"));
        assert!(QUICKSTART_TEXT.contains("--list"));
        assert!(QUICKSTART_TEXT.contains("--global"));
        assert!(QUICKSTART_TEXT.contains("--creator-agent"));
        assert!(QUICKSTART_TEXT.contains("--creator-model"));
    }

    #[test]
    fn test_quickstart_text_contains_trace_runs_replay() {
        assert!(QUICKSTART_TEXT.contains("TRACE, RUNS & REPLAY"));
        assert!(QUICKSTART_TEXT.contains("wg trace show"));
        assert!(QUICKSTART_TEXT.contains("wg runs list"));
        assert!(QUICKSTART_TEXT.contains("wg replay"));
        assert!(QUICKSTART_TEXT.contains("--below-score"));
        assert!(QUICKSTART_TEXT.contains("--subgraph"));
    }

    #[test]
    fn test_quickstart_text_contains_analysis() {
        assert!(QUICKSTART_TEXT.contains("ANALYSIS"));
        assert!(QUICKSTART_TEXT.contains("wg analyze"));
        assert!(QUICKSTART_TEXT.contains("wg bottlenecks"));
        assert!(QUICKSTART_TEXT.contains("wg critical-path"));
        assert!(QUICKSTART_TEXT.contains("wg forecast"));
    }

    #[test]
    fn test_quickstart_text_contains_dead_agents() {
        assert!(QUICKSTART_TEXT.contains("DEAD AGENT DETECTION"));
        assert!(QUICKSTART_TEXT.contains("wg dead-agents"));
        assert!(QUICKSTART_TEXT.contains("--purge"));
        assert!(QUICKSTART_TEXT.contains("--delete-dirs"));
    }

    #[test]
    fn test_quickstart_text_contains_peer_workgraphs() {
        assert!(QUICKSTART_TEXT.contains("PEER WORKGRAPHS"));
        assert!(QUICKSTART_TEXT.contains("wg peer add"));
        assert!(QUICKSTART_TEXT.contains("--repo"));
    }

    #[test]
    fn test_quickstart_text_context_scopes_explained() {
        assert!(QUICKSTART_TEXT.contains("--context-scope clean"));
        assert!(QUICKSTART_TEXT.contains("--context-scope task"));
        assert!(QUICKSTART_TEXT.contains("--context-scope graph"));
        assert!(QUICKSTART_TEXT.contains("--context-scope full"));
    }

    #[test]
    fn test_json_output_has_new_sections() {
        let output = json_output();
        assert!(output.get("visualization").is_some());
        assert!(output.get("configuration").is_some());
        assert!(output.get("context_scopes").is_some());
        assert!(output.get("trace_runs_replay").is_some());
        assert!(output.get("analysis").is_some());
        assert!(output.get("dead_agents").is_some());
        assert!(output.get("peer_workgraphs").is_some());
    }

    #[test]
    fn test_quickstart_json_convergence_emphasis() {
        let output = json_output();
        let convergence = output["cycles"]["convergence"].as_str().unwrap();
        assert!(
            convergence.contains("IMPORTANT"),
            "JSON convergence note should be emphatic"
        );
        let done_converged = output["commands"]["completion"]["done_converged"]
            .as_str()
            .unwrap();
        assert!(
            done_converged.contains("--converged"),
            "JSON should have done_converged command"
        );
    }

    #[test]
    fn test_quickstart_text_all_sections_present() {
        let text = QUICKSTART_TEXT.trim();
        let required_sections = [
            "WORKGRAPH AGENT QUICKSTART",
            "GETTING STARTED",
            "SKILL & BUNDLE SETUP",
            "AGENCY SETUP",
            "COORDINATOR SERVICE REMINDER",
            "SERVICE MODE",
            "MANUAL MODE",
            "DISCOVERING & ADDING WORK",
            "TASK STATE COMMANDS",
            "CONTEXT & ARTIFACTS",
            "CYCLES",
            "HOUSEKEEPING",
            "GROWING THE GRAPH",
            "TIPS",
            "EXECUTORS & MODELS",
            "REUSABLE FUNCTIONS",
            "VISUALIZATION",
            "CONFIGURATION",
            "TRACE, RUNS & REPLAY",
            "ANALYSIS",
            "DEAD AGENT DETECTION",
            "PEER WORKGRAPHS",
            "EVALUATION & MONITORING",
        ];
        for section in &required_sections {
            assert!(text.contains(section), "Missing section: {}", section);
        }
    }
}

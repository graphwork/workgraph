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
  wg status                   # Quick one-screen overview of your project

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

  Placement: when auto_place is enabled, the assignment step also decides
  dependency edges for each task (merged into the .assign-* LLM call).

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
  wg service restart          # Graceful stop then start
  wg service pause            # Pause coordinator (no new spawns, running agents continue)
  wg service resume           # Resume coordinator
  wg service freeze           # SIGSTOP all agents and pause service
  wg service thaw             # SIGCONT agents and resume service
  wg agents                   # Who's working on what
  wg kill <agent-id>          # Kill agent + pause its task (prevents re-dispatch)
  wg kill <agent-id> --redispatch  # Kill agent, leave task open for re-dispatch
  wg kill --all               # Kill all agents + pause their tasks
  wg list                     # What's done, what's pending
  wg tui                      # Interactive dashboard

  Multi-coordinator sessions:

  wg service create-coordinator   # Create a new coordinator session
  wg service stop-coordinator <n> # Stop a coordinator session
  wg service archive-coordinator <n>  # Archive a coordinator session
  wg service delete-coordinator <n>   # Delete a coordinator session

  Chat with the coordinator:

  wg chat "message"               # Send a message to the coordinator
  wg chat -i                      # Interactive REPL mode
  wg chat --attachment file.txt   # Attach a file to the message
  wg chat --coordinator 1         # Target a specific coordinator session
  wg chat --history               # Show chat history
  wg chat --clear                 # Clear chat history

  Advanced service tools:

  wg screencast                       # Render TUI event traces into screencasts
  wg tui-dump                         # Dump TUI screen contents (requires running wg tui)
  wg server                           # Multi-user server setup automation

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
  wg edit <task-id>           # Edit title, description, deps, model, tags, etc.

  Provider-specific models (use provider:model format):

  wg add "X" --model openrouter:google/gemini-2.5-flash

  Per-task timeout and scheduling:

  wg add "X" --timeout 30m             # Task agent killed after 30 minutes
  wg add "X" --cron "0 0 9 * * *"      # Recurring task (6-field cron: sec min hour day month dow)

  Skills, inputs, and deliverables:

  wg add "X" --skill rust --input src/lib.rs --deliverable report.md

  Suppress implicit dependency on the creating task:

  wg add "X" --independent              # No --after on the creating task

  Execution modes control the agent's capabilities:

  wg add "X" --exec-mode full    # Default: full agent with all tools
  wg add "X" --exec-mode light   # Read-only tools (research/review tasks)
  wg add "X" --exec-mode bare    # Only wg CLI (coordination-only tasks)
  wg add "X" --exec-mode shell   # Shell command, no LLM (use with wg exec --set)

  Context scopes control how much context the coordinator injects into the
  agent's prompt when dispatching a task:

  wg add "X" --context-scope clean  # Minimal: just the task description
  wg add "X" --context-scope task   # Standard default: task + predecessor context
  wg add "X" --context-scope graph  # Task + transitive dependency chain
  wg add "X" --context-scope full   # Everything: full graph state

  Scheduling — delay dispatch or set an absolute start time:

  wg add "X" --delay 1h              # Ready after 1 hour
  wg add "X" --not-before 2026-04-01T09:00:00Z  # ISO 8601 timestamp

  Placement hints — control where tasks land in the graph:

  wg add "X" --no-place             # Skip auto-placement, dispatch immediately
  wg add "X" --place-near task-a    # Place near related tasks
  wg add "X" --place-before task-b  # Place before specific tasks

TASK STATE COMMANDS
─────────────────────────────────────────
  wg done <task-id>           # Mark task complete (loop fires if present)
  wg done <task-id> --converged  # Complete and STOP the loop
  wg fail <task-id> --reason  # Mark failed (can be retried)
  wg retry <task-id>          # Retry a failed task (resets to open)
  wg abandon <task-id>        # Give up permanently
  wg pause <task-id>          # Pause task (coordinator skips it until resumed)
  wg wait <task-id> --until "condition"  # Park task until condition is met
  wg resume <task-id>         # Resume a paused/waiting task
  wg unclaim <task-id>        # Release a claimed task (back to open)
  wg requeue <task-id> --reason "..."  # Requeue in-progress task for triage

  Wait conditions:
    --until "task:dep-a=done"   # Wait for another task to reach a status
    --until "timer:5m"          # Wait for a timer (e.g., 5m, 1h, 2d)
    --until "message"           # Wait for a message to arrive
    --until "human-input"       # Wait for a human message
    --until "file:path/to/file" # Wait for a file to change

  wg reschedule <task-id> --after 24   # Ready after 24 hours from now
  wg reschedule <task-id> --at <ISO>   # Ready at a specific timestamp

  Dependency edge management:
    wg add-dep <task> <dependency>     # Add a dependency: task waits for dependency
    wg rm-dep <task> <dependency>      # Remove a dependency edge

VALIDATION (--verify gate)
─────────────────────────────────────────
  Tasks created with --verify have an extra gate before completion:

  wg add "Task" --verify "cargo test passes"  # Set validation criteria
  wg done <task-id>           # Moves to pending-validation (not done yet!)
  wg approve <task-id>        # Approve → transitions to Done
  wg reject <task-id> --reason "Tests failing"  # Reject → reopens task

  After max rejections, the task transitions to Failed instead of reopening.

MESSAGING
─────────────────────────────────────────
  Inter-agent and task-scoped messaging:

  wg msg send <task-id> "message"    # Send a message to a task
  wg msg list <task-id>              # List all messages for a task
  wg msg read <task-id>              # Read unread messages (marks as read)
  wg msg poll <task-id>              # Poll for new messages (exit code 0/1)

  Agents MUST check messages before and after working on a task. Unreplied
  messages mean the task is not complete. Use --agent <id> with read/poll
  to filter by your agent identity.

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

    # 3. Close the loop: edit the FIRST task to add a back-edge + iteration cap
    wg edit cleanup-code --add-after verify-build --max-iterations 5

    This creates: cleanup → commit → verify → cleanup (up to 5 iterations)

    NOTE: Do NOT use 'wg add "Cleanup code" --after verify-build' here — that
    creates a NEW task instead of adding a back-edge to the existing one.
    Always use 'wg edit <id> --add-after <last-task>' to close cycles.

  ANOTHER EXAMPLE — write/review cycle:

    wg add "Write draft"
    wg add "Review draft" --after write-draft
    wg edit write-draft --add-after review-draft --max-iterations 3

  INSPECTING CYCLES:

    wg cycles                   # List detected cycles
    wg show <task-id>           # See loop_iteration to know which pass you're on

  IMPORTANT — Signaling convergence:
  Agents in a cycle MUST check whether the work has converged (i.e., no
  further changes are needed). When converged:

    wg done <task-id> --converged

  This stops the loop/cycle. Using plain 'wg done' causes the cycle to
  iterate again. Only use plain 'wg done' if you want the next iteration.

  ADVANCED CYCLE OPTIONS:

    wg add "X" --after Y --max-iterations 5 --no-converge
      # Force all iterations to run — agents cannot signal early stop

    wg add "X" --after Y --max-iterations 10 --no-restart-on-failure
      # Don't restart the cycle if an iteration fails

    wg add "X" --after Y --max-iterations 10 --max-failure-restarts 1
      # Allow at most 1 failure-triggered restart (default: 3)

  KEY RULES:
  • One cycle, not N copies of tasks — let the iteration mechanism repeat
  • Use --max-iterations to prevent runaway loops (always set a cap)
  • Each agent in the cycle sees its loop_iteration count via wg show
  • Check for convergence: if nothing changed, use --converged to stop

SHELL EXECUTION
─────────────────────────────────────────
  Run tasks as shell commands instead of LLM agents:

  wg exec --set build-task "cargo build --release"  # Set shell command
  wg exec build-task                                 # Run it (claim + exec + done/fail)
  wg exec --dry-run build-task                       # Preview without running
  wg exec --clear build-task                         # Remove the shell command

  Use with --exec-mode shell on task creation for fully automated steps.

COMPACT, SWEEP & CHECKPOINT
─────────────────────────────────────────
  wg compact                    # Distill graph state into context.md
  wg sweep                      # Detect and recover orphaned in-progress tasks
  wg sweep --dry-run            # Preview what sweep would fix
  wg checkpoint <task-id> -s "Progress summary"  # Save checkpoint
  wg checkpoint <task-id> --list                 # List checkpoints for a task
  wg stats                      # Show time counters and agent statistics

HOUSEKEEPING
─────────────────────────────────────────
  wg archive                    # Archive completed tasks to a separate file
  wg archive --older 7d         # Only archive tasks completed more than 7 days ago
  wg archive --list             # List previously archived tasks
  wg gc                         # Garbage collect failed/abandoned tasks
  wg gc --dry-run               # Preview what would be removed
  wg gc --include-done          # Also remove done tasks (default: only failed+abandoned)
  wg gc --older 7d              # Only gc tasks older than 7 days
  wg cleanup orphaned           # Clean up orphaned worktrees
  wg cleanup recovery-branches  # Clean up old recovery branches
  wg cleanup nightly            # Comprehensive nightly cleanup
  wg metrics                    # Display cleanup and monitoring metrics

DISCOVERY & PUBLISHING
─────────────────────────────────────────
  wg discover                       # Show tasks completed in the last 24h
  wg discover --since 7d            # Completed in the last 7 days
  wg discover --with-artifacts      # Include artifact paths in output

  wg publish <task-id>              # Publish a draft task (validates deps, resumes subgraph)
  wg publish <task-id> --only       # Publish just this task (skip subgraph propagation)

  wg reclaim <task-id> --from <actor> --to <actor>  # Reclaim task from dead agent

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
• Use 'wg why-blocked <task-id>' for the full transitive blocking chain

EXECUTORS & MODELS
─────────────────────────────────────────
  The coordinator spawns agents using an executor (default: claude).
  Switch to amplifier for OpenRouter-backed models:

  wg config --coordinator-executor amplifier

  Set a default model for all agents:

  wg service start --model anthropic:claude-sonnet-4-6   # CLI override
  # Or in .workgraph/config.toml under [coordinator]: model = "anthropic:claude-sonnet-4-6"

  Per-task model selection (overrides the default):

  wg add "Fast task" --model openrouter:google/gemini-2.5-flash
  wg add "Heavy task" --model opus

  Model format: use provider:model (e.g., openrouter:deepseek/deepseek-v3.2).
  Short names (opus, sonnet, haiku) are also accepted.

  Model hierarchy: task --model > executor model > coordinator model > 'default'

MODEL REGISTRY & API KEYS
─────────────────────────────────────────
  Model registry — manage models and per-role routing:

  wg model list                  # Show all models (built-in + user-defined)
  wg model add <id> --tier <t>  # Add/update a model in the registry
  wg model remove <id>          # Remove a model from the registry
  wg model set-default <id>     # Set the default dispatch model
  wg model routing              # Show per-role model routing
  wg model set <role> <model>   # Set model for a specific dispatch role

  Browse and search models from OpenRouter:

  wg models list                 # List models from local registry
  wg models search <query>       # Search OpenRouter by name/description
  wg models remote               # List all models available on OpenRouter
  wg models add <id>             # Add a model from OpenRouter to local registry
  wg models set-default <id>     # Set default model
  wg models init                 # Initialize models.yaml with defaults

  Endpoint management (for OpenRouter, custom hosts, etc.):

  wg endpoints list              # List all configured endpoints
  wg endpoints add               # Add a new endpoint
  wg endpoints remove <name>     # Remove an endpoint
  wg endpoints set-default <name>  # Set default endpoint
  wg endpoints test <name>       # Test endpoint connectivity

  API key management:

  wg key set <provider>          # Configure an API key for a provider
  wg key check <provider>        # Validate API key availability
  wg key list                    # Show key status for all providers

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
  wg velocity                         # Task completion rate per week
  wg aging                            # Task age distribution (stale work detection)
  wg workload                         # Agent workload balance
  wg coordinate                       # Coordination status: ready, in-progress, parallel opportunities
  wg impact <task-id>                 # What tasks depend on this one (downstream impact)
  wg plan --hours 8                   # Plan work that fits within a time budget
  wg cost <task-id>                   # Calculate cost including dependencies

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

NOTIFICATION & COMMUNICATION
─────────────────────────────────────────
  Telegram (human escalation):

  wg telegram send "message"          # Send a message to the configured chat
  wg telegram ask "question"          # Send and wait for reply
  wg telegram poll                    # Poll for replies
  wg telegram status                  # Show Telegram configuration status

  Matrix (team notifications):

  wg matrix                           # Matrix integration commands
  wg notify                           # Send task notification to Matrix room

RESOURCE MANAGEMENT
─────────────────────────────────────────
  wg resource add                     # Add a new resource
  wg resource list                    # List all resources
  wg resources                        # Show resource utilization (committed vs available)

PROVIDER PROFILES
─────────────────────────────────────────
  wg profile list                     # List available provider profiles
  wg profile show                     # Show current profile and model mappings
  wg profile set <name>               # Set the active provider profile
  wg profile refresh                  # Refresh model data from OpenRouter

USER BOARDS
─────────────────────────────────────────
  wg user init                        # Create a user conversation board
  wg user list                        # List all user boards
  wg user archive                     # Archive active board and create successor

COST & SPENDING
─────────────────────────────────────────
  wg spend                            # Show token usage and cost summaries
  wg spend --today                    # Show only today's spend
  wg openrouter                       # OpenRouter cost monitoring
"#;

fn json_output() -> serde_json::Value {
    serde_json::json!({
        "getting_started": [
            "wg init",
            "wg setup",
            "wg agency init",
            "wg service start",
            "wg add \"My first task\"",
            "wg status"
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
                "description": "When auto_place is enabled, the assignment step also decides dependency edges for each task (merged into the .assign-* LLM call).",
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
                "monitor": ["wg service status", "wg agents", "wg list", "wg tui"],
                "control": {
                    "pause": "wg service pause (no new spawns, running agents continue)",
                    "resume": "wg service resume",
                    "freeze": "wg service freeze (SIGSTOP all agents + pause)",
                    "thaw": "wg service thaw (SIGCONT agents + resume)"
                },
                "kill_agent": "wg kill <agent-id> (pauses task by default)",
                "kill_agent_redispatch": "wg kill <agent-id> --redispatch (leave task open)",
                "kill_all": "wg kill --all (pauses all tasks)"
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
                "add": "Add a new task (supports --context-scope, --exec-mode, --model provider:model, --delay, --not-before, --no-place, --place-near, --place-before, --cron, --timeout, --skill, --independent)",
                "edit": "Edit an existing task (title, description, deps, model, tags, etc.)",
                "ready": "See tasks available to work on (manual mode)",
                "status": "Quick one-screen status overview"
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
                "retry": "Retry a failed task (resets to open)",
                "abandon": "Give up permanently",
                "pause": "Pause task (coordinator skips it until resumed)",
                "wait": "Park task until condition met (wg wait <id> --until \"condition\")",
                "resume": "Resume a paused/waiting task",
                "reschedule": "Set not_before timestamp (wg reschedule <id> --after 24)",
                "unclaim": "Release a claimed task back to open",
                "requeue": "Requeue in-progress task for triage (wg requeue <id> --reason \"...\")"
            },
            "dependencies": {
                "add_dep": "Add a dependency edge (wg add-dep <task> <dependency>)",
                "rm_dep": "Remove a dependency edge (wg rm-dep <task> <dependency>)"
            }
        },
        "validation": {
            "description": "Tasks with --verify have a pending-validation gate before completion.",
            "create": "wg add \"task\" --verify \"cargo test passes\"",
            "approve": "wg approve <task-id>",
            "reject": "wg reject <task-id> --reason \"reason\"",
            "note": "After max rejections, the task transitions to Failed instead of reopening."
        },
        "messaging": {
            "description": "Inter-agent and task-scoped messaging. Agents must check messages before and after working.",
            "send": "wg msg send <task-id> \"message\"",
            "list": "wg msg list <task-id>",
            "read": "wg msg read <task-id>",
            "poll": "wg msg poll <task-id>",
            "agent_filter": "Use --agent <id> with read/poll to filter by agent identity"
        },
        "wait_conditions": {
            "description": "Park a task until a condition is met.",
            "task": "wg wait <id> --until \"task:dep-a=done\"",
            "timer": "wg wait <id> --until \"timer:5m\"",
            "message": "wg wait <id> --until \"message\"",
            "human-input": "wg wait <id> --until \"human-input\"",
            "file": "wg wait <id> --until \"file:path/to/file\""
        },
        "discovery_publishing": {
            "discover": "wg discover",
            "discover_since": "wg discover --since 7d",
            "discover_artifacts": "wg discover --with-artifacts",
            "publish": "wg publish <task-id>",
            "publish_only": "wg publish <task-id> --only",
            "reclaim": "wg reclaim <task-id> --from <actor> --to <actor>"
        },
        "exec_modes": {
            "description": "Control agent capabilities per task.",
            "modes": {
                "full": "Default: full agent with all tools",
                "light": "Read-only tools (research/review tasks)",
                "bare": "Only wg CLI (coordination-only tasks)",
                "shell": "Shell command, no LLM (use with wg exec --set)"
            },
            "usage": "wg add \"task\" --exec-mode <mode>"
        },
        "scheduling": {
            "delay": "wg add \"task\" --delay 1h",
            "not_before": "wg add \"task\" --not-before 2026-04-01T09:00:00Z",
            "cron": "wg add \"task\" --cron \"0 0 9 * * *\" (6-field: sec min hour day month dow)",
            "timeout": "wg add \"task\" --timeout 30m (per-task timeout)",
            "independent": "wg add \"task\" --independent (suppress implicit --after)",
            "placement": {
                "no_place": "wg add \"task\" --no-place (skip auto-placement)",
                "place_near": "wg add \"task\" --place-near task-a",
                "place_before": "wg add \"task\" --place-before task-b"
            }
        },
        "cycles": {
            "description": "Structural cycles model repeating workflows via after back-edges with CycleConfig.",
            "create": "wg edit write --add-after review --max-iterations 3",
            "inspect": ["wg show <task-id>", "wg cycles"],
            "convergence": "IMPORTANT: Use 'wg done <task-id> --converged' to stop a cycle when work is complete. Plain 'wg done' causes the cycle to iterate again.",
            "advanced": {
                "no_converge": "wg add \"X\" --after Y --max-iterations 5 --no-converge (force all iterations)",
                "no_restart_on_failure": "wg add \"X\" --after Y --max-iterations 10 --no-restart-on-failure",
                "max_failure_restarts": "wg add \"X\" --after Y --max-iterations 10 --max-failure-restarts 1"
            }
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
            "Check 'wg blocked <task-id>' if a task isn't appearing in ready list",
            "Use 'wg why-blocked <task-id>' for the full transitive blocking chain"
        ],
        "executors_and_models": {
            "switch_executor": "wg config --coordinator-executor amplifier",
            "set_model_cli": "wg service start --model anthropic:claude-sonnet-4-6",
            "set_model_config": "[coordinator] model = \"anthropic:claude-sonnet-4-6\"",
            "per_task_model": "wg add \"task\" --model openrouter:google/gemini-2.5-flash",
            "model_format": "provider:model (e.g., openrouter:deepseek/deepseek-v3.2). Short names (opus, sonnet, haiku) also accepted.",
            "hierarchy": "task --model > executor model > coordinator model > 'default'"
        },
        "model_registry": {
            "model": {
                "list": "wg model list",
                "add": "wg model add <id> --tier <tier>",
                "remove": "wg model remove <id>",
                "set_default": "wg model set-default <id>",
                "routing": "wg model routing",
                "set_role": "wg model set <role> <model>"
            },
            "models": {
                "list": "wg models list",
                "search": "wg models search <query>",
                "remote": "wg models remote",
                "add": "wg models add <id>",
                "set_default": "wg models set-default <id>",
                "init": "wg models init"
            }
        },
        "endpoints": {
            "list": "wg endpoints list",
            "add": "wg endpoints add",
            "remove": "wg endpoints remove <name>",
            "set_default": "wg endpoints set-default <name>",
            "test": "wg endpoints test <name>"
        },
        "api_keys": {
            "set": "wg key set <provider>",
            "check": "wg key check <provider>",
            "list": "wg key list"
        },
        "shell_execution": {
            "set_command": "wg exec --set <task> \"command\"",
            "run": "wg exec <task>",
            "dry_run": "wg exec --dry-run <task>",
            "clear": "wg exec --clear <task>"
        },
        "compact_sweep_checkpoint": {
            "compact": "wg compact",
            "sweep": "wg sweep",
            "sweep_dry_run": "wg sweep --dry-run",
            "checkpoint": "wg checkpoint <task> -s \"summary\"",
            "checkpoint_list": "wg checkpoint <task> --list",
            "stats": "wg stats"
        },
        "multi_coordinator": {
            "create": "wg service create-coordinator",
            "stop": "wg service stop-coordinator <n>",
            "archive": "wg service archive-coordinator <n>",
            "delete": "wg service delete-coordinator <n>"
        },
        "chat": {
            "send": "wg chat \"message\"",
            "interactive": "wg chat -i",
            "attachment": "wg chat --attachment file.txt",
            "coordinator": "wg chat --coordinator 1",
            "history": "wg chat --history",
            "clear": "wg chat --clear"
        },
        "housekeeping": {
            "archive": "wg archive",
            "archive_older": "wg archive --older 7d",
            "archive_list": "wg archive --list",
            "gc": "wg gc",
            "gc_dry_run": "wg gc --dry-run",
            "gc_include_done": "wg gc --include-done",
            "gc_older": "wg gc --older 7d",
            "cleanup_orphaned": "wg cleanup orphaned",
            "cleanup_branches": "wg cleanup recovery-branches",
            "cleanup_nightly": "wg cleanup nightly",
            "metrics": "wg metrics"
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
            "creator_agent": "wg config --creator-agent <hash>"
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
            "forecast": "wg forecast (completion date from velocity)",
            "velocity": "wg velocity (task completion rate per week)",
            "aging": "wg aging (task age distribution)",
            "workload": "wg workload (agent workload balance)",
            "coordinate": "wg coordinate (coordination status: ready, in-progress, parallel opportunities)",
            "impact": "wg impact <task-id> (downstream impact analysis)",
            "plan": "wg plan --hours 8 (plan work within a budget)",
            "cost": "wg cost <task-id> (calculate cost including dependencies)"
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
        },
        "notification": {
            "telegram": {
                "send": "wg telegram send \"message\"",
                "ask": "wg telegram ask \"question\"",
                "poll": "wg telegram poll",
                "status": "wg telegram status"
            },
            "matrix": "wg matrix",
            "notify": "wg notify"
        },
        "resources": {
            "add": "wg resource add",
            "list": "wg resource list",
            "utilization": "wg resources"
        },
        "profiles": {
            "list": "wg profile list",
            "show": "wg profile show",
            "set": "wg profile set <name>",
            "refresh": "wg profile refresh"
        },
        "user_boards": {
            "init": "wg user init",
            "list": "wg user list",
            "archive": "wg user archive"
        },
        "cost_spending": {
            "spend": "wg spend",
            "spend_today": "wg spend --today",
            "openrouter": "wg openrouter"
        },
        "advanced_service": {
            "screencast": "wg screencast",
            "tui_dump": "wg tui-dump",
            "server": "wg server"
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
            "VALIDATION (--verify gate)",
            "MESSAGING",
            "CONTEXT & ARTIFACTS",
            "CYCLES",
            "DISCOVERY & PUBLISHING",
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
            "NOTIFICATION & COMMUNICATION",
            "RESOURCE MANAGEMENT",
            "PROVIDER PROFILES",
            "USER BOARDS",
            "COST & SPENDING",
        ];
        for section in &required_sections {
            assert!(text.contains(section), "Missing section: {}", section);
        }
    }

    #[test]
    fn test_quickstart_text_contains_wait_command() {
        assert!(QUICKSTART_TEXT.contains("wg wait"));
        assert!(QUICKSTART_TEXT.contains("--until"));
        assert!(QUICKSTART_TEXT.contains("task:dep-a=done"));
        assert!(QUICKSTART_TEXT.contains("timer:5m"));
    }

    #[test]
    fn test_quickstart_text_contains_messaging() {
        assert!(QUICKSTART_TEXT.contains("MESSAGING"));
        assert!(QUICKSTART_TEXT.contains("wg msg send"));
        assert!(QUICKSTART_TEXT.contains("wg msg read"));
        assert!(QUICKSTART_TEXT.contains("wg msg poll"));
    }

    #[test]
    fn test_quickstart_text_contains_validation() {
        assert!(QUICKSTART_TEXT.contains("VALIDATION"));
        assert!(QUICKSTART_TEXT.contains("wg approve"));
        assert!(QUICKSTART_TEXT.contains("wg reject"));
        assert!(QUICKSTART_TEXT.contains("pending-validation"));
    }

    #[test]
    fn test_quickstart_text_contains_discover_publish() {
        assert!(QUICKSTART_TEXT.contains("DISCOVERY & PUBLISHING"));
        assert!(QUICKSTART_TEXT.contains("wg discover"));
        assert!(QUICKSTART_TEXT.contains("wg publish"));
        assert!(QUICKSTART_TEXT.contains("wg reclaim"));
    }

    #[test]
    fn test_json_output_has_new_command_sections() {
        let output = json_output();
        assert!(output.get("validation").is_some());
        assert!(output.get("messaging").is_some());
        assert!(output.get("wait_conditions").is_some());
        assert!(output.get("discovery_publishing").is_some());
    }

    #[test]
    fn test_quickstart_text_contains_notification() {
        assert!(QUICKSTART_TEXT.contains("NOTIFICATION & COMMUNICATION"));
        assert!(QUICKSTART_TEXT.contains("wg telegram send"));
        assert!(QUICKSTART_TEXT.contains("wg telegram ask"));
    }

    #[test]
    fn test_quickstart_text_contains_resources() {
        assert!(QUICKSTART_TEXT.contains("RESOURCE MANAGEMENT"));
        assert!(QUICKSTART_TEXT.contains("wg resource add"));
        assert!(QUICKSTART_TEXT.contains("wg resources"));
    }

    #[test]
    fn test_quickstart_text_contains_profiles() {
        assert!(QUICKSTART_TEXT.contains("PROVIDER PROFILES"));
        assert!(QUICKSTART_TEXT.contains("wg profile list"));
        assert!(QUICKSTART_TEXT.contains("wg profile set"));
    }

    #[test]
    fn test_quickstart_text_contains_user_boards() {
        assert!(QUICKSTART_TEXT.contains("USER BOARDS"));
        assert!(QUICKSTART_TEXT.contains("wg user init"));
    }

    #[test]
    fn test_quickstart_text_contains_cost_spending() {
        assert!(QUICKSTART_TEXT.contains("COST & SPENDING"));
        assert!(QUICKSTART_TEXT.contains("wg spend"));
    }

    #[test]
    fn test_quickstart_text_contains_unclaim_requeue() {
        assert!(QUICKSTART_TEXT.contains("wg unclaim"));
        assert!(QUICKSTART_TEXT.contains("wg requeue"));
    }

    #[test]
    fn test_quickstart_text_contains_dep_management() {
        assert!(QUICKSTART_TEXT.contains("wg add-dep"));
        assert!(QUICKSTART_TEXT.contains("wg rm-dep"));
    }

    #[test]
    fn test_quickstart_text_contains_cleanup() {
        assert!(QUICKSTART_TEXT.contains("wg cleanup orphaned"));
        assert!(QUICKSTART_TEXT.contains("wg cleanup nightly"));
        assert!(QUICKSTART_TEXT.contains("wg metrics"));
    }

    #[test]
    fn test_quickstart_text_contains_extended_analysis() {
        assert!(QUICKSTART_TEXT.contains("wg velocity"));
        assert!(QUICKSTART_TEXT.contains("wg aging"));
        assert!(QUICKSTART_TEXT.contains("wg workload"));
        assert!(QUICKSTART_TEXT.contains("wg coordinate"));
        assert!(QUICKSTART_TEXT.contains("wg impact"));
        assert!(QUICKSTART_TEXT.contains("wg plan"));
        assert!(QUICKSTART_TEXT.contains("wg cost"));
    }

    #[test]
    fn test_quickstart_text_contains_advanced_service() {
        assert!(QUICKSTART_TEXT.contains("wg screencast"));
        assert!(QUICKSTART_TEXT.contains("wg tui-dump"));
        assert!(QUICKSTART_TEXT.contains("wg server"));
    }

    #[test]
    fn test_quickstart_text_provider_model_format() {
        assert!(QUICKSTART_TEXT.contains("provider:model"));
        assert!(QUICKSTART_TEXT.contains("openrouter:google/gemini-2.5-flash"));
        // Deprecated --provider flag should NOT appear
        assert!(!QUICKSTART_TEXT.contains("--provider openrouter"));
    }

    #[test]
    fn test_quickstart_text_contains_cron_timeout() {
        assert!(QUICKSTART_TEXT.contains("--cron"));
        assert!(QUICKSTART_TEXT.contains("--timeout"));
        assert!(QUICKSTART_TEXT.contains("--independent"));
    }

    #[test]
    fn test_json_output_has_new_sections_apr12() {
        let output = json_output();
        assert!(output.get("notification").is_some());
        assert!(output.get("resources").is_some());
        assert!(output.get("profiles").is_some());
        assert!(output.get("user_boards").is_some());
        assert!(output.get("cost_spending").is_some());
        assert!(output.get("advanced_service").is_some());

        // Check analysis has new fields
        let analysis = output.get("analysis").unwrap();
        assert!(analysis.get("velocity").is_some());
        assert!(analysis.get("aging").is_some());
        assert!(analysis.get("workload").is_some());
        assert!(analysis.get("coordinate").is_some());
        assert!(analysis.get("impact").is_some());
        assert!(analysis.get("plan").is_some());
        assert!(analysis.get("cost").is_some());

        // Check housekeeping has cleanup
        let hk = output.get("housekeeping").unwrap();
        assert!(hk.get("cleanup_orphaned").is_some());
        assert!(hk.get("metrics").is_some());

        // Check executors has model_format
        let em = output.get("executors_and_models").unwrap();
        assert!(em.get("model_format").is_some());
        // No deprecated provider field
        assert!(em.get("per_task_provider").is_none());

        // Check commands has dependencies section
        let commands = output.get("commands").unwrap();
        assert!(commands.get("dependencies").is_some());
    }
}

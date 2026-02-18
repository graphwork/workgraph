use anyhow::Result;

const QUICKSTART_TEXT: &str = r#"
╔══════════════════════════════════════════════════════════════════════════════╗
║                         WORKGRAPH AGENT QUICKSTART                           ║
╚══════════════════════════════════════════════════════════════════════════════╝

GETTING STARTED
─────────────────────────────────────────
  wg init                     # Create a .workgraph directory
  wg agency init              # Bootstrap roles, motivations, and a default agent
  wg service start            # Start the coordinator
  wg add "My first task"      # Add work — the service dispatches automatically

AGENCY SETUP
─────────────────────────────────────────
  'wg agency init' creates sensible defaults so the service can auto-assign
  agents to tasks immediately. It sets up:

  • Roles     — what agents do (Programmer, Reviewer, Documenter, Architect)
  • Motivations — constraints on how (Careful, Fast, Thorough, Balanced)
  • Agent     — a role+motivation pairing (default: Careful Programmer)
  • Config    — enables auto_assign and auto_evaluate

  You can also set up manually:
    wg role add "Name" --outcome "What it produces" --skill skill-name
    wg motivation add "Name" --accept "Slow" --reject "Untested"
    wg agent create "Name" --role <hash> --motivation <hash>
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

  wg add "Do the thing" --blocked-by prerequisite-task

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
  wg add "X" --blocked-by Y   # Add task blocked by another

TASK STATE COMMANDS
─────────────────────────────────────────
  wg done <task-id>           # Mark task complete
  wg fail <task-id> --reason  # Mark failed (can be retried)
  wg abandon <task-id>        # Give up permanently

CONTEXT & ARTIFACTS
─────────────────────────────────────────
  wg context <task-id>        # See context from dependencies
  wg artifact <task-id> path  # Record output file/artifact
  wg log <task-id> --list     # View task's progress log

LOOP EDGES (cyclic processes)
─────────────────────────────────────────
  Some workflows repeat. A loops_to edge fires when its task completes,
  resetting a target task back to open and incrementing loop_iteration.
  Intermediate tasks in the chain are also re-opened automatically.

  wg add "Revise" --loops-to write --loop-max 3          # loop back to write
  wg add "Poll" --loops-to poll --loop-max 10 --loop-delay 5m  # self-loop with delay
  wg show <task-id>           # See loop_iteration to know which pass you're on
  wg loops                    # List all loop edges and their status

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

  Model registry (catalog available models with cost/capability metadata):

  wg models init                      # Seed registry with defaults
  wg models list                      # Show available models and tiers
  wg models add <id> --tier <tier>    # Add a custom model entry
"#;

fn json_output() -> serde_json::Value {
    serde_json::json!({
        "getting_started": [
            "wg init",
            "wg agency init",
            "wg service start",
            "wg add \"My first task\""
        ],
        "agency": {
            "description": "Agency gives the service agents to assign to tasks.",
            "quick_setup": "wg agency init",
            "concepts": {
                "roles": "What agents do (skills + desired outcome)",
                "motivations": "Constraints on how agents work (acceptable/unacceptable trade-offs)",
                "agents": "A role + motivation pairing that gets assigned to tasks"
            },
            "manual_setup": [
                "wg role add \"Name\" --outcome \"...\" --skill name",
                "wg motivation add \"Name\" --accept \"...\" --reject \"...\"",
                "wg agent create \"Name\" --role <hash> --motivation <hash>",
                "wg config --auto-assign true --auto-evaluate true"
            ]
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
                "add": "Add a new task",
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
                "fail": "Mark failed (can be retried)",
                "abandon": "Give up permanently"
            }
        },
        "loops": {
            "description": "Loop edges model repeating workflows. A loops_to edge fires when its task completes, resetting a target back to open and incrementing loop_iteration.",
            "create": "wg add \"Revise\" --loops-to write --loop-max 3",
            "inspect": ["wg show <task-id>", "wg loops"],
            "note": "Agents read loop_iteration from wg show to know which pass they are on"
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
            "hierarchy": "task --model > executor model > coordinator model > 'default'",
            "model_registry": ["wg models init", "wg models list", "wg models add <id> --tier <tier>"]
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
    fn test_quickstart_text_contains_loop_edges() {
        assert!(QUICKSTART_TEXT.contains("LOOP EDGES"));
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
        assert!(QUICKSTART_TEXT.contains("Motivations"));
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
        assert!(output.get("loops").is_some());
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

        // Check loops fields
        let loops = output.get("loops").unwrap();
        assert!(loops.get("description").is_some());
        assert!(loops.get("create").is_some());
        assert!(loops.get("inspect").is_some());

        // Check tips is an array with entries
        let tips = output.get("tips").unwrap().as_array().unwrap();
        assert!(!tips.is_empty());
        assert!(tips.len() >= 5);

        // Check executors_and_models section
        let em = output.get("executors_and_models").unwrap();
        assert!(em.get("switch_executor").is_some());
        assert!(em.get("per_task_model").is_some());
        assert!(em.get("hierarchy").is_some());
        assert!(em.get("model_registry").is_some());
    }

    #[test]
    fn test_quickstart_text_contains_executors_and_models() {
        assert!(QUICKSTART_TEXT.contains("EXECUTORS & MODELS"));
        assert!(QUICKSTART_TEXT.contains("--coordinator-executor amplifier"));
        assert!(QUICKSTART_TEXT.contains("--model"));
        assert!(QUICKSTART_TEXT.contains("wg models"));
    }

    #[test]
    fn test_quickstart_text_all_sections_present() {
        let text = QUICKSTART_TEXT.trim();
        let required_sections = [
            "WORKGRAPH AGENT QUICKSTART",
            "GETTING STARTED",
            "AGENCY SETUP",
            "COORDINATOR SERVICE REMINDER",
            "SERVICE MODE",
            "MANUAL MODE",
            "DISCOVERING & ADDING WORK",
            "TASK STATE COMMANDS",
            "CONTEXT & ARTIFACTS",
            "LOOP EDGES",
            "TIPS",
            "EXECUTORS & MODELS",
        ];
        for section in &required_sections {
            assert!(text.contains(section), "Missing section: {}", section);
        }
    }
}

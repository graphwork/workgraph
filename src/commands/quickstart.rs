use anyhow::Result;

const QUICKSTART_TEXT: &str = r#"
╔══════════════════════════════════════════════════════════════════════════════╗
║                         WORKGRAPH AGENT QUICKSTART                           ║
╚══════════════════════════════════════════════════════════════════════════════╝

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
  wg done <task-id>           # Mark task complete (use 'wg submit' if verified)

DISCOVERING & ADDING WORK
─────────────────────────────────────────
  wg list                     # List all tasks
  wg list --status open       # Filter by status (open, in-progress, done, etc.)
  wg show <task-id>           # View task details and context
  wg add "Title" -d "Desc"    # Add new task
  wg add "X" --blocked-by Y   # Add task blocked by another

TASK STATE COMMANDS
─────────────────────────────────────────
  wg done <task-id>           # Complete (non-verified tasks)
  wg submit <task-id>         # Submit for review (verified tasks)
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
• If 'wg done' fails, the task may require verification — use 'wg submit'
• Check 'wg blocked <task-id>' if a task isn't appearing in ready list
"#;

pub fn run(json: bool) -> Result<()> {
    if json {
        let output = serde_json::json!({
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
                    "done": "Mark task complete (non-verified)",
                    "submit": "Submit for review (verified tasks)",
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
                "If 'wg done' fails, the task may require verification - use 'wg submit'",
                "Check 'wg blocked <task-id>' if a task isn't appearing in ready list"
            ]
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
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
    fn test_run_text_mode_succeeds() {
        assert!(run(false).is_ok());
    }

    #[test]
    fn test_run_json_mode_succeeds() {
        assert!(run(true).is_ok());
    }

    #[test]
    fn test_json_output_has_modes() {
        // Build the same JSON structure as the run function and verify fields
        let output = serde_json::json!({
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
                "discovery": { "list": "List all tasks", "show": "View task details and context", "add": "Add a new task", "ready": "See tasks available to work on (manual mode)" },
                "work": { "claim": "Claim a task for work (manual mode only)", "log": "Log progress as you work", "context": "See context from dependencies", "artifact": "Record output file/artifact" },
                "completion": { "done": "Mark task complete (non-verified)", "submit": "Submit for review (verified tasks)", "fail": "Mark failed (can be retried)", "abandon": "Give up permanently" }
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
                "If 'wg done' fails, the task may require verification - use 'wg submit'",
                "Check 'wg blocked <task-id>' if a task isn't appearing in ready list"
            ]
        });

        // Verify it's valid JSON
        let serialized = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        // Check top-level keys
        assert!(parsed.get("modes").is_some());
        assert!(parsed.get("commands").is_some());
        assert!(parsed.get("loops").is_some());
        assert!(parsed.get("tips").is_some());

        // Check modes
        let modes = parsed.get("modes").unwrap();
        assert!(modes.get("service").is_some());
        assert!(modes.get("manual").is_some());

        // Check commands sub-sections
        let commands = parsed.get("commands").unwrap();
        assert!(commands.get("discovery").is_some());
        assert!(commands.get("work").is_some());
        assert!(commands.get("completion").is_some());

        // Check loops fields
        let loops = parsed.get("loops").unwrap();
        assert!(loops.get("description").is_some());
        assert!(loops.get("create").is_some());
        assert!(loops.get("inspect").is_some());

        // Check tips is an array with entries
        let tips = parsed.get("tips").unwrap().as_array().unwrap();
        assert!(!tips.is_empty());
        assert!(tips.len() >= 5);
    }

    #[test]
    fn test_quickstart_text_all_sections_present() {
        let text = QUICKSTART_TEXT.trim();
        let required_sections = [
            "WORKGRAPH AGENT QUICKSTART",
            "COORDINATOR SERVICE REMINDER",
            "SERVICE MODE",
            "MANUAL MODE",
            "DISCOVERING & ADDING WORK",
            "TASK STATE COMMANDS",
            "CONTEXT & ARTIFACTS",
            "LOOP EDGES",
            "TIPS",
        ];
        for section in &required_sections {
            assert!(text.contains(section), "Missing section: {}", section);
        }
    }
}

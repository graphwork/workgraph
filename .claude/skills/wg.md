# Workgraph Skill

Interact with the workgraph task coordination system.

## Invocation

- `/wg` - Show status and ready tasks
- `/wg ready` - List ready tasks
- `/wg add <title>` - Add a task
- `/wg done <id>` - Mark complete
- `/wg status` - Full project status

## Instructions

Use `./target/debug/workgraph` (or `wg` if in PATH).

### `/wg` (default)

```bash
./target/debug/workgraph ready
./target/debug/workgraph check
```

Summarize what's ready and any issues.

### `/wg add <title>`

```bash
./target/debug/workgraph add "<title>" [--blocked-by X] [--hours N] [--cost N] [-t tag]
```

### `/wg done <id>`

```bash
./target/debug/workgraph done <id>
./target/debug/workgraph ready  # show what's unblocked
```

### `/wg status`

```bash
./target/debug/workgraph list
./target/debug/workgraph bottlenecks
./target/debug/workgraph forecast
```

## All Commands

| Command | Description |
|---------|-------------|
| `init` | Initialize workgraph |
| `add` | Add task |
| `done` | Mark done |
| `claim/unclaim` | Agent coordination |
| `ready` | List ready tasks |
| `list` | List all tasks |
| `blocked <id>` | Direct blockers |
| `why-blocked <id>` | Full blocker chain |
| `impact <id>` | What depends on this |
| `bottlenecks` | Tasks blocking most work |
| `structure` | Entry points, dead ends |
| `loops` | Cycle detection |
| `aging` | Task age distribution |
| `velocity` | Completion rate |
| `forecast` | Project completion estimate |
| `plan` | Budget/hours planning |
| `cost <id>` | Cost with dependencies |
| `check` | Verify graph health |
| `graph` | DOT output |
| `actor add/list` | Manage actors |
| `resource add/list` | Manage resources |
| `reschedule` | Set not_before timestamp |
| `coordinate` | Parallel dispatch status |

All commands support `--json` for machine output.

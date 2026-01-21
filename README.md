# workgraph

Task coordination for humans and AI agents.

## Install

```bash
cargo install --path .
```

Or for development:
```bash
cargo build
./target/debug/wg --help
```

## Quick Start

```bash
# Initialize a new workgraph in current directory
wg init

# Add tasks
wg add "Design API"
wg add "Implement backend" --blocked-by design-api
wg add "Write tests" --blocked-by implement-backend

# See what's ready to work on
wg ready

# Claim a task
wg claim design-api

# Mark it done
wg done design-api
```

## Conceptual Model

**Core Entities**
- **Tasks**: Units of work with a title, status, and optional metadata
- **Actors**: Humans or AI agents that claim and complete tasks
- **Resources**: Shared assets that tasks may require (future extension point)

**The Graph Model**

Tasks can block other tasks, forming a directed graph of dependencies. While typically a DAG, cycles are allowed for recurring/iterative work patterns. Use `wg why-blocked` to trace dependency chains and `wg impact` to see downstream effects.

**Status Flow**

```
open → in-progress → done
         ↑
      (claim)
```

A task is **blocked** (derived state) when any of its blockers are incomplete. Only unblocked tasks appear in `wg ready`.

**Timestamps**

Each task tracks `created_at`, `started_at`, and `completed_at` for temporal analysis, forecasting, and performance metrics.

**Agent Coordination**

Multiple agents can work in parallel:
1. `wg ready` — find available tasks
2. `wg claim <id>` — atomically claim a task (prevents double-work)
3. `wg done <id>` — mark complete, unblocking dependents
4. `wg unclaim <id>` — release if interrupted

The claim mechanism ensures safe concurrent execution across distributed agents.

## Commands

| Command | Description |
|---------|-------------|
| `wg init` | Initialize workgraph in current directory |
| `wg add "task title"` | Add a new task |
| `wg done <id>` | Mark task as complete |
| `wg ready` | List tasks ready to work on |
| `wg claim <id>` | Claim a task for work |
| `wg why-blocked <id>` | Show why a task is blocked |
| `wg impact <id>` | Show what depends on this task |
| `wg bottlenecks` | Find tasks blocking the most work |
| `wg forecast` | Estimate when work will complete |
| `wg coordinate` | Show ready tasks for parallel dispatch |
| `wg analyze` | Comprehensive health report |
| `wg critical-path` | Show longest dependency chain |

## Storage

All data lives in `.workgraph/graph.jsonl` — one JSON object per line. Human-readable, version-control friendly, easy to parse.

## License

[MIT](LICENSE)

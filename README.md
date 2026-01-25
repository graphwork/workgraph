# workgraph

A task graph for getting things done. Works for humans, works for AI agents, works for both at once.

## What is this?

You've got tasks. Some block others. Multiple people (or AIs) need to coordinate without stepping on each other. Workgraph handles that.

```bash
wg init
wg add "Design the API"
wg add "Build the backend" --blocked-by design-the-api
wg add "Write tests" --blocked-by build-the-backend

wg ready        # what can I work on?
wg claim design-the-api --actor erik
wg done design-the-api   # automatically unblocks the next task
```

That's it. Tasks flow through `open → in-progress → done`. Dependencies are respected. No one works on the same thing twice.

## Install

```bash
cargo install --path .
```

## The basics

**Tasks** are units of work. They have a status, can block other tasks, and track who's working on them.

**Actors** are the humans or AI agents doing the work. They claim tasks, complete them, and move on.

**The graph** is just tasks pointing at other tasks. "I can't start until X is done." Usually it's a nice clean DAG, but cycles are fine too for iterative stuff.

## Working with it

```bash
# See what's ready
wg ready

# Claim something
wg claim design-api --actor alice

# Log progress as you go
wg log design-api "Finished endpoint specs"

# Done
wg done design-api

# Blocked? Find out why
wg why-blocked build-backend

# What happens if I finish this?
wg impact design-api
```

## For AI agents

Agents can run autonomously:

```bash
# Register an agent
wg actor add claude-1 --role agent -c coding -c testing

# Let it loose
wg agent --actor claude-1
```

The agent loops: wake up, find work, claim it, do it, mark done, sleep, repeat. Multiple agents can run in parallel on independent tasks.

For tasks with shell commands attached:

```bash
wg exec run-tests --set "cargo test"
wg agent --actor ci-bot  # will automatically run the command
```

## Analysis

```bash
wg bottlenecks     # what's blocking the most stuff?
wg critical-path   # longest chain = minimum time to finish
wg forecast        # when will we be done?
wg analyze         # full health report
```

## Context flow

Tasks can declare what files they need and what they produce:

```bash
wg add "Design schema" --deliverable schema.sql
wg add "Build DB layer" --blocked-by design-schema --input schema.sql

# Later, see what's available
wg context build-db-layer
```

For AI agents with limited context windows, trajectories suggest the best order to claim tasks:

```bash
wg trajectory design-schema  # shows the chain of related work
```

## Storage

Everything lives in `.workgraph/graph.jsonl`. One JSON object per line. Human-readable, git-friendly, easy to hack on.

```jsonl
{"kind":"task","id":"design-api","title":"Design the API","status":"done"}
{"kind":"task","id":"build-backend","title":"Build the backend","status":"open","blocked_by":["design-api"]}
{"kind":"actor","id":"alice","name":"Alice","role":"engineer"}
```

## Commands at a glance

| Command | What it does |
|---------|--------------|
| `wg init` | Start a new workgraph |
| `wg add "title"` | Create a task |
| `wg ready` | What can be worked on now? |
| `wg claim <id>` | Take a task |
| `wg done <id>` | Finish a task |
| `wg fail <id>` | Mark as failed (can retry later) |
| `wg why-blocked <id>` | Trace the blocker chain |
| `wg impact <id>` | What depends on this? |
| `wg bottlenecks` | Find high-impact tasks |
| `wg agent --actor X` | Run autonomous agent loop |
| `wg analyze` | Full project health report |

See `wg --help` for everything else, or check [docs/](docs/) for the deep dive.

## License

MIT

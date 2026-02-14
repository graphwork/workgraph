# Workgraph Documentation

Workgraph (`wg`) is a task coordination system designed for both humans and AI agents. It provides a dependency-aware task graph that enables parallel work coordination, progress tracking, and project analysis.

## Table of Contents

- [Core Concepts](#core-concepts)
- [Quick Start](#quick-start)
- [Command Reference](./COMMANDS.md)
- [Agent Guide](./AGENT-GUIDE.md)
- [Storage Format](#storage-format)
- [JSON Output](#json-output)

## Core Concepts

### Tasks

Tasks are the fundamental units of work. Each task has:

- **id**: Unique identifier (auto-generated from title or specified manually)
- **title**: Human-readable description of the work
- **status**: Current state (open, in-progress, done, failed, abandoned)
- **blocked_by**: List of task IDs that must complete before this task can start
- **assigned**: Agent currently working on the task
- **estimate**: Optional hours and/or cost estimate
- **skills**: Required capabilities to complete the task
- **inputs**: Files or context needed to start work
- **deliverables**: Expected outputs when complete
- **artifacts**: Actual produced outputs (populated on completion)

### Status Flow

```
     ┌──────────────────────────────────────────┐
     │                                          │
     v                                          │
   open ──────> in-progress ──────> done        │
     │              │                           │
     │              │                           │
     │              v                           │
     │          failed ────────> (retry) ───────┘
     │              │
     │              v
     │         abandoned
     │
     └──────────────────────────────────────────────> abandoned
```

- **open**: Task exists but work has not started
- **in-progress**: Task has been claimed and is being worked on
- **done**: Task completed successfully
- **failed**: Task attempted but failed (can be retried)
- **abandoned**: Task will not be completed (terminal state)

A task is **blocked** (derived state) when any of its `blocked_by` dependencies are not yet done. Only unblocked, open tasks appear in `wg ready`.

### Agents

Agents represent humans or AIs who perform work. An agent is a unified identity that combines:

- **name**: Display name
- **role + motivation**: What the agent does and why (required for AI, optional for human)
- **capabilities**: Skills for task matching
- **trust_level**: verified, provisional, or unknown
- **capacity**: Maximum concurrent task capacity
- **rate**: Hourly cost rate
- **contact**: Contact info (email, Matrix ID, etc.)
- **executor**: How the agent receives work (claude, matrix, email, shell)

### Resources

Resources represent consumable or limited assets:

- **id**: Unique identifier
- **type**: Category (money, compute, time, etc.)
- **available**: Current available amount
- **unit**: Unit of measurement (usd, hours, gpu-hours, etc.)

### Dependencies (The Graph)

Tasks form a directed graph through `blocked_by` relationships. While typically a DAG (directed acyclic graph), cycles are permitted for iterative/recurring work patterns.

Key graph concepts:

- **Ready tasks**: No incomplete blockers, can start immediately
- **Blocked tasks**: Waiting on one or more incomplete dependencies
- **Critical path**: Longest dependency chain determining minimum project duration
- **Bottlenecks**: Tasks blocking the most downstream work
- **Impact**: Forward view of what depends on a given task

### Context Flow

Tasks can specify inputs and deliverables to establish an implicit data flow:

```
Task A                    Task B
├─ deliverables:          ├─ inputs:
│  └─ src/api.rs     ──────> └─ src/api.rs
│                         ├─ blocked_by:
│                              └─ task-a
```

The `wg context` command shows available inputs from completed dependencies.

### Trajectories

For AI agents with limited context windows, trajectories provide an optimal task ordering that minimizes context switching. The `wg trajectory` command computes paths through related tasks based on shared files and skills.

## Quick Start

### Initialize a New Project

```bash
wg init
```

Creates `.workgraph/graph.jsonl` in the current directory.

### Add Tasks

```bash
# Simple task
wg add "Design API schema"

# Task with dependencies
wg add "Implement API" --blocked-by design-api-schema

# Task with full metadata
wg add "Write API tests" \
  --blocked-by implement-api \
  --hours 4 \
  --skill testing \
  --deliverable tests/api_test.rs
```

### View Project Status

```bash
# What can I work on right now?
wg ready

# All tasks
wg list

# Project health overview
wg analyze
```

### Work on Tasks

```bash
# Claim a task
wg claim design-api-schema --actor erik

# Log progress
wg log design-api-schema "Defined initial endpoints"

# Complete the task
wg done design-api-schema
```

### Analyze the Project

```bash
# What's blocking this task?
wg why-blocked implement-api

# What depends on this task?
wg impact design-api-schema

# What are the bottlenecks?
wg bottlenecks

# When will we finish?
wg forecast
```

## Storage Format

All data is stored in `.workgraph/graph.jsonl` - a newline-delimited JSON file with one node per line. This format is:

- Human-readable and editable
- Version control friendly (line-based diffs)
- Easy to parse with standard tools

Example content:

```jsonl
{"kind":"task","id":"design-api","title":"Design API","status":"done","completed_at":"2026-01-15T10:00:00Z"}
{"kind":"task","id":"impl-api","title":"Implement API","status":"open","blocked_by":["design-api"]}
```

Configuration is stored in `.workgraph/config.toml`:

```toml
[agent]
executor = "claude"
model = "opus-4-5"
interval = 10

[project]
name = "My Project"
```

## JSON Output

All commands support `--json` for machine-readable output:

```bash
# Task list as JSON
wg list --json

# Single task details
wg show task-id --json

# Analysis results
wg analyze --json
```

This enables integration with scripts, CI/CD pipelines, and other tools:

```bash
# Count open tasks
wg list --json | jq '[.[] | select(.status == "open")] | length'

# Get IDs of ready tasks
wg ready --json | jq -r '.[].id'
```

## See Also

- [Command Reference](./COMMANDS.md) - Complete command documentation
- [Agent Guide](./AGENT-GUIDE.md) - Autonomous agent operation guide

# Logging Gaps Research: Current State vs Full Work Provenance

## 1. What IS Logged Today

### 1.1 Task Log Entries (`task.log[]` in graph.jsonl)

**Format:** Array of `LogEntry { timestamp: String, actor: Option<String>, message: String }`

**Which commands write log entries:**

| Command | Writes LogEntry? | What it records |
|---------|-----------------|-----------------|
| `wg claim` | Yes | "Task claimed by @{actor}" or "Task claimed" |
| `wg unclaim` | Yes | "Task unclaimed (was assigned to @{actor})" |
| `wg done` | Yes | "Task marked as done" (actor = task.assigned) |
| `wg fail` | Yes | "Task marked as failed: {reason}" (actor = task.assigned) |
| `wg abandon` | Yes | Reason + assigned actor |
| `wg retry` | Yes | "Task reset for retry (attempt #N)" |
| `wg pause` | Yes | "Task paused" (actor = None) |
| `wg resume` | Yes | "Task resumed" (actor = None) |
| `wg reclaim` | Yes | From/to actor transfer |
| `wg log` | Yes | User-provided message with optional actor |
| `spawn` (internal) | Yes | "Spawned by coordinator --executor {type}" |

**Which commands do NOT write log entries (but DO mutate the graph):**

| Command | What it mutates | LogEntry? |
|---------|----------------|-----------|
| `wg add` | Creates a new task | No |
| `wg edit` | Modifies title, description, tags, skills, blocked_by, model, loops_to | No |
| `wg archive` | Moves done tasks from graph.jsonl to archive.jsonl | No |
| `wg artifact` | Adds/removes artifact paths on a task | No |
| `wg reschedule` | Modifies scheduling fields | No |
| `wg evolve` | Creates/modifies/retires roles and motivations; creates tasks | No (to tasks) |
| `wg evaluate` | Records evaluation JSON, updates agent performance | No (to tasks) |
| `wg assign` | Sets task.agent content hash | No |

### 1.2 Task Timestamps

Three timestamps are tracked on each task:

- `created_at`: Set by `wg add` (RFC 3339)
- `started_at`: Set by `wg claim` / spawn (when status -> InProgress)
- `completed_at`: Set by `wg done` (when status -> Done)

**NOT tracked:**
- When task was edited (no `modified_at`)
- When description/title/tags changed
- When blocked_by edges were added or removed
- When task was assigned to an agent (only the current value is stored)

### 1.3 Agent Output Directory (`.workgraph/agents/agent-{N}/`)

Each spawned agent gets a directory containing:

| File | Content | Always present? |
|------|---------|----------------|
| `metadata.json` | `{agent_id, executor, model, pid, started_at, task_id, timeout}` | Yes |
| `run.sh` | Wrapper shell script that runs the executor and auto-completes task | Yes (recent agents) |
| `prompt.txt` | Full prompt piped to executor (task description + context + identity) | Yes (recent agents) |
| `output.log` | Stdout/stderr from executor + wrapper messages | Yes |

**Note:** Older agents (before agent-27) only have `metadata.json` and `output.log` — the `run.sh`/`prompt.txt` pattern was introduced later.

### 1.4 Task Output Capture (`.workgraph/output/{task-id}/`)

Created by `capture_task_output()` (called from `wg done` and `wg fail`):

| File | Content |
|------|---------|
| `changes.patch` | Git diff from `task.started_at` commit to current working tree |
| `artifacts.json` | JSON array of `{path, size}` for registered artifacts |
| `log.json` | Full task.log entries as JSON |

**Limitations observed:**
- `changes.patch` often contains `# git diff failed: No such file or directory` — the git diff capture is fragile
- Only captures diff at the moment of done/fail — intermediate states are lost
- No capture of which files the agent read (only what it changed)

### 1.5 Daemon Log (`.workgraph/service/daemon.log`)

Unstructured text log with entries like:
```
2026-02-03T01:00:31.743Z [INFO] Daemon starting (PID 2173242, socket /tmp/wg-.workgraph.sock)
2026-02-03T01:00:31.744Z [INFO] Coordinator config: poll_interval=60s, max_agents=3, executor=claude, model=default
[coordinator] Cleaned up 1 dead agent(s): ["agent-60"]
[coordinator] Spawning agent for: document-service-setup - Document service setup and management
[coordinator] Spawned agent-61 (PID 2173243)
2026-02-03T01:00:31.851Z [INFO] Coordinator tick #1 complete: agents_alive=2, tasks_ready=2, spawned=2
```

Records: daemon start/stop, coordinator tick summaries, agent spawn events, dead agent cleanup, IPC events (reconfigure, shutdown, graph-changed).

**Not structured** — plain text, no JSON, not machine-parseable without regex.

### 1.6 Evaluation Records (`.workgraph/agency/evaluations/*.json`)

Per-task evaluation files containing:
```json
{
  "id": "eval-{task-id}-{timestamp}",
  "task_id": "...",
  "agent_id": "{content-hash}",
  "role_id": "{content-hash}",
  "motivation_id": "{content-hash}",
  "score": 0.97,
  "dimensions": {
    "correctness": 0.98,
    "completeness": 0.97,
    "efficiency": 0.95,
    "style_adherence": 0.95
  },
  "notes": "...",
  "evaluator": "claude:opus",
  "timestamp": "..."
}
```

### 1.7 Service State Files

| File | Content |
|------|---------|
| `service/state.json` | Daemon PID, socket path, started_at |
| `service/coordinator-state.json` | Tick count, agents_alive, tasks_ready, config |
| `service/registry.json` | All agents ever registered, with PID, task_id, executor, heartbeat, status |

### 1.8 Archive (`.workgraph/archive.jsonl`)

Same format as `graph.jsonl` — archived tasks retain their full structure including log entries, artifacts, timestamps, and all fields. Tasks are moved here by `wg archive` (done tasks only).

---

## 2. What is NOT Logged

### 2.1 Graph Mutations (Critical Gap)

There is **no audit log of graph mutations**. When any command modifies `graph.jsonl`, the file is overwritten atomically (temp file + rename). The previous state is lost.

**Lost information:**
- History of task field changes (title edits, description updates, tag additions)
- History of dependency edge additions/removals (blocked_by changes)
- History of model assignment changes
- When and why a task was paused/resumed (only current `paused: bool` stored)
- Who/what added or removed artifacts
- Sequence of status transitions (only current status + log entries give partial reconstruction)

### 2.2 CLI Invocation History

No record of which CLI commands were run, by whom, or with what arguments. Example losses:

- `wg add "Build widget" --blocked-by design --skill rust` — the fact that this command was run at a specific time with these arguments is not recorded anywhere except the resulting task in graph.jsonl
- `wg edit my-task --add-tag urgent` — no trace that this edit happened, only the current state shows the tag exists
- `wg evolve --strategy mutation` — the invocation itself isn't logged; only the resulting role/motivation changes are persisted

### 2.3 Full Prompts for Non-Agent Commands

- `wg evolve` sends a large structured prompt to an LLM with performance data, evaluation history, and strategy instructions. The prompt is not preserved.
- `wg evaluate` sends a prompt with task output, role/motivation context, and evaluation criteria. The prompt is not preserved (only the resulting evaluation JSON is saved).
- `wg assign` (auto-assignment via agency) involves an LLM analyzing agent performance data. The prompt and reasoning are not preserved.

### 2.4 Intermediate Artifacts and Conversation Context

- Agent `output.log` captures stdout/stderr but this is the raw CLI output, not structured conversation turns
- No record of which files the agent read during execution
- No record of which tools/commands the agent invoked (only what it chose to `wg log`)
- No record of the agent's internal reasoning or decision-making process
- No conversation transcript (Claude's `--print` mode produces a flat stream, not structured turns)

### 2.5 Dependency Resolution History

- When a task becomes "ready" (all blockers resolved), this event isn't recorded
- When a task becomes blocked/unblocked due to dependency changes, no log
- The order in which tasks were dispatched by the coordinator is only in the ephemeral daemon.log

### 2.6 Cost and Resource Tracking

- No record of LLM API costs per task (token counts, model used, duration)
- No record of wall-clock time per agent execution (only `started_at` and `completed_at` on the task)
- No resource utilization data (how many agents were running concurrently at each point)

### 2.7 Configuration History

- No log of config changes (`wg config` mutations)
- No log of executor configuration changes
- Service reconfiguration events are in daemon.log but not structured

---

## 3. Nikete's Use Case: Workflow Replay with Different Models

The goal is **enough provenance to replay a project's workflow with a different model** — i.e., take everything that happened in a workgraph session and re-execute it with a different LLM backend, then compare results.

### 3.1 What Replay Requires

To replay a workflow, you need:

1. **Initial graph state**: The task graph as it existed before work began (or the sequence of `wg add` commands that created it)
2. **Task dispatch order**: Which tasks were dispatched in what sequence, and to which agents
3. **Full prompts**: The exact prompt each agent received (task description + context from dependencies + identity)
4. **Agent identity context**: Role, motivation, skills, desired outcome — all the parameters that shape agent behavior
5. **External inputs**: Any files or context the agent read from the filesystem that wasn't in the prompt
6. **Evaluation criteria**: The exact evaluation prompt used to score each task
7. **Graph mutations between tasks**: Any edits, dependency changes, or new tasks added during execution

### 3.2 What's Currently Capturable

| Requirement | Current State | Gap |
|-------------|--------------|-----|
| Initial graph state | Not captured (graph.jsonl is mutable) | Need snapshot at session start |
| Task dispatch order | Partially in daemon.log (unstructured) | Need structured dispatch log |
| Full prompts | `prompt.txt` in agent dirs (recent only) | Good for agent tasks; missing for evolve/evaluate |
| Agent identity context | Derivable from agent hash → role + motivation | OK if entities aren't retired/modified |
| External inputs | Not captured at all | Major gap |
| Evaluation criteria | Not captured (prompt is ephemeral) | Need to save eval prompts |
| Graph mutations | Not captured | Need mutation log |

### 3.3 Specific Replay Blockers

1. **No graph snapshot**: The graph is continuously mutated. To replay, you'd need to reconstruct the initial state from the archive + current graph + reverse-engineering mutations. This is fragile.

2. **Evolve prompts lost**: `wg evolve` constructs a complex prompt from evaluation data and sends it to an LLM. The prompt determines what role/motivation changes are proposed. Without the prompt, you can't replay the evolution step.

3. **Evaluate prompts lost**: `wg evaluate` constructs a prompt from task output + role context. Without the prompt, you can't compare how different models evaluate the same work.

4. **Config at point-in-time**: The coordinator config (which model, which executor, max agents) may have changed during the session. No history of config changes means you can't reproduce the exact execution environment.

5. **Agent output.log is flat text**: The raw output isn't structured enough to separate "what the agent thought" from "what the agent did". For replay comparison, you'd want structured turn-by-turn data.

---

## 4. Recommendations for a Unified Logging Architecture

### 4.1 Append-Only Event Log

Introduce a single append-only event log (`.workgraph/events.jsonl`) that records every state-changing operation:

```jsonl
{"ts":"...","event":"task.created","task_id":"build-widget","actor":"cli","data":{"title":"Build widget","blocked_by":["design"],"skills":["rust"]}}
{"ts":"...","event":"task.claimed","task_id":"build-widget","actor":"agent-42","data":{"prev_status":"open"}}
{"ts":"...","event":"task.edited","task_id":"build-widget","actor":"cli","data":{"field":"description","old":"...","new":"..."}}
{"ts":"...","event":"task.done","task_id":"build-widget","actor":"agent-42","data":{"duration_s":180}}
{"ts":"...","event":"agent.spawned","agent_id":"agent-42","task_id":"build-widget","data":{"executor":"claude","model":"opus","pid":12345}}
{"ts":"...","event":"config.changed","actor":"cli","data":{"key":"coordinator.max_agents","old":"3","new":"5"}}
{"ts":"...","event":"evolve.invoked","actor":"cli","data":{"strategy":"mutation","prompt_hash":"abc123"}}
```

**Key properties:**
- Append-only (never modified, never deleted)
- One event per line (JSONL for easy streaming/tailing)
- Every event has timestamp + actor + event type
- Contains enough data to reconstruct graph state at any point in time
- Prompt contents referenced by hash (stored separately to keep events compact)

### 4.2 Prompt Archive

Store all LLM prompts in a content-addressed store (`.workgraph/prompts/{sha256}.txt`):
- Agent task prompts (already saved as `prompt.txt` — just also hash and index them)
- Evolve prompts
- Evaluate prompts
- Assign prompts (for auto-assignment)

Reference prompts by hash in the event log. This enables exact replay — feed the same prompt to a different model.

### 4.3 Graph Snapshots

Periodically (or on explicit request), snapshot the entire graph state:
- Before `wg service start` (capture initial state)
- Before/after `wg evolve` (capture agency state transitions)
- On `wg snapshot` command (manual checkpointing)

Store as `.workgraph/snapshots/{timestamp}.jsonl`.

### 4.4 Structured Daemon Log

Replace the current text-based daemon.log with structured JSONL:
```jsonl
{"ts":"...","level":"info","event":"daemon.started","data":{"pid":12345,"config":{...}}}
{"ts":"...","level":"info","event":"coordinator.tick","data":{"tick":1,"agents_alive":3,"tasks_ready":2,"spawned":1}}
{"ts":"...","level":"info","event":"agent.dead","data":{"agent_id":"agent-42","task_id":"build-widget","reason":"no_heartbeat"}}
```

This unifies with the event log architecture and makes daemon activity machine-parseable.

### 4.5 Implementation Priority

| Priority | Change | Effort | Replay Value |
|----------|--------|--------|-------------|
| P0 | Append-only event log for graph mutations | Medium | Critical — enables state reconstruction |
| P0 | Save evolve/evaluate prompts to content-addressed store | Low | Critical — enables LLM replay |
| P1 | Structured daemon log | Medium | High — enables dispatch order replay |
| P1 | Graph snapshots before service start | Low | High — known-good starting point |
| P2 | CLI invocation log (command + args + timestamp) | Low | Medium — human audit trail |
| P2 | Cost/token tracking per agent run | Medium | Medium — comparative cost analysis |
| P3 | Structured agent conversation log | Hard | Nice-to-have — requires executor changes |

### 4.6 Design Constraints

- **Backward compatible**: Existing graph.jsonl format unchanged. Event log is additive.
- **Performance**: Append-only writes are fast. No read overhead on hot paths.
- **Storage**: Prompts are large but compressible. Content-addressing deduplicates identical prompts (e.g., assignment prompts share a common template).
- **Privacy**: Prompts may contain sensitive code. The event log and prompt store should follow the same access model as the rest of `.workgraph/`.
- **Rotation**: Event log and prompt store need rotation/compaction for long-running projects (see the `impl-log-rotation` task).

---

## 5. Summary Table

| Data Category | Logged Today? | Where? | Sufficient for Replay? |
|--------------|--------------|--------|----------------------|
| Task creation | Partial | graph.jsonl (created_at) | No — need full creation event |
| Task status changes | Partial | task.log[] entries | Partial — missing edit/assign events |
| Task field edits | No | — | No |
| Dependency changes | No | — | No |
| Agent prompts | Yes (recent) | agents/agent-N/prompt.txt | Yes for agent tasks |
| Evolve prompts | No | — | No |
| Evaluate prompts | No | — | No |
| Agent output | Yes | agents/agent-N/output.log | Partial (flat text) |
| Task output capture | Yes | output/{task-id}/ | Partial (patch often fails) |
| Evaluation scores | Yes | agency/evaluations/*.json | Yes |
| Daemon activity | Partial | service/daemon.log | Partial (unstructured) |
| Config changes | No | — | No |
| Graph snapshots | No | — | No |
| CLI invocations | No | — | No |

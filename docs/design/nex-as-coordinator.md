# Unified run loop: `nex = task = evaluate = coordinate`

## The principle (2026-04-18 refinement)

There are currently four LLM-driven runtimes in workgraph. They should
be one codepath with pluggable input/output surfaces:

| Role | Input source | Output sink | System prompt | Tool filter |
|---|---|---|---|---|
| `nex` (interactive REPL) | rustyline on stdin | stderr streaming | generic+tools | full (minus wg_* in non-coord mode) |
| task agent (autonomous) | task description, injected state | streaming file, journal | role+tradeoff prompt | full |
| `evaluate` | eval prompt (one-shot) | JSON record | evaluator prompt | none (no tools) |
| coordinator | `mpsc::Receiver<ChatRequest>` | chat/&lt;id&gt;/streaming | coordinator prompt | wg_* tools only |

**The conversation loop itself — turn boundary, microcompact, cancel
token, L0 defense, streaming idle watchdog, file re-injection, inbox
drain — is IDENTICAL across all four.** Only the surfaces differ.

Today only `nex` and `task` share the `AgentLoop` path. `evaluate`
uses one-shot `run_lightweight_llm_call` (fine for its cost profile
but conceptually the same loop with `max_turns=1` and no tools).
`coordinator` has its own ~1400-line hand-rolled loop in
`coordinator_agent::native_coordinator_loop` — which is the one the
user is seeing through the TUI chat.

The cost of keeping them separate: every improvement to the nex loop
has to be manually ported. Stages A–F, L0 defense, microcompact,
streaming idle watchdog, Stage G cancel threading — all apply to
nex/task but NOT to the coordinator, which is why the TUI coordinator
chat "feels different."

## Why

Workgraph currently has two parallel agent runtimes:

- **Coordinator daemon** (`wg service start`) — a long-running background
  process with a deterministic tick loop. Checks the graph, identifies
  ready tasks, spawns worker agents, manages lifecycle. Silent unless
  you tail its log.
- **`wg nex`** — an interactive REPL backed by the same `AgentLoop`,
  but one conversation, one session, chat-style.

Both use the native executor. Both share tools, compaction, journal,
cancel, inbox. They're *almost* the same code already — but with
enough seams between them that a design change in one rarely
propagates to the other.

The cost of that duplication has accumulated into real bugs (verify
autospawn ran in the daemon but not nex; the TUI double-render was
daemon-only; the microcompact pathology surfaced in nex first, fix
benefited daemon second). Every change to the run loop has to be
applied in two places, and they drift.

The user's observation: **these should be the same code path**. The
coordinator IS an agent. It happens to run long, talk to itself a
lot, and dispatch work. But it's still an LLM-driven loop over a
task list, with tools, context pressure, compaction, journaling.

## What unification looks like

### The coordinator is a `wg nex` session with a specific role

Today `wg nex --role coordinator` already exists: it keeps the
`wg_*` mutation tools (`wg_add`, `wg_done`, `wg_fail`, now
`wg_rescue`) and adds a system-prompt addendum. The remaining step
is to make this session **long-running, file-backed, restartable,
and dispatchable by external processes** — which is to say, promote
it to first-class infrastructure, not just a "role flag."

### One binary, two invocation shapes

```bash
# Human interactive (today):
wg nex

# Human coordinator, interactive (today, kept):
wg nex --role coordinator

# Headless coordinator daemon (new — replaces `wg service start`):
wg nex --role coordinator --detach [--foreground]
```

`--detach` forks to background, sets up the file-based inbox at
`<workgraph>/inbox/coordinator-<id>.jsonl` (reusing the Stage F
inbox machinery), redirects streaming output to
`<workgraph>/coordinator-<id>/stream.ndjson`, and keeps the journal
at `<workgraph>/coordinator-<id>/conversation.jsonl`.

From the outside it looks exactly like `wg service start` looks
today — a daemon with a log and a pid. From the inside it's a
`wg nex` session talking to itself.

### The tick loop is turns

The current coordinator has a deterministic scheduling tick: check
graph, find ready tasks, spawn workers, respect rate limits, log.
The new coordinator's tick is **a turn in its agentic loop**:

1. Agent-loop turn boundary fires (the same one as nex today).
2. Inbox drains any new messages from `wg send <coord> ...`.
3. Microcompact runs if pressure.
4. LLM call: "here's the graph state, here are ready tasks, here are
   active workers, what should I do?"
5. Model responds with tool calls: `wg_list`, `wg_spawn`, `wg_rescue`,
   `wg_log`, etc.
6. Tools execute. Graph mutates. Next turn.

The LLM does the *judgment* calls: which task to prioritize when
multiple are ready, when to rescue, when to escalate. The determinism
lives in the tools (file-locked graph mutations, claim/unclaim
mechanics, worktree setup) — same as today, just reached via tool
calls instead of direct function calls.

### Between-turn pacing

A naive translation would have the coordinator LLM-call on every
tick, at LLM speed (multi-second per decision). That's too slow AND
too expensive. Pace it:

- **Idle tick** (nothing happening, no workers, no inbox): sleep for
  `coordinator.tick_interval` (default 30s). No LLM call. The loop
  just spins on the file-inbox/graph-change notifier.
- **Event tick** (graph changed, inbox delivered, worker finished):
  wake immediately, run a turn.
- **Scheduled tick** (task became ready due to time-based unblock,
  cron fire): wake at the exact moment.

Skipping an LLM call when nothing changed is the default. The LLM
only runs when there's something to think about.

### Existing deterministic machinery stays

- **File-locked graph mutation** via `modify_graph` → stays.
- **Worktree setup / teardown** → stays.
- **Agent spawn** (`wg spawn`, claim, subprocess management) →
  stays as tools the coordinator LLM calls.
- **Heartbeat / liveness detection** → stays, but the coordinator
  can also *decide* what to do when a worker goes quiet (kill,
  triage, rescue) rather than having a hardcoded policy.
- **Cycle detection, dependency resolution** → stays as utilities
  the tools use.

What goes away: the deterministic scheduler-as-state-machine in
`src/commands/service/coordinator.rs`. That file shrinks to thin
wrappers around tool calls and notifier infrastructure.

## Migration plan

### Phase 1: shared run loop (no behavior change)

Refactor `AgentLoop::run_interactive` and the autonomous task-agent
path into a single `run` function with two pluggable surfaces
(interactive input source, headless input source). Stages A–F
already moved most of the shared machinery into one place; the
remaining seams are in input handling and session lifecycle.

Tests: all existing nex + task-agent tests must pass. No new
behavior.

### Phase 2: coordinator tools

New wg tools the coordinator LLM calls to manage the graph:

- `wg_spawn(task_id, [model])` — claim + fork worker agent.
- `wg_check_workers()` — list alive workers + status.
- `wg_kill_worker(agent_id)` — cooperative or hard kill.
- `wg_ready_tasks()` — structured list of tasks ready to dispatch.

These wrap existing functionality. Tests: each tool in isolation.

### Phase 3: coordinator-as-nex prototype — **shipped 2026-04-18**

The I/O surface is done: `wg nex --chat-id N [--role coordinator] [--resume]`
now reads user turns from `.workgraph/chat/N/inbox.jsonl`, streams
tokens to `.workgraph/chat/N/.streaming`, and appends finalized
replies to `.workgraph/chat/N/outbox.jsonl` — same paths, same
`ChatMessage` format, same streaming dotfile that the TUI already
tails. Journal is pinned to `.workgraph/chat/N/conversation.jsonl`
so `--resume` picks up the right session deterministically.

See `src/executor/native/chat_surface.rs` (adapter over `crate::chat`)
and the `with_chat_id` builder on `AgentLoop`. Smoke-tested against
qwen3-coder-30b on lambda01: seeded `{"content":"Respond with
exactly: ACK-SMOKE"}` into the inbox, nex produced the expected
outbox entry within seconds.

Still open in this phase:

- The TUI / `wg service start` spawn path still runs the legacy
  `native_coordinator_loop` directly in-process. The swap —
  `Command::new("wg").args(["nex", "--chat-id", N, "--role",
  "coordinator", "--resume"])` — is ready on the producing side
  but needs the `send_message` and `route_chat_to_agent` paths
  refactored to write to the inbox instead of the mpsc channel
  (the subprocess reads the inbox directly via `.nex-cursor`,
  bypassing the channel).
- Coordinate `.coordinator-cursor` (daemon-side) and `.nex-cursor`
  (agent-side) so messages aren't double-consumed or missed on
  crash recovery.
- Full integration test spawning the actual `wg` binary.

(Original plan below, kept for history:)

Add `wg nex --detach` that forks, sets up file inbox, runs
`run_interactive` with `--role coordinator` + a coordinator-specific
system prompt that explains the tick semantics. Keep old
`wg service start` working side-by-side.

System prompt for the coordinator role explicitly covers:
- "You are workgraph's coordinator."
- "Your tools are: wg_list, wg_show, wg_ready_tasks, wg_spawn,
  wg_check_workers, wg_kill_worker, wg_add, wg_done, wg_fail,
  wg_rescue, wg_log."
- "On each turn you'll see recent graph state and any queued inbox
  messages. Decide what to do next."
- "Prefer least-action: if everything's running and nothing's
  blocked, just log 'nothing to do' and wait. Don't make busy-work."

Phase 3 runs parallel to the old daemon. Test both.

### Phase 4: flip the default

Once Phase 3 is stable, make `wg service start` delegate to
`wg nex --role coordinator --detach`. Deprecate the old
deterministic coordinator loop; keep it available for a release
via config flag, then delete.

### Phase 5: cleanup

Delete the old `src/commands/service/coordinator.rs` state-machine
code. Cross-cutting benefits ripple out: any Stage-A–F-level
improvement to the run loop now benefits the coordinator automatically.

## Trade-offs to be explicit about

**Good:**
- One codepath. Stage A–F features (cancel, inbox, compaction,
  re-injection, microcompact, L0 defense) all apply to the
  coordinator automatically.
- Coordinator can explain itself. `wg msg send <coord> "why did
  you spawn that task?"` is a reasonable thing to do.
- Coordinator can be redirected mid-decision. "Stop that task"
  goes through the same inbox as anything else.
- Coordinator's journal is readable — audit log of scheduling
  decisions in natural language, not just structured events.

**Risky:**
- LLM-driven scheduling is non-deterministic. A deterministic
  scheduler is predictable; a conversational one isn't. Mitigate
  by keeping the tools deterministic and the LLM as the judgment
  layer only.
- Per-tick LLM cost. Mitigate with the idle/event/scheduled
  pacing above — LLM only runs when there's something to decide.
- LLM might "forget" to dispatch ready tasks, or dispatch the
  wrong one. Mitigate with a watchdog: if N ready tasks sit
  un-dispatched for M minutes, inject a system-reminder-style
  note into the next turn.
- A pathological coordinator (stuck in a compaction loop,
  generating too many tool calls) could waste budget. Same
  mitigations as any other agent: `max_turns`, timeout,
  circuit-breaker.

**Unanswered:**
- How does the coordinator's own FLIP / eval happen? Probably
  by another coordinator (coordinators evaluate each other's
  decisions, in a future governance extension).
- What's the upgrade story when the coordinator system prompt
  changes? A restart re-seeds; a journal-replay restores from
  a known point; a `wg nex --role coordinator --resume` brings
  it back with memory intact.

## What to NOT do (scope)

- **Not** replacing the current coordinator's wire protocol for
  task claims / worker heartbeats. Those stay file-based and
  deterministic. Only the *scheduling judgment* moves to the LLM.
- **Not** making the coordinator the only runtime. `wg nex`
  stays usable for humans; short-lived task-agents stay short-lived.
- **Not** giving the coordinator access to arbitrary bash /
  network. Its tool set is graph-scoped: list, spawn, kill,
  rescue, log. It doesn't read source files or run tests; that's
  the workers' job.

## Related design work

- `docs/design/native-executor-run-loop.md` — the turn-boundary
  refactor that made this possible (Stages A–F).
- `docs/design/coordinator-as-regular-agent.md` — earlier thinking
  along these lines, which this document supersedes.
- `docs/design/agent-message-queue.md` — the inbox protocol the
  unified coordinator would use for external control.

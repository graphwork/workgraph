# Research: Why Condition G Always Times Out on Olivo

**Date:** 2026-04-08
**Task:** research-why-condition
**Status:** Research complete

---

## Executive Summary

Condition G agents consistently hit the 30-minute trial timeout on Olivo rather than
completing cleanly. The root cause is a **compounding timeout mismatch**: agents have
no awareness of elapsed time, no mechanism to receive "wrap up" signals, and the
multi-agent autopoietic design creates work that exceeds the per-agent timeout even
when individual agents are productive. The recently-landed heartbeat implementation
(impl-tb-heartbeat) partially addresses this but has a critical gap: `budget_secs`
is passed as `None` for non-TB runs, and the adapter doesn't pass it for TB runs either.

**Root cause hypothesis (HIGH confidence):** The timeout is not caused by agents
spinning or failing — it's caused by agents doing useful work that simply doesn't
converge within 30 minutes, combined with zero awareness of the approaching deadline.
The 64% pass rate from run 4 confirms agents *can* solve the tasks; they just can't
tell when to stop iterating and commit their partial progress.

---

## Question-by-Question Analysis

### Q1: What does a typical condition G agent's log look like in the last 5 minutes before timeout?

**Answer: Agents are still doing useful work, not spinning.**

Evidence from the experiment progress report and condition G status doc:

- In run 4 (64% pass rate, the best result), "this came from agents working until
  timeout rather than clean autopoietic iteration" (experiment-progress-report.md:55)
- 11 of 13 trials in run 3 hit `AgentTimeoutError` — "the agent never signaled
  convergence" (experiment-progress-report.md:83)
- The 2 clean completions both had simple task structures; complex graphs deadlocked
  (experiment-progress-report.md:84-85)

**What's happening in the final minutes:** The agent or its spawned sub-agents are
still actively implementing, testing, and iterating. The verification cycle
(`--max-iterations 5`) keeps restarting because tests haven't passed yet — each
iteration spawns a fresh agent that starts implementing from scratch. By the time
the trial timeout fires, the system is mid-iteration with partially complete work.

There is no local Olivo run log data available for direct analysis (only calibration
runs exist in `terminal-bench/results/`), but the experiment progress report provides
strong indirect evidence from 6 runs totaling ~230 trials.

### Q2: Do agents have any awareness of elapsed time or remaining time budget?

**Answer: No. Zero awareness.**

The agent prompt contains no information about:
- Wall-clock time elapsed since trial start
- Remaining time budget
- Per-task timeout from `task.toml` (15min to 3.3hr per task)
- The 30-minute adapter trial timeout (`DEFAULT_TRIAL_TIMEOUT`)
- The 30-minute per-agent timeout (`coordinator.agent_timeout`)

**Evidence:**
- `_build_config_toml_content()` (`adapter.py:761-790`) writes no timeout-related
  config that agents would see in their prompt
- The `CONDITION_G_META_PROMPT` (`adapter.py:483-531`) mentions no time constraints
- The agent's `REQUIRED_WORKFLOW` prompt section (injected by native executor) has
  no time awareness
- The heartbeat prompt template (`coordinator_agent.rs:400-414`) includes
  `Time elapsed: {elapsed}s | Budget remaining: ~{remaining}s` — but `budget_secs`
  is passed as `None` in the daemon's heartbeat call (`mod.rs:2823`), making
  `remaining` always show `~0s`

### Q3: Is there a mechanism for the coordinator or runtime to signal 'wrap up now'?

**Answer: No graceful wrap-up mechanism exists.**

The system has only two termination modes:

1. **Hard kill via `timeout` wrapper:** Each agent process is wrapped with
   `timeout --signal=TERM --kill-after=30 <secs> <command>` (`execution.rs:449-453`).
   When the timeout fires, SIGTERM is sent, and if the process doesn't exit within
   30 seconds, SIGKILL follows. The agent gets no chance to save state or commit
   partial work — SIGTERM to Claude CLI causes immediate termination.

2. **Adapter-level stop:** When `_run_native_executor()` exceeds `timeout_secs`,
   it calls `wg service stop` (`adapter.py:708`). The default `wg service stop`
   does NOT kill agents (`kill_agents: false` in `ipc.rs:763`) — it only stops
   the daemon. Agents are detached (setsid) processes that survive daemon shutdown.
   They continue running until their own `timeout` wrapper kills them.

**What's missing:** A "soft deadline" signal that tells agents "you have N minutes
remaining — commit what you have and stop iterating." The heartbeat design doc
(`design-tb-heartbeat-orchestration.md`) proposes this but it hasn't been implemented
in the adapter-to-daemon bridge.

### Q4: What's the timeout value, and is it reasonable for the task sizes being dispatched?

**Answer: Three overlapping timeouts, all set to ~30 minutes, which is often insufficient.**

| Timeout | Value | Source | Effect |
|---------|-------|--------|--------|
| `DEFAULT_TRIAL_TIMEOUT` | 1800s (30min) | `adapter.py:72` | Adapter stops polling, calls `wg service stop` |
| `coordinator.agent_timeout` | `"30m"` | `config.rs:2353-2354` | Per-agent `timeout` wrapper sends SIGTERM |
| TB per-task `task.toml` timeouts | 900s–12000s | TB 2.0 dataset | Harbor's `AgentTimeoutError` |

**Compliance issue:** Our `reproduce.sh` overrides all per-task timeouts with a flat
30-minute cap (`--timeout 1800`). This truncates hard tasks that need up to 3.3 hours.
(Documented in `research-tb-timeout-scoring.md:72-78`.)

**For Condition G specifically:** The problem is compounded because:
- The seed agent takes time to build the graph (~2-5 minutes)
- Sub-agents each get their own 30-minute timeout (but the trial timeout is also 30min)
- Verification cycles restart work from scratch, burning through the budget
- With `max_agents=8`, there's parallelism, but the trial timeout is the binding constraint

**Is 30 minutes reasonable?** For single-agent conditions (A, F), 30 minutes is
sufficient for most tasks — F achieves 45% pass rate. For Condition G's multi-agent
iterative approach, 30 minutes is clearly insufficient. The autopoietic design
assumes multiple iterations, but one iteration (graph build + implement + verify)
takes 10-15 minutes, leaving only 1-2 iterations before timeout.

### Q5: How does the agent's executor (Claude CLI) handle SIGTERM/timeout — does it get a chance to save state?

**Answer: No. SIGTERM causes immediate unclean termination with no state save.**

The kill chain:

1. `timeout --signal=TERM --kill-after=30 <secs>` wrapper sends SIGTERM
2. Claude CLI receives SIGTERM → exits (no graceful shutdown handler documented)
3. If still alive after 30s → SIGKILL
4. The wrapper script (`execution.rs:993-1010`) has post-exit logic that calls
   `wg done "$TASK_ID"` on exit code 0, but timeout exit is non-zero (exit 124)
5. Task remains `in-progress` with a dead agent → orphan reconciliation picks it up
   on next coordinator tick, resets to `open`

**For the coordinator agent** (`coordinator_agent.rs:433-447`):
- SIGINT (not SIGTERM) is used for interrupts
- Claude CLI treats SIGINT as "stop generating" and emits TurnComplete
- But this only applies to the coordinator, not task agents

**Key gap:** When a task agent is killed by timeout, any uncommitted work (code
changes, test results, debugging insights) is lost completely. The next agent starts
from the same base state, potentially redoing the same work.

### Q6: Does the heartbeat implementation (impl-tb-heartbeat) address any of this?

**Answer: Partially addresses strategic oversight; does NOT address time awareness or graceful completion.**

**What the heartbeat implementation provides:**

1. **Integrated heartbeat timer** (`mod.rs:2472-2482, 2806-2838`): The daemon tracks
   `last_heartbeat` and sends a synthetic prompt to the coordinator agent when
   `heartbeat_interval` elapses. This is wired up and working.

2. **Heartbeat prompt** (`coordinator_agent.rs:400-414`): Includes time elapsed and
   budget remaining, plus a 5-point review checklist (stuck agents, failed tasks,
   ready work, progress check, strategic).

3. **Config support** (`config.rs:2404`): `heartbeat_interval` defaults to 0
   (disabled). Condition G sets it to 30 in the adapter config.

**What it does NOT address:**

1. **`budget_secs` is always `None`:** The daemon passes `None` for budget_secs
   (`mod.rs:2823`), so the heartbeat prompt always shows `Budget remaining: ~0s`.
   The coordinator has no idea how much time is left.

2. **No "wrap up" action:** The heartbeat prompt tells the coordinator to review and
   dispatch, but doesn't include guidance like "if time is running low, tell agents
   to commit partial work" or "signal convergence to stop cycles."

3. **No per-agent time signaling:** Even if the coordinator knew the budget, there's
   no mechanism to send a "wrap up" message to running task agents that they'd
   actually act on mid-execution.

4. **Adapter doesn't configure it for TB:** The adapter writes `heartbeat_interval = 30`
   in config.toml, but the daemon has no way to know the trial's total time budget.

### Q7: TB adapter (terminal-bench/wg/adapter.py) condition G config — what timeouts, iteration limits, etc. are set?

**Answer: Configuration is present but incomplete.**

**Condition G config** (`adapter.py:127-137`):

```python
"G": {
    "exec_mode": "full",
    "context_scope": "graph",
    "agency": None,
    "exclude_wg_tools": False,
    "max_agents": 8,
    "autopoietic": False,            # Phase 3: no meta-prompt
    "coordinator_agent": True,        # Phase 3: persistent coordinator
    "heartbeat_interval": 30,         # Phase 3: 30s heartbeat
    "coordinator_model": "sonnet",    # Phase 3: capable coordinator model
}
```

**Config.toml generation** (`adapter.py:761-790`):

```toml
[coordinator]
max_agents = 8
executor = "native"
model = "openrouter:minimax/minimax-m2.7"
worktree_isolation = false
max_verify_failures = 0
max_spawn_failures = 0
coordinator_agent = true
heartbeat_interval = 30

[agent]
model = "openrouter:minimax/minimax-m2.7"
context_scope = "graph"
exec_mode = "full"

[agency]
auto_assign = false
auto_evaluate = false
```

**What's missing from config:**

| Setting | Current | Needed |
|---------|---------|--------|
| `agent_timeout` | Default `"30m"` (not explicitly set) | Should match TB per-task timeout |
| `coordinator_model` | Written to adapter dict but NOT used in config.toml | Need to wire `coordinator_model` into config generation |
| `max_iterations` | Not set in config | The meta-prompt says 5, but this is guidance, not enforced |
| `worktree_isolation` | `false` | Should be `true` for multi-agent — agents share a single worktree, causing conflicts |

**Trial timeout chain:**

```
Harbor task.toml timeout (900-12000s)
  └─ overridden by reproduce.sh --timeout 1800
     └─ adapter DEFAULT_TRIAL_TIMEOUT = 1800
        └─ adapter polls with timeout → calls wg service stop
           └─ daemon dies but agents survive (setsid)
              └─ agents killed by their own timeout wrapper (30m from spawn time)
```

---

## Root Cause Analysis

### Primary Cause: No Time Awareness → No Convergence Signal (HIGH confidence)

The fundamental issue is that **the entire system is time-blind**:

1. **Agents don't know the clock is ticking.** No prompt mentions elapsed time, budget,
   or deadlines. An agent in iteration 3 of a verification cycle has no way to know
   it should stop iterating and commit partial work.

2. **The coordinator can't warn agents.** Even with heartbeats, the coordinator doesn't
   know the trial budget and has no "soft deadline" message mechanism.

3. **The adapter has no graceful shutdown.** When the trial timeout fires, the adapter
   calls `wg service stop` (which doesn't kill agents), then the function returns.
   There's no "5 minutes remaining" warning, no "commit what you have" signal.

### Contributing Causes

| Factor | Impact | Evidence |
|--------|--------|---------|
| **Verification cycle restarts from scratch** | Each iteration starts fresh, wasting prior work | Meta-prompt says `wg done` (iterate) vs `--converged` (stop), but agents rarely signal `--converged` |
| **Prompt competition** | REQUIRED_WORKFLOW overrides meta-prompt guidance | experiment-progress-report.md:73-77 — agent implements directly instead of delegating |
| **M2.7 meta-cognition limits** | Model can't judge "is my approach converging?" | experiment-progress-report.md:168-173 — model struggles with delegation and evaluation |
| **Stale worktree state** | `worktree_isolation = false` means agents can conflict | Config explicitly disables isolation despite max_agents=8 |
| **Agent timeout = trial timeout** | Both 30min — no room for the seed agent's setup time | Agent timeout starts at spawn, but trial timeout starts at adapter.run() |

### The Timeout Paradox

Condition G's 64% pass rate (run 4) shows the approach *works* — agents just need more
time or better time management. But the timeout is not the wrong length; the agents are
doing the wrong thing near the end. The core issue is:

> **An agent that has solved the problem but hasn't verified it yet will keep iterating
> until killed, rather than committing its work and signaling done.**

This is because:
1. The verify task checks if tests pass → they might not pass yet → `wg done` (iterate)
2. Another work agent spawns → re-implements → verify → repeat
3. Eventually timeout kills everything, including possibly-correct work

---

## Recommendations

### R1: Wire `budget_secs` into the heartbeat (CRITICAL, easy fix)

The daemon already has the heartbeat prompt template showing budget remaining. The
adapter needs to pass the trial timeout to the daemon (via config or env var), and
the daemon needs to pass it to `send_heartbeat()`.

**Mechanism:** Add `trial_budget_secs` to config.toml. In `_build_config_toml_content()`,
add `trial_budget_secs = <timeout>`. The daemon reads it and passes to `send_heartbeat()`.

### R2: Add a "wrap-up" phase to the coordinator heartbeat (CRITICAL)

When `budget_remaining < 300s` (5 minutes), the heartbeat prompt should change from
"review and dispatch" to:

> "TIME CRITICAL: <5 minutes remaining. Stop creating new tasks. Send `wg msg` to all
> in-progress agents: 'Commit your current work NOW and run `wg done`. Do not start
> new iterations.' Kill any stuck agents. Mark the overall task done if tests pass."

### R3: Give agents elapsed-time awareness (IMPORTANT)

Add to the agent prompt (via REQUIRED_WORKFLOW or a new section):

> "This task has a wall-clock budget of {timeout}s. Time elapsed since task start:
> {elapsed}s. If you are running low on time, commit your best work and call `wg done`
> rather than starting another iteration."

The wrapper script could inject `$WG_TASK_TIMEOUT` and `$WG_TASK_START` env vars.
The agent prompt builder could use these.

### R4: Enable worktree isolation for multi-agent conditions (IMPORTANT)

`worktree_isolation = false` with `max_agents = 8` means 8 agents share one working
tree. This causes file conflicts and wasted work. Set `worktree_isolation = true` in
the Condition G config.

### R5: Make verification preserve progress (MEDIUM)

Currently, when a verification cycle iterates, the next work agent starts fresh. It
should be able to see what the previous agent did (via `wg context` or artifact links).
The verification task should capture test output and pass it to the next iteration via
`wg log` or `wg msg`.

### R6: Set `agent_timeout` shorter than `trial_timeout` (MEDIUM)

With both at 30 minutes, the first agent can consume the entire trial budget. Set
`agent_timeout` to `trial_timeout / 2` or `trial_timeout - 300` to leave room for
the seed task and a cleanup phase.

---

## Relationship to Heartbeat Implementation

The heartbeat implementation (impl-tb-heartbeat) is **necessary but not sufficient**:

| Heartbeat provides | Still needed |
|-------------------|-------------|
| Periodic coordinator review | Time budget awareness (`budget_secs` wiring) |
| Stuck agent detection | "Wrap-up" phase behavior change |
| Strategic re-planning | Per-agent time signals |
| NOOP on healthy systems | Graceful partial-completion commit |

The heartbeat is the right *mechanism* — it gives the coordinator a voice during
autonomous runs. But without time awareness and wrap-up behavior, the coordinator
will just keep dispatching work until the trial timeout kills everything.

---

## Source References

| Source | Path | Key findings |
|--------|------|-------------|
| Adapter implementation | `terminal-bench/wg/adapter.py` | Trial timeout = 1800s, no agent time awareness |
| Condition G config | `adapter.py:127-137` | max_agents=8, heartbeat=30s, worktree_isolation=false |
| Config.toml builder | `adapter.py:761-790` | No agent_timeout, no coordinator_model wiring |
| Agent timeout resolution | `src/commands/spawn/execution.rs:420-446` | CLI > task > executor > coordinator (default 30m) |
| Timeout wrapper | `src/commands/spawn/execution.rs:449-456` | `timeout --signal=TERM --kill-after=30` |
| Heartbeat daemon loop | `src/commands/service/mod.rs:2806-2838` | `budget_secs: None` — no time budget passed |
| Heartbeat prompt | `src/commands/service/coordinator_agent.rs:388-418` | Template has budget field, but always shows ~0s |
| Kill chain | `src/service/mod.rs:36-75` | SIGTERM → wait → SIGKILL, no graceful save |
| Zero-output detection | `src/commands/service/zero_output.rs` | 5-min threshold, circuit breaker at 2 respawns |
| Experiment progress | `terminal-bench/docs/experiment-progress-report.md` | G run 4: 64% but "worked until timeout" |
| Condition G status | `terminal-bench/docs/research-condition-g-status.md` | Full history, prompt competition analysis |
| Heartbeat design | `terminal-bench/docs/design-tb-heartbeat-orchestration.md` | Phase 3 architecture, addresses oversight gap |
| TB timeout rules | `terminal-bench/docs/research-tb-timeout-scoring.md` | Per-task 900s-12000s, compliance issue with flat 30m |
| Agent turn budget | `terminal-bench/docs/research-tb-agent-turn-budget.md` | Claude CLI has no turn limit, only timeout |

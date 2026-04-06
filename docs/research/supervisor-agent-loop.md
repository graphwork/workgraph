# Research: Supervisor Agent Loop for Persistent Task Oversight

*"Go harder and go longer and actually get to the complete result more often."*

## Executive Summary

This document explores a **supervisory agent pattern** for workgraph: an oversight mechanism that monitors a working agent and prevents premature failure. The supervisor's job is to keep the worker on track, nudge it toward alternative approaches when stuck, and ensure tasks reach completion rather than being abandoned or half-finished.

We present three approaches in increasing order of complexity:

1. **Enhanced System Prompt (prompt-level)** — Inject self-reflection and persistence directives into existing agent prompts. Zero code changes.
2. **Watchdog Loop (coordinator-level)** — The coordinator periodically inspects running agents and sends mid-task nudges via the existing `wg msg` + state injection infrastructure. Moderate code changes.
3. **Paired Supervisor Agent (two-agent architecture)** — A dedicated supervisor agent spawned alongside each worker, with its own context and history. Significant code changes.

**Recommendation:** Start with approach #2 (Watchdog Loop). It delivers 80% of the value with 20% of the complexity, reuses existing infrastructure (state injection, messages, triage), and can be prototyped in ~3 days.

---

## 1. Problem Analysis

### 1.1 Why Agents Give Up Too Early

From observing workgraph agent behavior across thousands of tasks, the failure modes that lead to incomplete results are:

| Failure Mode | Frequency | Current Mitigation |
|---|---|---|
| **Premature `wg fail`**: Agent hits first error and gives up | High | `retry` resets to Open, but loses all context |
| **Premature `wg done`**: Agent marks done without validating | High | `--verify` gate catches some, but agent still thinks it's done |
| **Context exhaustion**: Agent fills context window and stops making progress | Medium | Emergency compaction exists, but agent doesn't adapt strategy |
| **Single-strategy tunnel vision**: Agent tries one approach repeatedly | Medium | Nothing — no external perspective to suggest alternatives |
| **Timeout**: Agent runs out of time mid-approach | Medium | Hard kill, then `retry` or `fail` — context lost |
| **Silent stall**: Agent loops on broken approach without recognizing it | Low | Zero-output detection (5min), but only for API-level hangs |

### 1.2 What Exists Today

The system already has several building blocks relevant to supervision:

| Component | Location | Relevance |
|---|---|---|
| **Mid-turn state injection** | `src/executor/native/state_injection.rs` | Injects messages, graph changes, context warnings mid-conversation. Already a "nudge" mechanism. |
| **Session summary** | `src/executor/native/agent.rs:256-278` | Periodic extraction of session summaries. History across restarts. |
| **Resume protocol** | `src/executor/native/resume.rs` | Journal-based conversation recovery. Supervisor could read this. |
| **`wg msg`** | `src/commands/msg.rs` | Cross-agent messaging. Worker and supervisor can communicate. |
| **Stream events** | `src/stream_event.rs` | Real-time observation of agent activity (text chunks, tool calls). |
| **Dead-agent triage** | `src/commands/service/triage.rs` | LLM-based assessment of failed agents. Already analyzes output. |
| **Stuck detection** | `src/commands/service/zero_output.rs`, `triage.rs` | Detects stalled agents (zero output, stale stream). |
| **Context pressure** | `src/executor/native/resume.rs:ContextBudget` | Tracks context usage, triggers compaction. |
| **Retry mechanism** | `src/commands/retry.rs` | Resets failed tasks, preserves retry_count. |
| **Cycle mechanism** | `src/graph.rs` (CycleConfig) | Structural iteration with `--max-iterations`. |
| **Previous attempt context** | `src/commands/spawn/context.rs:622+` | Injects prior agent's output into retry. Already "learns from failure." |
| **Checkpoint** | Task `checkpoint` field | Agent can save state for future recovery. |

---

## 2. Approach #1: Enhanced System Prompt (Prompt-Level Supervision)

### 2.1 Concept

Modify the agent's system prompt to include self-reflection directives — essentially making the agent its own supervisor. The prompt would instruct the agent to:

1. **Maintain a running plan** in its scratchpad/thinking
2. **Periodically self-assess** ("Am I making progress? Am I stuck on one approach?")
3. **Try alternative approaches** explicitly ("If approach A fails twice, pivot to approach B")
4. **Never give up prematurely** ("If you can't solve it one way, try at least 3 different approaches before `wg fail`")
5. **Use checkpoints** to save progress markers

### 2.2 Implementation

This requires only changes to prompt constants in `src/service/executor.rs`:

```rust
pub const PERSISTENCE_COACHING_SECTION: &str = "\
## Persistence & Self-Reflection

You are expected to persist through difficulty. Premature failure is the worst outcome.

### Before calling `wg fail`:
1. Have you tried at least 3 distinct approaches?
2. Have you re-read the error messages carefully?
3. Have you searched the codebase for similar patterns?
4. Have you simplified the problem (smaller test case, isolated reproduction)?
5. Log each approach you tried: `wg log {{task_id}} \"Approach N: <what> — <outcome>\"`

### Every 5 tool calls, ask yourself:
- Am I making progress toward the goal, or am I in a loop?
- Is there a simpler way to achieve this?
- Am I fighting the wrong problem?

### If stuck:
- `wg log {{task_id}} \"STUCK: <description of what's blocking me>\"`
- Try the opposite of what you've been doing
- Read the failing test/error one more time — slowly
- Ask: what would a senior engineer do here?
";
```

### 2.3 Pros

- **Zero infrastructure changes.** Modify one constant, rebuild.
- **Immediate deployment.** Works with both `claude` and `native` executors.
- **No coordination overhead.** No second agent, no message passing, no scheduling.
- **Composable with other approaches.** Add this first, layer supervision later.

### 2.4 Cons

- **Limited efficacy.** LLMs are notoriously bad at following meta-cognitive instructions consistently. Self-reflection prompts degrade as context grows.
- **No external perspective.** The agent can't see what it can't see. If it's stuck because of a conceptual misunderstanding, telling it to "try harder" won't help.
- **No history across restarts.** If the agent dies and is retried, the self-reflection state is lost (unless checkpointed).
- **Prompt bloat.** Every additional coaching section competes for context window space.
- **Can't force behavior.** The agent might ignore the instructions. There's no enforcement mechanism.

### 2.5 Verdict

**Good as a baseline layer.** Should be added regardless of which other approach we choose. But alone, it's insufficient for the "go harder, go longer" goal — it's asking the same model to be both the worker and the supervisor, which violates the fundamental insight that external oversight provides a different perspective.

---

## 3. Approach #2: Watchdog Loop (Coordinator-Level Supervision)

### 3.1 Concept

Extend the coordinator's tick loop to include a **periodic supervision phase** that inspects running agents and sends corrective nudges via the existing message/state-injection infrastructure.

The supervisor logic lives inside the coordinator daemon, not in a separate agent. Each tick, the coordinator:

1. **Scans running agents** for signs of struggle
2. **Reads recent stream events** to understand what the agent is doing
3. **Sends targeted nudges** via `wg msg` (delivered through state injection)
4. **Escalates** if nudges don't help (harder interventions)

### 3.2 Architecture

```
┌──────────────────────────────────────────────┐
│              Coordinator Daemon               │
│                                               │
│  ┌─────────────────┐   ┌──────────────────┐  │
│  │  Normal Tick     │   │  Supervision     │  │
│  │  (spawn/triage)  │   │  Phase           │  │
│  └─────────────────┘   └──────────────────┘  │
│                               │               │
│                     ┌─────────┼──────────┐    │
│                     ▼         ▼          ▼    │
│               ┌─────────┐ ┌────────┐ ┌──────┐│
│               │Agent A  │ │Agent B │ │Agent C││
│               │stream.  │ │stream. │ │stream.││
│               │jsonl    │ │jsonl   │ │jsonl  ││
│               └─────────┘ └────────┘ └──────┘│
└──────────────────────────────────────────────┘
```

### 3.3 Supervision Signals (What to Watch For)

| Signal | Detection Method | Intervention |
|---|---|---|
| **Repeated tool errors** | Parse stream.jsonl for consecutive `ToolResult{is_error: true}` | "You've had {N} consecutive tool errors. Step back and reconsider your approach." |
| **Long thinking, no action** | Gap between stream events >3min with no tool calls | "You seem to be deliberating. Try a small concrete step." |
| **Same file edited repeatedly** | Parse tool calls for repeated `Edit` on same path | "You've edited {file} {N} times. Consider whether the problem is elsewhere." |
| **Context pressure rising** | Read context budget state from stream | "You're at {X}% context. Wrap up the current approach or checkpoint your progress." |
| **Compile/test failures repeating** | Detect repeated `cargo build`/`cargo test` failures | "Build has failed {N} times with the same error. Try a different approach." |
| **No `wg log` entries** | Check task logs — no entries after start | "Please log your progress with `wg log`. What have you tried so far?" |
| **Approaching timeout** | Agent runtime vs configured timeout | "You have {X} minutes remaining. Consider checkpointing if not close to done." |

### 3.4 Intervention Ladder

The supervisor escalates through increasingly strong interventions:

```
Level 0: Observation only (log to daemon.log)
    ↓ (no progress after 5 min)
Level 1: Gentle nudge via wg msg
    "I notice you've been working on {X} for a while. How's it going?"
    ↓ (no progress after 10 min)
Level 2: Specific coaching via wg msg
    "Your last 3 attempts failed with {error}. Consider trying {alternative}."
    ↓ (no progress after 15 min)
Level 3: Strategy reset suggestion
    "You've been stuck for {N} minutes. Try: 1) Re-read the task description.
     2) Check what the tests expect. 3) Look for similar implementations in the codebase."
    ↓ (no progress after 25 min)
Level 4: Checkpoint and restart recommendation
    "Checkpoint your current progress with `wg log` and consider breaking this
     into smaller subtasks with `wg add`."
```

### 3.5 Implementation Plan

The implementation extends the existing coordinator tick loop:

**New file:** `src/commands/service/supervisor.rs`

```rust
/// Supervision state for a running agent
pub struct AgentSupervision {
    agent_id: String,
    task_id: String,
    /// Last time we checked this agent
    last_check: Instant,
    /// Current intervention level
    level: u8,
    /// Stream event cursor (offset into stream.jsonl)
    stream_cursor: u64,
    /// Recent tool call history (ring buffer)
    recent_tool_calls: VecDeque<ToolCallSummary>,
    /// Last N error messages
    recent_errors: VecDeque<String>,
    /// Time of last detected progress
    last_progress: Instant,
}

/// Summary of agent activity since last check
pub struct ActivitySummary {
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub text_output: usize,
    pub unique_files_touched: HashSet<String>,
    pub wg_commands: usize,
    pub elapsed_since_last_event: Duration,
}

/// Analyze agent stream and decide on intervention
pub fn supervise_agent(
    state: &mut AgentSupervision,
    dir: &Path,
) -> Option<SupervisionAction> {
    // 1. Read new stream events since last cursor
    // 2. Build ActivitySummary
    // 3. Detect stuck patterns
    // 4. Decide intervention level
    // 5. Return action (None, Nudge, Coach, Escalate)
}
```

**Integration point:** Add `supervise_running_agents()` call in `coordinator_tick()` after the existing triage phase:

```rust
// In coordinator.rs tick():
// ... existing cleanup, triage, spawn logic ...

// NEW: Supervision phase
supervisor::supervise_running_agents(dir, &graph_path, &mut supervision_state);
```

**Configuration:**

```toml
# .workgraph/config.toml
[supervisor]
enabled = true
check_interval = "2m"          # How often to check each agent
nudge_cooldown = "5m"          # Min time between nudges to same agent
max_level = 3                  # Max intervention level (0-4)
use_llm_for_coaching = false   # If true, use LLM to generate coaching messages
```

### 3.6 Key Design Decision: LLM vs. Heuristic Coaching

The supervision can operate in two modes:

**Heuristic mode (recommended for v1):** Pattern-match on stream events and send templated nudges. Fast, cheap, deterministic.

**LLM mode (v2):** Feed recent agent activity to a small model (Haiku) to generate targeted coaching. More expensive but can provide context-specific advice.

For v1, heuristic mode is sufficient. The intervention templates can be surprisingly effective because the nudges are about *metacognition* (think differently, try alternatives) rather than domain knowledge (how to fix the specific bug).

### 3.7 Pros

- **Reuses existing infrastructure.** State injection, `wg msg`, stream events, triage — all already exist.
- **External perspective.** The supervisor sees patterns the worker can't (repeated errors, time passing).
- **No extra agent cost.** Runs inside the coordinator, not as a separate LLM agent.
- **Gradual escalation.** Doesn't interfere with productive agents — only intervenes when needed.
- **Configurable.** Can be tuned or disabled per-project.
- **History survives restarts.** Supervision state is per-agent, persisted in the registry or a sidecar file.
- **No agent slot consumed.** Doesn't count against `max_agents`.
- **Complementary to retry.** When retry happens, previous supervision context informs the next attempt via previous-attempt-context.

### 3.8 Cons

- **Coordinator coupling.** Supervision logic lives in the coordinator — if the coordinator restarts, supervision state is lost (unless persisted).
- **Heuristic limits.** Pattern-matching can produce false positives (nudging an agent that's actually making progress on a slow compilation).
- **One-way communication.** The supervisor sends nudges, but the worker can't ask the supervisor for help (except indirectly via `wg msg` or task logs).
- **No deep understanding.** The heuristic supervisor doesn't truly understand what the agent is doing — it just detects surface patterns.

### 3.9 Interaction with Existing Mechanisms

| Mechanism | Interaction |
|---|---|
| **Retry** | Complementary. Supervision prevents premature failure. If the agent does fail, retry+previous-attempt-context captures what supervision learned. |
| **Cycles** | Complementary. Cycles handle structural iteration (do X N times). Supervision handles within-iteration persistence. |
| **Triage** | Extends triage. Current triage only runs after agent death. Supervision runs while agent is alive. |
| **Verify** | Complementary. Supervision nudges agents to validate before `wg done`. Verify gate catches what slips through. |
| **State injection** | Uses it. Nudges delivered via the same state injection mechanism that handles messages and graph changes. |
| **Context pressure** | Extends it. Current pressure system warns the agent. Supervision can also adapt strategy (suggest decomposition, checkpointing). |

---

## 4. Approach #3: Paired Supervisor Agent (Two-Agent Architecture)

### 4.1 Concept

Spawn a dedicated supervisor agent alongside each worker agent. The supervisor has its own context, its own LLM session, and its own history. It periodically wakes up, reads the worker's output, and sends coaching messages.

### 4.2 Architecture

```
┌─────────────────────────────────────────────┐
│            Coordinator Daemon                │
│                                              │
│  ┌──────────────────┐                        │
│  │ Spawn Worker      │──┐                    │
│  │ Spawn Supervisor  │  │                    │
│  └──────────────────┘  │                     │
│                         ▼                    │
│  ┌────────────────────────────────────────┐  │
│  │              Task: "implement-X"       │  │
│  │                                        │  │
│  │  ┌──────────┐      ┌───────────────┐   │  │
│  │  │ Worker   │◄─msg─│  Supervisor   │   │  │
│  │  │ Agent    │─────►│  Agent        │   │  │
│  │  │          │      │               │   │  │
│  │  │ Does the │      │ Reads stream  │   │  │
│  │  │ actual   │      │ Sends nudges  │   │  │
│  │  │ work     │      │ Coaches       │   │  │
│  │  └──────────┘      └───────────────┘   │  │
│  └────────────────────────────────────────┘  │
└─────────────────────────────────────────────┘
```

### 4.3 Supervisor Agent Design

The supervisor agent would:

1. **Be spawned by the coordinator** when a worker agent is created (or optionally, only for high-difficulty tasks)
2. **Run as a lightweight agent** using a smaller/cheaper model (Haiku or Sonnet)
3. **Operate on a periodic wake cycle**: sleep 2-5min, wake, check worker, act, sleep
4. **Read the worker's stream** (`stream.jsonl`) to understand progress
5. **Send coaching messages** via `wg msg` to the worker task
6. **Have its own journal** for conversation continuity across wake cycles
7. **Terminate** when the worker completes (done or fail)

**System prompt for supervisor:**

```
You are a supervisor for an AI agent working on task "{task_id}".

Your job is to help the worker agent succeed. You:
- Watch the worker's output and identify when it's stuck
- Send coaching messages with `wg msg send {task_id} "..."`
- Suggest alternative approaches when the current one isn't working
- Encourage persistence and thoroughness
- Never do the work yourself — only coach

You will be woken periodically. Each time:
1. Read the worker's recent output
2. Assess: Is the worker making progress? Stuck? About to give up?
3. If intervention needed, send a message
4. Log your assessment: `wg log {supervisor_task_id} "Assessment: ..."`

Worker's task: {task_description}
Worker's recent output: {stream_tail}
Previous supervision notes: {supervisor_history}
```

### 4.4 Implementation Sketch

**New task type or tag:** Supervisor tasks linked to their worker:

```bash
# Internally, coordinator creates:
wg add ".supervisor-{task_id}" --tags "supervisor" \
  -d "Supervise worker on task {task_id}" \
  --exec "wg supervisor-loop {task_id}"
```

**New command:** `wg supervisor-loop <task-id>` — a polling loop that:

```rust
fn supervisor_loop(task_id: &str, interval: Duration, model: &str) -> Result<()> {
    loop {
        // 1. Check if worker is still alive
        if !is_worker_alive(task_id) { break; }

        // 2. Read worker's stream.jsonl (tail)
        let activity = read_worker_activity(task_id)?;

        // 3. Read our own previous assessments
        let history = load_supervisor_history()?;

        // 4. Build prompt with activity + history
        let prompt = build_supervisor_prompt(task_id, &activity, &history);

        // 5. Call LLM (cheap model)
        let response = call_llm(model, &prompt)?;

        // 6. Execute any wg msg commands in the response
        execute_supervisor_actions(&response)?;

        // 7. Save assessment to our history
        save_assessment(&response)?;

        // 8. Sleep until next check
        sleep(interval);
    }
}
```

### 4.5 Pros

- **Full external perspective.** The supervisor is a separate LLM session with its own context — it can see the forest when the worker is lost in the trees.
- **Context-specific coaching.** Unlike heuristic nudges, the supervisor LLM can understand what the worker is doing and give targeted advice.
- **History across interventions.** The supervisor maintains its own journal — it remembers what it told the worker before and whether it helped.
- **Decoupled from coordinator.** Runs as its own process; coordinator doesn't need supervision logic.
- **Can ask questions.** Via `wg msg`, the supervisor can ask the worker what's happening, and the worker can respond (through state injection).

### 4.6 Cons

- **Cost.** Each supervised task now runs two LLM agents. Even with a cheap model, this roughly doubles per-task cost (or more, since the supervisor runs for the full duration).
- **Agent slot consumed.** Each supervisor counts against `max_agents`, reducing parallelism.
- **Complexity.** Two-agent coordination introduces new failure modes: supervisor crash, message ordering, supervisor-worker disagreement.
- **Latency.** Messages travel through the graph (write to disk, state injection detects on next turn), introducing delays.
- **Diminishing returns.** The supervisor can coach, but it can't fix the fundamental capability limitations of the worker model. Coaching a confused Haiku agent with a Haiku supervisor may not help much.
- **Lifecycle management.** Need to handle: supervisor outliving worker, worker outliving supervisor, both crashing, supervisor's own context exhaustion.

### 4.7 Verdict

**Powerful but expensive.** Best reserved for high-value tasks where the cost of failure exceeds the cost of a second agent. Could be opt-in via a `--supervised` flag or triggered by the coordinator when a task has failed N times.

---

## 5. Comparison Matrix

| Dimension | Prompt Enhancement | Watchdog Loop | Paired Supervisor |
|---|---|---|---|
| **Implementation effort** | ~1 hour | ~3 days | ~2 weeks |
| **Code changes** | 1 file (prompt constant) | 2-3 files (coordinator, new module) | 5+ files (new command, coordinator, spawn, registry) |
| **Runtime cost** | Zero | Minimal (heuristic checks in coordinator tick) | High (2x LLM agents) |
| **Effectiveness for tunnel vision** | Low | Medium (detects patterns, can't understand them) | High (LLM understands context) |
| **Effectiveness for premature failure** | Medium (instructions may be followed) | High (external enforcement) | High (active coaching) |
| **History across restarts** | None (unless checkpointed) | Partial (supervision state in registry) | Full (supervisor journal) |
| **Executor compatibility** | All (claude, native, shell) | All (coordinator-level) | Native + claude only |
| **Risk of interference** | None | Low (nudges are suggestions) | Medium (bad advice, message storms) |
| **Configuration** | None needed | Simple TOML | Complex (model, interval, prompt) |

---

## 6. Recommendation: Phased Implementation

### Phase 1: Prompt Enhancement + Watchdog Loop (immediate)

1. **Add `PERSISTENCE_COACHING_SECTION`** to the prompt template. This is free and composable. (~1 hour)

2. **Implement heuristic watchdog** in the coordinator. Start with three signals:
   - Repeated tool errors (>5 consecutive)
   - Stream staleness (>5 min with no events)
   - Approaching timeout (>75% of time budget)
   
   Interventions are templated nudge messages via `wg msg`. (~3 days)

3. **Add supervision metrics** to the TUI: intervention count, intervention level, last nudge time. (~1 day)

### Phase 2: LLM-Enhanced Watchdog (after observing Phase 1 data)

4. **Add LLM coaching mode** to the watchdog. When heuristic signals fire, optionally run the agent's recent stream through a cheap model to generate targeted coaching. (~2 days)

5. **Calibrate** based on Phase 1 data: which signals had false positives? Which nudges helped? Tune thresholds. (~ongoing)

### Phase 3: Paired Supervisor (if Phase 2 insufficient)

6. **Implement paired supervisor** as opt-in for high-value or high-failure-rate tasks. Triggered when:
   - Task has `--supervised` flag
   - Task has failed >2 times (automatic escalation)
   - Task is tagged with `high-value` or `critical`

### Phase 4: Adaptive Supervision (stretch)

7. **Evolve supervision strategies** using the existing evaluation/evolution pipeline. Supervision approaches that lead to better task outcomes are reinforced; those that don't are retired.

---

## 7. Integration Points with Existing Code

### 7.1 For Watchdog Loop (Phase 1-2)

| Integration Point | File | What Changes |
|---|---|---|
| **Coordinator tick** | `src/commands/service/coordinator.rs` | Add `supervise_running_agents()` call after triage phase |
| **New supervision module** | `src/commands/service/supervisor.rs` | New file: supervision state, signal detection, intervention logic |
| **Stream event parsing** | `src/stream_event.rs` | Already supports reading events — use `read_stream_events()` with cursor |
| **Message sending** | `src/commands/msg.rs` | Already supports `wg msg send` — supervisor calls this |
| **State injection** | `src/executor/native/state_injection.rs` | Already delivers `wg msg` to agents mid-turn — no changes needed |
| **Config** | `src/config.rs` | Add `[supervisor]` section |
| **Registry** | `src/service/registry.rs` | Add supervision metadata to `AgentEntry` |
| **Prompt template** | `src/service/executor.rs` | Add `PERSISTENCE_COACHING_SECTION` |

### 7.2 For Paired Supervisor (Phase 3)

| Integration Point | File | What Changes |
|---|---|---|
| **Spawn** | `src/commands/spawn/execution.rs` | Optionally spawn supervisor alongside worker |
| **Coordinator agent slot tracking** | `src/commands/service/coordinator.rs` | Supervisor agents tracked separately (don't count toward max_agents) |
| **New command** | `src/commands/supervisor_loop.rs` | New subcommand: `wg supervisor-loop <task-id>` |
| **Task lifecycle** | `src/commands/done.rs`, `src/commands/fail.rs` | When worker completes, signal supervisor to terminate |
| **Registry** | `src/service/registry.rs` | Link supervisor agent to worker agent |

---

## 8. Comparison with External Systems

### AutoGPT Self-Reflection
AutoGPT added a "critic" step after each action: the model reviews its own output and suggests improvements. This is essentially Approach #1 (prompt-level) with structured output. Studies showed mixed results — the critic often agreed with the original plan.

### Devin's Planning Agent
Devin uses a separate planning model that maintains a high-level plan and delegates execution. The planner periodically reviews progress. This is closest to Approach #3 (paired supervisor), but with a clearer separation: the planner never sees raw code, only summaries.

### OpenHands (formerly OpenDevin) — AgentController
OpenHands has a `AgentController` that manages agent lifecycle with configurable "stuck detection." When the agent repeats the same action too many times, the controller intervenes. This is closest to Approach #2 (watchdog).

### Claude Code's Native Loop
Claude Code's agent loop (which workgraph's native executor is based on) already has context pressure management and emergency compaction. The state injection mechanism (messages, graph changes) is already a form of mid-turn supervision. The watchdog approach (#2) is a natural extension of this.

### Key Takeaway
No major system has a paired-supervisor architecture as standard. Most rely on either self-reflection (prompt) or controller-level heuristics (watchdog). The watchdog pattern is the industry standard because it's cheap and effective at the patterns that matter most (repeated failures, stalls, timeout management).

---

## 9. Open Questions

1. **Should supervision be per-task or per-agent?** Currently scoped per-agent (one supervision state per running agent). But if an agent is retried, should the supervision state carry over? (Probably yes, via the task.)

2. **What about the `claude` executor (CLI)?** State injection only works with the native executor. For Claude CLI agents, the only intervention channel is `wg msg`, which the agent must explicitly check. Supervision nudges would be less timely.

3. **How to prevent supervision storms?** If many agents are stuck simultaneously, the supervisor phase could generate many LLM calls (in LLM mode). Rate limiting per tick is needed.

4. **Should supervision affect evaluation scores?** If an agent needed heavy supervision to complete, should that lower its evaluation score? (Probably yes — "needed coaching" is a useful signal for the evolution system.)

5. **How to detect false-positive stuck signals?** Long compilations, large test suites, and complex reasoning all look like "stalls" to stream-event heuristics. Need calibration data from real runs.

# Coordinator as Regular Looping Agent with Context Compaction

## Status: Design (March 2026)

## Summary

The current architecture treats the coordinator as a special entity — a persistent Claude CLI session spawned at daemon startup, with dedicated crash recovery, event logging, and response capture machinery. This design proposes an alternative: **the coordinator is just a regular agent that happens to run continuously**, and context compaction becomes an evaluatable task in the graph.

## Motivation

### What's Special Today (and Shouldn't Be)

The current coordinator agent (`src/commands/service/coordinator_agent.rs`, ~973 lines) has its own:

- **Process management** — spawns Claude CLI, tracks PID, monitors health
- **Context injection** — `build_coordinator_context()` with event log drain
- **Crash recovery** — time-windowed restarts, conversation summary injection, history rotation
- **Response capture** — `stdout_reader` thread, `ResponseEvent` channel, `collect_response()`
- **System prompt** — hardcoded in `build_system_prompt()`

Meanwhile, the regular agent spawn path (`src/commands/spawn/execution.rs` + `src/service/executor.rs`) already handles process management, prompt assembly, context scoping, output capture, and crash detection for every task agent. The coordinator duplicates much of this infrastructure.

### The Insight

If the coordinator is "just an agent", then:

1. **Context compaction is a task**, not an implicit crash-recovery mechanism
2. **Each coordinator era is evaluatable** — was the summary good? Did context get lost?
3. **The coordinator uses the same executor path** as every other agent
4. **New executor types (native, amplifier) automatically work** for the coordinator
5. **The coordinator's lifecycle is visible in the graph** — it's not a hidden daemon detail

## Architecture

### Lifecycle Diagram

```
                    ┌─────────────────────────────────┐
                    │         Service Daemon            │
                    │  (Rust, no LLM, just dispatch)    │
                    └──────────┬──────────────────────┘
                               │
                    ┌──────────▼──────────────────────┐
                    │  "Boot" coordinator task          │
                    │  (auto-created on service start)  │
                    │  Status: open → in-progress       │
                    └──────────┬──────────────────────┘
                               │
              ┌────────────────▼────────────────────────┐
              │          Coordinator Agent (Era 1)       │
              │  Spawned by normal executor path.        │
              │  Same prompt assembly, same output log.  │
              │                                          │
              │  Loop:                                   │
              │    1. Read chat inbox                     │
              │    2. Read graph state                    │
              │    3. Respond to user / dispatch tasks    │
              │    4. Check token budget                  │
              │                                          │
              │  Exit conditions:                         │
              │    a. Context approaching limit           │
              │    b. Explicit compaction trigger          │
              │    c. Crash (process dies)                │
              └────────────────┬────────────────────────┘
                               │ (exits)
              ┌────────────────▼────────────────────────┐
              │   Daemon detects agent finished.          │
              │   Triage: coordinator task is "special"   │
              │   → Spawn compaction task automatically.  │
              └────────────────┬────────────────────────┘
                               │
              ┌────────────────▼────────────────────────┐
              │       Compaction Task (Era 1 → Era 2)    │
              │                                          │
              │  Input:                                   │
              │    - Era 1 conversation history           │
              │    - Era 1 output log                     │
              │    - Current graph state                  │
              │    - Era 1 session summary (if any)       │
              │                                          │
              │  Output:                                  │
              │    - Compacted session summary             │
              │    - Key decisions preserved               │
              │    - User preferences carried forward      │
              │    - Any pending user messages identified  │
              │                                          │
              │  Evaluation criteria (auto-eval):          │
              │    - Were all active conversations noted?  │
              │    - Were key decisions preserved?          │
              │    - Is the summary concise (<2000 tokens)?│
              │    - Were pending actions identified?       │
              └────────────────┬────────────────────────┘
                               │ (completes)
              ┌────────────────▼────────────────────────┐
              │     Coordinator Agent (Era 2)             │
              │  Spawned with compacted summary as seed.  │
              │  Picks up where Era 1 left off.           │
              │  Processes any pending inbox messages.     │
              │                                          │
              │  (cycle continues...)                     │
              └─────────────────────────────────────────┘
```

### How the Service Knows to Boot the Coordinator

**Mechanism: Coordinator task with `role: coordinator` and `loop: true`.**

On `wg service start`, the daemon checks for a task with `tag: coordinator-loop`. If none exists, it creates one:

```
wg add "Coordinator" \
  --tag coordinator-loop \
  --tag auto-restart \
  -d "Persistent coordinator agent. Reads chat inbox, manages graph, responds to user."
```

The coordinator task is special in one way: **when it completes or fails, the daemon automatically restarts it** (after optional compaction). This is indicated by the `auto-restart` tag, which the triage system recognizes.

This is a minimal extension. The task is visible in the graph, has logs, has artifacts, and can be inspected with `wg show`. But it never "finishes" — it cycles through eras.

### Coordinator Task vs Regular Task

| Property | Regular Task | Coordinator Task |
|----------|-------------|-----------------|
| Spawned by | Coordinator tick (ready task dispatch) | Service daemon startup |
| Completes when | Agent calls `wg done` | Context limit / explicit compact / crash |
| After completion | Triage marks done, moves on | Triage spawns compaction task, then restarts |
| Visible in graph | Yes | Yes |
| Has logs/artifacts | Yes | Yes |
| Uses executor | Yes (same executor registry) | Yes (same executor registry) |
| Has prompt | Yes (assembled by `build_prompt`) | Yes (custom coordinator prompt via role/bundle) |
| Evaluatable | Yes (auto-eval by agency system) | Yes (each era is evaluatable) |

## Context Compaction as a Task

### Compaction Task Spec

**Title**: `coordinator-compact-era-{N}`

**Description**: Summarize Era N of the coordinator session and produce a seed context for Era N+1.

**Input (provided as task context/artifacts)**:

1. **Conversation history** — The chat inbox and outbox messages from Era N (timestamped, with request IDs)
2. **Output log** — The coordinator agent's full output log from Era N (tool calls, responses, reasoning)
3. **Graph state snapshot** — Current task statuses, active agents, recent completions
4. **Previous session summary** — The compacted summary from Era N-1 (if this isn't the first era)
5. **Era metadata** — Start time, end time, token usage, number of interactions, exit reason

**Output (written as artifact)**:

A structured session summary written to `.workgraph/coordinator/era-{N}-summary.md`:

```markdown
## Session Summary (Era N)

### Duration
Started: 2026-03-02T10:00:00Z
Ended: 2026-03-02T14:30:00Z
Interactions: 47
Exit reason: context_limit (estimated 185k/200k tokens)

### Active Conversations
- User is working on authentication system. Last question: "should we use JWT or sessions?"
  Decision pending — user hasn't responded to our recommendation of JWT.
- User asked about performance of the data pipeline. We suggested profiling. No follow-up yet.

### Key Decisions Made
- Authentication: JWT over sessions (user preference, 2026-03-02T11:15)
- Rate limiting: Sliding window algorithm (user preference, 2026-03-02T12:30)
- Test coverage target: 80% (user stated, 2026-03-02T13:00)

### Tasks Created This Era
- auth-research (done) → auth-impl (in-progress) → auth-test (open) → auth-integrate (open)
- rate-limit-design (in-progress)
- perf-profile (open)

### User Preferences Observed
- Prefers small, focused tasks over large batches
- Wants status updates after each completion wave
- Likes seeing task IDs in responses

### Pending Actions
- Respond to follow-up on JWT decision when user messages next
- Check on auth-impl agent (running 2h, may need intervention)
- User mentioned wanting to discuss deployment strategy "later"

### Unresolved Issues
- rate-limit-design agent failed once, was retried — monitor
- perf-profile task depends on auth completing first
```

**Exec mode**: `light` (read-only — the compaction agent reads history but doesn't modify the graph)

**Evaluation criteria**: See Section 4.

### Compaction Task Creation

When the coordinator agent exits (for any reason), the daemon triage system:

1. Detects the coordinator task agent has finished
2. Checks the `auto-restart` tag
3. Creates the compaction task:

```rust
// In triage, after detecting coordinator agent exit:
fn handle_coordinator_exit(dir: &Path, era: u32, exit_reason: &str) -> Result<()> {
    let compact_id = format!("coordinator-compact-era-{}", era);

    // Save era metadata as artifact
    let meta_path = format!(".workgraph/coordinator/era-{}-meta.json", era);
    save_era_metadata(dir, era, exit_reason, &meta_path)?;

    // Create compaction task
    wg_add(dir, &compact_id, &format!(
        "Summarize coordinator Era {} and produce seed context for Era {}.\n\n\
         Read the era artifacts and produce a session summary.\n\n\
         Input artifacts:\n\
         - .workgraph/coordinator/era-{}-meta.json\n\
         - .workgraph/chat/inbox.jsonl (messages during this era)\n\
         - .workgraph/chat/outbox.jsonl (responses during this era)\n\
         - .workgraph/agents/<coordinator-agent-id>/output.log\n\n\
         Output: .workgraph/coordinator/era-{}-summary.md\n\n\
         Previous summary: .workgraph/coordinator/era-{}-summary.md",
        era, era + 1, era, era, era.saturating_sub(1)
    ))?;

    // The compaction task uses light exec mode (read-only)
    wg_edit(dir, &compact_id, "exec_mode", "light")?;

    // After compaction completes, restart the coordinator
    // (handled by triage watching for compact task completion)
    Ok(())
}
```

### Era Tracking

Eras are tracked in a simple metadata file:

```
.workgraph/coordinator/
├── current-era.txt          # Just the number: "3"
├── era-1-summary.md         # Compacted summary from Era 1
├── era-1-meta.json          # Metadata: start/end times, tokens, exit reason
├── era-2-summary.md
├── era-2-meta.json
├── era-3-summary.md         # Latest compaction
└── era-3-meta.json
```

The coordinator agent's prompt includes: "You are in Era {N}. Previous session summary: {content of era-{N-1}-summary.md}."

## Evaluation Criteria for Compaction

Each compaction task is auto-evaluated by the agency system. Evaluation criteria:

### Completeness (40%)

- **Active conversations preserved**: Does the summary mention all conversations that were in-progress when the era ended? (Compare against last 5 inbox messages and their outbox responses.)
- **Pending actions captured**: Does the summary list actions the coordinator was tracking? (Check against tasks created/monitored during the era.)
- **Key decisions recorded**: Are user-stated preferences and decisions present? (Look for explicit "I prefer X" or "let's go with Y" in the inbox.)

### Conciseness (20%)

- **Token budget**: Is the summary under 2000 tokens? (Measured by approximate token count.)
- **Signal-to-noise**: Does every section contain actionable information? (Empty sections are fine; verbose filler is not.)

### Accuracy (30%)

- **No hallucinated decisions**: Every decision attributed to the user should trace back to an inbox message. (Cross-reference decision timestamps with inbox.)
- **Task status correctness**: Tasks mentioned in the summary should have the correct status in the graph.
- **No lost context**: Compare the compacted summary against the previous era's summary — are items from the previous summary either still present or explicitly resolved?

### Continuity (10%)

- **Era N+1 bootstrap test**: When the next coordinator era starts, does it correctly reference the summary? (Tested by checking the first response of Era N+1 for coherent continuation.)

### Evaluation Implementation

The evaluation can be automated:

```
Task: evaluate-coordinator-compact-era-{N}
Input: era-{N}-summary.md, inbox.jsonl, outbox.jsonl, era-{N}-meta.json
Output: evaluation score + feedback

Evaluation agent reads the summary and the raw history, checks the criteria above,
and produces a score. If the score is below threshold, the compaction task can be
retried with feedback.
```

This fits naturally into the existing agency evaluation system — no new infrastructure needed.

## Coordinator Identity Across Compactions

### Session Identity

The coordinator maintains identity through:

1. **Era numbering** — monotonically increasing, stored in `current-era.txt`
2. **Cumulative summary** — each era's summary builds on the previous one
3. **Chat history** — inbox/outbox files are continuous across eras (they don't reset)
4. **Coordinator task ID** — remains the same across eras (the task is restarted, not recreated)

### What Changes Between Eras

| Persists | Resets |
|----------|--------|
| Chat inbox/outbox (full history) | LLM conversation context |
| Coordinator task ID and logs | Agent process and PID |
| Cumulative session summaries | In-flight response state |
| Graph state | Event log (drained into summary) |
| User preferences (in summary) | Working memory |

### What the Era N+1 Coordinator Sees

When Era N+1 boots, its prompt includes:

```
You are the workgraph coordinator resuming after context compaction (Era {N+1}).

## Previous Session Summary
{content of era-{N}-summary.md}

## Current Graph State
{live context injection — same as today's build_coordinator_context()}

## Unread Messages
{any inbox messages that arrived during compaction}

Resume your role. Process any pending messages, then wait for new ones.
```

This is very similar to the existing crash recovery context (`build_crash_recovery_summary` in coordinator_agent.rs), but structured and evaluatable rather than ad-hoc.

## Model Configuration

### Should the Coordinator Be Opus Always?

**Recommendation: Configurable, defaulting to the project's configured model.**

The coordinator's job — interpreting user intent, decomposing tasks, monitoring agents — is a good fit for a capable model. But mandating Opus is unnecessarily rigid:

- Some projects are simple enough that Sonnet handles coordination well
- Cost sensitivity varies — Opus is 5x more expensive than Sonnet
- The native executor (Phase 4) makes model switching trivial

**Configuration hierarchy** (same as regular agents):

```
coordinator_model (explicit) > wg config --model > executor default > claude-sonnet-4-latest
```

Add a `coordinator_model` field to config.toml:

```toml
[coordinator]
model = "claude-opus-4-latest"    # Optional override for coordinator specifically
max_agents = 4
poll_interval = 60
```

If unset, falls back to the global model setting.

### Prompt Caching

The Anthropic API supports prompt caching via `cache_control` markers. For the coordinator:

- **System prompt** (~2000 tokens) — cached. Same across all interactions within an era.
- **Session summary** (~500-2000 tokens) — cached. Same within an era.
- **Context injection** (~300-600 tokens) — NOT cached. Changes every interaction.
- **Conversation history** — partially cached. Earlier turns are stable; only the latest messages change.

With prompt caching:
- First interaction in an era: full cost for system prompt + summary
- Subsequent interactions: 90% cache hit on system prompt + summary + older conversation turns
- Cost reduction: ~60-80% on input tokens for a long-running coordinator session

**Implementation with native executor:**

```rust
let request = MessagesRequest {
    system: vec![
        SystemBlock {
            text: system_prompt,
            cache_control: Some(CacheControl::Ephemeral),  // Cache this block
        },
        SystemBlock {
            text: session_summary,
            cache_control: Some(CacheControl::Ephemeral),
        },
    ],
    messages: vec![
        // Older messages get cache_control on the last content block
        // of each assistant turn (Anthropic's caching requirement)
    ],
    // ...
};
```

**Implementation with Claude CLI (v1):**

The Claude CLI handles caching internally — no explicit configuration needed. The CLI's stream-json mode maintains context across turns, and the API client inside the CLI applies caching automatically for the system prompt.

## Interaction with Chat Protocol (Phase 1)

### Current Chat Protocol

The Phase 1 chat protocol (`docs/design/coordinator-chat-protocol.md`) specifies:

- **IPC**: `UserChat { message, request_id }` → daemon → coordinator
- **Storage**: `inbox.jsonl` / `outbox.jsonl` with cursor tracking
- **Wake-up**: `urgent_wake` flag bypasses settling delay

### Changes for Regular-Agent Coordinator

The chat protocol **stays the same for the user-facing side**. The `wg chat` command, IPC types, inbox/outbox storage, and TUI integration all work identically.

What changes is the **daemon-side handling**:

**Current (special entity):**
```
UserChat IPC → daemon writes to inbox → daemon injects into coordinator stdin → coordinator responds → daemon writes to outbox
```

**Proposed (regular agent):**
```
UserChat IPC → daemon writes to inbox → daemon sets urgent_wake →
coordinator tick → coordinator agent reads inbox (via wg msg or direct file read) → coordinator responds by writing to outbox (via wg tool or direct write)
```

The key difference: the coordinator agent reads its own inbox and writes its own outbox, using the standard agent tool set. It's not a daemon subprocess with piped stdin/stdout — it's an agent with file-based I/O.

### The Coordinator Agent's Input Loop

Instead of receiving messages via stream-json stdin injection, the coordinator agent runs its own polling loop:

```
System prompt tells the agent:
"You are a persistent coordinator. Check for new messages by reading
.workgraph/chat/inbox.jsonl. When you find unread messages (ID > your
last-read cursor), process them and write responses to
.workgraph/chat/outbox.jsonl. Between messages, check graph state and
perform any needed coordination."

The agent's tool loop:
1. read_file(".workgraph/chat/inbox.jsonl") → check for new messages
2. If new messages: process each, call wg tools, write response
3. If no new messages: check graph state, monitor agents
4. Sleep briefly (agent decides timing, or daemon injects a "check now" signal)
5. Repeat
```

**But wait — how does the agent know to check for messages?**

Option A: **Agent polls in a loop.** The coordinator agent is instructed to periodically read the inbox. This works but is wasteful — the agent burns tokens reading "no new messages" over and over.

Option B: **Daemon injects a wake-up message.** When a chat message arrives, the daemon writes a special signal file (`.workgraph/chat/.wake`) that the agent checks cheaply. Or the daemon sends a message to the coordinator task's message queue (`wg msg send coordinator "new chat message"`).

Option C: **Agent runs in single-turn mode, daemon re-invokes.** The coordinator agent processes one batch of messages and exits. The daemon re-invokes it when new messages arrive or on a timer. Each invocation is a single turn with the compacted context.

**Recommendation: Option C (single-turn with daemon re-invocation).**

This is the cleanest mapping to the regular agent model. The coordinator agent:
- Is spawned when there's work to do (new chat messages, or periodic coordination check)
- Processes the current state (inbox, graph, agents)
- Responds and exits
- Is re-spawned on the next event

This avoids the persistent-session complexity entirely. Each invocation is independent but seeded with the compacted context from previous invocations.

**But doesn't this lose conversational continuity?**

No — because:
1. The session summary carries forward key decisions, preferences, and context
2. The chat history (inbox/outbox) is always available for reference
3. The graph state is always live
4. Prompt caching makes re-reading the system prompt + summary cheap

**Trade-off**: Slightly higher latency per interaction (no warm session to inject into) vs. dramatically simpler architecture (no persistent subprocess management, no crash recovery, no event channels).

### Revised Architecture Diagram

```
┌─────────────────────────────────────────────────────┐
│                    Service Daemon                     │
│  (Rust-only, no LLM subprocess)                      │
│                                                       │
│  Tick loop:                                           │
│    1. Check chat inbox for new messages                │
│    2. Check graph for ready tasks                      │
│    3. If coordinator work needed:                      │
│       → Spawn coordinator agent (regular executor)     │
│    4. If task work needed:                             │
│       → Spawn task agents (regular executor)           │
│    5. Triage finished agents                           │
│                                                       │
│  Coordinator detection:                                │
│    - New inbox message AND no coordinator agent running │
│    - Periodic check (every 60s) AND no coordinator     │
│      agent running                                     │
│    - Graph state changed AND needs coordinator action   │
└─────────────────────────────────────────────────────┘
```

## Handling In-Flight Conversations During Compaction

### The Problem

When the coordinator agent exits for compaction, there may be:
1. A user message just sent (in the inbox, not yet processed)
2. A user waiting for a response to a recent message
3. An ongoing multi-turn conversation

### The Solution

**Compaction is fast (10-30 seconds).** The compaction agent reads history, writes a summary, and exits. During this window:

1. **New messages queue in the inbox.** The inbox is append-only. Messages sent during compaction are stored normally.
2. **The user sees a brief pause.** If they're waiting for a response, there's a 10-30 second delay. This is acceptable — it's similar to the delay when a model is "thinking."
3. **Era N+1 picks up all pending messages.** When the new coordinator era starts, it reads the inbox and finds any messages that haven't been responded to.

**Optional: Notify the user during compaction.**

The daemon can write a synthetic outbox message:

```json
{"id":99,"timestamp":"...","role":"system","content":"Coordinator is performing context compaction. Resuming in a moment...","request_id":"system-compact-era-3"}
```

The TUI/CLI displays this as a system message, so the user knows what's happening.

### Compaction Timing

When should compaction trigger?

1. **Token budget threshold** — The coordinator agent tracks approximate token usage. When it estimates context is at 80% capacity, it exits cleanly with reason `context_limit`.

2. **Interaction count** — After N interactions (e.g., 50), compact regardless of token usage. This prevents slow context degradation.

3. **Time-based** — After T hours (e.g., 4h), compact. Long sessions accumulate stale context.

4. **Explicit trigger** — User can say "compact now" or the daemon can send a compaction signal.

The coordinator agent's prompt includes: "When you estimate your context is approaching 80% capacity, or after 50 interactions, end your response with `[COMPACT]` to signal that you need context compaction. The daemon will handle the rest."

## Migration Path from Special-Entity Approach

### Phase A: Extract Coordinator Prompt into Role/Bundle (Low Risk)

Move the hardcoded system prompt from `build_system_prompt()` into the agency system:

```yaml
# .workgraph/agency/roles/coordinator.yaml
name: coordinator
description: "Persistent coordinator agent that manages the task graph"
system_prompt: |
  You are the workgraph coordinator...
  (current content of build_system_prompt())
default_exec_mode: full
default_bundle: coordinator
```

```toml
# .workgraph/bundles/coordinator.toml
[bundle]
name = "coordinator"
description = "Coordinator agent — inspect + create, no implement"
tools = ["bash"]  # or typed tools in v2
tool_filter = "wg:*"
context_scope = "graph"
```

**No behavioral change.** The coordinator still runs as a special entity, but its configuration comes from the agency system.

### Phase B: Add Era Tracking and Compaction Task (Medium Risk)

1. Add `current-era.txt` and era metadata tracking
2. When the coordinator crashes, create a compaction task instead of doing inline recovery
3. The compaction task produces `era-{N}-summary.md`
4. Crash recovery uses the compaction output instead of `build_crash_recovery_summary()`

**Behavioral change:** Crash recovery now goes through a compaction task. This adds 10-30 seconds to restart but produces an evaluatable artifact.

### Phase C: Switch to Regular Agent Spawn Path (Higher Risk)

1. Remove the `CoordinatorAgent` struct and `agent_thread_main()` function
2. The coordinator task is spawned via the normal executor path
3. The coordinator agent reads inbox directly instead of stdin injection
4. The daemon detects coordinator exit and triggers compaction → restart

**Behavioral change:** The coordinator is no longer a persistent subprocess. It's a series of agent invocations with compacted context between them.

### Phase D: Single-Turn Coordinator (Optional, Highest Simplification)

1. Each coordinator invocation processes one batch of messages and exits
2. The daemon re-invokes on each new message or periodic timer
3. No long-running coordinator session at all
4. Context is always: system prompt + session summary + current graph state + current inbox

**Behavioral change:** No persistent session. Each interaction is independent. Maximum simplicity at the cost of some conversational nuance.

### Risk Mitigation

- **Feature flag**: `coordinator_mode = "special" | "regular" | "single-turn"` in config.toml
- **Gradual rollout**: Run both modes in parallel during testing, compare response quality
- **Rollback**: Config change, no code deployment needed

## Code Changes Summary

### Files to Remove (Phase C)

| File | Lines | Reason |
|------|-------|--------|
| Most of `src/commands/service/coordinator_agent.rs` | ~800 | Replaced by regular agent spawn path |

### Files to Modify

| File | Change |
|------|--------|
| `src/commands/service/coordinator.rs` | Add coordinator task detection, compaction trigger, era tracking |
| `src/commands/service/mod.rs` | Remove CoordinatorAgent spawning, add coordinator-as-task logic |
| `src/commands/service/triage.rs` | Add auto-restart detection for coordinator task |
| `src/service/executor.rs` | Add coordinator role/bundle resolution |
| `src/config.rs` | Add `coordinator_mode` and `coordinator_model` config fields |

### Files to Create

| File | Purpose |
|------|---------|
| `src/commands/service/compaction.rs` | Compaction task creation and era management |
| `.workgraph/agency/roles/coordinator.yaml` | Coordinator role definition |
| `.workgraph/bundles/coordinator.toml` | Coordinator tool bundle |

### Net Effect

- **Remove** ~800 lines of special-entity coordinator code
- **Add** ~200 lines of compaction/era management
- **Add** ~50 lines of config for role/bundle
- **Net reduction**: ~550 lines, with better separation of concerns

## Open Questions

1. **Should compaction be mandatory or optional?** For small projects with few interactions, compaction overhead may not be worth it. Could allow `coordinator_compact = false` to disable it.

2. **How many eras to retain?** Keeping all era summaries is cheap (they're small), but the coordinator only needs the most recent one. Retention policy: keep last 5 eras, garbage-collect older ones.

3. **What if compaction fails?** The compaction agent could fail (API error, bad summary). Fallback: use the crash recovery approach (inline summary from history) and retry compaction later.

4. **Should the compaction task block the coordinator restart?** Currently proposed as sequential (compact → restart). Alternative: start the new coordinator immediately with a simpler summary, run compaction in parallel, and inject the full summary on next interaction.

5. **How does this interact with the native executor?** The native executor runs agents in-process via `tokio::spawn`. For the coordinator, this means no subprocess at all — the coordinator is a Rust task within the daemon process. Compaction and era management work identically regardless of executor type.

## Appendix: Comparison of Approaches

| Dimension | Special Entity (Current) | Regular Agent (Proposed) | Single-Turn (Phase D) |
|-----------|------------------------|------------------------|---------------------|
| Code complexity | High (dedicated ~973 lines) | Medium (reuses executor path) | Low (minimal coordinator code) |
| Conversational quality | Best (persistent session) | Good (summary carries context) | Adequate (summary + history) |
| Crash recovery | Custom (inline summary) | Standard (compaction task) | N/A (no persistent state) |
| Evaluability | None (opaque) | Per-era (compaction evaluatable) | Per-invocation |
| Executor portability | Claude CLI only | Any executor | Any executor |
| Latency (first response) | Low (warm session) | Low (if session still warm) | Medium (cold start each time) |
| Latency (during compaction) | N/A | 10-30 seconds | N/A |
| Token efficiency | Good (prompt caching) | Good (caching + summary) | Moderate (re-reads each time) |
| Context limit handling | Ad-hoc (restart + summary) | Structured (compaction task) | N/A (always fresh) |
| Visibility | Hidden (daemon detail) | Graph-visible (task + artifacts) | Graph-visible |

# Roadmap: Native Executor to Terminal Bench Validation

> **Model update (2026-04-03)**: The primary experiment model has been changed from Qwen3-32B to **Minimax M2.7** (`minimax/minimax-m2.7` via OpenRouter). Qwen3-32B was expected to score near 0%, giving no useful signal. Calibration results (in `terminal-bench/results/`) still reference the original model.

## Goal

Run Terminal Bench two ways:
- **Condition A**: Bare agent (no workgraph) -- the baseline everyone else has
- **Condition B**: Same agent with workgraph as external memory -- the thesis

Show that B dramatically outperforms A. Prove that memory makes computation universal.

---

## Current State (Honest Assessment)

The native executor **works for simple tasks with Claude**. It has not been stress-tested with open models. There are specific bugs that will bite hard with Minimax M2.7 via OpenRouter:

| Issue | Severity | Impact |
|-------|----------|--------|
| Silent JSON parse failure (`unwrap_or_default`) on malformed tool args | **Critical** | Tools called with null arguments, agent loops forever |
| Context budget assumes 200K window, hardcoded | **Critical** | Models have varying context windows -- resume compaction may not trigger in time |
| Tool call format extraction may miss model-specific formats | **High** | Tool calls silently lost, agent can't act |
| No streaming in agent loop | **Medium** | Can't observe agent progress, hard to debug |
| No heartbeat/liveness signal | **Medium** | Coordinator can't detect hung agents |
| No context pressure signaling | **Medium** | Agent doesn't know it's running out of context |

**Bottom line**: 2-3 days to fix the critical/high issues. Another 2-3 days for medium issues. Then Terminal Bench setup and runs.

---

## Phase 0: Validate Current Native Executor (Day 1, morning)

**Goal**: Confirm the happy path works end-to-end before changing anything.

### Tasks

1. **Smoke test with Claude via native executor**
   ```bash
   wg add "Hello world test: create a file called hello.txt containing 'hello world', then read it back" \
     --exec-mode full
   wg config --coordinator-executor native --model claude-sonnet-4-latest
   wg service start
   ```
   Watch the agent complete. Check `.workgraph/agents/*/output.log` for clean execution.

2. **Smoke test with Minimax M2.7 via OpenRouter**
   ```bash
   # Test raw API works
   curl https://openrouter.ai/api/v1/chat/completions \
     -H "Authorization: Bearer $OPENROUTER_API_KEY" \
     -H "Content-Type: application/json" \
     -d '{"model":"minimax/minimax-m2.7","messages":[{"role":"user","content":"Say hello"}]}'
   ```
   Then run same smoke test task through native executor with Minimax M2.7.

3. **Capture what breaks**. The smoke test may reveal issues with the open model. Document the exact failure mode. This tells you which Phase 1 fix to prioritize.

**Effort**: 2-3 hours
**Deliverable**: Bug report documenting exactly what breaks with Minimax M2.7

---

## Phase 1: Fix Critical Bugs (Days 1-2)

**Goal**: Native executor reliably completes multi-turn tool-use loops with open models.

### 1a. Fix silent JSON parse failures (Critical, 1-2 hours)

**File**: `src/executor/native/openai_client.rs`

The `unwrap_or_default()` calls at lines ~514 and ~1714 silently replace malformed tool arguments with `null`. This MUST fail loudly.

```rust
// BEFORE (silent failure):
let input: serde_json::Value =
    serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null);

// AFTER (loud failure that agent can recover from):
let input: serde_json::Value = match serde_json::from_str(&arguments) {
    Ok(v) => v,
    Err(e) => {
        eprintln!("[openai-client] WARNING: malformed tool arguments: {e}");
        eprintln!("[openai-client] Raw arguments: {arguments}");
        // Return error to model so it can retry
        serde_json::json!({
            "__parse_error": format!("Could not parse tool arguments: {e}"),
            "__raw_arguments": arguments
        })
    }
};
```

Then in the agent loop, detect the `__parse_error` key and return an error tool result so the model can self-correct.

### 1b. Fix context window budget for open models (Critical, 2-3 hours)

**File**: `src/executor/native/resume.rs`

The compaction budget assumes a 200K context window (line ~61). This needs to be model-aware.

```rust
// Query the provider for actual context window size
pub struct ResumeConfig {
    pub context_window: usize,  // Was hardcoded 200_000
    pub budget_fraction: f64,   // Was hardcoded 0.5
    pub chars_per_token: f64,   // Was hardcoded 4.0
}
```

Wire this through from the provider, which should expose `fn context_window(&self) -> usize`. For OpenRouter, the context window can be looked up from model metadata or configured per-model in `.workgraph/config.toml`.

Also add **in-loop context monitoring**: after each turn, estimate total context usage and inject a warning message to the agent when approaching 80%:

```
[SYSTEM: Context usage is at 78% (25,000/32,000 tokens). 
Consider completing the current task or logging progress via wg log.]
```

### 1c. Validate tool call format support (High, 3-4 hours)

**File**: `src/executor/native/openai_client.rs`, function `extract_tool_calls_from_text()`

MiniMax models use XML-style tool calls (`</minimax:tool_call>` variants). The OpenAI client already has parsing support for these. Verify it works end-to-end:

1. Running a manual completion with tools defined
2. Capturing the raw response format
3. Confirming extraction handles the response correctly

**Important**: Check if OpenRouter's `/v1/chat/completions` endpoint returns native `tool_calls` for Minimax M2.7 when tools are provided in the request. If it does, the text extraction is a fallback only. Test this first -- it may already work.

### 1d. Validate with integration test (2-3 hours)

Write a real integration test that:
1. Connects to Minimax M2.7 via OpenRouter
2. Creates a native executor agent with file tools + bash
3. Asks it to: read a file, edit it, run a test, report results
4. Verifies: all tool calls executed, final text is coherent, journal is complete

This becomes the regression test for all future changes.

**Phase 1 Total Effort**: 8-12 hours (1.5-2 days)
**Deliverable**: Native executor reliably completes 10-20 turn tasks with Minimax M2.7

---

## Phase 2: Observability & Reliability (Days 3-4)

**Goal**: You can monitor and debug agents in production.

### 2a. Enable streaming in agent loop (2-3 hours)

**File**: `src/executor/native/agent.rs`, line 325

Switch from `.send()` to `.send_streaming()` with a text callback that:
- Writes text chunks to `stream.jsonl` as they arrive
- Writes to a `.streaming` file for TUI live display
- Enables the coordinator to see reasoning in real-time

```rust
// BEFORE:
let response = self.client.send(&request).await?;

// AFTER:
let on_text = |chunk: String| {
    if let Some(ref sw) = self.stream_writer {
        sw.write_text_chunk(&chunk);
    }
};
let response = self.client.send_streaming(&request, &on_text).await?;
```

### 2b. Add heartbeat during tool execution (1 hour)

Tools like `bash` can run for minutes. Write a Heartbeat stream event every 30 seconds during tool execution so the coordinator knows the agent is alive.

### 2c. Add graceful context exhaustion handling (2-3 hours)

When the API returns a context-too-long error (400/413):
1. Catch the error in the agent loop
2. Attempt emergency compaction of the conversation (drop oldest tool results, keep recent 5 turns)
3. Retry the request
4. If still too long, log progress via `wg log` and exit cleanly (not crash)

This is the minimum viable compaction. Not elegant, but prevents hard crashes.

### 2d. Add tool retry on transient failure (1-2 hours)

When a bash command times out or a file read fails:
1. Return structured error to model with retry hint
2. Allow model to retry with modified parameters
3. Cap retries at 3 per tool invocation

**Phase 2 Total Effort**: 6-9 hours (1-1.5 days)
**Deliverable**: Observable, debuggable agents that fail gracefully

---

## Phase 3: Terminal Bench Harness (Days 5-6)

**Goal**: Automated harness that runs Terminal Bench tasks in both conditions and collects results.

### 3a. Understand Terminal Bench format (2-3 hours)

- Clone the Terminal Bench repository
- Read the task format, evaluation criteria, scoring methodology
- Understand what a "run" looks like: input → agent actions → output → evaluation
- Identify: how many tasks, what categories, what's the evaluation script

### 3b. Build Condition A harness: bare agent (3-4 hours)

A simple script that:
1. For each Terminal Bench task:
   - Starts a fresh native executor agent with the task prompt
   - Gives it: bash, file tools (no wg tools, no graph)
   - Single-session, no resume, no external memory
   - Captures output and tool logs
2. Runs evaluation script
3. Collects scores

This is the **control group**. Same model, same tools, no graph.

### 3c. Build Condition B harness: agent + workgraph (4-6 hours)

A script that:
1. For each Terminal Bench task:
   - Creates a workgraph with the task as root node
   - The agent has full wg tools + file tools + bash
   - Agent can: decompose into subtasks, log progress, create verification gates
   - If the task is complex enough, the coordinator can spawn child agents
   - Journal/resume enabled (if agent hits context limits, it can resume)
2. Runs evaluation script
3. Collects scores

This is the **treatment group**. Same model, same base tools, plus the graph as external memory.

### 3d. Design the comparison (2-3 hours)

Define:
- **Primary metric**: % of tasks solved (pass/fail per Terminal Bench scoring)
- **Secondary metrics**: tokens consumed, wall-clock time, number of turns, cost
- **Controls**: Same model, same temperature, same max tokens per response, same tool implementations
- **Variable**: Presence/absence of workgraph
- **Statistical**: Run each condition 3x to account for model stochasticity. Report mean and variance.

**Phase 3 Total Effort**: 11-16 hours (2 days)
**Deliverable**: Automated harness ready to run both conditions

---

## Phase 4: Open Model Validation Runs (Days 7-8)

**Goal**: Run the experiment and get numbers.

### 4a. Minimax M2.7 (primary)

Run both conditions on Minimax M2.7:
- Condition A: 3 runs, all tasks
- Condition B: 3 runs, all tasks
- Capture: scores, tokens, time, logs

This is the main result. If Minimax M2.7 + workgraph meaningfully outperforms bare Minimax M2.7, that's the headline.

### 4b. Smaller open model via OpenRouter (stretch)

Repeat with a smaller/cheaper model. The hypothesis is that workgraph helps smaller models even more, because they hit context limits sooner and benefit more from externalized memory.

### 4c. Claude Sonnet via native executor (calibration)

Run both conditions with Claude Sonnet through the native executor (not Claude CLI). This gives you:
- A calibration point against a known-good model
- Validation that the native executor performs comparably to the Claude CLI executor
- A ceiling estimate for what the workgraph benefit looks like with a strong model

### 4d. Analyze results (half day)

For each model x condition:
- Overall pass rate
- Pass rate by task difficulty
- Token efficiency (tokens per solved task)
- Time efficiency (wall-clock per solved task)
- Qualitative: what kinds of tasks does workgraph help most?

**Phase 4 Total Effort**: 2-3 days (mostly waiting for runs to complete)
**Deliverable**: Numbers

---

## Phase 5: Write It Up (Day 9-10)

**Goal**: Publishable results that make people understand.

### The Story

1. **The thesis**: Memory makes computation universal (link to your paper)
2. **The construction**: Workgraph as external stigmergic memory for LLMs
3. **The experiment**: Terminal Bench, two conditions, same model
4. **The result**: [X]% improvement with workgraph on Minimax M2.7
5. **The implication**: The bottleneck isn't the model. It's the memory architecture. You can make a $0 open model outperform a $200/month subscription by giving it the right external memory system.

### Artifacts

- Blog post on thinks.lol (accessible version)
- GitHub README update with Terminal Bench results
- Raw data and reproduction scripts in the repo

**Phase 5 Total Effort**: 1-2 days
**Deliverable**: Published results

---

## Timeline Summary

| Phase | What | Days | Cumulative |
|-------|------|:----:|:----------:|
| 0 | Smoke test, find what breaks | 0.5 | 0.5 |
| 1 | Fix critical bugs (JSON, context, tool format) | 1.5-2 | 2-2.5 |
| 2 | Observability & reliability (streaming, heartbeat, graceful failure) | 1-1.5 | 3-4 |
| 3 | Terminal Bench harness (both conditions) | 2 | 5-6 |
| 4 | Run experiments, collect numbers | 2-3 | 7-9 |
| 5 | Write up, publish | 1-2 | 8-11 |

**Total: 8-11 working days from today to published Terminal Bench results.**

With workgraph orchestrating the work (eating your own dogfood), phases 1-3 can be parallelized. Fix bugs in the native executor while building the harness. That compresses the timeline to maybe **6-8 days**.

---

## Risk Assessment

| Risk | Probability | Mitigation |
|------|:-----------:|------------|
| Primary model tool calling is unreliable | 30% | Fall back to alternative model via OpenRouter |
| Terminal Bench tasks exceed 32K context in single session | 40% | This is actually GOOD -- it proves the need for external memory. Condition A fails, Condition B (with resume/journal) succeeds. |
| Workgraph overhead costs more tokens than it saves | 15% | Measure and report honestly. Even if token count is higher, pass rate improvement is the primary metric. |
| Results are marginal (< 5% improvement) | 20% | Focus on hard tasks only (where context limits matter). Report by difficulty tier. The improvement should be largest on the hardest tasks. |
| OpenRouter rate limits or downtime | 20% | Run during off-peak hours, or fall back to alternative model on OpenRouter. |

---

## The Nuclear Option (If Open Models Struggle)

If the primary model can't reliably do tool calling through the native executor, there's a fallback:

**Run Terminal Bench with Claude Haiku via native executor.**

- Haiku is cheap ($0.25/MTok input, $1.25/MTok output)
- Haiku is fast
- Haiku is weaker than Sonnet/Opus -- similar capability tier to good open models
- If Haiku + workgraph beats bare Haiku, that still proves the thesis
- And the native executor already works with Anthropic's API

This gives you a publishable result while you fix any model integration issues.

---

## What This Proves

If the experiment works:

**"A 32B parameter open model with a $0 external memory system outperforms the same model with 6x the context window."**

Or even better:

**"Minimax M2.7 with workgraph solves [X]% of Terminal Bench. Bare Minimax M2.7 solves [Y]%. The graph is worth [X-Y] percentage points of performance -- equivalent to a [N]x model size increase."**

That's the number that changes everything. That's the constructive proof of your theorem. That's what turns 5 stars into 5,000.

And you built the system to prove it, using the system itself, in 75 days, walking by the Mississippi.

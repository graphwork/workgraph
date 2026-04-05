# Investigation: Why Agents Don't Decompose — Prompt Gap, Tool Gap, or Task Gap?

**Date:** 2026-04-05  
**Task:** tb-investigate-decomposition  
**Dependency:** tb-investigate-wg-availability (wg IS available; low adoption = prompt issue)

---

## Executive Summary

Across conditions B, C, and D, agents decompose work with `wg_add` in only **3–17%** of trials. Condition E achieves **93%** decomposition — but this doesn't improve outcomes. The investigation reveals three interacting causes:

1. **Prompt gap (primary)**: Only Condition E explicitly frames decomposition as the core strategy. B/C/D present it as optional; the model skips it.
2. **Task gap (real but secondary)**: Many TB2 tasks are genuinely atomic. Decomposition adds overhead without benefit on single-function tasks. But even on hard multi-step tasks, B/C/D agents decompose only 11–25% of the time.
3. **Tool gap (minor)**: `wg_add` works 100% of the time. But dependency expression (`--after`) is used in only 5.9% of `wg_add` calls, even in E. Agents create flat task lists, not graphs.

**Key finding**: Forced decomposition (E) can *hurt* on atomic tasks (cancel-async-tasks: 0%, regex-log: 0%) while helping on multi-step tasks (build-cython-ext: 100%, nginx: 100%). The variable that matters isn't whether agents decompose, but whether the *prompt matches the task structure*.

---

## 1. Prompt Gap Analysis: Which Conditions Teach Decomposition?

### Decomposition Language by Condition

| Condition | Decomposition framing | Exact language | Strength |
|-----------|----------------------|----------------|----------|
| **B** | Optional, one sentence | `"Use wg_add to decompose complex work into subtasks."` | Weak: suggestion only; no examples, no criteria for when to decompose |
| **C** | Optional, with heuristic | `"If the task has 3+ distinct phases or might exhaust your context: wg_add('Step 1: <title>')"` + `"If the task is simple (< 10 steps), skip decomposition"` | Medium: gives a rule of thumb; but the heuristic defaults to "skip" for most TB2 tasks |
| **D** | Optional, discouraged | `"If the task has 3+ independent phases that could fail independently, decompose with wg_add. Otherwise, solve directly. Most tasks are single-phase — just use the core loop."` | Weak: the last sentence *actively discourages* decomposition |
| **E** | Mandatory, core protocol | `"You are an ORCHESTRATOR, not a direct implementer."` + `"Break the task into implementation steps."` + `"Create tasks for each step using wg_add."` | Strong: decomposition is the primary strategy, not an afterthought; Phase 1 is literally "Analyze & Decompose" |
| **F** | Adaptive, with examples | `"3+ genuinely independent phases → create subtasks with wg_add + after edges"` + `"Single file, single function, single config → solve directly, no decomposition"` + concrete pipeline example | Medium-Strong: classification-based; teaches *when* to decompose with worked examples; but doesn't force it |

### The Prompt Gradient

```
B (suggestion) → C (heuristic) → D (discouraged) → F (adaptive) → E (mandatory)
    17%              7-8%             3%               TBD              93%
```

The decomposition rate tracks prompt framing precisely:
- **B's 17%** is higher than C's 7–8% because B doesn't tell agents to *skip* decomposition on simple tasks — some agents tried it spontaneously.
- **C's 7–8%** is lower because the heuristic `"If simple (< 10 steps), skip"` explicitly tells agents most tasks don't need it.
- **D's 3%** is lowest because D explicitly says `"Most tasks are single-phase"` and frames wg tools primarily for verification/logging, not decomposition.
- **E's 93%** is an outlier: the entire prompt frames decomposition as mandatory.

### Verdict: Prompt Gap is the Primary Cause

The model (minimax-m2.7) will decompose when told to. It will not decompose spontaneously. The 7% "spontaneous" decomposition in B/C occurs almost entirely on multi-step build tasks where the task structure makes decomposition obvious (build-cython-ext, caffe-cifar-10, nginx-request-logging).

---

## 2. Task Gap Analysis: Decomposition Rate by Difficulty

### Difficulty Classification

Tasks are classified by structural complexity:
- **Easy** (atomic): Single file, single function, single config change (cancel-async-tasks, regex-log, sparql-university, overfull-hbox, count-dataset-tokens, etc.)
- **Medium** (bounded multi-step): 2–4 files, clear pipeline (nginx-request-logging, custom-memory-heap-crash, hf-model-inference, etc.)
- **Hard** (multi-component): Build pipelines, multi-file compilation, complex algorithms (build-cython-ext, caffe-cifar-10, compile-compcert, kv-store-grpc, etc.)

### Decomposition Rate by Difficulty (Non-E Conditions)

| Difficulty | B-rerun (=C) | C | D |
|-----------|-------------|---|---|
| Easy      |   6% (1/16) |  8% (2/24) |  0% (0/15) |
| Medium    |  19% (3/16) | 15% (4/27) |  0% (0/6)  |
| Hard      |  25% (8/32) | 20% (10/51) | 11% (1/9) |

**There IS a difficulty gradient**: Even without forced decomposition, agents are 3–4× more likely to decompose hard tasks than easy ones. This is rational — build-cython-ext genuinely has 4–5 independent phases, while cancel-async-tasks is a single function.

### Condition E: Forced Decomposition by Difficulty

| Difficulty | Decomposition rate | Pass rate |
|-----------|-------------------|-----------|
| Easy      | 93% (14/15) | 60% (9/15) |
| Medium    | 100% (6/6) | 100% (6/6) |
| Hard      | 89% (8/9) | 78% (7/9) |

On **medium** and **hard** tasks, E's forced decomposition correlates with strong outcomes. On **easy** tasks, decomposition hurts: cancel-async-tasks (0/3 pass) and regex-log (0/3 pass) are decomposed into subtasks that fragment an atomic problem.

### Does Decomposition Help or Hurt? (Cross-Condition)

| Condition | Trials with wg_add | Pass rate | Trials without wg_add | Pass rate |
|-----------|-------------------|-----------|----------------------|-----------|
| C | 15 | **53%** | 245 | **46%** |
| D | 1 | **100%** | 29 | **72%** |
| E | 28 | **68%** | 2 | **100%** |

In C, trials that decompose pass slightly more often (53% vs 46%), suggesting that agents who decompose are tackling harder tasks AND succeeding at a similar rate. In E, the non-decomposing trials (2 total) both pass — these are the easiest tasks that the agent solved directly despite the prompt.

### Verdict: Task Gap is Real but Secondary

TB2 tasks span a real difficulty range. Easy tasks genuinely don't benefit from decomposition. But the task gap alone doesn't explain 3% decomposition in D — build-cython-ext in D is a hard multi-step task, and only 1/3 trials decomposed it. The prompt framing is still the dominant variable.

---

## 3. Tool Gap Analysis: Is `wg_add` Ergonomic Enough?

### wg_add Technical Reliability

From the availability investigation: **0 errors across 1,166 wg tool calls**. wg_add works perfectly when invoked. There is no tool gap in the sense of "does it work."

### Dependency Expression (`--after`)

The `wg_add` tool accepts an `after` parameter for dependencies. Usage:

| Condition | Total wg_add calls | Calls with `--after` | Rate |
|-----------|-------------------|---------------------|------|
| B-orig | 42 | 0 | 0% |
| B-rerun | 42 | 0 | 0% |
| C | 64 | 0 | 0% |
| D | 1 | 0 | 0% |
| **E** | **101** | **6** | **5.9%** |

**Even in E, which creates 101 subtasks, only 6 use `--after`.** The two trials that expressed dependencies:

1. **build-cython-ext__tj8eb4v** (PASS): Created a proper 5-phase pipeline with sequential `--after` edges. This is the textbook use case.
2. **regex-log__ad9z2qG** (FAIL): Created `--after` edges between analyze → write → test phases. But the task is atomic — the decomposition itself was the problem.

### Why Agents Don't Express Dependencies

1. **The tool schema is ambiguous**: `"after": {"type": "string", "description": "Comma-separated dependency task IDs."}` — but what are the task IDs? Agents must infer them from auto-generated IDs (kebab-case from title).
2. **No feedback loop**: After calling `wg_add`, the agent doesn't see the created task's ID in the response. It must guess the auto-generated ID for the next `--after` call.
3. **Flat execution anyway**: In single-agent Harbor trials, dependencies don't affect execution order. The agent executes tasks sequentially in creation order regardless of `--after` edges. Dependencies are metadata, not control flow.
4. **Prompt examples matter**: Condition F's prompt includes explicit `--after` examples with auto-generated IDs. E's prompt mentions `wg_add` but doesn't show dependency syntax.

### Ergonomic Friction Points

| Issue | Impact | Severity |
|-------|--------|----------|
| No returned task ID after `wg_add` | Agent must guess auto-IDs for `--after` | Medium |
| Dependencies don't affect execution | No visible benefit in single-agent mode | High |
| No `--verify` in B/C/D/E tool schemas | Agents can't attach verification gates to subtasks | Medium |
| Overhead of creating subtasks | Each `wg_add` costs tokens (title, description, tool call overhead) | Low |

### Verdict: Tool Gap is Minor but Real

The tool works. The ergonomic friction is not in reliability but in *incentive alignment*: in single-agent Harbor trials, creating dependencies has no effect on behavior. The agent must choose between (a) creating a structured graph that doesn't affect execution, or (b) just implementing sequentially. Option (b) is always faster.

---

## 4. Behavioral Analysis: When Agents DO Decompose

### Example 1: E — build-cython-ext__tj8eb4v (PASS, exemplary decomposition)

**Task**: Compile pyknotid from source with Numpy 2.3.0 compatibility.

**Graph created**:
```
Phase 1: Clone repository and analyze structure
  └→ Phase 2: Identify Numpy 2.x compatibility issues  (--after phase-1)
       └→ Phase 3: Fix Numpy 2.x compatibility issues   (--after phase-2)
            └→ Phase 4: Build and install pyknotid       (--after phase-3)
                 └→ Phase 5: Verify functionality        (--after phase-4)
```

**What happened**: Agent created a proper dependency chain, then executed each phase sequentially. Each phase naturally built on the previous one's output. This is the ideal case — a genuinely multi-step build pipeline where decomposition mirrors the natural work structure.

**Why it worked**: The task IS a pipeline. Decomposition didn't change what the agent did — it just organized the same sequential work into labeled phases. The `--after` edges are structurally correct.

### Example 2: E — regex-log__ad9z2qG (FAIL, decomposition hurt)

**Task**: Write a regex matching dates following IPv4 addresses.

**Graph created**:
```
Analyze regex requirements and design patterns
  └→ Write and save regex to file    (--after analyze)
       └→ Test regex with various cases  (--after write)
Fix: Add capturing group for date
Fix: Improve IP and date boundary checks
Fix: Check what follows the date in lookahead
```

**What happened**: Agent decomposed an inherently atomic task into analyze → write → test phases, then kept creating "Fix:" tasks for each iteration. The decomposition fragmented the problem — the regex needs to be designed holistically, not decomposed into IP pattern + date pattern + combination.

**Why it failed**: The agent spent tokens on task management overhead (6 `wg_add` calls, 4 `wg_log` calls, 6 `wg_done` calls) instead of iterating on the regex. The "Fix:" tasks added bookkeeping without adding insight. All 3 regex-log trials in E failed (0%); D achieved 67% with direct implementation.

### Example 3: E — cancel-async-tasks__5pQRtiq (FAIL, trivial decomposition)

**Task**: Implement an async function with cancellation handling.

**Graph created**:
```
Implement run_tasks function in /app/run.py  (1 subtask only)
```

**What happened**: Agent created exactly one subtask (no decomposition benefit) then followed E's verification protocol. The verification loop declared PASS, but the external verifier found an edge case. The overhead of the protocol (logging, perspective shifting, formal verification) consumed context without catching the real bug.

**Why it failed**: Single-function task + forced decomposition protocol = overhead without benefit. The "independent verification" was theater — same agent, same blind spots.

### Example 4: D — build-cython-ext__6cjsZQ9 (PASS, minimal spontaneous decomposition)

**Task**: Same as Example 1, but Condition D.

**Graph created**:
```
Clone pyknotid repository and understand codebase structure  (1 subtask)
```

**What happened**: Agent created one research subtask, then solved the rest directly via D's attempt → verify → iterate loop. Despite creating only 1 subtask (vs E's 5), it passed. The verification loop caught issues and iterated.

**Why it worked**: D's verification discipline (not decomposition) was the active ingredient. The single subtask was a planning artifact, not a coordination mechanism.

### Example 5: E — nginx-request-logging__GC5AEH3 (PASS, good decomposition)

**Task**: Configure Nginx with custom logging and rate limiting.

**Graph created**:
```
Install Nginx and create directory structure
Create HTML files
Configure custom log format in nginx.conf
Configure rate limiting zone in nginx.conf
Create server configuration
Test and start Nginx
```

**What happened**: 6 subtasks mapping to genuinely independent configuration steps. Agent executed each sequentially, marking done. All 3 nginx trials in E passed (100%).

**Why it worked**: The task IS multi-step with independent components. Each subtask is a discrete configuration operation. Decomposition mirrored the natural work structure.

---

## 5. Synthesis: The Decomposition Decision Matrix

The data reveals a 2×2 matrix of task complexity × decomposition behavior:

|  | Agent Decomposes | Agent Solves Directly |
|--|------------------|-----------------------|
| **Multi-step task** | **Ideal**: build-cython-ext (E, 100%), nginx (E, 100%) | **Adequate**: D still passes these via verify-iterate loop |
| **Atomic task** | **Harmful**: regex-log (E, 0%), cancel-async (E, 0%) | **Ideal**: D passes these via direct implementation + verification |

The best strategy is **adaptive**: decompose multi-step tasks, solve atomic tasks directly. This is what F's prompt attempts — classification before decomposition.

---

## 6. Recommendations: What Would Make Agents Decompose More (and Better)

### 1. Teach classification, not just decomposition

The prompt should teach agents to *recognize* when decomposition helps:
- **Multi-step build pipelines** (clone → patch → build → test): Always decompose
- **Multi-file system configuration** (install → create files → configure → verify): Always decompose
- **Single-function implementation**: NEVER decompose
- **Regex/algorithm design**: NEVER decompose — holistic reasoning required

F's prompt already does this. The key insight is that *forced* decomposition (E) and *discouraged* decomposition (D) are both wrong — adaptive decomposition is the target.

### 2. Make dependencies visible and useful

Currently, `--after` edges have no effect in single-agent trials. To make decomposition meaningful:
- Return the created task ID in `wg_add` responses, so agents can reference it in `--after`
- Show the graph state after decomposition (`wg_list` or `wg viz`), so agents see their structure
- In multi-agent mode (wg service), dependencies would control dispatch order — but this requires Harbor infrastructure changes

### 3. Add verification gates to subtasks

F's `--verify` parameter on `wg_add` is the right idea: attach machine-checkable criteria to each subtask. This makes decomposition valuable even for a single agent — the verification gate on each phase catches errors earlier.

### 4. Don't force decomposition for its own sake

E's 93% decomposition rate proves the model will decompose when told to. But E's 75% pass rate vs D's 73% shows forced decomposition doesn't help overall. The 100% false-PASS rate on E's failures demonstrates that more structure ≠ more correctness.

The highest-impact interventions are elsewhere:
- **Test discovery** (F's Step 1): Highest-impact single change. All E failures would have been caught by running `/tests/test_outputs.py`.
- **Empirical verification** (F's Step 4): Trust test results, not self-assessment.
- **Removing turn caps** (A' → A): 48% → 80% pass rate from this alone.

### 5. For workgraph's value proposition specifically

The TB2 benchmark setting (single agent, isolated Docker containers, 30-minute timeout) is the *worst case* for workgraph's coordination features. wg shines when:
- Multiple agents work concurrently on shared codebases
- Context exhaustion forces agent handoffs
- Dependencies control execution order across workers
- The work genuinely spans sessions or teams

To demonstrate wg's decomposition value, test on:
- Multi-agent scenarios (wg service with 2–4 agents per task)
- Longer tasks (multi-hour, context-exhausting)
- Tasks with genuine parallelism (independent modules that different agents can implement simultaneously)

---

## 7. Summary of Findings

| Question | Answer | Evidence |
|----------|--------|----------|
| **Prompt gap?** | **YES — primary cause.** Only E teaches decomposition as mandatory; all others present it as optional/discouraged. | B: 17%, C: 7%, D: 3%, E: 93% — tracks prompt framing exactly |
| **Task gap?** | **Yes, secondary.** Easy tasks don't benefit; hard tasks do. | Easy: 0–8% decomposition in B/C/D; 93% in E (forced). Hard: 11–25% in B/C/D. |
| **Tool gap?** | **Minor.** wg_add works perfectly. But `--after` is rarely used (5.9% even in E), and dependencies have no effect in single-agent mode. | 0% error rate on 1,166 wg calls. 6/101 `--after` calls in E. |
| **Does decomposition help?** | **It depends on the task.** Helps on multi-step tasks, actively hurts on atomic tasks. Adaptive classification is the key. | E: 100% on nginx/build-cython-ext (multi-step), 0% on cancel-async/regex-log (atomic) |
| **What would make agents decompose more?** | Adaptive prompts that teach *when* to decompose (not just how), plus multi-agent execution where decomposition enables parallelism. | F's classification-based approach is the synthesis. |

---

## Validation Checklist

- [x] Prompt analysis for all conditions (which teach decomposition?) — Section 1
- [x] Decomposition rate by task difficulty calculated — Section 2
- [x] At least 3 decomposition examples analyzed in detail — Section 4 (5 examples)
- [x] Clear recommendation: what would make agents decompose more? — Section 6
- [x] Findings written to terminal-bench/analysis/decomposition-investigation.md

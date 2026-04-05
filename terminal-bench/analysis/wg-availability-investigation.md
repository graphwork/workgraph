# Investigation: Is wg Service Actually Available to Trial Agents?

**Date:** 2026-04-05  
**Task:** tb-investigate-wg-availability  
**Verdict:** wg IS reliably available. Low adoption is a prompt/model compliance issue, not an availability bug.

---

## 1. How is wg Bootstrapped in Each Condition?

### Two Execution Paths

Terminal Bench has two distinct execution paths:

1. **`tb-harness.sh`** (shell-based, conditions A/B/C only): Creates a temp directory, runs `wg init`, configures OpenRouter endpoint, builds a prompt file, calls `wg native-exec`. Used for early calibration runs.

2. **`adapter.py` (Harbor framework, all conditions A-F)**: The primary path for all full/pilot/rerun trials. Key architecture decision: **wg commands run on the HOST, not inside Docker containers**.

### Per-Condition Bootstrap (adapter.py path)

| Condition | `wg init` | Root task created | Agency bootstrap | Agent identity | `wg_add` tool available | `wg service start` |
|-----------|-----------|-------------------|------------------|----------------|------------------------|---------------------|
| A | No | No | No | No | No | No |
| B | Yes (temp dir on host) | Yes (`wg add` with root task ID) | No | No | Yes | No |
| C | Yes (temp dir on host) | Yes | No | No | Yes | No |
| D | Yes (temp dir on host) | Yes + `wg assign` | Yes (`wg agency init`) | solver/programmer/careful | Yes | No |
| E | Yes (temp dir on host) | Yes + `wg assign` | Yes (`wg agency init`) | orchestrator/architect/thorough | Yes | No |
| F | Yes (temp dir on host) | Yes | No | No | Yes (enhanced: `--verify`, `--id`) | No |

**Key finding:** `wg service start` is NEVER called in any condition. This is correct because the adapter itself acts as the agent loop — it doesn't need the wg service to dispatch agents. The wg tools (`wg_add`, `wg_done`, `wg_log`, etc.) are function-calling tools that route to `_exec_wg_cmd_host()`, which runs the wg binary on the host pointing at the temp workgraph directory.

### The Host-Side Architecture (Critical)

```
┌──────────────────────────────────────┐
│  Harbor Framework (host)             │
│  ┌────────────────────────────────┐  │
│  │  WorkgraphAgent (adapter.py)   │  │
│  │  - LLM loop (litellm)         │  │
│  │  - Tool dispatch               │  │
│  │    ├─ bash/file → env.exec()  │───│──→ Docker container
│  │    └─ wg_* → wg binary (host) │───│──→ /tmp/tb-wg-XXXX/.workgraph
│  └────────────────────────────────┘  │
└──────────────────────────────────────┘
```

The wg binary is NOT injected into Docker containers. Instead:
- **bash, read_file, write_file, edit_file, glob, grep** → Execute inside the Docker container
- **wg_log, wg_add, wg_done, wg_fail, wg_show, wg_list, wg_artifact, wg_msg_send, wg_msg_read** → Execute on the host via `_exec_wg_cmd_host()`

This means **agents cannot run `wg` via the bash tool** — there is no wg binary inside containers. They must use the dedicated wg_* function-calling tools.

---

## 2. Can Agents Actually Run wg Tools?

### Verification: Zero Errors Across All Conditions

| Condition | Trials with ndjson | wg tool calls | wg errors | Error rate |
|-----------|-------------------|---------------|-----------|------------|
| B (orig) | 114 | 191 | 0 | 0% |
| C (full) | 160 | 442 | 0 | 0% |
| D (pilot) | 30 | 88 | 0 | 0% |
| E (pilot) | 30 | 444 | 0 | 0% |
| F (smoke) | 1 | 1 | 0 | 0% |

**Every single wg tool call succeeded.** No errors, no timeouts, no "wg not found" messages. When agents use wg tools, they work 100% of the time.

---

## 3. wg Adoption Rates by Condition

| Condition | Trials (with ndjson) | Any wg tool | wg_add (decomposition) |
|-----------|---------------------|-------------|----------------------|
| A (control) | 356 | 0% | 0% |
| B (orig) | 114 | **46%** | 17% |
| B-rerun (=C) | 161 | **88%** | 7% |
| C (full) | 160 | **86%** | 8% |
| D (pilot) | 30 | **100%** | 3% |
| E (pilot) | 30 | **97%** | 93% |
| F (smoke) | 1 | 100% | 0% |

### What Drives Adoption

1. **B (46%)**: Tools are listed but prompt gives minimal guidance ("Use `wg_log` to record progress"). Model treats them as optional.

2. **C (86%)**: Skill injection prompt with explicit templates (`wg_log("{root_task_id}", "Starting: <plan>")`) dramatically increases adoption. But only 8% use `wg_add` for decomposition — agents prefer to solve directly.

3. **D (100%)**: The autopoietic verification loop prompt makes `wg_log` and `wg_done` mandatory parts of the protocol. But almost no decomposition (3%) — the prompt frames wg as a verification/logging tool.

4. **E (97%/93% wg_add)**: The organization-generation prompt explicitly frames the agent as an "ORCHESTRATOR" who decomposes tasks. This drives both overall wg usage and decomposition.

### The 7% wg_add Decomposition Rate Across B/C/D

The low decomposition rate is NOT a wg availability problem. It's caused by:

1. **Task complexity doesn't require it**: Most TB2 tasks (cancel-async-tasks, circuit-fibsqrt, etc.) are solvable in 10-20 tool calls. The overhead of creating subtasks exceeds the benefit.

2. **Model compliance gap**: minimax-m2.7 doesn't reliably follow tool-usage instructions, especially when the task appears simple. The model takes the shortest path (bash + file).

3. **Prompt framing**: Only Condition E's prompt frames the agent as an orchestrator who MUST decompose. All other conditions present decomposition as optional.

---

## 4. Trial Transcript Analysis (10+ transcripts examined)

### 5 Zero-wg Transcripts (Condition C)

| Trial | Turns | Tool calls | First turn content | Why no wg? |
|-------|-------|------------|-------------------|------------|
| `cancel-async-tasks__5Drxper` | 19 | 19 | "Simple task - I'll implement directly" | Model classified as simple, skipped wg |
| `cancel-async-tasks__5tEMSZY` | 14 | 26 | (no content, jumped to bash) | Model never acknowledged wg tools |
| `circuit-fibsqrt__cYrK9xk` | ~50 | 51 | (no content) | Long task but model never considered wg |
| `circuit-fibsqrt__k9UkqF7` | 2 | 2 | N/A | Very short run, likely early termination |
| `constraints-scheduling__Lsa85gj` | 5 | 5 | N/A | Short task, direct implementation |

**Pattern**: Agents never attempt wg and fail — they simply don't try. No error messages, no "command not found". The model decides not to use wg tools based on its assessment of task complexity.

### 5 With-wg Transcripts (Conditions C/D/E)

| Trial | Condition | wg_log | wg_add | wg_done | Pattern |
|-------|-----------|--------|--------|---------|---------|
| `build-cython-ext__Y6qkQoc` | C | 2 | 4 | 5 | Full decomposition: clone → build → install → test |
| `caffe-cifar-10__t9cyjYJ` | C | 1 | 6 | 4 | Decomposed multi-step ML build pipeline |
| `hf-model-inference__ZumWaao` | C | 5 | 4 | 5 | Decomposed + artifacts recorded |
| `build-cython-ext__6cjsZQ9` | D | 1 | 1 | 1 | Minimal: log start, one subtask, done |
| `build-cython-ext__Dt7Spu6` | E | many | many | many | Full org-gen: analyze → decompose → implement → verify |

**Pattern**: When agents DO use wg, the tools work correctly. `wg_add` creates subtasks, `wg_done` marks them complete, `wg_log` records progress. The wg backend on the host reliably processes all commands.

---

## 5. Docker Failures (Separate Issue)

A large number of trials failed at Docker container setup, before the agent loop even started:

| Condition | Total trials | Docker failures | % |
|-----------|-------------|-----------------|---|
| A (full) | 267 | 148 | 55% |
| B (orig) | 267 | 153 | 57% |
| C (full) | 162 | 2 | 1% |
| D (pilot) | 30 | 0 | 0% |
| E (pilot) | 30 | 0 | 0% |

The high Docker failure rate in A-full and B-orig is a Harbor environment issue, not a wg issue. Later runs (C, D, E) had near-zero Docker failures, suggesting the infrastructure was fixed between runs.

---

## 6. Verdict

### wg IS reliably available in all conditions B-F

- Every condition B-F properly initializes a workgraph directory on the host
- The wg binary is found and used without errors
- All wg tool calls succeed (0% error rate across 1,166 calls)
- Agents in conditions D and E achieve 97-100% wg adoption

### Low wg_add usage (~7%) is a prompt/model issue, not availability

- Condition B: Tools listed but not motivated → 46% any-wg, 17% decomposition
- Condition C: Skill injection → 86% any-wg, but only 8% decomposition
- Condition D: Verification loop → 100% any-wg, but only 3% decomposition
- Condition E: Orchestrator framing → 97% any-wg, **93% decomposition**

The variable that drives decomposition is **prompt framing** (particularly E's orchestrator identity), not wg availability. The model (minimax-m2.7) will use wg_add when the prompt makes it a core part of the protocol, but ignores it when presented as optional.

### Recommendations for downstream tasks

1. **Condition F sweep**: wg will be available. Focus on whether the distilled context injection + empirical verification prompt drives better outcomes than E's org-gen approach.

2. **Decomposition investigation** (tb-investigate-decomposition): The answer to "why agents don't decompose" is primarily prompt framing. The tool gap is zero — wg_add works perfectly when called. The task gap is real — most TB2 tasks don't benefit from decomposition.

3. **No infrastructure fix needed**: wg availability is not the bug. The B-orig low adoption (46%) is the expected result of a minimal prompt with optional tools.

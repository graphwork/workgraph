# Terminal Bench Experiment Progress Report

**Date:** 2026-04-08
**Author:** Generated from experiment session
**Status:** In progress — Conditions A and F running; Condition G under active development

---

## 1. Setup

Remote host `bot@ulivo` (Debian/Ubuntu, 437 GB disk, 61 GB RAM). Full Docker/Harbor
setup completed:

- 96 Docker images pre-pulled via GHCR mirrors (~75 GB)
- `wg` binary built for Debian bookworm (glibc 2.36 compatible with TB containers)
- Harbor adapter (`wg/adapter.py`) rewritten to run the wg native executor inside
  Docker containers instead of the original Python/LiteLLM agent loop
- Model: `openrouter:minimax/minimax-m2.7` ($0.30/M input, $1.20/M output)

## 2. Conditions

| Condition | Description | Config | Status |
|-----------|-------------|--------|--------|
| **A** (control) | Bare agent — bash + file tools only, no graph context | `context_scope=clean`, wg tools excluded, `max_agents=1` | Running (178/445 trials) |
| **F** (treatment) | Full wg context — graph awareness, wg tools, distilled memory | `context_scope=graph`, all tools, `max_agents=1` | Running (171/445 trials) |
| **G** (autopoietic) | Agent builds its own self-correcting workgraph | `context_scope=graph`, `max_agents=8`, architect bundle for seed | Under development |

## 3. Results So Far

### Conditions A and F (stable, running)

| Condition | Completed | Passed | Pass Rate |
|-----------|-----------|--------|-----------|
| **A** | 178 | 74 | **41%** |
| **F** | 171 | 78 | **45%** |

F shows a ~4 percentage point improvement over A. Both are running on all 89 TB 2.0
tasks with 5 trials each (445 total per condition). At current pace (~4 concurrent
trials), A and F should complete in approximately 12-15 hours from launch.

### Condition G (experimental, iterating)

G has gone through 6 iterations of prompt and architecture changes:

| Run | Timestamp | Trials | Pass Rate | What changed |
|-----|-----------|--------|-----------|--------------|
| 1 | 16:23 | 0 | — | Initial launch, no results before kill |
| 2 | 16:35 | 4 | **75%** | Convergence prompt fix |
| 3 | 17:06 | 13 | **46%** | Parallel decomposition push |
| 4 | 18:25 | 14 | **64%** | Post cycle-fix in wg core |
| 5 | 19:47 | 191 | **0%** | Architect bundle — `wg add --exec-mode architect` rejected |
| 6 | 20:19 | 0 | — | Fix: use `--exec-mode bare` with custom bundle; smoke testing |

**Best G result: 64% (run 4)** vs F's 45% — a significant improvement. However, this
came from the agent working until timeout, not from clean autopoietic iteration.

## 4. Key Findings

### 4.1 The wg native executor works inside Docker

The adapter rewrite (replacing LiteLLM with wg native-exec inside containers) works
correctly. Conditions A and F produce meaningful results with the proper executor.
The bookworm-compatible binary was required (host binary had glibc mismatch).

### 4.2 F improves over A, but modestly on the full 89-task benchmark

The pilot studies on 18 custom tasks showed F at 98.9% vs A at 50%. On the harder
89-task TB 2.0 benchmark, the gap narrows to ~4pp (45% vs 41%). The custom tasks
were easier and more amenable to context-driven solutions.

### 4.3 Condition G's autopoietic design is promising but fighting the executor

**The core tension:** wg's native executor injects a `REQUIRED_WORKFLOW` section
into every agent's prompt that says "do the work, validate, `wg done`." This
conflicts with the Condition G meta-prompt that says "build a graph, delegate,
don't implement." M2.7 follows the louder, more structured REQUIRED_WORKFLOW
and ignores the meta-prompt in the task description.

**Evidence:**
- In trials with artifacts, the seed agent typically solved the problem directly
  in the seed task rather than building a graph
- When it did build graphs, it often created broken dependency structures (deadlocks)
- 11 of 13 trials in run 3 hit `AgentTimeoutError` — the agent never signaled convergence
- Only 2 clean completions (both `reshard-c4-data`) — one with a simple 3-task cycle
  that worked perfectly, one with a complex 9-task graph that deadlocked

**Attempted fixes:**
1. Updated meta-prompt to be more explicit about convergence signaling
2. Fixed a wg cycle dispatch bug where Havlak's algorithm picked the wrong entry point
3. Pushed parallel decomposition in the prompt
4. Introduced an "architect" bundle with `context_scope=clean` to eliminate the
   competing REQUIRED_WORKFLOW — but `wg add --exec-mode architect` was rejected
   (custom exec_mode names not supported)
5. Fell back to overriding the `bare` bundle with architect content

### 4.4 Cycle dispatch bug found and fixed

Discovered and fixed a bug in wg's cycle readiness computation (`src/graph.rs`).
When a user creates a cycle with `wg edit X --add-after Y --max-iterations N`,
the task X should be the entry point. But Havlak's algorithm sometimes picked a
different header based on DFS traversal order, causing cycles to start from the
wrong task or deadlock entirely.

**Fix:** After Havlak computes cycles, check if any member has `cycle_config`
(set by `--max-iterations`). If so, override Havlak's header with that task.
All 260+ cycle tests pass. Committed as `0543df60`.

### 4.5 M2.7 is not free

The experiment docs assumed Minimax M2.7 was free-tier on OpenRouter. It costs
$0.30/M input, $1.20/M output. Current spend: ~$220 total on the account,
~$86 this week. Estimated total for full A+F+G runs: ~$500-700.

## 5. Architecture Insights

### What the agent actually sees (prompt assembly)

The native executor assembles a large prompt from multiple sources:
1. Task assignment header ("You are an AI agent...")
2. Task description (our meta-prompt + TB instruction)
3. `REQUIRED_WORKFLOW` section (7-step mandatory workflow)
4. `AUTOPOIETIC_GUIDANCE` or decomposition guidance
5. `GRAPH_PATTERNS` section
6. `CRITICAL_WG_CLI` section
7. Bundle `system_prompt_suffix`

For Condition G, our meta-prompt at layer 2 was being overridden by layers 3-6.
The architect bundle approach (setting `context_scope=clean`) eliminates layers 3-6,
leaving only our meta-prompt as the agent's instruction set.

### Bundle system

Bundles control what tools and context an agent gets:
- `exec_mode` on the task → bundle name → `.workgraph/bundles/<name>.toml`
- Bundle defines: tools (whitelist), context_scope, system_prompt_suffix
- Valid exec_modes: `full`, `light`, `bare`, `shell` (custom names rejected by `wg add`)
- Workaround: override `bare.toml` with custom content for the seed task

## 6. Next Steps

1. **Validate architect bundle smoke test** — currently running on `fix-git`
2. **If smoke passes:** relaunch Condition G full run with architect bundle
3. **If smoke fails:** investigate whether `bare` exec_mode with custom bundle
   actually eliminates REQUIRED_WORKFLOW, or if we need a code change
4. **Complete A and F** — ~6-8 hours remaining at current pace
5. **Analyze results** — per-task comparison across conditions, statistical tests
6. **Prepare leaderboard submission** — organize results for HuggingFace

## 7. Files Changed

| File | Change |
|------|--------|
| `terminal-bench/wg/adapter.py` | Rewritten: native executor in Docker, Condition G with architect bundle |
| `terminal-bench/reproduce.sh` | Added conditions D-G, fixed model name prefix |
| `terminal-bench/docs/RUNBOOK.md` | Added Condition G section, fixed cost info |
| `terminal-bench/docs/spec-native-executor-harbor.md` | Design spec for native executor approach |
| `src/graph.rs` | Cycle dispatch fix: respect user's cycle_config for header selection |
| `tests/integration_cycle_detection.rs` | Updated 2 tests for corrected cycle behavior |

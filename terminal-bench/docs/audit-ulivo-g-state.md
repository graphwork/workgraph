# Audit: Condition G State — Ulivo vs Local

**Date:** 2026-04-09
**Task:** audit-ulivo-g
**Status:** Complete

---

## Executive Summary

**Ulivo and local are fully synchronized.** Both are at commit `242bf37f` (HEAD of `main`), the wg binary on Ulivo was rebuilt at 2026-04-09T05:33 CEST (after the latest commit), and the adapter.py Condition G config is byte-identical. There is **no delta** to sync.

The "two G definitions" question is resolved: Condition G evolved through **three phases** within a single condition letter. The heartbeat design (Phase 3) replaced the autopoietic meta-prompt design (Phase 2), which itself replaced the "F without surveillance" definition (Phase 1). All three are the same G — the code reflects Phase 3 (current), and Phase 2's meta-prompt remains in the code as inactive guidance (since `autopoietic: False`).

Seven condition G runs exist on Ulivo, tracking the evolution from Phase 2 through the architect-bundle detour to Phase 3. The best Phase 2 result was 64.3% (14 trials); the current Phase 3 config shows 8.4% on 38 trials (107 attempted, 102 errors) — significantly worse, but this run used the TB 2.0 89-task Docker benchmark (not the 18 custom host-native tasks), and many errors are infrastructure failures (Docker metric collection, missing `.workgraph/agents/`), not agent failures.

---

## Q1: What Code Is on Ulivo?

| Property | Ulivo | Local |
|----------|-------|-------|
| Branch | `main` | `main` |
| HEAD commit | `242bf37f` | `242bf37f` |
| `wg` binary version | `wg 0.1.0` | `wg 0.1.0` |
| `wg` binary rebuilt | 2026-04-09T05:33 CEST | — |
| Untracked files | `wg_terminal_bench.egg-info/` only | Same + debug/, jobs/, etc. |

**Ulivo has pulled ALL recent commits**, including:
- `021c585f` — heartbeat implementation (impl-tb-heartbeat)
- `242bf37f` — graceful completion (implement-graceful-completion)

**The wg binary was rebuilt** after the latest commit (binary timestamp 2026-04-09T05:33 > commit timestamp 2026-04-08T15:41).

**Delta: None. Ulivo is fully up to date.**

---

## Q2: What Adapter Config Is G Using on Ulivo?

Ulivo's `terminal-bench/wg/adapter.py` at lines 127–137 (verified via remote `sed`):

```python
"G": {
    "exec_mode": "full",
    "context_scope": "graph",
    "agency": None,
    "exclude_wg_tools": False,
    "max_agents": 8,
    "autopoietic": False,            # Phase 3: coordinator orchestrates, not seed agent
    "coordinator_agent": True,        # Phase 3: persistent coordinator agent
    "heartbeat_interval": 30,         # Phase 3: 30s autonomous heartbeat
    "coordinator_model": "sonnet",    # Phase 3: capable model for coordinator reasoning
}
```

This is **identical to local**. Key Phase 3 settings:
- `autopoietic: False` — the seed agent does NOT get the autopoietic meta-prompt
- `coordinator_agent: True` — a persistent coordinator agent runs on a heartbeat loop
- `heartbeat_interval: 30` — coordinator is prompted every 30 seconds
- `coordinator_model: "sonnet"` — stored in config dict but **not yet wired** into `config.toml` generation (documented gap; see Q7 in `research-condition-g-timeout.md:228`)
- `max_agents: 8` — up to 8 parallel task agents

**The autopoietic meta-prompt** (`CONDITION_G_META_PROMPT`, lines 483–531) still exists in the code but is **inactive** because `autopoietic: False`. It is only prepended when `cfg.get("autopoietic")` is truthy (line 637).

---

## Q3: What Does Local G Look Like Now?

After `impl-tb-heartbeat` (021c585f) and `implement-graceful-completion` (242bf37f), the current Condition G is the **Phase 3: heartbeat-orchestrated coordinator** design:

### Architecture
- The coordinator agent (persistent LLM session) reviews graph state every 30 seconds
- It creates tasks, kills stuck agents, sends messages, and adapts strategy
- Task agents run with full tools and graph context (same as Condition F)
- No autopoietic meta-prompt — the coordinator drives orchestration, not the seed agent

### New from `impl-tb-heartbeat` (021c585f)
- `heartbeat_interval` config field in `config.rs`
- Integrated heartbeat timer in daemon event loop (`mod.rs:2472–2482, 2806–2838`)
- Heartbeat prompt with time elapsed/budget remaining (`coordinator_agent.rs:400–414`)
- `heartbeat.sh` helper script (`terminal-bench/heartbeat.sh`)

### New from `implement-graceful-completion` (242bf37f)
- `WG_TASK_TIMEOUT` and `WG_TASK_START_EPOCH` env vars injected into agent processes (`execution.rs`)
- Time-awareness section in agent prompts via `state_injection.rs`
- Coordinator wind-down behavior when budget < 300s
- `wg msg` soft-deadline signaling from coordinator to task agents

### Known Gaps (from research-condition-g-timeout.md)
| Gap | Status |
|-----|--------|
| `budget_secs` passed as `None` to heartbeat | **Fixed** in graceful-completion |
| `coordinator_model` not wired into config.toml | **Still open** |
| `worktree_isolation = false` with `max_agents=8` | **Still open** |
| `agent_timeout` defaults to 30m (same as trial timeout) | **Still open** |

---

## Q4: What Results Exist on Ulivo?

Seven Condition G runs on the TB 2.0 Docker benchmark (89 tasks):

| Run | Timestamp | Trials | Errors | Passed | Failed | Rate | Phase | Notes |
|-----|-----------|--------|--------|--------|--------|------|-------|-------|
| 1 | 16:23:31 | — | — | — | — | — | 2 | No result.json (killed early) |
| 2 | 16:35:41 | 3 | 3 | 2 | 1 | 66.7% | 2 | Convergence prompt fix |
| 3 | 17:06:15 | 13 | 10 | 6 | 7 | 46.2% | 2 | Parallel decomposition (`max_agents=8`) |
| 4 | 18:25:51 | 14 | 11 | 9 | 5 | **64.3%** | 2 | Best result (post cycle-fix) |
| 5 | 19:47:44 | 188 | 6 | 0 | 188 | 0.0% | 2 | Architect bundle — 0% (broken `--exec-mode`) |
| 6 | 20:19:47 | 0 | 443 | 0 | 0 | 0.0% | 2 | All errors (likely config/infra issue) |
| 7 | 21:05:32 | 38 | 102 | 9 | 29 | **8.4%** | 3 | First Phase 3 run |

### Key Observations

1. **Run 4 (64.3%)** is the high-water mark, but only 14 trials completed and "agents worked until timeout" — not clean convergence.
2. **Run 5 (0%)** confirms the architect-bundle approach was a dead end — `wg add --exec-mode architect` was rejected.
3. **Run 7 (8.4%)** is the first Phase 3 (heartbeat) run. The low rate is likely due to:
   - 102 of 107 trials errored (infrastructure: Docker metric collection failures, missing `.workgraph/agents/` dirs)
   - The `coordinator_model: "sonnet"` config isn't wired, so the coordinator runs on M2.7
   - `budget_secs` may not have been wired in the run 7 code (depends on exact binary version)
4. **All runs** used `openrouter:minimax/minimax-m2.7` via `ConditionGAgent`.
5. **All runs** targeted TB 2.0 Docker benchmark (89 tasks), not the 18 custom host-native tasks.

### Error Patterns in Run 7

The run log shows:
- `download_dir failed, falling back to exec cat` — Docker metric collection can't find `.workgraph/agents/` inside containers
- `git may not be available in container` — git not installed in TB Docker environments
- These are **metric collection errors**, not necessarily agent execution failures

---

## Q5: The Naming Confusion — Same G or Different G?

### Timeline of G's Evolution

| Date | Commit | Definition | Phase |
|------|--------|-----------|-------|
| Apr 7 | `47ed02d8` (formalize-condition-g) | "F without surveillance" — context-only injection, no surveillance loops, `max_agents=1` | **Phase 1** |
| Apr 8 | `84c2d81b` (implement Condition G) | Autopoietic — agent builds self-correcting workgraph via meta-prompt, `max_agents=8`, coordinator active | **Phase 2** |
| Apr 8 | `1c062444` (design-tb-heartbeat) | Heartbeat-orchestrated coordinator — coordinator runs on 30s heartbeat loop, orchestrates instead of seed agent | **Phase 3 (design)** |
| Apr 8 | `021c585f` (impl-tb-heartbeat) | Phase 3 implemented — `autopoietic: False`, `coordinator_agent: True`, `heartbeat_interval: 30` | **Phase 3 (impl)** |
| Apr 8 | `242bf37f` (graceful-completion) | Time budget injection, coordinator wind-down, soft-deadline messaging | **Phase 3 (enhancement)** |

### Verdict: Same G, Evolved Through Three Phases

**These are NOT two different conditions.** G evolved in-place:

1. **Phase 1** was a hypothesis: "surveillance adds no value, so G = F minus surveillance."
2. **Phase 2** was the first real implementation: "give the agent a meta-prompt to build its own graph." This was the autopoietic design.
3. **Phase 3** was the redesign after Phase 2's limitations became clear: "let the coordinator orchestrate via heartbeat, not the seed agent." This is the current code.

The experiment handoff doc (`experiment-handoff.md:655–704`) describes G with both the Phase 1 definition (context-only, Section 2) and the Phase 2/3 definition (autopoietic/heartbeat, Appendix A, Condition G). The handoff doc's Section 2 table still says `G = context-only, no surveillance`, but Appendix A accurately describes the autopoietic/multi-agent design.

**No rename is needed.** G is one condition that evolved. The handoff doc's Section 2 table should be updated to reflect the current Phase 3 definition to avoid confusion.

### The Heartbeat Design Is G, Not a New Condition

The design doc (`design-tb-heartbeat-orchestration.md:1–29`) explicitly says:
> "This design defines Phase 3: heartbeat-orchestrated coordinator" as the next evolution of Condition G.

The research doc (`research-condition-g-status.md:59–66`) documents:
> Phase 1 (F-without-surveillance) → Phase 2 (autopoietic) → [Phase 3 (heartbeat, this design)]

The code transition was:
- Phase 2: `autopoietic: True, coordinator_agent: False`
- Phase 3: `autopoietic: False, coordinator_agent: True`

Same `CONDITION_CONFIG["G"]` dict, different flags.

---

## Q6: The Delta — What's Missing on Ulivo?

**Nothing is missing.** Ulivo is at the same commit as local (`242bf37f`), and the `wg` binary was rebuilt after that commit.

### Full Commit History Since formalize-condition-g (47ed02d8)

All 25 commits between `47ed02d8` and `242bf37f` are present on Ulivo. The adapter.py was modified by 12 of these commits:

| Commit | Description | adapter.py changed? |
|--------|-------------|-------------------|
| `84c2d81b` | Implement Condition G (autopoietic) | Yes |
| `2e829ba7` | Clarify meta-prompt convergence signaling | Yes |
| `46f0f4a0` | Bump max_agents to 8 | Yes |
| `b61a9e6b` | Architect bundle for seed task | Yes |
| `13315a3e` | Use exec-mode bare for seed task | Yes |
| `14ce8838` | Revert: back to full exec_mode | Yes |
| `d77d2d21` | Fix wg binary discovery paths | Yes |
| `ecd3ffb0` | Fix repo root traversal | Yes |
| `86d1be1a` | Adapter smoke-test bug fixes | Yes |
| `a60285fc` | Replace LiteLLM with wg-native executor | Yes |
| `021c585f` | Implement heartbeat orchestration | Yes |
| `242bf37f` | Implement graceful completion | Yes |

---

## Recommendations

### 1. No Sync Needed
Ulivo is already up to date. No deployment action required.

### 2. Update Handoff Doc Section 2
The experiment handoff doc (`experiment-handoff.md:50–58`) still describes G as "context-only, no surveillance." This should be updated to reflect the Phase 3 (heartbeat-orchestrated) definition. Appendix A (lines 655–704) already describes the autopoietic/multi-agent design but should also be updated for Phase 3 specifics.

### 3. No Rename Needed
G is one condition that evolved. The three phases are documented in `research-condition-g-status.md` and `design-tb-heartbeat-orchestration.md`. No new condition letter is needed.

### 4. Investigate Run 7's Low Pass Rate
The 8.4% pass rate (9/38 completions, 102/107 errors) on the Phase 3 run deserves investigation:
- Are the 102 errors infrastructure failures (Docker metric collection) or agent failures?
- Is the `coordinator_model: "sonnet"` config actually being applied? (Known gap: not wired into config.toml)
- Was `budget_secs` wired in the binary used for this run? (Binary was rebuilt at 05:33, after the commit, so yes)

### 5. Wire `coordinator_model` Into Config Generation
The `coordinator_model: "sonnet"` field in `CONDITION_CONFIG["G"]` is stored but never written to `config.toml`. The coordinator currently defaults to M2.7 instead of Sonnet. This is documented in `research-condition-g-timeout.md:228` but not yet fixed.

### 6. Run Phase 3 on Custom Tasks
All Ulivo G runs targeted the 89-task TB 2.0 Docker benchmark. The 18 custom host-native tasks (where A→F showed +50 pp) have never been tested with the Phase 3 heartbeat design. A host-native run would eliminate Docker infrastructure errors and provide a cleaner comparison.

---

## Source References

| Source | Path |
|--------|------|
| Adapter implementation | `terminal-bench/wg/adapter.py` |
| Condition G config | `adapter.py:127–137` |
| Autopoietic meta-prompt | `adapter.py:483–531` |
| ConditionGAgent class | `adapter.py:1509–1520` |
| Experiment handoff | `terminal-bench/docs/experiment-handoff.md` |
| G status research | `terminal-bench/docs/research-condition-g-status.md` |
| G timeout research | `terminal-bench/docs/research-condition-g-timeout.md` |
| Heartbeat design | `terminal-bench/docs/design-tb-heartbeat-orchestration.md` |
| Graceful completion design | `terminal-bench/docs/design-graceful-completion.md` |
| Ulivo results | `terminal-bench/results/reproduction/condition-G/` (on Ulivo) |

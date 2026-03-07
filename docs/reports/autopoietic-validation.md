# Autopoietic Validation Report

**Date:** 2026-03-07
**Task:** autopoietic-validation
**Sources:** validate-core-dispatch, validate-safety-resilience, validate-agency-pipeline, validate-tui-observability

## 1. Executive Summary

Workgraph is a functional self-organizing system: it dispatches its own tasks, evaluates its own output, evolves its own agent prompts, and recovers from failures without human intervention in the common case. The core loop (spec -> implement -> evaluate -> evolve) runs end-to-end. Key gaps remain in compactor runtime (code exists but no artifacts yet), firehose observability (in-progress), and the evolver has not yet auto-triggered — its runs were manual invocations rather than coordinator-initiated cycles.

## 2. Per-Subsystem Maturity Scores

| Subsystem | Score | Rationale |
|-----------|-------|-----------|
| **Safety & resilience** | 4 | All safety commands work (fail/retry/abandon/pause/resume). Service guard prevents agent self-destruction. Zero-output circuit breakers with 3-layer detection. Stop/start recovery preserves state. No retract/cascade-stop as named commands but equivalent behavior exists. |
| **Infrastructure (locking, liveness, compactor)** | 3 | Flock-based atomic graph mutations prevent TOCTOU races. Dead-agent detection and cleanup automatic. Compactor code + 11 unit tests pass but **zero runtime artifacts produced** — no context.md, no state.json. Recently wired; expected but not yet proven. |
| **Agency (eval, FLIP, evolver, dispatch)** | 4 | 1172 evals with non-degenerate distribution (mean 0.806, 77 unique score values). 233 FLIP evals with verification pipeline (14 .verify-flip-* tasks, 12 done). Evolver consumed 831 evals and produced 12 data-driven amendments across 2 runs. Self-dispatch loop ~2s latency. Model routing correct across 3 roles (opus/haiku/sonnet). Evolver auto-trigger not yet activated (no evolver_state.json). |
| **Human integration (notifications, webhooks)** | 2 | Notification channel trait exists. Telegram and webhook notification tasks completed but scored low on FLIP (0.29-0.46), triggering verify tasks. No validation report covers runtime notification delivery. No evidence of notifications actually firing in production. |
| **TUI & observability** | 4 | Inspector cycling with slide animation, markdown rendering (pulldown-cmark + syntect), token novel/cached split, lifecycle indicators with pink coloring, graph health via status bar — all verified with tests. Firehose tab in-progress (6 compile errors when that branch merges, but the feature is actively being built by agent-7297). |
| **Task lifecycle** | 4 | Cascade abandon, retry+re-eval, supersession, zombie detection (11 integration tests), done-blocks-on-incomplete-deps, PendingValidation status — all pass. Full lifecycle from creation through evaluation. |
| **Service management** | 4 | Coordinator tick loop with 7 phases. Stop/start/restart with config preservation. Agent guard prevents self-shutdown. Max-agent limits, poll interval config. Force-start for recovery. One gap: no graceful drain (stop kills immediately rather than waiting for agents to finish current work). |
| **Agent coordination (git, concurrency)** | 3 | TOCTOU fix for duplicate agent spawn (alive-agent check + atomic mutate_graph claim). 155 commits with strong conventional commit discipline (48% with task ID tracing). However: 38 stashes accumulated (root-caused, guidelines written, not yet cleaned up). Worktree isolation designed but not yet eliminating all conflicts. |

**Overall: 3.5** — Functional and largely self-sustaining, with specific gaps that prevent a "4" rating.

## 3. Cross-Track Interactions

| Interaction | Status | Evidence |
|-------------|--------|----------|
| **Liveness -> Remediation** | Working | `zero_output` sweep runs in coordinator Phase 1.3. Kills zombie agents, resets tasks for respawn. Circuit breaker fails tasks after 2 consecutive zero-output events. Verified by 17 unit tests. |
| **Validation -> Evaluation** | Working | `build_auto_evaluate_tasks` creates `.evaluate-*` tasks for `PendingValidation` tasks. Observed in core-dispatch test: probe task completed -> `.evaluate-validation-probe-self` auto-created and dispatched with eval-inline haiku within seconds. |
| **Self-decomposition -> Scheduling** | Working | Coordinator auto-dispatches system tasks (`.assign-*`, `.evaluate-*`). Model routing directs `.assign-*` to Assigner role, `.evaluate-*` to haiku. Verified by fix-assign-model (commit 8bbc38b). |
| **Atomic task addition -> Concurrency** | Working | `mutate_graph` with flock serialization prevents concurrent graph corruption. Fix-duplicate-agent (commit 78d084f) added alive-agent check + atomic claim to prevent TOCTOU races in spawn. |
| **Eval -> Evolver -> Prompt** | Partial | Data flows: 831 evals consumed, 12 amendments generated. But `evolved-amendments.md` is currently empty — amendments were produced in evolution runs but not yet applied to active prompts. The loop doesn't fully close. |
| **FLIP -> Verification -> Quality** | Working | Low-FLIP scores (<0.70) trigger `.verify-flip-*` tasks. 14 verification tasks created, 12 completed. Verification results feed back into task status (fail/pass). |

**Key finding:** The eval->evolver->prompt loop is the weakest cross-track interaction. The evolver produces amendments but they aren't being consumed by agents yet (empty evolved-amendments.md, no auto-trigger). This means the system learns from its mistakes but doesn't yet automatically apply those lessons.

## 4. Failures and Gaps

### Hard Failures
- None — no subsystem is broken.

### Significant Gaps

1. **Compactor has no runtime output.** Code and tests exist. Recently wired into coordinator. But no `context.md` or `state.json` has been generated. Agents are operating without compacted context, which means each agent session starts without historical knowledge of what previous agents learned.

2. **Evolver auto-trigger never fired.** Two evolution runs exist but both appear to be manual `wg evolve` invocations. No `evolver_state.json` exists. The system can evolve but doesn't do so autonomously yet.

3. **Evolved amendments not applied.** `evolved-amendments.md` is empty despite 12 amendments being generated. The evolver's output isn't reaching agent prompts.

4. **Human integration unvalidated at runtime.** Telegram and webhook notification tasks completed but FLIP scored them 0.29-0.46. No evidence of notifications actually being delivered. This subsystem was not covered by any validation report.

5. **38 git stashes accumulated.** Root-caused and guidelines written, but the debt remains. Not actively causing problems but indicates past coordination issues.

6. **Firehose tab incomplete.** In-progress by another agent. 6 compile errors on the feature branch. Expected to resolve when `tui-firehose-inspector` completes.

### Minor Issues
- Service guard unit tests require `--test-threads=1` due to unsafe env var manipulation (benign test isolation issue).
- No graceful agent drain on service stop.
- Health badge uses status bar rather than upper-right widget (functional equivalent, aesthetic divergence from spec).

## 5. The Autopoietic Question

**Is the system genuinely self-sustaining?**

**Partially, with a critical gap in the learning loop.**

What works autonomously:
- **Task dispatch**: Create a task, coordinator picks it up in ~2 seconds, assigns an agent, agent executes, work gets done. No human needed.
- **Quality control**: Every completed task gets auto-evaluated. Low-quality work triggers FLIP verification. Failed verification can reject and restart work. This runs without human intervention.
- **Failure recovery**: Dead agents detected and cleaned up. Zero-output agents killed and tasks respawned. Circuit breakers prevent infinite respawn loops. Service survives stop/start cycles.
- **Cycle execution**: Iterative workflows run autonomously with convergence detection.

What still needs human intervention:
- **Evolution application**: The evolver produces amendments but a human must run `wg evolve` and ensure amendments reach agent prompts. The system observes its own performance but doesn't autonomously improve from those observations.
- **Compactor bootstrapping**: The compactor needs its first run to start generating context. Until then, agents lack institutional memory between sessions.
- **Strategic direction**: The system executes tasks but cannot set its own goals. Task creation still requires human or orchestrating-agent input for high-level objectives.
- **Conflict resolution**: Git stash accumulation shows the system doesn't fully handle concurrent file modifications. Worktree isolation is designed but not eliminating all conflicts.
- **Monitoring at scale**: The system can run many agents but a human still needs to watch for systemic issues (e.g., all agents scoring poorly, resource exhaustion).

**Honest assessment: The system is at the "mostly self-sustaining for execution" stage (score 3.5/5). It reliably converts task descriptions into completed work with quality checks. But the self-improvement loop (eval -> evolve -> apply -> measure) doesn't close automatically yet, which means it cannot get better at its own job without human help. A truly autopoietic system would close this loop.**

## 6. Recommended Next Steps

**Priority 1 — Close the learning loop:**
1. Wire evolver auto-trigger into coordinator tick (create `evolver_state.json`, set thresholds)
2. Ensure evolved amendments are written to and read from `evolved-amendments.md` by agent prompts
3. Verify compactor produces its first runtime artifacts and agents consume them

**Priority 2 — Harden coordination:**
4. Clean up 38 accumulated git stashes
5. Complete firehose tab (in-progress by agent-7297)
6. Add graceful drain to service stop (wait for agents to finish current phase)

**Priority 3 — Validate gaps:**
7. Runtime test of notification delivery (Telegram, webhook) — current FLIP scores suggest these may not work
8. Load test with >10 concurrent agents to validate coordination under pressure
9. End-to-end test of compactor -> agent context injection

**Priority 4 — Mature observability:**
10. Add health badge widget per original spec (upper-right, red/yellow/green)
11. Complete firehose multi-agent stream merging
12. Add system-level metrics (task throughput, mean time to completion, eval score trends over time)

---

## Source Reports

| Report | Location | Tests | Result |
|--------|----------|-------|--------|
| Core Dispatch | docs/reports/validate-core-dispatch.md | 3 live tests | ALL PASS |
| Safety & Resilience | docs/reports/validate-safety-resilience.md | 12 CLI + 19 unit tests | ALL PASS |
| Agency Pipeline | docs/reports/validate-agency-pipeline.md | 5 areas, 1172 evals analyzed | PASS (compactor partial) |
| TUI & Observability | docs/reports/validate-tui-observability.md | 7 items validated | 6/7 PASS (firehose in-progress) |

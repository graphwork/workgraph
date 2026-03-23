# Demo Medley: Screencast Scenario Catalog

*A library of short screencasts showing workgraph's capabilities — from first task to autopoietic loops.*

**Produced by:** research-demo-medley  
**Date:** 2026-03-23  
**Status:** Catalog complete, ready for production prioritization

---

## Table of Contents

1. [Catalog Summary](#catalog-summary)
2. [Scenario Catalog](#scenario-catalog) (14 scenarios)
3. [Priority Ordering](#priority-ordering)
4. [Website Gap Analysis](#website-gap-analysis)
5. [Freshness Strategy](#freshness-strategy)

---

## Catalog Summary

The existing website shows **3 fun demos** (heist, haiku, pancakes) that demonstrate basic fan-out/fan-in and pipeline patterns. These are excellent "wow" moments but leave major capability gaps:

- No demo of **cycles, validation gates, or the agency system**
- No demo of **human + AI collaboration** (all demos are fully autonomous)
- No demo of **CLI-first workflows** (all demos are TUI-only)
- No demo of **real software engineering** (all demos use toy/creative tasks)
- No demo of **analysis commands** (bottlenecks, forecast, impact, why-blocked)
- No demo of **multi-coordinator or cross-repo** workflows

The medley below fills these gaps with 14 scenarios organized into 4 tiers.

---

## Scenario Catalog

### Tier 1: Core Workflows (produce first — these explain what workgraph IS)

---

#### 1. First Five Minutes

| Field | Value |
|-------|-------|
| **Teaching purpose** | What workgraph does — from `wg init` to tasks flowing through agents. The "hello world." |
| **Duration** | 30–40s compressed |
| **Graph pattern** | Pipeline: `design-api → implement-api → write-tests` |
| **Graph setup** | Clean project, `wg init` from scratch |
| **Recording method** | **Simulated.** Pre-script the CLI commands with tmux send-keys. No live LLM calls needed — use `wg add` + `wg claim` + `wg done` to walk through the lifecycle. |
| **What viewer sees** | Shell: `wg init` → `wg add` (×3 with --after) → `wg viz` (ASCII graph) → `wg service start` → `wg tui` → tasks go open→in-progress→done. Exit TUI, `wg list` shows all done. |
| **Key moments** | (1) ASCII `wg viz` showing the dependency chain, (2) service auto-dispatching agents, (3) all-green completion in TUI |
| **Existing asset?** | No. The current demos skip init/setup and jump into TUI chat. |
| **Freshness** | Fully simulatable — no LLM dependency. Re-record with `wg claim`/`wg done` timed sleeps. |

---

#### 2. Chat-Driven Decomposition (existing — refresh)

| Field | Value |
|-------|-------|
| **Teaching purpose** | The coordinator decomposes a natural-language request into a task graph. One sentence in, structured parallel work out. |
| **Duration** | 35–45s compressed |
| **Graph pattern** | Fan-out → converge (3-way parallel + judge) |
| **Graph setup** | Clean demo project via `setup-demo.sh` |
| **Recording method** | **Live or simulated.** Live: `record-auto.sh haiku`. Simulated: `record-pancakes-sim.sh` approach with pre-created tasks + chat history. |
| **What viewer sees** | TUI opens → user types prompt → coordinator streams response → tasks materialize in graph → agents race in parallel → convergence task completes. |
| **Key moments** | (1) Typing a one-liner prompt, (2) tasks appearing in real-time, (3) three agents running simultaneously |
| **Existing asset?** | **Yes** — haiku.cast, heist.cast, pancakes.cast exist. Keep the best one (haiku recommended) and retire the others from the hero carousel to make room for new demos. |
| **Freshness** | Simulated path is deterministic. Live path depends on LLM but compress-cast.py normalizes timing. |

---

#### 3. Validation Gates

| Field | Value |
|-------|-------|
| **Teaching purpose** | Tasks can require validation before completion. Shows the `--verify` flag, `pending-validation` status, and `wg approve` / `wg reject` flow. |
| **Duration** | 25–35s compressed |
| **Graph pattern** | Pipeline with gate: `implement-feature → (pending-validation) → deploy` |
| **Graph setup** | Pre-create 3 tasks. First task has `--verify "cargo test test_feature passes"`. |
| **Recording method** | **Simulated.** `wg add` with --verify → `wg claim` → `wg done` (transitions to pending-validation) → show status → `wg approve` → downstream unblocks. |
| **What viewer sees** | CLI or TUI: task completes but pauses at "pending-validation" (yellow/amber). Human runs `wg approve`. Task flips to done, downstream starts. |
| **Key moments** | (1) The pending-validation pause — work stops until reviewed, (2) `wg approve` unblocking the next task |
| **Existing asset?** | No. None of the current demos show validation. |
| **Freshness** | Fully simulatable. No LLM needed. |

---

#### 4. Dependency Analysis

| Field | Value |
|-------|-------|
| **Teaching purpose** | Workgraph isn't just task execution — it's a thinking tool. Show `wg why-blocked`, `wg impact`, `wg bottlenecks`, `wg forecast`. |
| **Duration** | 25–30s compressed |
| **Graph pattern** | Diamond: 1 root → 3 parallel → 1 integrator, with one task blocked |
| **Graph setup** | Pre-create 5 tasks in diamond pattern. Mark root and one branch as done. One branch still in-progress (creating a bottleneck). |
| **Recording method** | **Simulated.** Pre-create graph state, then run analysis commands in sequence. |
| **What viewer sees** | Shell: `wg viz` (see the shape) → `wg why-blocked integrator` (shows which dep is pending) → `wg impact slow-branch` (shows what's downstream) → `wg bottlenecks` (highlights the slow branch) → `wg forecast` (shows estimated completion). |
| **Key moments** | (1) `wg why-blocked` pinpointing the exact blocker, (2) `wg bottlenecks` naming the critical path |
| **Existing asset?** | No. Zero analysis commands shown anywhere on the website. |
| **Freshness** | Fully simulatable. Deterministic graph state + CLI output. |

---

### Tier 2: Intermediate Patterns (produce second — differentiate from flat task managers)

---

#### 5. Cycle / Iteration Loop

| Field | Value |
|-------|-------|
| **Teaching purpose** | Workgraph supports cycles — not just DAGs. Show a write→review→revise loop that iterates until converged. |
| **Duration** | 30–40s compressed |
| **Graph pattern** | Cycle: `write-draft → review-draft → revise-draft → write-draft` with `--max-iterations 3` |
| **Graph setup** | Create cycle with `wg add` + back-edge. Use `wg cycles` to show detected cycle. |
| **Recording method** | **Simulated.** Walk through 2 iterations: write→review→revise (iteration 1, loop resets) → write→review (iteration 2, `wg done --converged`). |
| **What viewer sees** | `wg viz` showing the cycle arrow (yellow back-edge). Tasks progress through iteration 1, reset to open, start iteration 2. Agent calls `wg done --converged` to break the loop. `wg show` displays `loop_iteration: 2`. |
| **Key moments** | (1) Yellow cycle edge in `wg viz` or TUI, (2) tasks resetting for iteration 2, (3) `--converged` cleanly ending the cycle |
| **Existing asset?** | No. Cycles are a major differentiator but completely undemonstrated. |
| **Freshness** | Fully simulatable. |

---

#### 6. Human + AI Side-by-Side

| Field | Value |
|-------|-------|
| **Teaching purpose** | Workgraph isn't just for AI. A human claims one task while agents handle others. Shows mixed coordination. |
| **Duration** | 30–40s compressed |
| **Graph pattern** | Diamond: `research → [human: design-api, agent: write-scaffolding] → integrate` |
| **Graph setup** | Pre-create 4 tasks. Start service with max_agents 2. |
| **Recording method** | **Semi-live.** Service dispatches agents to `research` and `write-scaffolding`. Human runs `wg claim design-api --actor erik`, logs progress with `wg log`, then `wg done`. `integrate` starts automatically after both finish. |
| **What viewer sees** | Split view: TUI showing agents working + shell showing human claiming/logging/completing. Both converge on the integration task. |
| **Key moments** | (1) Human `wg claim` alongside running agents, (2) `wg agents` showing both human and AI listed, (3) integration task auto-starting when both sides finish |
| **Existing asset?** | No. All existing demos are fully autonomous. |
| **Freshness** | Simulated portion is deterministic. Human interaction is scripted via tmux. |

---

#### 7. Edge Tracing in the TUI

| Field | Value |
|-------|-------|
| **Teaching purpose** | The TUI's trace mode (`t` key) highlights upstream (magenta) and downstream (cyan) dependencies. Shows how to navigate complex graphs. |
| **Duration** | 20–25s compressed |
| **Graph pattern** | Diamond or larger (6+ tasks) with cross-dependencies |
| **Graph setup** | Pre-create a moderately complex graph (6-8 tasks, mix of done/in-progress/open). |
| **Recording method** | **Simulated.** Open TUI, arrow-key through tasks, press `t` to toggle trace, select different nodes to show changing edge colors. |
| **What viewer sees** | TUI graph. User selects a task → magenta edges light up (what it depends on). Selects another → cyan edges light up (what depends on it). A central "hub" task shows both colors simultaneously. |
| **Key moments** | (1) Pressing `t` and seeing edges light up, (2) magenta vs cyan distinction, (3) navigating to a different node and seeing the trace shift |
| **Existing asset?** | Partially — the storyboard v2 includes trace as segment 4, but it's embedded in the haiku demo. This would be a standalone, focused demo. |
| **Freshness** | Fully simulatable. Pre-built graph state + scripted keystrokes. |

---

#### 8. Service Lifecycle

| Field | Value |
|-------|-------|
| **Teaching purpose** | How `wg service start` works — daemon spawns agents, handles failures, respects max_agents. Shows operational model. |
| **Duration** | 25–35s compressed |
| **Graph pattern** | 5 independent tasks (no deps) to show max_agents throttling |
| **Graph setup** | Pre-create 5 open tasks. Configure `max_agents 3`. |
| **Recording method** | **Semi-live.** `wg service start` → `wg service status` → `wg agents` (shows 3 of 5 claimed) → tasks complete → remaining 2 get claimed → all done → `wg service stop`. |
| **What viewer sees** | Shell commands showing service starting, agents spawning up to the limit, tasks draining through the pool. `wg agents` and `wg service status` provide live monitoring. |
| **Key moments** | (1) Only 3 agents spawn despite 5 ready tasks (max_agents throttle), (2) new agents auto-spawn as slots open, (3) clean service stop |
| **Existing asset?** | No. Service is used implicitly in other demos but never explained. |
| **Freshness** | Semi-live (needs real agent spawning for authenticity) but timing can be compressed. Simulated fallback: `wg claim` + sleep + `wg done`. |

---

### Tier 3: Advanced Features (produce third — for users evaluating workgraph deeply)

---

#### 9. Agency System: Roles, Assignment, Evaluation

| Field | Value |
|-------|-------|
| **Teaching purpose** | Agents aren't generic — they have roles (skills + desired outcomes) and tradeoffs (constraints). The system auto-assigns based on fit and evaluates results. |
| **Duration** | 35–45s compressed |
| **Graph pattern** | 3 tasks with different skill requirements (coding, docs, testing) |
| **Graph setup** | `wg agency init` → show seeded roles. Create 3 tasks with different `--skill` tags. |
| **Recording method** | **Simulated.** `wg agency init` → `wg agency list-roles` → `wg add` tasks with skills → service assigns agents → `wg show <task>` to see assigned agent details → `wg evaluate run <task>` → show evaluation scores. |
| **What viewer sees** | Agency initialization, role listing, automatic assignment with rationale visible, evaluation scores after completion. |
| **Key moments** | (1) Different agents auto-assigned to matching tasks, (2) evaluation showing per-dimension scores, (3) `wg evolve run` creating new role variants based on performance |
| **Existing asset?** | No. The agency system is documented but never demonstrated visually. |
| **Freshness** | Mostly simulatable. Evaluation needs LLM but can use cached results. |

---

#### 10. Task Messaging

| Field | Value |
|-------|-------|
| **Teaching purpose** | Agents can send messages to tasks — questions, status updates, coordination signals. Shows `wg msg send` / `wg msg read`. |
| **Duration** | 20–25s compressed |
| **Graph pattern** | Pipeline: `research → implement` where research agent sends findings to implement task |
| **Graph setup** | Pre-create 2 tasks. Research task in-progress. |
| **Recording method** | **Simulated.** Agent on research sends `wg msg send implement "Found that we need to use async — here's the approach..."`. Agent on implement reads messages, replies. |
| **What viewer sees** | TUI Messages tab or CLI: message sent from one task, read from another. A coordination dialog between tasks without direct agent-to-agent communication. |
| **Key moments** | (1) Message appearing in the TUI Messages tab, (2) reply flow, (3) the fact that the graph itself is the communication medium (stigmergy) |
| **Existing asset?** | No. Messaging is invisible in all current demos. |
| **Freshness** | Fully simulatable. |

---

#### 11. Workgraph Building Workgraph (Self-Hosting)

| Field | Value |
|-------|-------|
| **Teaching purpose** | The ultimate proof of capability — workgraph coordinating its own development. Shows real software engineering, not toy tasks. |
| **Duration** | 40–50s compressed |
| **Graph pattern** | Real graph from `.workgraph/graph.jsonl` — whatever's current |
| **Graph setup** | Use the actual workgraph project's own graph. Filter to an interesting recent subgraph (e.g., the screencast work itself, or a recent feature branch). |
| **Recording method** | **Live capture from real session.** Record `wg tui` during actual development. Or replay: extract a subgraph from history, recreate it in a demo project, and simulate the progression. |
| **What viewer sees** | A real, complex task graph with 10+ tasks. Multiple agents working on Rust code. Tasks completing with real commit hashes in logs. The graph is visibly non-trivial — this isn't a toy. |
| **Key moments** | (1) Scale — many tasks, real dependencies, (2) agent logs showing actual code commits, (3) the meta moment: "this screencast was coordinated by the tool it's demonstrating" |
| **Existing asset?** | No, but raw material exists in the current `.workgraph/graph.jsonl`. Could extract and replay. |
| **Freshness** | Semi-live. Capture during actual development sessions. New versions captured naturally as the project evolves. |

---

#### 12. Worktree Isolation

| Field | Value |
|-------|-------|
| **Teaching purpose** | Multiple agents can work simultaneously without conflicts because each gets its own git worktree. Shows the isolation mechanism. |
| **Duration** | 20–30s compressed |
| **Graph pattern** | 2-3 parallel tasks modifying different files |
| **Graph setup** | Pre-create parallel tasks that each modify a different file. |
| **Recording method** | **Semi-live.** Start service → `ls .git/worktrees/` showing isolated worktrees → `wg agents` showing each agent in its own worktree → agents complete → changes merged back. |
| **What viewer sees** | Multiple worktree directories appearing, agents working in isolation, clean merge back to main. |
| **Key moments** | (1) The worktree directory listing, (2) parallel agents making commits in different worktrees, (3) no merge conflicts |
| **Existing asset?** | No. Isolation is invisible in all current demos despite being critical infrastructure. |
| **Freshness** | Semi-live. Needs real agent spawning for worktree creation. |

---

### Tier 4: Ecosystem & Advanced (produce last — for power users and evaluators)

---

#### 13. Coordinator Chat Room

| Field | Value |
|-------|-------|
| **Teaching purpose** | The coordinator isn't just a dispatcher — it's a conversational partner. Show multi-turn chat: ask a question, get a plan, refine it, then execute. |
| **Duration** | 35–45s compressed |
| **Graph pattern** | Evolving — starts with a question, coordinator proposes graph, user refines, final graph executes |
| **Graph setup** | Clean demo project. |
| **Recording method** | **Live or semi-live.** User chats: "I need to add auth to my API" → coordinator proposes tasks → user says "add rate limiting too" → coordinator adds tasks → `wg service start` dispatches. |
| **What viewer sees** | TUI chat panel: multi-turn conversation. Graph panel: tasks appearing incrementally as the plan evolves. The plan isn't fixed upfront — it's negotiated. |
| **Key moments** | (1) Iterative refinement — the graph grows through conversation, (2) coordinator adjusting the plan based on feedback, (3) seamless transition from planning to execution |
| **Existing asset?** | No. Current demos show single-prompt → execution. No iterative planning. |
| **Freshness** | Live path depends on LLM. Semi-live: pre-populate chat history showing the conversation, then show the resulting graph. |

---

#### 14. Notification Integration

| Field | Value |
|-------|-------|
| **Teaching purpose** | Workgraph can notify humans via Matrix, Slack, email, or webhook when tasks complete or need attention. Shows the operational integration story. |
| **Duration** | 20–25s compressed |
| **Graph pattern** | Pipeline with a `--verify` gate that triggers a notification |
| **Graph setup** | Configure notification backend (Matrix or webhook for demo). Create task with --verify. |
| **Recording method** | **Semi-live.** Task completes → pending-validation → notification appears in Matrix/Slack/email. Human approves from CLI. |
| **What viewer sees** | Split screen: TUI showing task reaching pending-validation + phone/chat showing notification arriving. Human `wg approve` from CLI. |
| **Key moments** | (1) Notification arriving in a real chat app, (2) the "human in the loop" moment where a phone buzz triggers action |
| **Existing asset?** | No. Notifications are documented but never demonstrated. |
| **Freshness** | Requires configured notification backend. Record once, re-record only if notification format changes. |

---

## Priority Ordering

Production order balances **teaching value** (what does a new user need to see first?), **implementation effort** (simulated vs live), and **gap severity** (what's most missing from the current website?).

| Priority | Scenario | Tier | Effort | Rationale |
|----------|----------|------|--------|-----------|
| **P1** | 1. First Five Minutes | Core | Low (simulated) | Every product needs a "getting started" demo. Current demos skip this entirely. |
| **P2** | 3. Validation Gates | Core | Low (simulated) | Major differentiator — shows workgraph isn't just "run tasks" but has quality gates. |
| **P3** | 5. Cycle / Iteration Loop | Intermediate | Low (simulated) | The "not just a DAG" story. Cycles are a headline feature with zero demos. |
| **P4** | 4. Dependency Analysis | Core | Low (simulated) | Shows workgraph as a thinking tool, not just an executor. Zero coverage today. |
| **P5** | 7. Edge Tracing in TUI | Intermediate | Low (simulated) | Visual, impressive, quick to produce. Already partially designed in storyboard v2. |
| **P6** | 2. Chat-Driven Decomposition | Core | Low (existing) | Refresh the best existing demo (haiku). Retire heist+pancakes from hero carousel. |
| **P7** | 6. Human + AI Side-by-Side | Intermediate | Medium (semi-live) | Critical narrative: workgraph isn't "replace humans" — it's "coordinate everyone." |
| **P8** | 8. Service Lifecycle | Intermediate | Medium (semi-live) | Explains the operational model. Important for adoption but less visually exciting. |
| **P9** | 10. Task Messaging | Advanced | Low (simulated) | Quick to produce, shows stigmergic coordination. |
| **P10** | 9. Agency System | Advanced | Medium (needs LLM for eval) | Complex feature, deserves a thorough demo. Needs more setup. |
| **P11** | 11. Self-Hosting | Advanced | High (live capture) | The most compelling "proof" demo but hardest to produce reliably. |
| **P12** | 12. Worktree Isolation | Advanced | Medium (semi-live) | Technical detail that matters for trust. Can wait until other demos exist. |
| **P13** | 13. Coordinator Chat Room | Ecosystem | Medium (semi-live) | Multi-turn chat is powerful but harder to compress into 45s. |
| **P14** | 14. Notification Integration | Ecosystem | Medium (needs infra) | Nice-to-have. Requires external service setup. |

### Recommended Batches

- **Batch 1 (Week 1):** Scenarios 1, 3, 4, 5 — all fully simulatable, no LLM needed. Can be produced in a single session using `record-pancakes-sim.sh` as a template.
- **Batch 2 (Week 2):** Scenarios 2 (refresh), 6, 7, 8 — mix of existing refresh and new semi-live recordings.
- **Batch 3 (Week 3+):** Scenarios 9–14 — advanced features, higher production effort.

---

## Website Gap Analysis

### What the current website shows well
- **Parallel execution:** Three demos all show fan-out/fan-in patterns with agents racing.
- **TUI visual appeal:** The asciinema player with Dracula theme looks polished.
- **Coordinator chat:** The haiku demo shows natural-language → task decomposition.
- **Auto-advance carousel:** The tab carousel with progress bar is smooth UX.

### What the current website does NOT explain

| Gap | Severity | Which demo fills it |
|-----|----------|-------------------|
| **How to get started** — no init/setup/first-task flow shown | Critical | #1 First Five Minutes |
| **Validation / quality gates** — `--verify`, pending-validation, approve/reject invisible | Critical | #3 Validation Gates |
| **Cycles** — headline feature, zero visibility. Visitors don't know workgraph supports loops | Critical | #5 Cycles |
| **Human participation** — all demos are fully autonomous, implying "this is just for AI" | High | #6 Human + AI |
| **Analysis tools** — why-blocked, bottlenecks, forecast, impact never shown | High | #4 Dependency Analysis |
| **CLI workflow** — every demo uses TUI. Users who prefer CLI see no path | High | #1, #3, #4 (all CLI-first) |
| **Service model** — how agents are spawned, throttled, monitored is opaque | Medium | #8 Service Lifecycle |
| **Agency system** — roles, tradeoffs, evaluation, evolution completely invisible | Medium | #9 Agency System |
| **Messaging** — inter-task communication not shown | Medium | #10 Messaging |
| **Worktree isolation** — critical for trust ("how do agents not conflict?") but invisible | Medium | #12 Worktree Isolation |
| **Real software work** — all demos use toy/creative tasks; no real engineering shown | Medium | #11 Self-Hosting |
| **Multi-turn planning** — demos show one-shot prompt, not iterative refinement | Low | #13 Chat Room |
| **Notifications** — operational integration story untold | Low | #14 Notifications |

### Specific website content recommendations

1. **Replace the 3-tab carousel with a 6-8 tab medley.** Current carousel has 3 very similar demos (all fun fan-out patterns). Replace with: First Five Minutes, Chat Decomposition (haiku refresh), Validation Gates, Cycles, Dependency Analysis, Human+AI. Each teaches something different.

2. **Add a "Features" section below the carousel** with short text + GIF/screenshot for each demo. The carousel auto-advances too fast for complex demos.

3. **Add a "Getting Started" screencast** prominently above or beside the hero carousel. New visitors need to see `wg init` → `wg add` → `wg service start` before they see the fancy TUI demos.

4. **Label each demo with its graph pattern** (already partially done with the `pattern` badge). Extend to include: "pipeline", "fan-out → converge", "cycle", "diamond + validation gate", etc.

---

## Freshness Strategy

### Problem
TUI rendering, CLI output format, and command syntax change as workgraph evolves. Screencasts become stale. Manual re-recording is expensive and error-prone.

### Solution: Simulation-first recording pipeline

All demos should be **simulatable** — meaning they can be re-recorded without live LLM calls by driving graph state transitions directly.

#### Architecture

```
scenario-spec.toml          # Declarative: tasks, deps, timing, keystrokes
        │
        ▼
record-sim.sh               # Generic runner: reads spec, drives tmux
        │
        ├── setup phase      # wg init, wg add (with --after, --verify, etc.)
        ├── progression phase # wg claim, sleep, wg done (timed transitions)
        ├── interaction phase # tmux send-keys for TUI navigation
        └── capture phase    # capture-tmux.py → raw .cast
        │
        ▼
compress-cast.py            # Time compression → final .cast
        │
        ▼
website/assets/casts/       # Deploy
```

#### Scenario Spec Format (proposed)

```toml
[meta]
name = "validation-gates"
duration_target = 30  # seconds, compressed
terminal = { cols = 120, rows = 36 }

[[tasks]]
id = "implement-feature"
title = "Implement auth endpoint"
verify = "cargo test test_auth passes"

[[tasks]]
id = "deploy-staging"
title = "Deploy to staging"
after = ["implement-feature"]

[[progression]]
action = "claim"
task = "implement-feature"
delay_before = 2.0  # seconds to wait before this action

[[progression]]
action = "done"
task = "implement-feature"
delay_before = 4.0

[[progression]]
action = "approve"
task = "implement-feature"
delay_before = 3.0

# ... etc

[[keystrokes]]
time = 0.0
keys = "wg tui\n"

[[keystrokes]]
time = 15.0
keys = ["Down", "Down", "t"]  # navigate + trace
```

#### CI Integration

```bash
# In CI or as a Makefile target:
for spec in screencast/scenarios/*.toml; do
    ./screencast/record-sim.sh "$spec"
done
# Compare checksums — if TUI rendering changed, flag for review
```

#### Freshness triggers

| Trigger | Action |
|---------|--------|
| TUI render code changes (`src/tui/`) | Re-record all TUI-based demos |
| CLI output format changes | Re-record CLI-based demos |
| New command added | Consider adding a demo for it |
| `compress-cast.py` updated | Re-compress all raw recordings |
| Release tag | Re-record full medley, publish to website |

#### Key design principles

1. **Simulation > Live recording.** Every demo that CAN be simulated SHOULD be simulated. Live LLM calls introduce nondeterminism, latency variance, and API cost.
2. **Spec is source of truth.** The `.toml` spec files are version-controlled. The `.cast` files are build artifacts.
3. **Raw + compressed.** Keep raw recordings alongside compressed ones. When `compress-cast.py` improves, re-compress from raw without re-recording.
4. **Idempotent.** Running the same spec twice should produce visually identical output (modulo timestamps in agent logs, which are cosmetic).

### Migration from current pipeline

The existing `record-auto.sh`, `record-heist-auto.sh`, and `record-pancakes-sim.sh` are good prototypes. The freshness strategy formalizes their patterns:

- `record-pancakes-sim.sh` → becomes the template for `record-sim.sh`
- `setup-demo.sh` → folded into the spec's setup phase
- `record-auto.sh` → kept for live-recording scenarios (#11 self-hosting, #13 chat room) that can't be fully simulated
- `compress-cast.py` → unchanged, shared across all scenarios

---

## Appendix: Recording Method Decision Tree

```
Is this demo fully deterministic (no LLM needed)?
├── YES → Simulated (record-sim.sh + scenario spec)
│         Examples: #1, #3, #4, #5, #7, #10
│         Pros: reproducible, fast, free, CI-friendly
│         Cons: doesn't show real agent output text
│
└── NO → Does it need real agent text/reasoning?
    ├── YES → Live (record-auto.sh + compress-cast.py)
    │         Examples: #2, #11, #13
    │         Pros: authentic, shows real AI output
    │         Cons: nondeterministic, API cost, harder to refresh
    │
    └── NO → Semi-live (simulated state + live service)
              Examples: #6, #8, #9, #12, #14
              Pros: real service behavior, reproducible graph shape
              Cons: needs running service, timing varies
```

---

*End of catalog. This document is an artifact of task research-demo-medley.*

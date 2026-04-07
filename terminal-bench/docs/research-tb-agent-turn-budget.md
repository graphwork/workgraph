# Research: TB Agent Turn Budget and Standard Practice Compliance

**Task:** research-tb-agent
**Date:** 2026-04-07
**Status:** Complete

---

## Q1: Does our Claude executor have a turn budget/max turns limit?

**Answer: The Claude executor itself has NO turn limit. But there IS a 30-minute timeout.**

The Claude executor (`src/commands/spawn/execution.rs:859-887`) spawns `claude --print` with no `--max-turns` flag — the Claude CLI doesn't even have such a flag. The Claude Code CLI has no built-in turn cap.

However, there are **two timeout mechanisms** that terminate agents:

1. **`coordinator.agent_timeout`** — default `"30m"` (`src/config.rs:2345-2347`). This wraps the agent command in `timeout --signal=TERM --kill-after=30 <secs> <command>` (`src/commands/spawn/execution.rs:441-445`).

2. **Executor-specific timeout** — can be set in executor config TOML (`settings.timeout`).

3. **CLI `--timeout` override** — on `wg spawn`.

Resolution cascade: CLI param > executor config > coordinator config (`src/commands/spawn/execution.rs:420-438`).

**The pilot-f-89 agent that ran ~9 hours and timed out at 59/90 trials was NOT hitting a turn limit.** The `run_pilot_f_89.py` script has its own `TRIAL_TIMEOUT = 1800` (30 min per trial, line 51). The 9-hour total runtime was 90 trials × ~6 min average. The single failure (`iterative-test-fix-r1`) hit the 1800s timeout after 137 turns and 1805 seconds — a genuine time exhaustion, not a turn cap.

**For our own native executor** (`wg native-exec`), there IS a `--max-turns` flag (default: 100, `src/cli.rs:1698-1699`), but this only applies to the Rust-native agent loop, not the Claude executor. When the coordinator spawns Claude agents, it does NOT use `wg native-exec`.

### Key files
- `src/commands/spawn/execution.rs:420-448` — timeout resolution
- `src/commands/spawn/execution.rs:859-887` — Claude executor command build (no --max-turns)
- `src/config.rs:2345-2347` — default agent timeout = "30m"
- `src/cli.rs:1698-1699` — native-exec --max-turns default = 100
- `terminal-bench/run_pilot_f_89.py:51` — TRIAL_TIMEOUT = 1800

---

## Q2: What are the EXACT requirements for a valid TB 2.0 leaderboard submission?

**Source:** `/home/erik/executors/workgraph/docs/terminal-bench/HOWTO-submit-to-leaderboard.md`

### Required directory structure
```
submissions/terminal-bench/2.0/<agent-name>__<model>/
  metadata.yaml        # Hand-written
  job-001/             # Harbor's raw output
    config.json
    trial-1/result.json   # Harbor-generated
    trial-2/result.json
    trial-3/result.json
    trial-4/result.json
    trial-5/result.json
```

### metadata.yaml format
```yaml
agent_url: https://github.com/graphwork/workgraph
agent_display_name: "Workgraph"
agent_org_display_name: "Poietic PBC"
models:
  - model_name: minimax-m2.7
    model_provider: openrouter
    model_display_name: "MiniMax-M2.7"
    model_org_display_name: "MiniMax"
```

### Bot validation checks (automated on PR)
1. `timeout_multiplier` must be `1.0`
2. No timeout or resource overrides
3. **Minimum 5 trials per task** (hard requirement)
4. Valid `result.json` files in every trial directory

### Rules
- Default timeouts: 15 min to 3.3 hours per task (from `task.toml`)
- Default resources: 1-2 CPUs, 2-4 GB RAM, 10 GB storage
- No overrides of any kind
- Multi-agent architectures allowed
- Retry/convergence loops within a trial allowed
- Agents cannot access tbench.ai or terminal-bench GitHub repo
- Must scrub API keys and proprietary prompts (submissions are public)

### Submission workflow
1. Fork `https://huggingface.co/datasets/harborframework/terminal-bench-2-leaderboard`
2. Add directory under `submissions/terminal-bench/2.0/`
3. Open PR → bot auto-validates → maintainer reviews → results appear on leaderboard

---

## Q3: How many trials per task does TB 2.0 require?

**Answer: 5 trials per task minimum.** This is a hard requirement enforced by the validation bot.

The HOWTO explicitly states: "Minimum 5 trials per task" and the example shows `harbor run ... -k 5`.

**Current state of our data:**

| Dataset | Tasks | Trials/task | Submittable? |
|---------|-------|-------------|-------------|
| pilot-a-89 | 89 | 1 | **NO** (need 5) |
| pilot-f-89 | 18 | 5 | **NO** (wrong format + only 18 tasks) |
| full-f-m27 | 89 | 3 | **NO** (need 5) |
| full-aprime-m27 | 89 | 3 | **NO** (need 5) |
| rerun-condition-a | 89 | 3 | **NO** (need 5) |
| rerun-condition-b | 89 | 3 | **NO** (need 5) |
| full-condition-c | 89 | 3 | **NO** (need 5) |

**No condition currently has 5 trials per task. All need additional trial runs.**

Source: `terminal-bench/docs/research-howto-submission-review.md:119-122`

---

## Q4: What is the full task set? 89 tasks? 18 unique × 5 replicas?

**Answer: Terminal-Bench 2.0 has exactly 89 tasks.** These are 89 unique, distinct tasks.

The "18 tasks × 5 replicas" was the pilot-f-89 design, NOT the full TB task set. pilot-f-89 ran a curated subset (8 calibration + 10 hard benchmark tasks = 18 unique tasks, each repeated 5 times = 90 trials).

For a valid leaderboard submission, you need all **89 tasks × 5 trials = 445 trials per condition**.

The full task list is defined in the Harbor dataset `terminal-bench@2.0`. Each task has a `task.toml` + Docker image in Harbor's package format.

The 18 custom tasks are NOT part of the TB 2.0 leaderboard — they're separate calibration/benchmark tasks for our own experiment.

Sources:
- `terminal-bench/docs/scale-experiment-design.md:52` — "89 tasks (canonical set)"
- `terminal-bench/docs/research-howto-submission-review.md:130-138`
- `terminal-bench/docs/pilot-results-synthesis.md:26` — "89 tasks x 1 replica" for A

---

## Q5: Are we running the standard TB harness (Harbor) or our own runner?

**Answer: It depends on the condition.**

### Harbor path (standard, submission-compatible)
- **Condition A** (pilot-a-89): Ran through Harbor via `reproduce.sh` using `ConditionAAgent` class in `terminal-bench/wg/adapter.py`. Produces standard Harbor `result.json` files. **This is the correct path for submissions.**
- **Conditions B, C**: Same Harbor path via `reproduce.sh`. Standard format.

### Custom runner (NOT submission-compatible)
- **Condition F** (pilot-f-89): Ran through `run_pilot_f_89.py`, a custom Python script that uses `wg service start` + `wg native-exec`. Produces `stats.json` files (not `result.json`), uses a non-Harbor directory structure (`f-file-ops-r0/stats.json` instead of `task-name__hash/result.json`). **This is NOT submission-compatible.**

### What the HOWTO says
The HOWTO is explicit: implement a `BaseAgent` subclass, run `harbor run`, and the output IS the submission. Harbor captures everything automatically.

**Our `reproduce.sh` (lines 104-114) does exactly this** for conditions A/B/C — it calls `harbor run` with the correct agent class. But it does NOT support Condition F. The `ConditionFAgent` class exists in `adapter.py` (line ~345) but `reproduce.sh` only handles A/B/C (lines 87-92).

Sources:
- `terminal-bench/reproduce.sh:87-92` — only A/B/C
- `terminal-bench/wg/adapter.py:548` — ConditionFAgent exists
- `terminal-bench/run_pilot_f_89.py` — custom runner, non-Harbor
- Submission format comparison: A has `result.json`, F has `stats.json`

---

## Q6: Is our Condition F runner actually compliant with TB submission requirements?

**Answer: NO. The pilot-f-89 runner is NOT compliant. Multiple violations.**

### Compliance audit (combining our data with `tb2-runtime-compliance-audit.md`)

| Requirement | Status | Detail |
|-------------|--------|--------|
| Harbor format (`result.json`) | **FAIL** | F produces `stats.json`, not `result.json` |
| Harbor directory structure | **FAIL** | F uses `f-task-rN/` not `task__hash/` |
| 89 tasks | **FAIL** | F ran 18 tasks, not 89 |
| 5 trials per task | **FAIL** | F has 5 trials per task but only for 18 tasks |
| No turn cap | **FAIL** | `reproduce.sh` has `MAX_TURNS=50` (affects A/B/C via Harbor) |
| Per-task timeouts | **FAIL** | `reproduce.sh` has flat `TIMEOUT=1800` overriding task.toml |
| `timeout_multiplier = 1.0` | PASS | Never overridden |
| Default resources | PASS | No resource overrides |
| Ran through Harbor | **FAIL** | F used custom `run_pilot_f_89.py` |

### What needs to change for a valid F submission

1. **Run F through Harbor.** Add Condition F to `reproduce.sh` (add `F) agent_class="wg.adapter:ConditionFAgent"` in the case statement).
2. **Fix `MAX_TURNS`.** Change from 50 to 1000000 in `reproduce.sh` line 30 (or remove `--max-turns` flag entirely). The TB2 reference agent uses max_turns=1,000,000.
3. **Fix `TIMEOUT`.** Remove `--timeout "$TIMEOUT"` from `reproduce.sh` line 112 so Harbor uses per-task `task.toml` timeouts (15 min to 3.3 hours).
4. **Run all 89 tasks × 5 trials.** Not just the 18-task subset.
5. **Verify `ConditionFAgent` works through Harbor.** It exists in `adapter.py` but has never been run through `harbor run` — only through the custom Python runner.

### Severity assessment
The turn cap (`MAX_TURNS=50`) is the most damaging violation. The `early-behavior-findings.md` found that **~45% of failed trials across ALL conditions hit the 50-turn limit** (`terminal-bench/analysis/early-behavior-findings.md:95`). This means we are artificially capping agent performance and our pass rates are lower than they should be.

Sources:
- `terminal-bench/analysis/tb2-runtime-compliance-audit.md` — full audit
- `terminal-bench/analysis/early-behavior-findings.md:87-95` — turn limit impact
- `terminal-bench/analysis/condition-f-final-design.md:364` — F's max_turns should be 1M

---

## Q7: What causes agent exit code 1 after long runs?

**Answer: Multiple possible causes, depending on executor type.**

### For Claude executor (wg's default)
The wrapper script (`src/commands/spawn/execution.rs:985-1103`) captures the exit code and handles it:

- **Exit code 0**: Agent succeeded → wrapper runs `wg done`
- **Exit code 124**: Killed by `timeout` command (hard timeout exceeded) → wrapper runs `wg fail --reason "Agent exceeded hard timeout"`
- **Any other non-zero exit (including 1)**: → wrapper runs `wg fail --reason "Agent exited with code $EXIT_CODE"`

Claude Code CLI (`claude --print`) returns exit code 1 when:
1. **API error** — rate limit, server error, network failure
2. **Context window exhaustion** — conversation too long for model's context
3. **Internal error** — unexpected crash or assertion failure
4. **Process killed** — by OOM killer, signal, or parent process

There is no explicit "max turns" exit from Claude CLI since it has no turn limit.

### For native executor (`wg native-exec`)
- **Max turns reached** (`src/executor/native/agent.rs:438-441`): Returns `[max turns reached]` as final text, but the exit code depends on whether the agent loop handled it gracefully. The journal records an `End` entry with reason `max_turns`.
- **Token/context exhaustion**: API returns error, agent loop catches and exits.

### For Harbor adapter (TB runs)
- **Turn limit**: `for turn in range(max_turns)` loop exits naturally → exit code 0 (adapter handles this internally)
- **LLM call failure**: `adapter.py:1462-1465` catches exceptions and breaks the loop
- **Timeout**: External `timeout` command kills the process → exit code 124

### In the pilot-f-89 case
The single failure (`iterative-test-fix-r1`) ran for 1805 seconds and 137 turns. It hit the `TRIAL_TIMEOUT = 1800` limit, which means the process was killed by the external timeout, producing exit code 124 (SIGTERM from timeout command), not exit code 1.

Sources:
- `src/commands/spawn/execution.rs:1065-1103` — wrapper script exit code handling
- `src/executor/native/agent.rs:438-441` — native max turns handling
- `terminal-bench/wg/adapter.py:1462-1465` — adapter error handling

---

## Honest Gap Assessment

### What IS compliant
- Condition A through Harbor produces valid `result.json` format ✅
- `timeout_multiplier = 1.0` ✅
- No resource overrides ✅
- Multi-agent architecture allowed (our design is valid) ✅
- `ConditionFAgent` class exists in `adapter.py` ✅

### What is NOT compliant
1. **Turn cap of 50** in `reproduce.sh` — should be 1,000,000 or removed. This is artificially degrading ALL conditions' pass rates. **HIGH severity.**
2. **Flat 30-min timeout** in `reproduce.sh` — should use per-task `task.toml` defaults. Extreme tasks need up to 3.3 hours. **HIGH severity.**
3. **Condition F has never run through Harbor** — all F data comes from a custom runner producing non-standard output. **BLOCKING for F submission.**
4. **No condition has 5 trials** — all have 1 or 3. Need 2-4 more trial runs per condition. **BLOCKING for any submission.**
5. **F submission directory doesn't exist** — no `metadata.yaml` for F in `leaderboard-submission/`. **Easy fix.**

### What this means
**Our existing pilot data cannot be submitted to the leaderboard.** The data is valuable for our own A-vs-F comparison research, but it is not in a format or quantity that the leaderboard bot will accept.

### Recommendation: Path to valid submission

**Phase 1 — Fix compliance (before running any more trials)**
1. Set `MAX_TURNS=1000000` in `reproduce.sh` (or remove `--max-turns` flag)
2. Remove `--timeout "$TIMEOUT"` from `reproduce.sh` `harbor run` invocation
3. Add `F) agent_class="wg.adapter:ConditionFAgent"` to `reproduce.sh` case statement
4. Verify `ConditionFAgent` works through Harbor (single-task smoke test)

**Phase 2 — Run compliant trials**
1. Run `harbor run ... -k 5` for each condition (A and F minimum)
2. This produces 89 × 5 = 445 trials per condition in Harbor's native format
3. Estimated time: ~8-16 hours per condition at 4-8 concurrent

**Phase 3 — Prepare and submit**
1. Create `metadata.yaml` for each condition
2. Copy Harbor output into submission directory structure
3. Audit for API key leakage
4. Fork HuggingFace repo, open PR

**Critical insight:** All existing trial data (3 trials per condition) must be re-run from scratch with compliant settings. The turn cap and timeout violations mean prior results are not valid reflections of agent capability, let alone submission-compatible.

---

## Files Referenced

| File | Purpose |
|------|---------|
| `/home/erik/executors/workgraph/docs/terminal-bench/HOWTO-submit-to-leaderboard.md` | Canonical submission guide |
| `terminal-bench/docs/scale-experiment-design.md` | Full-scale experiment design |
| `terminal-bench/docs/research-howto-submission-review.md` | Prior research on submission readiness |
| `terminal-bench/analysis/tb2-runtime-compliance-audit.md` | Runtime compliance audit |
| `terminal-bench/wg/AUDIT-adapter-bypass-points.md` | Adapter architecture audit |
| `terminal-bench/analysis/condition-f-final-design.md` | Condition F design (includes max_turns fix) |
| `terminal-bench/analysis/early-behavior-findings.md` | Turn limit impact data |
| `terminal-bench/reproduce.sh` | Main experiment runner (has compliance issues) |
| `terminal-bench/run_pilot_f_89.py` | Custom F runner (non-Harbor) |
| `terminal-bench/wg/adapter.py` | Harbor agent adapter (all conditions) |
| `terminal-bench/docs/pilot-results-synthesis.md` | Pilot comparison results |
| `src/commands/spawn/execution.rs` | Agent spawn and wrapper script |
| `src/config.rs:2345-2347` | Default agent timeout |
| `src/cli.rs:1698-1699` | Native executor max-turns default |

# Design: Hard TB Benchmarks Requiring Graph Coordination

**Task:** design-hard-tb
**Date:** 2026-04-05
**Status:** Proposed
**Depends on:** Full A' vs F benchmark results, full TB catalog survey

---

## 1. Problem Statement

The current A' vs F comparison uses 7 calibration tasks (file-ops, text-processing, debugging, shell-scripting, data-processing, algorithm, ml) — all produce a **ceiling effect** at 100% pass rate for both conditions. These are single-file, single-step problems where graph coordination adds overhead but no value.

Meanwhile, Terminal Bench 2.0 has a catalog of **89 tasks** at various difficulty levels. Across the full catalog with model minimax-m2.7:
- **Condition A' (bare agent):** 45.6% mean pass rate
- **Condition B (wg tools + skill injection):** 53.1% mean pass rate (+7.5pp)
- **36 tasks** have 0% pass rate for A' — these are hard problems
- **28 tasks** have partial pass rates (20-67%) — the discriminating range
- **25 tasks** have 100% pass rate — too easy to differentiate conditions

The goal: select tasks from the existing TB catalog that are (a) hard enough that A' doesn't always pass, and (b) have structural characteristics where graph coordination should help.

---

## 2. Full TB Catalog Survey

### 2.1 Pass Rate Comparison (A' vs B, 89 tasks)

| Tier | A' Tasks | B Tasks | Description |
|------|---------|---------|-------------|
| 0% both | 36 | 36 | Unsolved by either — too hard or environmental |
| 0% A', >0% B | 0 | varies | B-only solves — potential coordination advantage |
| Partial (20-67%) | 28 | 27 | Discriminating range — best for benchmarking |
| 100% both | 25 | 23 | Ceiling effect — not useful |

### 2.2 Task Categories by Coordination Potential

I categorized all 89 TB tasks by their structural characteristics:

#### Category A: Multi-Step Pipelines (build/configure/integrate)
Tasks requiring sequential stages where each depends on the previous.

| Task | A' Rate | B Rate | Expert Min | Steps | F-Potential |
|------|---------|--------|-----------|-------|-------------|
| **kv-store-grpc** | 100% | 100% | 180 | 5 (install → proto → codegen → server → run) | Medium |
| **pypi-server** | 100% | 100% | — | 4 (package → build → server → verify) | Medium |
| **configure-git-webserver** | 67% | 67% | — | 4 (git → hooks → webserver → test) | **High** |
| **git-multibranch** | 100% | 100% | 180 | 5 (git → ssh → nginx → hooks → deploy) | **High** |
| **mailman** | 33% | 67% | 60 | 4 (postfix → mailman3 → config → test) | **High** |
| **build-pov-ray** | 67% | 33% | — | 3 (download → patch → compile) | Medium |
| **compile-compcert** | 50% | 50% | — | 3 (configure → build → verify) | Medium |
| **build-cython-ext** | 50% | 67% | 60 | 4 (clone → fix compat → compile → install) | Medium |

#### Category B: Multi-File Code Tasks (cross-cutting changes)
Tasks requiring understanding and modifying multiple files/modules.

| Task | A' Rate | B Rate | Expert Min | Files | F-Potential |
|------|---------|--------|-----------|-------|-------------|
| **cobol-modernization** | 67% | 100% | — | 4+ (COBOL → Python, multiple data files) | **High** |
| **fix-code-vulnerability** | 100% | 67% | 120 | 2+ (analyze → report → fix → test) | **High** |
| **modernize-scientific-stack** | 100% | 67% | 120 | 3 (analyze legacy → create modern → deps) | Medium |
| **multi-source-data-merger** | 67% | 100% | 30 | 5 (3 sources → merge → conflict report) | **High** |
| **financial-document-processor** | 67% | 100% | 30 | 10+ (classify → extract → summarize) | **High** |
| **large-scale-text-editing** | 33% | 67% | — | 2 (analyze → vim macros) | Low |
| **reshard-c4-data** | 100% | 100% | 30 | 3 (compress.py → decompress.py → test) | Medium |

#### Category C: Complex Algorithm/System Implementation
Tasks requiring iterative development with testing.

| Task | A' Rate | B Rate | Expert Min | F-Potential |
|------|---------|--------|-----------|-------------|
| **llm-inference-batching-scheduler** | 100% | 67% | 45 | Medium |
| **cancel-async-tasks** | 50% | 33% | — | Medium |
| **constraints-scheduling** | 67% | 33% | 15 | Medium |
| **torch-pipeline-parallelism** | 0% | 0% | 240 | **High** (explicit pipeline decomposition) |
| **torch-tensor-parallelism** | 0% | 0% | 240 | **High** (parallel structure) |
| **custom-memory-heap-crash** | 67% | 100% | 30 | Medium (debug → fix → verify) |
| **db-wal-recovery** | 0% | 0% | 45 | Medium |

#### Category D: Build/Environment Setup
Tasks requiring multi-step system configuration.

| Task | A' Rate | B Rate | Expert Min | F-Potential |
|------|---------|--------|-----------|-------------|
| **qemu-startup** | 67% | 67% | — | Low |
| **qemu-alpine-ssh** | 20% | 20% | — | Low |
| **nginx-request-logging** | 50% | 100% | — | Medium |
| **openssl-selfsigned-cert** | 100% | 67% | — | Low |

#### Category E: Hard Algorithmic/Research (likely single-file)
Tasks that are hard due to algorithmic complexity, not coordination needs.

| Task | A' Rate | B Rate | F-Potential |
|------|---------|--------|-------------|
| circuit-fibsqrt | 0% | 0% | Low (single gates.txt file) |
| gpt2-codegolf | 0% | 0% | Low (single C file) |
| path-tracing | 0% | 0% | Low (single C file) |
| write-compressor | 0% | 0% | Low (single file) |
| regex-chess | 0% | 0% | Low (single regex) |
| chess-best-move | 0% | 0% | Low (single algorithm) |
| make-doom-for-mips | 0% | 0% | Low (build chain, but 0% for all) |
| schemelike-metacircular-eval | 0% | 0% | Low (single file) |

---

## 3. Selected Hard Benchmark Tasks

### Selection Criteria

1. **Partial pass rate (20-67%) in A'** — avoids both ceiling and floor effects
2. **Multi-step or multi-file structure** — where graph coordination can help
3. **Clear machine-checkable pass/fail** — TB already provides this
4. **Container-solvable** — TB already runs in Docker
5. **Reasonable time (5-30 min)** — excludes tasks with 400+ minute expert estimates

### 3.1 Primary Selection: 8 Tasks from Existing TB Catalog

These tasks are selected from TB 2.0's existing catalog. They span the discriminating range (20-67% A' pass) and have multi-step structure where F's graph coordination should help.

#### Tier 1: High F-Advantage Expected (multi-step pipelines, multi-file coordination)

| # | Task | A' Rate | B Rate | Category | Why F Helps |
|---|------|---------|--------|----------|-------------|
| 1 | **configure-git-webserver** | 67% | 67% | pipeline | 4 sequential steps: git server → post-receive hook → webserver → integration test. Each depends on previous. F can verify each stage. |
| 2 | **mailman** | 33% | 67% | pipeline | 4 steps: postfix config → mailman3 setup → list config → integration test. Config consistency across services critical. |
| 3 | **multi-source-data-merger** | 67% | 100% | multi-file | 3 input sources with schema mapping → merge → conflict report. F can decompose: parse each source independently, then merge. |
| 4 | **financial-document-processor** | 67% | 100% | multi-file | Classify 10+ documents → extract data → summarize to CSV. Natural fan-out (classify each doc) + fan-in (summarize). |
| 5 | **cobol-modernization** | 67% | 100% | multi-file | Understand COBOL → re-implement in Python → verify against 3 data files. Iterative: implement → compare → fix discrepancies. |

#### Tier 2: Medium F-Advantage Expected (complex with partial multi-step)

| # | Task | A' Rate | B Rate | Category | Why F Helps |
|---|------|---------|--------|----------|-------------|
| 6 | **build-cython-ext** | 50% | 67% | pipeline | Clone → fix numpy compat → compile extensions → install → verify. Build debugging benefits from stage-by-stage verification. |
| 7 | **fix-code-vulnerability** | 100% | 67% | multi-file | Analyze repo → identify CWEs → write report → fix code → run tests. Sequential analysis pipeline. (Note: A' is 100% but B drops — F may restore.) |
| 8 | **constraints-scheduling** | 67% | 33% | algorithm | Parse 3 ICS files → check constraints → find valid slot → generate output ICS. Multi-input with constraint satisfaction. |

### 3.2 Supplementary: 2 Tasks for Gap-Filling

These are newly designed tasks that fill a gap in the TB catalog — tasks where the coordination advantage is maximal. The existing catalog lacks tasks that **explicitly require modifying 5+ interdependent files** in dependency order.

#### 9. multi-module-type-migration (NEW)

**Category:** cascading-change
**Difficulty:** hard
**Expected Duration:** 8-12 minutes

A Python package with 6 modules in a dependency DAG. A core type `UserId = str` must change to a `UserId` dataclass. All 5 consumer modules must update — constructors, comparisons, serialization all break.

**Why F helps:** Modules must be updated in dependency order (core → services → handlers → main). F naturally decomposes this into per-module subtasks with `--after` edges.

**Setup:** Script creates `/tmp/type_migration/` with core/types.py, services/{auth,notifications}.py, handlers/{user,admin}_handler.py, main.py, and tests/.

**Verify:** `cd /tmp/type_migration && python3 -c "from core.types import UserId; assert not isinstance(UserId, type(str))" && python3 -m pytest tests/ -v`

**Predicted:** A' 60%, F 85%

#### 10. iterative-test-fix (NEW)

**Category:** iterative-refinement
**Difficulty:** hard
**Expected Duration:** 10-15 minutes

A Python task scheduler with 6 interrelated bugs. 15 unit tests, 9 fail. Fixing one bug can break/fix others. Requires structured iterate-test-fix cycles.

**Why F helps:** Natural cycle with `--max-iterations`. Each iteration: fix → test → analyze. F's verify gates track convergence (6/15 → 12/15 → 15/15). A' tends to fix all at once and miss interrelations.

**Setup:** Script creates `/tmp/iterative_fix/` with scheduler.py (6 bugs) and tests/test_scheduler.py (15 tests).

**Verify:** `cd /tmp/iterative_fix && python3 -m pytest tests/ -v 2>&1 | grep -c PASSED | python3 -c "import sys; sys.exit(0 if int(sys.stdin.read())>=15 else 1)"`

**Predicted:** A' 45%, F 75%

---

## 4. Predicted Performance Matrix

| # | Task | Source | Category | A' Predicted | F Predicted | F-Advantage |
|---|------|--------|----------|-------------|-------------|-------------|
| 1 | configure-git-webserver | TB 2.0 | pipeline | 67% | 85% | +18% |
| 2 | mailman | TB 2.0 | pipeline | 33% | 60% | +27% |
| 3 | multi-source-data-merger | TB 2.0 | multi-file | 67% | 90% | +23% |
| 4 | financial-document-processor | TB 2.0 | multi-file | 67% | 85% | +18% |
| 5 | cobol-modernization | TB 2.0 | multi-file | 67% | 85% | +18% |
| 6 | build-cython-ext | TB 2.0 | pipeline | 50% | 75% | +25% |
| 7 | fix-code-vulnerability | TB 2.0 | multi-file | 100%* | 90% | −10%* |
| 8 | constraints-scheduling | TB 2.0 | algorithm | 67% | 80% | +13% |
| 9 | multi-module-type-migration | NEW | cascading | 60% | 85% | +25% |
| 10 | iterative-test-fix | NEW | iterative | 45% | 75% | +30% |

*fix-code-vulnerability: A' is 100% but B drops to 67% — likely model/tool interference. F may recover. Included as a control to detect if F's tools cause regressions.

**Aggregate predictions:**
- A' mean pass rate: ~62%
- F mean pass rate: ~81%
- Expected F-advantage: ~19 percentage points
- Tasks in discriminating range: 8/10

### Rationale for Predictions

**F-advantage sources on TB tasks:**
1. **Pipeline tasks** (configure-git-webserver, mailman, build-cython-ext): Each step depends on the previous. F's `--after` edges + `--verify` gates catch errors at each stage instead of discovering them at the end.
2. **Multi-file tasks** (multi-source-data-merger, financial-document-processor, cobol-modernization): F can fan-out independent file processing, then fan-in for integration.
3. **Iterative tasks** (constraints-scheduling, iterative-test-fix): F's cycle support enables structured convergence. A' must manually track fix-test-fix loops.
4. **Cascading changes** (multi-module-type-migration): F models the dependency DAG explicitly. A' must hold it mentally.

**Why F may NOT help on some tasks:**
- Tasks that are fundamentally single-file (regex-chess, gpt2-codegolf)
- Tasks where the difficulty is algorithmic, not organizational
- Tasks where wg overhead exceeds the coordination benefit (already seen: F uses 3.9× more tokens than A')

---

## 5. Implementation Plan for TB Condition System

### 5.1 Existing TB Tasks (8 of 10)

These require NO new task creation — they already exist in the Terminal Bench 2.0 catalog (github.com/laude-institute/terminal-bench-2). The implementation steps:

1. **Update `run_full_a_prime_vs_f.py`** to support selecting tasks by name from the Harbor/TB registry
2. **Add a hard task list** to the trial config:

```json
{
  "run_id": "hard-a-prime-vs-f",
  "conditions": ["A", "F"],
  "tasks": [
    "configure-git-webserver",
    "mailman",
    "multi-source-data-merger",
    "financial-document-processor",
    "cobol-modernization",
    "build-cython-ext",
    "fix-code-vulnerability",
    "constraints-scheduling"
  ],
  "replicas": 3,
  "model": "openrouter:minimax/minimax-m2.7",
  "timeout_s": 1800
}
```

3. **Use Harbor's native runner** (`harbor run`) with the existing `wg.adapter:ConditionAAgent` and a new `ConditionFAgent` that maps to the wg-native executor with graph context

### 5.2 New Custom Tasks (2 of 10)

These require creating new task definitions in Terminal Bench format:

```
tasks/hard/
├── multi-module-type-migration/
│   ├── task.toml
│   ├── instruction.md
│   ├── environment/
│   │   └── Dockerfile
│   ├── tests/
│   │   ├── test.sh
│   │   └── test_outputs.py
│   └── solution/           # reference implementation
└── iterative-test-fix/
    ├── task.toml
    ├── instruction.md
    ├── environment/
    │   └── Dockerfile
    ├── tests/
    │   ├── test.sh
    │   └── test_outputs.py
    └── solution/
```

Each task needs:
- `task.toml`: metadata, timeouts, docker image
- `instruction.md`: agent prompt
- `Dockerfile`: environment setup with pre-populated files
- `test.sh` / `test_outputs.py`: Harbor-compatible verification

### 5.3 Runner Integration

The `run_full_a_prime_vs_f.py` script currently hardcodes `TB_TASKS` with custom verify commands. For the hard benchmark, two approaches:

**Option A: Harbor-native runner** (preferred)
- Use `harbor run` with task names from the TB 2.0 dataset
- Each condition uses its own agent adapter (ConditionAAgent vs ConditionFAgent)
- Verification uses TB's built-in Docker-based verifiers
- Pro: Established, tested, Docker isolation per trial
- Con: Requires Harbor runner changes for condition F

**Option B: Extended `run_full_a_prime_vs_f.py`**
- Add the 8 existing tasks to `TB_TASKS` dict with verify commands
- Requires extracting verify commands from TB's test.sh files
- Pro: Uses existing infrastructure
- Con: Duplicates TB verification logic, may miss Docker-dependent checks

**Recommendation: Option A for existing TB tasks, Option B for 2 new custom tasks.**

### 5.4 Trial Configuration

```
Hard benchmark: 10 tasks × 2 conditions × 3 replicas = 60 trials
Estimated time: ~60 trials × 15 min avg = 15 hours
```

### 5.5 Phased Rollout

1. **Phase 1 (pilot):** Run 3 existing TB tasks (mailman, multi-source-data-merger, cobol-modernization) × 2 conditions × 2 replicas = 12 trials. Validates the F adapter works with TB's Docker environment.

2. **Phase 2 (full existing):** Run all 8 existing TB tasks × 2 conditions × 3 replicas = 48 trials.

3. **Phase 3 (custom tasks):** Build and test the 2 new custom task environments. Run 2 new tasks × 2 conditions × 3 replicas = 12 trials.

4. **Phase 4 (full sweep):** Combined 60-trial sweep with analysis.

---

## 6. Risk Analysis

### 6.1 F Overhead May Negate Coordination Benefit

Current data shows F uses 3.9× more tokens and is 22% slower than A'. On harder tasks, this overhead may be worse. Mitigation: monitor token usage and time per trial.

### 6.2 Docker Environment Compatibility

TB tasks run in custom Docker images. The F condition needs the wg binary available in the container. Options:
- Mount host wg binary into container
- Install wg in the Docker image
- Use native executor with host-side graph (current approach in run_full_a_prime_vs_f.py)

### 6.3 Model Sensitivity

Results are model-dependent. minimax-m2.7 is the benchmark model, but the tasks may have very different pass rates on other models. Run calibration with at least one other model (e.g., claude-sonnet-4-6) to check robustness.

### 6.4 Task Selection Bias

We're selecting tasks where we *predict* F will do better — this is a form of selection bias. Mitigate by including fix-code-vulnerability (where A' is 100% and B drops) as a regression control, and reporting the full methodology transparently.

---

## 7. Appendix: Full Task Catalog (89 Tasks) with Pass Rates

### A' Pass Rate = 0% (36 tasks) — Likely Too Hard for Benchmarking

These tasks have 0% pass rate across all conditions tested, suggesting they are too difficult for the current model regardless of tooling. Not selected because they would produce floor effects.

Notable multi-step tasks in this tier that COULD be useful with stronger models:
- **torch-pipeline-parallelism** (expert: 240 min) — explicit pipeline decomposition
- **torch-tensor-parallelism** (expert: 240 min) — parallel structure
- **make-mips-interpreter** — multi-stage build + emulation

### A' Pass Rate = 100% (25 tasks) — Too Easy for Benchmarking

These tasks always pass, producing ceiling effects. However, some show B/F regressions:
- **fix-code-vulnerability**: A' 100%, B 67% — tool interference
- **llm-inference-batching-scheduler**: A' 100%, B 67%

### A' Pass Rate = 20-67% (28 tasks) — Discriminating Range

This is the sweet spot. Our 8 selected tasks come from this tier, filtered for multi-step/multi-file structure.

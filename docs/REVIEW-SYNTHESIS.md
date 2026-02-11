# Workgraph Code Review Synthesis

**Date:** 2026-02-11
**Source:** 10 review documents covering the full codebase
**Scope:** 92 source files (~45,800 lines), 9 test files (~7,650 lines), 67 CLI commands

---

## 1. Executive Summary

Workgraph is a well-engineered task graph coordinator with a clean core and ambitious feature set. The foundational layer — graph model, parser, query engine, validation — is solid at ~2,100 lines with good test coverage and few issues. The service daemon with IPC-based coordination is architecturally sound. The agency system (roles/motivations/agents) is the most thoroughly tested subsystem.

**However, the codebase has grown organically to ~45,800 lines and shows signs of accumulated tech debt:**

- **Significant code duplication** — `make_task` (duplicated 22+ times), `build_reverse_index` (6 times), `spawn.rs` run/spawn_agent (~250 duplicate lines), Matrix command parser (full copy-paste between features), and more. Conservatively **1,500–2,000 lines** of pure duplication.
- **Dead and vestigial code** — The `Executor` trait is never used in production, `src/executors/` is dead re-exports, `petgraph` dependency is unused, 4 of 7 `AgentStatus` variants are never set, the old `wg agent` loop coexists with the service daemon without clear demarcation.
- **Test coverage is deeply uneven** — The agency system has ~3,850 lines of excellent tests, but CLI commands (~23,000 lines), TUI (~4,362 lines), Matrix integration (~2,787 lines), and the evolution module (~2,677 lines) have **zero** test coverage. Only 1 of 9 integration test files runs in CI.
- **Documentation is stale** — COMMANDS.md documents ~35 of 67 commands. AGENT-SERVICE.md describes a design that doesn't match the implementation. The README is missing significant features (edit, submit/approve/reject, status, dag).
- **Architectural redundancy** — Two identity systems (Actor vs Agent), two prompt/executor systems (Executor trait vs spawn.rs), two Matrix implementations sharing copy-pasted code.

**Overall codebase health: 6.5/10** — Strong fundamentals, but debt is accumulating faster than it's being paid down. The next phase of work should focus on consolidation over new features.

---

## 2. Top 10 Recommendations (Ranked by Impact vs Effort)

| # | Recommendation | Value | Effort | Source Review |
|---|---------------|-------|--------|--------------|
| **1** | **Extract shared test helpers** (`make_task`, `wg_cmd`, `wait_for`, `build_reverse_index`) into common modules | High — eliminates ~1,000+ lines of duplication, makes adding tests easier | Low — mechanical extraction | Analysis, Tests, Core |
| **2** | **Fix failing CI** — update stale test assertions in `config.rs` (`"opus-4-5"` → `"opus"`), add remaining 8 integration test files to CI | High — CI is currently red; 8/9 test files never run in CI | Low — trivial assertion fix + CI config | Config/Build |
| **3** | **Deduplicate `spawn.rs`** — extract `spawn_inner()` shared by `run()` and `spawn_agent()` | High — eliminates ~250 lines of the highest-risk duplication (bug fixes must be applied twice) | Low-Med — both functions are structurally similar | Service Layer |
| **4** | **Remove `blocks` field from Task** — compute on demand from `blocked_by` | High — eliminates the biggest data integrity hazard in the core engine (unsynchronized dual representation) | Low — `blocks` is only used in 3 visualization commands | Core Graph |
| **5** | **Consolidate Matrix code** — extract shared command parser and executor into `matrix_common` | High — eliminates ~560 lines of copy-paste; fixes zero-test coverage on lite commands | Med — requires careful `#[cfg]` wiring | Matrix |
| **6** | **Add `Task::new(id, title)` constructor with defaults** | Med — eliminates 22+ duplicated `make_task` helpers, makes 28-field struct manageable | Low — simple constructor + Default impl | Core Graph |
| **7** | **Resolve Executor trait question** — either use it in production or remove it (+ `src/executors/` dead code) | Med — removes confusion about where spawning logic lives; ~1,500 lines clarified or deleted | Med — requires deciding the target architecture | Service Layer |
| **8** | **Reduce graph loads in `coordinator_tick()`** — load once, pass `&mut WorkGraph`, save once | Med — eliminates up to 5 redundant graph loads per tick; closes TOCTOU windows | Med — requires refactoring 300-line function | Service Layer |
| **9** | **Update COMMANDS.md** — add ~30 missing commands, fix stale entries | High for adoption — the command reference is at 50% accuracy | Med — significant writing effort | Documentation |
| **10** | **Merge redundant analysis commands** — fold `bottlenecks` and `structure` into `analyze` | Med — reduces command count, eliminates duplicated computation | Low-Med — `analyze` already has equivalent sections | Analysis |

---

## 3. Simplification Roadmap

### Phase 1: Quick Wins (1-2 days)

**Goal: Fix CI, eliminate mechanical duplication, remove dead code.**

| Action | Lines Affected | Files |
|--------|---------------|-------|
| Fix `config.rs` test assertions (`"opus"`) | ~4 lines | `src/config.rs` |
| Add all integration tests to CI | ~20 lines | `.github/workflows/ci.yml` |
| Remove unused `petgraph` dependency | 1 line | `Cargo.toml` |
| Remove duplicate `libc` dependency entry | 2 lines | `Cargo.toml` |
| Remove `src/executors/` dead re-exports | ~60 lines deleted | 3 files removed |
| Add `impl Default for PerformanceRecord` | ~5 lines added, ~60 reduced | `src/agency.rs` + 20 call sites |
| Extract `extract_json()` to shared utility | ~30 lines net reduction | `src/commands/evaluate.rs`, `src/commands/evolve.rs` |
| Remove deprecated `start_sync_thread()` and unused `sync_loop()` | ~40 lines deleted | `src/matrix/mod.rs` |
| Remove `VerificationEvent` stub from matrix-lite | ~10 lines deleted | `src/matrix_lite/mod.rs` |

**Estimated net reduction: ~200 lines deleted, CI goes green.**

### Phase 2: Structural Deduplication (3-5 days)

**Goal: Eliminate the major copy-paste hotspots.**

| Action | Lines Affected | Files |
|--------|---------------|-------|
| Extract `spawn_inner()` from `spawn.rs` | ~250 lines deduped | `src/commands/spawn.rs` |
| Create `tests/common/mod.rs` with shared helpers | ~500 lines deduped | All 9 test files |
| Extract `build_reverse_index` + `collect_transitive_dependents` to `src/graph_utils.rs` | ~180 lines deduped | 6 command files |
| Consolidate Matrix command parser + executor into `src/matrix_common/` | ~560 lines deduped | 6 Matrix files |
| Extract critical path computation to library | ~100 lines deduped | `critical_path.rs`, `forecast.rs`, `viz.rs` |
| Add `Task::new(id, title)` constructor | ~660 lines deduped (test helpers) | `src/graph.rs` + 22 files |
| Extract CLI boilerplate `with_task_mut()` helper | ~100 lines deduped | 15 command files |

**Estimated net reduction: ~2,000 lines.**

### Phase 3: Architectural Cleanup (1-2 weeks)

**Goal: Resolve redundant systems and structural debt.**

| Action | Impact |
|--------|--------|
| Remove `blocks` field from Task, compute on demand | Eliminates data integrity hazard |
| Decide Executor trait fate: use it or remove it | Clarifies spawning architecture (~1,500 lines) |
| Refactor `coordinator_tick()` — single graph load, extract auto-assign/auto-evaluate | Reduces 300-line function, eliminates TOCTOU |
| Deprecate standalone `wg agent` loop in favor of `wg service start` | Removes user confusion about which system to use |
| Clarify Actor (graph node) vs Agent (agency entity) — deprecate one | Removes overlapping identity systems |
| Merge `bottlenecks` + `structure` into `analyze`; consider `aging` + `workload` | Net -700 to -2,100 lines |
| Clean up unused AgentStatus variants (Starting, Idle, Done, Failed) | Simplifies state machine |
| Gate `tokio` behind matrix features | Faster builds for non-Matrix users |

### Phase 4: Documentation & Testing (ongoing)

| Action | Impact |
|--------|--------|
| Rewrite COMMANDS.md (add ~30 missing commands) | Most impactful doc improvement |
| Add verified workflow (submit/approve/reject) to README | Undiscoverable feature becomes visible |
| Add `wg edit`, `wg status`, `wg dag` to README | Commonly-used commands become documented |
| Archive stale design docs (AGENT-SERVICE.md, ROLES-IDEA.md, architectural-issues.md) | Reduces confusion |
| Add `wg evolve` unit tests (mutation, crossover, tournament) | Covers highest-risk untested code (2,677 lines) |
| Add CLI smoke tests (--help on all commands) | Catches argument parsing regressions |
| Add Matrix feature CI build | Prevents silent breakage of feature-gated code |
| Atomic file writes in `save_graph` (temp + rename) | Prevents data loss on crash |

---

## 4. Architectural Concerns & Tech Debt Hotspots

### 4.1 Hotspot: `src/commands/service.rs` (2,293 lines)

The daemon/coordinator module is the complexity center of the codebase. `coordinator_tick()` alone is 300+ lines doing 7 distinct things. It loads the graph up to 5 times per tick. The auto-assign and auto-evaluate blocks are 90-120 lines each, embedded inline. This is the most fragile code path — any bug here affects all agent coordination.

**Risk:** High. Changes to coordinator logic are error-prone due to the function's length and the repeated graph load/save pattern.

### 4.2 Hotspot: `src/commands/spawn.rs` (998 lines)

Contains two nearly-identical 260-line functions (`run()` and `spawn_agent()`). Any fix to agent spawning must be manually applied to both. The wrapper script generation (bash that pipes prompts, checks status via `grep -o` JSON parsing) is fragile.

**Risk:** High. The duplication is a maintenance timebomb — divergence between the two paths will cause subtle bugs.

### 4.3 Hotspot: `src/main.rs` (2,014 lines)

67-variant `Commands` enum with ~780 lines of argument definitions. The 535-line `main()` dispatch function and 72-line manual `command_name()` mapping must be updated in lockstep when commands change. The `Config` command alone has 28 fields.

**Risk:** Medium. Adding new commands requires touching 3-4 places in main.rs. Not architecturally dangerous but a significant maintenance friction.

### 4.4 Concern: Two Spawning/Executor Systems

The `Executor` trait (`src/service/executor.rs`, `claude.rs`, `shell.rs` — ~2,068 lines) defines a clean abstraction for agent spawning. But the production coordinator doesn't use it — `spawn.rs` reimplements everything from scratch. The `Executor::spawn()` method is dead code. This is ~1,500 lines of infrastructure that doesn't serve its intended purpose.

### 4.5 Concern: Two Identity Systems

`Actor` (graph.rs) and `Agent` (agency.rs) both model "who does work" with overlapping fields (capabilities/skills, assignment). The `Actor` system is used by `wg match` and `wg next`; the `Agent` system is used by `wg assign`, identity prompts, and evaluations. Users must understand both to use the system effectively.

### 4.6 Concern: Speculative Complexity in Evolution

The evolution system (`src/commands/evolve.rs` — 2,677 lines) implements mutation, crossover, tournament selection, and population management. It requires accumulated evaluation data to be useful. Combined with agency stats (675 lines) and lineage tracking, this is ~3,350 lines of infrastructure for a feature that only becomes valuable after many evaluation cycles — a threshold most projects won't reach. The code has **zero test coverage**.

### 4.7 Concern: Test Coverage Cliff

The test suite has a dramatic gap: the agency data model is tested at ~3,850 lines (excellent), but **~30,000 lines of CLI commands, TUI, Matrix, and evolution code have no tests**. Only 1 of 9 integration test files runs in CI. Two unit tests are currently failing (stale assertions).

### 4.8 Concern: Data Integrity

- `blocks` and `blocked_by` are unsynchronized dual representations — no code enforces consistency.
- `save_graph` truncates and rewrites the file under lock — a crash mid-write loses data (no atomic rename).
- Matrix listeners bypass `flock`-based graph locking — concurrent Matrix and CLI modifications can corrupt the graph.
- `unclaim` can reset a Done task to Open with no guard — likely a bug.

---

## 5. Prioritized Action Plan

### Immediate (This Week)

1. **Fix CI** — Update `config.rs` test assertions, add all integration test files to CI workflow.
2. **Remove dead code** — `src/executors/`, unused `petgraph`, duplicate `libc` entry, Matrix dead code.
3. **Fix `unclaim` bug** — Reject unclaim on Done/Abandoned tasks.

### Short-Term (Next 2 Weeks)

4. **Extract shared test helpers** — `tests/common/mod.rs` with `make_task`, `wg_cmd`, `wait_for`, etc.
5. **Deduplicate `spawn.rs`** — Extract `spawn_inner()`.
6. **Consolidate Matrix shared code** — Single command parser, single executor, add graph locking.
7. **Add `Task::new()` constructor** — Eliminate test helper duplication.
8. **Add log entries to all state transitions** — `done`, `fail`, `abandon`, `retry`, `claim`, `unclaim` should all log.

### Medium-Term (Next Month)

9. **Remove `blocks` field** — Compute on demand from `blocked_by`.
10. **Resolve Executor trait** — Either integrate into spawn path or remove.
11. **Refactor `coordinator_tick()`** — Single graph load, extracted sub-functions.
12. **Atomic file writes in `save_graph`** — Temp file + rename pattern.
13. **Update COMMANDS.md** — Add all 30+ missing commands.
14. **Update README** — Add edit, submit/approve/reject, status, dag, link AGENCY.md.
15. **Add `wg evolve` tests** — At minimum: mutation, crossover, tournament selection unit tests.

### Long-Term (Next Quarter)

16. **Merge redundant analysis commands** into `analyze`.
17. **Deprecate `wg agent` standalone loop** — Consolidate on service daemon.
18. **Clarify Actor vs Agent** — Deprecate Actor system or document separation clearly.
19. **Split `main.rs`** — Move subcommand enums into modules, extract help system.
20. **Typed timestamps** — `Option<DateTime<Utc>>` instead of `Option<String>`.
21. **Add CLI smoke tests** — Run `--help` on all 67 commands.
22. **Archive stale docs** — Move resolved design docs to `docs/archive/`.
23. **Migrate `serde_yaml` to `serde_yml`** — Deprecated crate.

---

## 6. Findings by Review Area

### Core Graph Engine (review-core-graph.md)
- **Health:** Good. ~2,119 lines, solid test coverage.
- **Top issues:** `blocks`/`blocked_by` unsynchronized (HIGH), Task struct 28 fields (HIGH), `TaskHelper` duplication (MED).
- **Key recommendation:** Remove `blocks`, add `Task::new()` constructor.

### CLI Structure (review-cli-commands.md)
- **Health:** Functional but strained. ~4,243 lines, 67 commands.
- **Top issues:** `unclaim` bug (HIGH), inconsistent state transition logging (HIGH), `main.rs` 2,014 lines (MED), boilerplate duplication (MED).
- **Key recommendation:** Fix `unclaim`, add consistent logging, extract `with_task_mut()` helper.

### Service Layer (review-service-layer.md)
- **Health:** Architecturally sound, implementation needs cleanup. ~8,800 lines.
- **Top issues:** `spawn.rs` duplication (HIGH), `coordinator_tick()` complexity (HIGH), dead Executor trait (MED), unused AgentStatus variants (LOW).
- **Key recommendation:** Deduplicate spawn, refactor coordinator_tick, resolve Executor question.

### Agency System (review-agency-system.md)
- **Health:** Well-designed and well-tested. ~8,328 lines (41% tests).
- **Top issues:** Speculative evolution complexity (MED), Actor vs Agent confusion (MED), minor duplication (LOW).
- **Key recommendation:** Defer evolution complexity, clarify Actor vs Agent.

### Analysis Commands (review-analysis-commands.md)
- **Health:** Feature-rich but duplicated. ~6,100 lines across 22 commands.
- **Top issues:** `build_reverse_index` duplicated 6 times (HIGH), 2 clearly redundant commands (MED), 3 critical path implementations (MED), `make_task` in all 22 files (HIGH).
- **Key recommendation:** Extract shared utilities, merge `bottlenecks`+`structure` into `analyze`.

### TUI (review-tui.md)
- **Health:** Production-quality for a monitoring TUI. ~4,362 lines.
- **Top issues:** Duplicated sort-key logic (MED), Debug-format snapshot diffing (MED), hardcoded viewport_height (MED), zero test coverage (but DAG layout has 8 inline tests).
- **Key recommendation:** Minor refactors only; no critical issues.

### Matrix Integration (review-matrix.md)
- **Health:** Working but architecturally flawed. ~3,315 lines.
- **Top issues:** 609 lines copy-pasted between features (CRITICAL), no graph locking in listeners (HIGH), no tests (HIGH), weak UUID generation (MED).
- **Key recommendation:** Consolidate shared code, add graph locking, test command parser.

### Configuration & Build (review-config-build.md)
- **Health:** Clean config system, CI has gaps. Cargo.toml well-structured.
- **Top issues:** Failing unit tests (P0), only 1/9 integration tests in CI (P1), `petgraph` unused (P2), `tokio` always compiled (P3).
- **Key recommendation:** Fix tests, expand CI, remove unused deps.

### Documentation (review-documentation.md)
- **Health:** Extensive but unevenly maintained. README strong, COMMANDS.md stale.
- **Top issues:** COMMANDS.md at 50% accuracy (HIGH), ~30 undocumented commands (HIGH), AGENT-SERVICE.md describes a different system (HIGH), stale model names (LOW).
- **Key recommendation:** Update COMMANDS.md and README as top doc priority.

### Test Coverage (review-tests.md)
- **Health:** Excellent where it exists, but massive gaps. ~7,650 lines covering agency/coordinator well.
- **Top issues:** ~30,000 lines of CLI/TUI/Matrix/evolve code untested (CRITICAL), 8/9 test files not in CI (HIGH), significant test helper duplication (MED).
- **Key recommendation:** Fix CI first, then add evolve tests and CLI smoke tests.

---

## 7. Codebase Metrics

| Metric | Value |
|--------|-------|
| Total source files | 92 |
| Total source lines | ~45,800 |
| Total test files | 9 |
| Total test lines | ~7,650 |
| Test-to-source ratio | ~17% |
| CLI commands | 67 |
| Largest file | `src/commands/evolve.rs` (2,677 lines) |
| Estimated duplicated lines | 1,500–2,000 |
| Estimated dead code lines | ~1,500 (Executor trait, executors/, unused variants) |
| Untested source lines | ~30,000 |
| Documentation accuracy | 6/10 overall |
| CI health | Red (2 stale assertions + 8/9 test files excluded) |

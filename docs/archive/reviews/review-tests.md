# Test Coverage & Quality Review

Systematic review of all test files (~7,651 lines across 9 files), cross-referenced against source modules (~45,000+ lines across 88 source files).

---

## 1. Test File Inventory

| File | Lines | Test Count | Approach |
|------|-------|------------|----------|
| `integration_service.rs` | 663 | 3 | End-to-end subprocess tests with real daemon |
| `integration_service_coordinator.rs` | 1,371 | 28 + 2 LLM | Library-level unit tests (no daemon) |
| `integration_auto_assignment.rs` | 1,295 | 18 + 2 LLM | Library-level + CLI subprocess tests |
| `integration_review_workflow.rs` | 473 | 12 | CLI subprocess tests |
| `integration_agency.rs` | 999 | 5 | Library-level lifecycle tests |
| `integration_agency_edge_cases.rs` | 960 | 30 | Library-level edge-case tests |
| `integration_agency_lineage.rs` | 636 | 22 | Library-level ancestry/lineage tests |
| `evaluation_recording.rs` | 999 | 22 | Library-level + end-to-end recording tests |
| `skill_resolution.rs` | 255 | 10 | Library-level skill resolution tests |
| **Total** | **7,651** | **~152** | |

---

## 2. What Each Test File Covers

### integration_service.rs (3 tests)
- **Auto-pickup via GraphChanged:** Starts a real daemon process, adds a task, sends IPC notification, verifies the task is picked up and completed via the GraphChanged fast-path (not the slow poll).
- **Fallback poll pickup:** Writes a task directly to graph.jsonl (bypassing `wg add`), verifies the background poll interval detects and picks up the task.
- **Dead-agent recovery:** Starts daemon, spawns a long-running task, kills the agent via SIGKILL, verifies the daemon detects the dead agent, marks it dead in the registry, and re-spawns a new agent.

**Approach:** Full subprocess integration tests using `tempdir`, a real `wg` binary, shell executor, and Unix IPC. Tests run `#[serial]` to avoid contention. Uses active polling with timeouts rather than fixed sleeps.

**Quality:** Excellent. These are true end-to-end tests that exercise the daemon, coordinator, agent lifecycle, and IPC. Well-structured helpers (`wg_binary()`, `wg_cmd()`, `wait_for()`, `setup_workgraph()`). Timeout-based assertions give clear error messages.

### integration_service_coordinator.rs (28 + 2 LLM tests)
Covers coordinator logic without starting a daemon:

1. **Ready task identification (2 tests):** Tests `ready_tasks()` with mixed statuses, dependency unblocking.
2. **Max agents limit (2 tests):** Slot accounting with alive/dead/done agents.
3. **Skip assigned tasks (1 test):** Coordinator skips tasks with `assigned.is_some()`.
4. **Dead agent detection (5 tests):** Process-exited PID, still-running PID, already-dead/done/failed agents.
5. **Cleanup flow (3 tests):** Unclaim in-progress task, skip done/failed tasks during cleanup.
6. **Agent registry operations (5 tests):** Register, lookup by task, mark dead, clean stale entries, persistence roundtrip, locked operations.
7. **Auto-evaluate subgraph (6 tests):** Creates eval tasks, skips evaluation/assignment/abandoned tasks, idempotent, unblocks on failed source.
8. **Slot accounting (4 tests):** Basic slots, mixed statuses, at-zero, saturating_sub no underflow.
9. **Process liveness (2 tests):** kill(pid, 0) for current and nonexistent PID.
10. **Agent lifecycle (1 test):** All status transitions (Starting→Working→Idle→Stopping→Done→Failed→Dead).
11. **Dependency chain (1 test):** A→B→C cascade unblocking.
12. **Registry counts (1 test):** active_count() and idle_count().
13. **LLM tests (2, gated behind `llm-tests` feature):** Real Claude agent spawn for simple task completion; LLM-driven agency component creation (role, motivation, agent).

**Quality:** Very thorough unit-level testing of coordinator internals. The auto-evaluate tests replicate the production logic faithfully. LLM tests are properly gated.

### integration_auto_assignment.rs (18 + 2 LLM tests)
Covers the auto-assignment pipeline:

1. **Assign subgraph construction (9 tests):** Creates assign-* tasks, blocks original task, includes description/skills, skips already-assigned/claimed tasks, idempotent, respects disabled config, handles multiple ready tasks, skips blocked tasks.
2. **Assignment CLI (3 tests):** Sets agent field, clear removes agent, prefix matching.
3. **Full pipeline (2 tests):** End-to-end assignment with agency entities, mixed task states.
4. **Prompt rendering (4 tests):** Agent identity in rendered prompts, empty identity for no-agent, agent field persistence, agent survives subgraph construction.
5. **LLM tests (2, gated):** Real Claude agent does assignment reasoning; LLM-driven evaluation recording.

**Quality:** Strong coverage of the assignment pipeline. The `build_assign_subgraph()` helper faithfully extracts production coordinator logic for testability. Good boundary testing (assigned vs. unassigned, enabled vs. disabled).

### integration_review_workflow.rs (12 tests)
Covers submit/approve/reject lifecycle:

1. **Submit (3 tests):** Requires InProgress status, transitions to PendingReview, checks blockers.
2. **Reject (4 tests):** Requires PendingReview status, transitions back to Open (clears assigned, increments retry_count), works without reason, increments existing retry_count.
3. **Approve (3 tests):** Requires PendingReview status, transitions to Done (sets completed_at), checks blockers.
4. **Full cycle (2 tests):** Complete review cycle with rejection then approval; multiple rejections incrementing retry_count.

**Quality:** Clean, thorough state-machine testing. Each status transition is tested both for the happy path and for precondition enforcement. The full-cycle test verifies the complete log trail.

### integration_agency.rs (5 tests)
Covers the core agency lifecycle:

1. **Full lifecycle:** Init → create role/motivation (content-hash IDs) → create task with agent → render identity prompt → simulate completion → record evaluation → verify cumulative performance → role-task matching.
2. **Seed starters:** Populates default roles/motivations, verifies idempotent re-seeding.
3. **Full lifecycle new design:** Three-level evaluation recording (agent, role, motivation), output capture (artifacts.json, log.json, changes.patch), agent lineage (mutation, crossover), legacy slug-based entity coexistence.
4. **Output capture standalone:** Verifies all three output files are created with correct content.
5. **Agent independent performance:** Two agents share a role but have different motivations; evaluations track independently.

**Quality:** Comprehensive lifecycle tests covering the full agency data model. The "new design" test is 500+ lines and verifies many subsystems. Good coverage of the three-level performance recording.

### integration_agency_edge_cases.rs (30 tests)
Covers edge cases across 9 categories:

1. **Nonexistent entity references (3 tests):** Agent with fake role/motivation, evaluation for nonexistent/empty agent, evaluation with nonexistent role+motivation.
2. **Deletion of referenced entities (2 tests):** Delete role/motivation referenced by agent; agent still loads, evaluation recording gracefully skips.
3. **Extreme performance scores (6 tests):** Score 0.0, 1.0, negative, mixed extremes, empty list, YAML roundtrip.
4. **Content hash collision resistance (12 tests):** Different descriptions/outcomes/skills/skill-order for roles; different descriptions/tradeoffs/swapped-categories for motivations; different/swapped agent pairings; determinism; name independence.
5. **Prefix lookup edge cases (5 tests):** Zero matches, exact one match, ambiguous prefix, empty directory, nonexistent directory.
6. **Corrupted YAML handling (8 tests):** Corrupted role/motivation/agent YAML, corrupted evaluation JSON, one-corrupted-in-batch, empty YAML, wrong schema, partial fields, nonexistent file.
7. **Additional edge cases (4 tests):** Empty fields hash, short_hash, unicode/YAML-special chars in descriptions, init idempotent, load_all on nonexistent dirs.

**Quality:** Excellent defensive testing. Covers every graceful-degradation path. The corrupted-data tests ensure no panics.

### integration_agency_lineage.rs (22 tests)
Covers lineage tracking across 7 categories:

1. **No parents (2 tests):** Gen-0 role and motivation (manual creation).
2. **Mutation single parent (2 tests):** Role and motivation via mutation → gen 1.
3. **Deep ancestry chain (2 tests):** Chain of 4 generations; verify all nodes walked.
4. **Crossover two parents (2 tests):** Both parents in ancestry for role and motivation.
5. **Generation increments (4 tests):** Mutation increments, crossover max-parent+1, deep chain 5 generations, mixed-gen crossover parents.
6. **Orphan resilience (4 tests):** Missing intermediate parent, missing crossover parent, nonexistent target.
7. **AncestryNode format (2 tests):** All fields populated, crossover parent IDs.
8. **Additional (4 tests):** Default lineage values, mutation/crossover constructors, diamond ancestry (no duplicate visits), empty directory.

**Quality:** Very thorough lineage testing. The diamond-pattern duplicate-visit test is particularly valuable.

### evaluation_recording.rs (22 tests)
Covers evaluation recording and performance aggregation:

1. **JSON format (4 tests):** All fields preserved, filename format, round-trip, empty dimensions.
2. **Multiple evaluations aggregation (2 tests):** Two evals avg, three evals incremental avg.
3. **Context ID tracking (3 tests):** Agent/role/motivation track different context_ids; role tracks different motivations; motivation tracks different roles.
4. **Performance record counts (4 tests):** 0, 1, 10+, and 12 evaluations end-to-end.
5. **Dimension scoring (4 tests):** Standard dimensions, custom dimensions, extreme values, independent of score.
6. **recalculate_avg_score (9 tests):** Empty, single, identical, 0+1, large count (100), negatives, all zeros, all ones, precision.

**Quality:** Thorough numerical testing with floating-point precision checks. The 12-evaluation end-to-end test validates all three levels (agent, role, motivation) and disk persistence.

### skill_resolution.rs (10 tests)
Covers skill resolution and prompt rendering:

1. **Individual resolution (6 tests):** Name tag, inline content, file relative/absolute/tilde paths, nonexistent file error, URL without http feature.
2. **Mixed resolution (3 tests):** Mixed types (skips failures), empty skills, all failures.
3. **Prompt rendering (2 tests):** Resolved skills appear in identity prompt; no-skills omits section.

**Quality:** Good coverage of SkillRef variants. The tilde-expansion test with HOME env var is a nice touch. Missing: URL resolution with the `matrix-lite` feature enabled.

---

## 3. Coverage Gaps — What Lacks Tests

### 3.1 CLI Commands (MAJOR GAP)

The test suite tests **0 of 66 CLI command modules** directly. All CLI testing is either:
- Done through the library API (bypassing argument parsing, error formatting)
- Done via subprocess calls (`wg_cmd()`) that only test a handful of commands

**Commands with ZERO test coverage (partial list of high-impact ones):**

| Command | Lines | Impact | Notes |
|---------|-------|--------|-------|
| `service.rs` | 2,293 | Critical | The coordinator loop, IPC server, polling — only tested via e2e integration_service.rs |
| `evolve.rs` | 2,677 | High | Evolutionary algorithm (mutation, crossover, tournament) — zero tests |
| `analyze.rs` | 1,135 | Medium | Comprehensive analysis — no tests |
| `viz.rs` | 1,089 | Medium | Graph visualization — no tests |
| `spawn.rs` | 998 | High | Agent spawn logic — only tested via e2e |
| `agent.rs` | 924 | Medium | Agent info display — no tests |
| `forecast.rs` | 813 | Medium | Completion forecasting — no tests |
| `workload.rs` | 713 | Medium | Workload distribution — no tests |
| `aging.rs` | 671 | Low | Task age analysis — no tests |
| `agency_stats.rs` | 675 | Medium | Agency performance stats — no tests |
| `critical_path.rs` | 647 | Medium | Critical path calculation — no tests |
| `velocity.rs` | 637 | Medium | Velocity tracking — no tests |
| `dead_agents.rs` | 537 | Medium | Dead agent detection CLI — no tests |
| `status.rs` | 542 | Medium | Project status summary — no tests |
| `why_blocked.rs` | 490 | Medium | Blocked explanation — no tests |
| `edit.rs` | 454 | Medium | Task editing with flock — no tests |
| `config_cmd.rs` | 457 | Medium | Config get/set — no tests |
| `trajectory.rs` | 456 | Medium | Task completion path — no tests |
| `show.rs` | 423 | Medium | Task detail display — no tests |
| `impact.rs` | 423 | Medium | Impact analysis — no tests |
| `kill.rs` | 419 | Medium | Agent kill — no tests |
| `coordinate.rs` | 405 | Medium | Coordination status — no tests |
| `archive.rs` | 399 | Medium | Archive completed tasks — no tests |
| `role.rs` | 394 | Medium | Role management CLI — tested via library only |
| `motivation.rs` | 447 | Medium | Motivation management CLI — tested via library only |
| `heartbeat.rs` | 434 | Medium | Agent heartbeat — no tests |

**Total untested CLI command code: ~20,000+ lines.**

### 3.2 TUI Module (ZERO coverage)

| File | Lines | Notes |
|------|-------|-------|
| `tui/mod.rs` | 1,422 | Interactive terminal UI |
| `tui/app.rs` | 1,370 | App state/events |
| `tui/dag_layout.rs` | 1,570 | DAG layout algorithm |
| **Total** | **4,362** | No tests at all |

The DAG layout algorithm in particular has complex logic (Sugiyama-style layer assignment, crossing reduction) that would benefit from unit tests.

### 3.3 Matrix Integration (ZERO coverage)

| File | Lines | Notes |
|------|-------|-------|
| `matrix/mod.rs` | 602 | Full Matrix client |
| `matrix/commands.rs` | 447 | Bot command parser |
| `matrix/listener.rs` | 513 | Event loop |
| `matrix_lite/mod.rs` | 538 | Lightweight client |
| `matrix_lite/commands.rs` | 162 | Simple command parser |
| `matrix_lite/listener.rs` | 425 | Polling listener |
| **Total** | **~2,787** | Feature-gated, no tests |

The command parsers (`MatrixCommand`) could be tested without a real Matrix server.

### 3.4 Source Modules with Inline Tests Only

These modules have `#[cfg(test)]` unit tests within the source file itself, but those are not included in the integration test files reviewed:

| Module | Unit Tests | Notes |
|--------|-----------|-------|
| `graph.rs` | ~27 | Serialization, deserialization, migration |
| `parser.rs` | ~10 | Load/save, locking |
| `query.rs` | ~34 | Queries, scheduling |
| `config.rs` | ~15 | Config loading, defaults |
| `check.rs` | ~12 | Cycle/orphan detection |
| `usage.rs` | ~10 | Usage logging |

These are **good** — they exist but are separate from the integration test files under review.

### 3.5 Specific Feature Gaps

| Feature | Status | Gap |
|---------|--------|-----|
| `wg evolve` (mutation/crossover/tournament) | **Untested** | 2,677-line module with complex evolutionary logic |
| `wg service` coordinator_tick internals | Partially tested | Auto-evaluate/assign tested via extracted logic; actual tick loop untested |
| Executor config loading (TOML files) | **Untested** | `ExecutorConfig`, `PromptTemplate` parsing |
| IPC protocol | Partially tested | GraphChanged only; status/stop commands untested |
| `wg spawn` wrapper script generation | **Untested** | The shell script that pipes prompts |
| `wg edit` with flock | **Untested** | File locking during edits |
| Graph locking under concurrency | Partially tested | parser.rs has unit tests; no concurrent integration test |
| `not_before` scheduling | Partially tested | query.rs unit tests; no integration test |
| `max_retries` enforcement | **Untested** | retry_count tracked but max_retries never enforced in tests |
| Task `requires` field | **Untested** | Resource requirements |
| Task `estimate` field | Partially tested | query.rs budget/hours tests exist |
| Error output formatting | **Untested** | CLI error messages, stderr formatting |

---

## 4. Reliability Assessment

### Flakiness Risk

| Test | Risk | Reason |
|------|------|--------|
| `test_auto_pickup_via_graph_changed` | **Medium** | Depends on daemon socket readiness timing |
| `test_fallback_poll_pickup` | **Medium** | 2s poll interval with 10s timeout; tight on slow CI |
| `test_dead_agent_recovery` | **Medium-High** | SIGKILL + zombie detection; PID reuse possible; 15s timeout for heartbeat |
| All LLM tests | **High** | Depend on external Claude API; network, cost, rate limits |

**Mitigation already in place:**
- `#[serial]` on service tests prevents parallel contention
- `wait_for()` with configurable timeouts replaces fixed sleeps
- LLM tests are gated behind `#[cfg(feature = "llm-tests")]` or `#[ignore]`

### Test Isolation
- All tests use `tempfile::TempDir` — excellent isolation
- No shared state between tests
- Registry/graph paths are per-test

### Helper Code Quality
- Significant helper duplication across files: `wg_binary()`, `wg_cmd()`, `wg_ok()`, `make_task()` appear in 4-5 files each
- Each file defines its own `make_task()` with slightly different signatures
- `wait_for()` is copied in 3 files
- LLM test setup (`setup_llm_workgraph()`) is duplicated in 2 files

---

## 5. Recommendations

### Priority 1: Critical Untested Code

1. **`wg evolve` tests** — The 2,677-line evolution module has zero test coverage. This is the most complex algorithmic code in the project. Recommend:
   - Unit tests for mutation/crossover operators
   - Tournament selection logic
   - Population management (generation tracking, fitness-proportionate selection)
   - Edge cases: evolve with 0 or 1 agent, evolve with all-equal scores

2. **Executor config parsing tests** — The TOML executor configs (`shell.toml`, `claude.toml`) are loaded at runtime with template variable substitution. No tests verify this parsing.

3. **`wg service` coordinator_tick coverage** — The test suite extracts coordinator logic into test helpers rather than testing the actual `coordinator_tick()` function. This means config interactions, error handling, and logging paths are untested.

### Priority 2: High-Value New Tests

4. **CLI command smoke tests** — A single test file that runs each CLI command with `--help` and verifies exit code 0. Catches argument parsing regressions.

5. **`wg show --json` output format** — Multiple test files depend on `wg show --json` output but never verify its complete schema. A dedicated test would catch output format regressions.

6. **`max_retries` enforcement** — Tests track `retry_count` but never test what happens when `max_retries` is reached.

7. **Concurrent graph operations** — parser.rs has file-locking unit tests, but no integration test exercises two processes competing for the graph lock.

### Priority 3: Consolidation & Patterns

8. **Extract shared test helpers into a common module** — Create `tests/common/mod.rs` with:
   - `wg_binary()`, `wg_cmd()`, `wg_ok()`, `wg_err()` — CLI helpers
   - `make_task()` with a builder pattern for flexible task construction
   - `wait_for()` — polling helper
   - `setup_workgraph()` — tempdir initialization
   - `setup_agency()` — agency directory setup
   - `setup_llm_workgraph()` — LLM test setup

   This would reduce ~500 lines of duplicated helper code and ensure consistency.

9. **Consider property-based tests** for:
   - Content hash determinism (same inputs → same hash, always)
   - Performance average calculation (update_performance matches recalculate_avg_score)
   - Graph save/load roundtrip (any valid graph survives serialization)

### Priority 4: Tests to Remove or Improve

10. **`test_is_process_alive_*` tests** (integration_service_coordinator.rs:846-857) — These test `libc::kill(pid, 0)` directly, not any workgraph code. They're testing the OS. Remove or replace with tests that exercise the actual `is_process_alive()` function in the registry.

11. **Duplicated `recalculate_avg_score` tests** — `test_recalculate_avg_score_empty` appears in both `integration_agency_edge_cases.rs` and `evaluation_recording.rs`. Remove the duplicate.

12. **Reduce LLM test fragility** — The `test_agent_creation_via_llm` test instructs Claude to run 4 specific commands and parse output hashes. Consider pre-creating the entities and only testing the final `wg done` step to reduce failure surface.

---

## 6. Coverage Summary

| Category | Source Lines | Test Lines | Coverage |
|----------|-------------|------------|----------|
| Agency system (agency.rs) | ~2,346 | ~3,850 | **Excellent** |
| Service coordinator logic | ~2,293 | ~2,034 | **Good** (via extracted logic) |
| Service registry (registry.rs) | ~917 | ~600 | **Good** |
| Review workflow | ~219 | ~473 | **Excellent** |
| Graph/Parser/Query/Config | ~2,366 | ~108 inline | **Good** (inline unit tests) |
| CLI commands (~66 files) | ~23,000 | ~0 | **None** |
| TUI (3 files) | ~4,362 | 0 | **None** |
| Matrix integration | ~2,787 | 0 | **None** |
| Evolution (evolve.rs) | ~2,677 | 0 | **None** |
| Executor config/template | ~969 | ~100 | **Minimal** |

**Overall assessment:** The test suite provides excellent coverage of the agency data model, coordinator logic, and review workflow. However, CLI commands (the largest code category at ~23,000 lines), TUI, Matrix integration, and the evolution module are completely untested. The agency system is the most thoroughly tested part of the codebase, with ~3,850 lines of tests covering edge cases, lineage, evaluation recording, and skill resolution in depth.

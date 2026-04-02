# Investigation: Triage System Task Naming — Dot vs True Tasks

**Task:** investigate-triage-system
**Date:** 2026-04-02
**Status:** CONFIRMED — triage creates **true tasks** (no dot prefix). No fix needed.

## Summary

The failed-dependency triage system (design-failed-dep-triage, impl-failed-dep-triage) correctly creates **true tasks** with no dot prefix. The dot prefix (`.`) is reserved for agency pipeline plumbing tasks (`.place-*`, `.assign-*`, `.flip-*`, `.evaluate-*`), and the triage system does not use it.

## Evidence

### 1. Triage Prompt Template (`src/service/executor.rs:306-332`)

The `TRIAGE_MODE_SECTION` constant instructs agents to create fix tasks with this template:

```
wg add "Fix: <description>" --before <failed-dep-id> \
  --verify "<validation command>" \
  -d "<details from failure logs>"
```

The `"Fix: <description>"` pattern produces kebab-case IDs like `fix-config-parser-nested-keys` — standard true task IDs with no dot prefix.

**No dot-prefix naming is suggested or enforced anywhere in the triage prompt.**

### 2. Context Injection (`src/commands/spawn/execution.rs:133-149`)

The failed-dep detection code in `execution.rs` sets `has_failed_deps = true` and populates `failed_deps_info` with dependency failure details. It does not inject any naming convention for fix tasks — that's entirely in the prompt template.

### 3. Integration Tests — All Use True Task IDs

| Test File | Fix Task IDs Created | Dot Prefix? |
|---|---|---|
| `tests/integration_triage.rs:189` | `fix-parser` | No |
| `tests/integration_triage_smoke.rs:159` | `fix-a` | No |
| `tests/integration_triage_smoke.rs:279,322,350` | `fix-v1`, `fix-v2`, `fix-v3` | No |
| `tests/integration_triage_smoke.rs:473,489` | `fix-x`, `fix-y` | No |
| `tests/integration_triage_smoke.rs:595,651` | `fix-config`, `fix-schema` | No |

All six test scenarios consistently use `fix-*` true task IDs.

### 4. Coordinator Dispatch — No Naming Enforcement

The coordinator (`src/commands/service/coordinator.rs`) does not enforce or suggest any naming convention for triage-created tasks. The design doc (`docs/designs/failed-dep-triage.md`) explicitly states the coordinator's role is minimal — triage is agent-driven.

### 5. System Task Definition (`src/graph.rs:381-385`)

```rust
pub fn is_system_task(task_id: &str) -> bool {
    task_id.starts_with('.')
}
```

System tasks (dot-prefixed) are used for agency pipeline steps:
- `.place-*` — placement
- `.assign-*` — assignment
- `.flip-*` — FLIP verification
- `.evaluate-*` — evaluation

Triage fix tasks (`fix-*`) do not match this pattern and are correctly treated as regular user-visible tasks.

### 6. Design Doc (`docs/designs/failed-dep-triage.md`)

The design doc's scenarios (Section 5) consistently show fix tasks as true tasks:
- `"Fix: config parser nested key handling"` (Scenario A, line 222)
- `"Fix: issue in A"` / `"Fix: issue in B"` (Scenario D, line 331-333)

No mention of dot-prefix naming anywhere in the design.

## Conclusion

**Current behavior is correct.** The triage system creates true tasks (no dot prefix). No code changes needed.

The fix-triage-system downstream task should be updated to reflect this finding — there is no bug to fix regarding task naming in the triage system.

## Files Examined

- `src/service/executor.rs:304-332` — `TRIAGE_MODE_SECTION` constant
- `src/commands/spawn/execution.rs:133-149` — failed dep detection
- `src/commands/spawn/context.rs:44-48` — failed dep context injection
- `src/commands/requeue.rs` — full file (requeue mechanism)
- `src/commands/service/coordinator.rs` — coordinator dispatch
- `src/graph.rs:381-385` — `is_system_task()` definition
- `docs/designs/failed-dep-triage.md` — design doc
- `tests/integration_triage.rs` — integration tests
- `tests/integration_triage_smoke.rs` — smoke tests

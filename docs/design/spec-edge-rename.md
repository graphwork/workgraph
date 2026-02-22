# Edge Rename Implementation Specification

## `blocked_by` → `after`, `blocks` → `before`

**Date:** 2026-02-21
**Status:** Implementation spec (Phase 2 of cycle-aware-graph.md)
**Scope:** Mechanical rename + serde/CLI backward compatibility

---

## Table of Contents

1. [Overview](#1-overview)
2. [Core Data Model](#2-core-data-model-srcgraphrs)
3. [Query Engine](#3-query-engine-srcqueryrs)
4. [Check Module](#4-check-module-srccheckrs)
5. [CLI Definitions](#5-cli-definitions-srcmainrs)
6. [Command Modules](#6-command-modules-srccommands)
7. [TUI Layer](#7-tui-layer-srctui)
8. [Other Source Files](#8-other-source-files)
9. [Test Files](#9-test-files)
10. [Documentation & Config](#10-documentation--config)
11. [Status::Blocked Decision](#11-statusblocked-decision)
12. [Function Renames](#12-function-renames)
13. [Backward Compatibility Summary](#13-backward-compatibility-summary)
14. [Parallelization Guide](#14-parallelization-guide)

---

## 1. Overview

### Rename mapping

| Old | New | Context |
|-----|-----|---------|
| `blocked_by` (field) | `after` | Task struct field, variables, function params |
| `blocks` (field) | `before` | Task struct field (computed inverse) |
| `--blocked-by` (CLI) | `--after` | CLI flag on `wg add`, `wg edit`, `wg split` |
| `--add-blocked-by` (CLI) | `--add-after` | CLI flag on `wg edit` |
| `--remove-blocked-by` (CLI) | `--remove-after` | CLI flag on `wg edit` |
| `blocked_by()` (function) | `pending_predecessors()` | Query function in `src/query.rs` |
| `is_blocker_satisfied()` | `is_predecessor_satisfied()` | Query function in `src/query.rs` |
| `wg blocked` (command) | `wg blocked` | **Keep** — shows what predecessors remain |
| `wg why-blocked` (command) | `wg why-blocked` | **Keep** — "blocked" here is runtime state |
| `Status::Blocked` (enum) | `Status::Blocked` | **Keep** — see §11 |
| `blocked` (summary field) | `waiting` | ProjectSummary field in query.rs |

### Principles

1. **Serde backward compat**: `#[serde(alias = "blocked_by")]` on `after`, `#[serde(alias = "blocks")]` on `before`.
2. **CLI backward compat**: `--blocked-by` as hidden alias for `--after`; `--add-blocked-by` / `--remove-blocked-by` as hidden aliases.
3. **Runtime state stays "blocked"**: The `wg blocked`, `wg why-blocked` commands and `Status::Blocked` variant are about runtime state, not edge type. They stay.
4. **Variable naming**: Internal variables like `blocker_id` become `pred_id`; `blockers` becomes `predecessors`.

---

## 2. Core Data Model — `src/graph.rs`

### 2.1 Task struct (line 148–235)

| Line | Old | New | Type |
|------|-----|-----|------|
| 161 | `pub blocks: Vec<String>,` | `pub before: Vec<String>,` | Field rename |
| 163 | `pub blocked_by: Vec<String>,` | `pub after: Vec<String>,` | Field rename |

**Serde attributes** — change from:
```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub blocks: Vec<String>,
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub blocked_by: Vec<String>,
```
to:
```rust
#[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocks")]
pub before: Vec<String>,
#[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocked_by")]
pub after: Vec<String>,
```

**Classification:** Mechanical rename + serde alias addition.

### 2.2 TaskHelper struct (line 254–319)

| Line | Old | New |
|------|-----|-----|
| 267 | `blocks: Vec<String>,` | `before: Vec<String>,` |
| 269 | `blocked_by: Vec<String>,` | `after: Vec<String>,` |

Add serde aliases on TaskHelper too:
```rust
#[serde(default, alias = "blocks")]
before: Vec<String>,
#[serde(default, alias = "blocked_by")]
after: Vec<String>,
```

### 2.3 Task Deserialize impl (line 321–372)

| Line | Old | New |
|------|-----|-----|
| 345 | `blocks: helper.blocks,` | `before: helper.before,` |
| 346 | `blocked_by: helper.blocked_by,` | `after: helper.after,` |

### 2.4 WorkGraph doc comment (line 427–431)

```
// Old: "Tasks depend on other tasks via `blocked_by`/`blocks` edges."
// New: "Tasks depend on other tasks via `after`/`before` edges."
```

### 2.5 remove_node() (line 526–543)

| Line | Old | New |
|------|-----|-----|
| 529 | comment: `blocked_by`, `blocks` | `after`, `before` |
| 535 | `task.blocked_by.retain(...)` | `task.after.retain(...)` |
| 536 | `task.blocks.retain(...)` | `task.before.retain(...)` |

### 2.6 LoopEdge doc comment (line 5–7)

```
// Old: "separate from `blocked_by` and don't affect"
// New: "separate from `after` and don't affect"
```

### 2.7 evaluate_loop_edges() (line 570–)

- Line 682 comment: `blocked_by` → `after`
- Line 755 comment: `blocked_by` → `after`
- Within `find_intermediate_tasks()`: any references to `blocked_by` become `after`

### 2.8 Tests in graph.rs

- Line 867: `assert!(!Status::Blocked.is_terminal());` — **Keep** (Status::Blocked stays)

**Test implications:** All tests in graph.rs that construct Task values with `blocked_by:` or `blocks:` fields need updating to `after:` and `before:`.

---

## 3. Query Engine — `src/query.rs`

### 3.1 ProjectSummary struct (line 34–42)

| Line | Old | New |
|------|-----|-----|
| 39 | `pub blocked: usize,` | `pub waiting: usize,` |

**Note:** This is a derived count, not a serialized field. But JSON output changes — `"blocked"` → `"waiting"` in `wg status` JSON. This is a **breaking JSON output change** — callers parsing `"blocked"` key need updating.

### 3.2 project_summary() (line 63–97)

| Line | Old | New |
|------|-----|-----|
| 70 | `let mut blocked_count = 0;` | `let mut waiting_count = 0;` |
| 79 | `blocked_count += 1;` | `waiting_count += 1;` |
| 89–92 | `Status::Blocked => { blocked_count += 1; }` | `Status::Blocked => { waiting_count += 1; }` |
| 100+ | `blocked: blocked_count,` | `waiting: waiting_count,` |

### 3.3 ready_tasks() (line 248–273)

| Line | Old | New |
|------|-----|-----|
| 265 | `task.blocked_by.iter().all(\|blocker_id\| {` | `task.after.iter().all(\|pred_id\| {` |
| 267 | `graph.get_task(blocker_id)` | `graph.get_task(pred_id)` |
| 269 | comment: `blocker doesn't exist, treat as unblocked` | `predecessor doesn't exist, treat as ready` |

### 3.4 is_blocker_satisfied() → is_predecessor_satisfied() (line 275–298)

- Rename function: `is_blocker_satisfied` → `is_predecessor_satisfied`
- Rename param: `blocker_id` → `pred_id`
- Update doc comment: "blocked_by dependency" → "after dependency"
- Update comment: "Can't resolve without workgraph dir; treat as blocked" → "...treat as waiting"

### 3.5 ready_tasks_with_peers() (line 300–323)

| Line | Old | New |
|------|-----|-----|
| 318 | `task.blocked_by.iter()` | `task.after.iter()` |
| 320 | `is_blocker_satisfied(blocker_id, ...)` | `is_predecessor_satisfied(pred_id, ...)` |

### 3.6 blocked_by() → pending_predecessors() (line 326–336)

- Rename function: `blocked_by` → `pending_predecessors`
- Internal: `task.blocked_by` → `task.after`
- All callers must update

### 3.7 build_reverse_index() (approx line 220–245)

References to `task.blocked_by` become `task.after`. Variable names `blocker_id` → `pred_id`.

### 3.8 Other references throughout query.rs

- Line 194: `let blockers_done = task.blocked_by.iter().all(...)` → `let preds_done = task.after.iter().all(...)`
- Line 236: `for blocker_id in &task.blocked_by` → `for pred_id in &task.after`

**Test implications:** All test helpers and assertions using `blocked_by` field name need updating.

---

## 4. Check Module — `src/check.rs`

### 4.1 StuckBlocked struct (line 31–36)

| Line | Old | New |
|------|-----|-----|
| 31 | comment: `blocked_by tasks` | `after tasks` |
| 36 | `pub blocked_by_ids: Vec<String>,` | `pub after_ids: Vec<String>,` |

### 4.2 Cycle detection (line 90–)

| Line | Old | New |
|------|-----|-----|
| 96 | comment: `Follow blocked_by edges` | `Follow after edges` |
| 97 | `for dep_id in &task.blocked_by {` | `for dep_id in &task.after {` |

### 4.3 stuck_blocked check (line 189–)

| Line | Old | New |
|------|-----|-----|
| 189 | comment: `status=Blocked where all blocked_by tasks` | `status=Blocked where all after tasks` |
| 198 | `task.blocked_by.is_empty()` | `task.after.is_empty()` |
| 201 | `task.blocked_by.iter().all(...)` | `task.after.iter().all(...)` |
| 209 | `blocked_by_ids: task.blocked_by.clone()` | `after_ids: task.after.clone()` |

### 4.4 Orphan reference detection (line 220–)

| Line | Old | New |
|------|-----|-----|
| 222 | `for blocked_by in &task.blocked_by {` | `for dep in &task.after {` |
| 227 | `relation: "blocked_by".to_string()` | `relation: "after".to_string()` |
| 232 | `for blocks in &task.blocks {` | `for dep in &task.before {` |
| 237 | `relation: "blocks".to_string()` | `relation: "before".to_string()` |

**Note:** The `relation` string values are used in `wg check` output. Old graphs checked with new code will see `"after"` instead of `"blocked_by"`. This is acceptable.

### 4.5 Tests (line 297–984)

All test functions constructing tasks with `blocked_by` / `blocks` fields need mechanical rename to `after` / `before`. ~30 test assertions reference these fields. The test `test_detects_orphan_blocked_by` (line 370) should be renamed `test_detects_orphan_after_ref`.

---

## 5. CLI Definitions — `src/main.rs`

### 5.1 Add command (line 47–126)

| Line | Old | New |
|------|-----|-----|
| 63 | `/// This task is blocked by another task` | `/// This task comes after another task (dependency)` |
| 64 | `#[arg(long = "blocked-by", ...)]` | `#[arg(long = "after", alias = "blocked-by", ...)]` |
| 65 | `blocked_by: Vec<String>,` | `after: Vec<String>,` |

### 5.2 Edit command (line 128–196)

| Line | Old | New |
|------|-----|-----|
| 141 | `/// Add a blocked-by dependency` | `/// Add an after dependency` |
| 142 | `#[arg(long = "add-blocked-by")]` | `#[arg(long = "add-after", alias = "add-blocked-by")]` |
| 143 | `add_blocked_by: Vec<String>,` | `add_after: Vec<String>,` |
| 145 | `/// Remove a blocked-by dependency` | `/// Remove an after dependency` |
| 146 | `#[arg(long = "remove-blocked-by")]` | `#[arg(long = "remove-after", alias = "remove-blocked-by")]` |
| 147 | `remove_blocked_by: Vec<String>,` | `remove_after: Vec<String>,` |

### 5.3 Blocked / WhyBlocked commands (line 279–289)

**Keep as-is.** These commands show runtime blocking state, which is still called "blocked" (a task is "blocked" when its predecessors aren't done). The command names `wg blocked` and `wg why-blocked` remain. Doc comments can be updated:

| Line | Old | New |
|------|-----|-----|
| 279 | `/// Show what's blocking a task` | `/// Show what predecessors are blocking a task` |
| 285 | `/// Show the full transitive chain explaining why a task is blocked` | Keep |

### 5.4 Split/Constraint command (approx line 1131)

| Line | Old | New |
|------|-----|-----|
| 1131 | `#[arg(long = "blocked-by")]` | `#[arg(long = "after", alias = "blocked-by")]` |
| 1132 | `blocked_by: Vec<String>,` | `after: Vec<String>,` |

### 5.5 Command dispatch (line 2150–2402)

All references to destructured `blocked_by`, `add_blocked_by`, `remove_blocked_by` variables need updating to `after`, `add_after`, `remove_after`.

**Classification:** Mechanical rename + hidden alias addition.

---

## 6. Command Modules — `src/commands/`

### 6.1 `src/commands/add.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 50 | `blocked_by: &[String],` | `after: &[String],` | Param rename |
| 100–101 | comment: `Validate blocked_by references` | `Validate after references` | Comment |
| 101 | `for blocker_id in blocked_by {` | `for pred_id in after {` | Variable |
| 165 | `blocks: vec![],` | `before: vec![],` | Field init |
| 166 | `blocked_by: blocked_by.to_vec(),` | `after: after.to_vec(),` | Field init |
| 195–204 | `blocks` bidirectional consistency | `before` | Field access |
| 245, 276, 309, 333, 371–372, 400–405 | same patterns | same renames | Mechanical |
| 883+ | tests: `blocked_by_updates_blocker_blocks_field` | `after_updates_predecessor_before_field` | Test rename |

**~40 lines affected. All mechanical.**

### 6.2 `src/commands/edit.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 17–18 | `add_blocked_by: &[String], remove_blocked_by: &[String],` | `add_after: &[String], remove_after: &[String],` | Params |
| 45 | `for dep in add_blocked_by {` | `for dep in add_after {` | Variable |
| 83–98 | `task.blocked_by.contains/push/remove` | `task.after.contains/push/remove` | Field access |
| 87 | `println!("Added blocked_by: {}", dep)` | `println!("Added after: {}", dep)` | User-facing output |
| 98 | `println!("Removed blocked_by: {}", dep)` | `println!("Removed after: {}", dep)` | User-facing output |
| 230–241 | `blocks` bidirectional consistency | `before` | Field access |
| 432+ | tests: `test_add_blocked_by`, `test_remove_blocked_by`, etc. | `test_add_after`, `test_remove_after` | Test rename |

**~50 lines affected. Mechanical + output string changes.**

### 6.3 `src/commands/show.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 45–46 | `blocked_by: Vec<BlockerInfo>, blocks: Vec<BlockerInfo>,` | `after: Vec<PredecessorInfo>, before: Vec<PredecessorInfo>,` | Field + type rename |
| 94–99 | `task.blocked_by` | `task.after` | Field access |
| 134 | tasks this task blocks | tasks this task comes before | Comment |
| 168–169 | `blocked_by: ..., blocks: ...` | `after: ..., before: ...` | Struct init |
| 281–296 | Display labels "Blocked by:" / "Blocks:" | "After:" / "Before:" | User-facing output |

**Rename `BlockerInfo` → `PredecessorInfo` or keep generic name since used for both directions.**

### 6.4 `src/commands/done.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 24 | comment: `Check for unresolved blockers` | `Check for unresolved predecessors` | Comment |
| 25 | `let blockers = query::blocked_by(&graph, id);` | `let predecessors = query::pending_predecessors(&graph, id);` | Function call |
| 175+ | tests with `blocked_by` field | `after` field | Test rename |

### 6.5 `src/commands/service.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 548, 556–557 | `blocks: vec![task_id.clone()], blocked_by: vec![]` | `before: vec![task_id.clone()], after: vec![]` | Field init |
| 587, 589 | `t.blocked_by.contains/push` | `t.after.contains/push` | Field access |
| 686–687 | `blocks: vec![], blocked_by: vec![task_id.clone()]` | `before: vec![], after: vec![task_id.clone()]` | Field init |
| 730–745 | `t.blocked_by.len()`, `t.blocked_by[0]`, `t.blocked_by.retain` | `t.after.len()`, `t.after[0]`, `t.after.retain` | Field access |
| 1669 | `blocked_by: Vec<String>,` | `after: Vec<String>,` | Struct field |
| 2454, 2472 | `blocked_by` variable | `after` variable | Variable |
| 2712, 2785–2786, 2814–2819 | same pattern | same rename | Mechanical |
| 3740–4162 | ~15 test Task inits with `blocks: vec![], blocked_by: vec![]` | `before: vec![], after: vec![]` | Test |

**~60 lines affected. Mechanical.**

### 6.6 `src/commands/replay.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 274 | comment: `blocked_by, blocks` | `after, before` | Comment |
| 283 | `for dep in &task.blocked_by {` | `for dep in &task.after {` | Field access |
| 306 | comment: `blocks edges forward` | `before edges` | Comment |
| 320 | `for blocked in &task.blocks {` | `for dep in &task.before {` | Field access |
| 405–580 | tests: `blocks`, `blocked_by` field inits | `before`, `after` | Test |

### 6.7 `src/commands/trace_instantiate.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 76 | `blocked_by: &[String],` | `after: &[String],` | Param |
| 132–133 | comment: `external blocked-by references` | `external after references` | Comment |
| 133 | `for dep in blocked_by {` | `for dep in after {` | Variable |
| 162–177 | `blocked_by` remapping logic | `after` | Variable + field |
| 169 | `"blocked_by '{}' in template"` | `"after '{}' in template"` | String literal |
| 221 | `print_dry_run_task(... &real_blocked_by ...)` | `&real_after` | Variable |
| 230–231 | `blocks: vec![], blocked_by: real_blocked_by.clone()` | `before: vec![], after: real_after.clone()` | Field init |
| 259–264 | `blocks` bidirectional consistency | `before` | Field access |
| 340–343 | `"blocked by {}"` format string | `"after {}"` | User-facing output |
| 466+ | `blocked_by: &[String]` param | `after: &[String]` | Param |
| 473–474 | `"Blocked by: {}"` output | `"After: {}"` | User-facing output |
| 555–592 | tests with `blocked_by` field | `after` | Test |
| 649 | test: `instantiate_remaps_blocked_by` | `instantiate_remaps_after` | Test rename |
| 760 | test: `instantiate_applies_external_blocked_by` | `instantiate_applies_external_after` | Test rename |
| 1093–1096 | `plan.blocks.contains(...)` | `plan.before.contains(...)` | Test assertion |

### 6.8 `src/commands/viz.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 99–101 | comments: `blocked_by` | `after` | Comment |
| 295–314 | `for blocked_by in &task.blocked_by { ... label="blocks" }` | `for pred in &task.after { ... label="after" }` | Field access + DOT label |
| 407–417 | same pattern (Mermaid output) | same rename | Output |
| 484–507 | forward index using `blocked_by` | `after` | Field access |
| 635–640 | parent → children via `blocked_by` | `after` | Field access |
| 760, 986 | `for blocker in &task.blocked_by {` | `for pred in &task.after {` | Variable |
| 928–937 | DOT edge label `"blocks"` | `"after"` or `"depends on"` | String literal |
| 1529–2384 | tests: `blocks`, `blocked_by` field inits | `before`, `after` | Test |
| 1900–1905 | helper: `blocked_by: blocked_by.into_iter()...` | `after: after.into_iter()...` | Test helper |
| 1933–2082 | many test lines with `blocked_by` | `after` | Test |

**~80 lines affected. Mechanical + DOT/Mermaid output strings.**

### 6.9 `src/commands/critical_path.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 15 | `blocked_by: Option<String>,` | `after: Option<String>,` | Struct field |
| 66 | comment: `task_id -> list of tasks it blocks` | `task_id -> list of tasks it comes before` | Comment |
| 75 | `t.blocked_by.iter().all(...)` | `t.after.iter().all(...)` | Field access |
| 115–125 | `blocked_by` in CritPathStep init | `after` | Field init |
| 234 | comment: `tasks that it blocks` | `tasks it precedes` | Comment |
| 248 | `for blocker_id in &task.blocked_by {` | `for pred_id in &task.after {` | Variable |
| 354 | `for dep_id in &task.blocked_by {` | `for dep_id in &task.after {` | Variable |
| 399–686 | tests with `blocks`, `blocked_by` | `before`, `after` | Test |

### 6.10 `src/commands/forecast.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 183 | `for blocker_id in &task.blocked_by {` | `for pred_id in &task.after {` | Variable |
| 315 | `task.blocked_by.iter().all(...)` | `task.after.iter().all(...)` | Field access |
| 499 | `"blocks {} tasks"` format | `"precedes {} tasks"` or similar | Output |
| 597+ | tests with `blocked_by` | `after` | Test |

### 6.11 `src/commands/analyze.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 56 | `pub transitive_blocks: usize,` | `pub transitive_before: usize,` | Struct field |
| 649–653 | `blocks` counting logic | `before` | Field access |
| 799–857 | bottleneck analysis using `blocks` | `before` | Field access |
| 1273, 1358–1374 | tests | Test |

### 6.12 `src/commands/bottlenecks.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 15–16 (approx) | `direct_blocks: usize, transitive_blocks: usize` | `direct_before: usize, transitive_before: usize` | Struct fields |
| 87, 89 | field access | rename | Mechanical |

### 6.13 `src/commands/blocked.rs`

This module implements `wg blocked <task-id>`. The **command stays** (`wg blocked` shows runtime blocking state). Internal code changes:

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 3 | `use workgraph::query::blocked_by;` | `use workgraph::query::pending_predecessors;` | Import |
| 17 | `let blockers = blocked_by(&graph, id);` | `let predecessors = pending_predecessors(&graph, id);` | Function call |
| 34 | `"Task '{}' is blocked by:"` | Keep (runtime state description) | Output |
| 96+ | tests using `blocked_by` field | `after` field | Test |

### 6.14 `src/commands/why_blocked.rs`

Command stays. Internal changes:

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 81 | `for blocker_id in &task.blocked_by {` | `for pred_id in &task.after {` | Variable |
| 137 | `task.blocked_by.iter().all(...)` | `task.after.iter().all(...)` | Field access |
| 284 | JSON key `"blocked_by"` in output | `"blocked_by"` → `"after"` | JSON output key |
| 320+ | tests with `blocked_by` field | `after` | Test |

### 6.15 `src/commands/trace_export.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 42 | `pub blocked_by: Vec<String>,` | `pub after: Vec<String>,` | Struct field (ExportedTask) |
| 44 | `pub blocks: Vec<String>,` | `pub before: Vec<String>,` | Struct field (ExportedTask) |
| 122–123 | `blocked_by: t.blocked_by.clone(), blocks: t.blocks.clone()` | `after: t.after.clone(), before: t.before.clone()` | Field init |

**Note:** This changes the exported JSON schema. Add `#[serde(alias = "blocked_by")]` and `#[serde(alias = "blocks")]` if import backward compat is needed.

### 6.16 `src/commands/coordinate.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 38 | `pub blocked_by: Vec<String>,` | `pub after: Vec<String>,` | Struct field |
| 60 | `blocked_by: task.blocked_by.clone(),` | `after: task.after.clone(),` | Field init |
| 82–83 | `t.blocked_by.is_empty()`, `t.blocked_by.iter().any(...)` | `t.after.is_empty()`, `t.after.iter().any(...)` | Field access |
| 195 | `task.blocked_by.join(", ")` | `task.after.join(", ")` | Field access |
| 263+ | tests with `blocked_by` | `after` | Test |

### 6.17 `src/commands/status.rs` (coordinate output)

References `blocked_by` in status display structs. Mechanical rename.

### 6.18 `src/commands/list.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 38 | `"blocked_by": t.blocked_by,` | `"after": t.after,` | JSON key |
| 317+ | tests with `blocked_by` field | `after` | Test |

**Note:** JSON output key changes from `"blocked_by"` to `"after"`. This is a **visible JSON output change**.

### 6.19 `src/commands/ready.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 28 | `task.blocked_by.iter().all(...)` | `task.after.iter().all(...)` | Field access |
| 335+ | tests with `blocked_by` | `after` | Test |

### 6.20 `src/commands/graph.rs` (DOT output)

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 208 | `for blocked in &task.blocked_by {` | `for pred in &task.after {` | Variable |
| 211 | `label="blocks"` | `label="after"` | DOT label |
| 263, 287 | `Status::Blocked => ...` | **Keep** | Status match |
| 398+ | tests with `blocked_by` | `after` | Test |

### 6.21 Remaining command files (mechanical only)

These files contain `blocked_by` or `blocks` field references in task construction or iteration. All are mechanical renames:

| File | Approximate lines affected |
|------|---------------------------|
| `src/commands/loops.rs` | 5–10 |
| `src/commands/notify.rs` | `Status::Blocked` match — **keep** |
| `src/commands/spawn.rs` | `Status::Blocked` match — **keep**; `blocked_by` field init — rename |
| `src/commands/evolve.rs` | field references |
| `src/commands/trace.rs` | field references |
| `src/commands/trace_function_cmd.rs` | field references |
| `src/commands/trace_extract.rs` | field references |
| `src/commands/gc.rs` | field references |
| `src/commands/impact.rs` | field references |
| `src/commands/cost.rs` | field references |
| `src/commands/context.rs` | field references |
| `src/commands/structure.rs` | field references |
| `src/commands/aging.rs` | field references |
| `src/commands/next.rs` | field references |
| `src/commands/trajectory.rs` | field references |
| `src/commands/quickstart.rs` | string literals `"--blocked-by"` in help text |
| `src/commands/peer.rs` | `Status::Blocked` — **keep** |
| `src/commands/resources.rs` | `Status::Blocked` — **keep** |

---

## 7. TUI Layer — `src/tui/`

### 7.1 `src/tui/mod.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 990 | `if !task.blocked_by.is_empty() {` | `if !task.after.is_empty() {` | Field access |
| 997 | `for blocker in &task.blocked_by {` | `for pred in &task.after {` | Variable |
| 1007 | `if !task.blocks.is_empty() {` | `if !task.before.is_empty() {` | Field access |
| 1014 | `for blocked in &task.blocks {` | `for dep in &task.before {` | Variable |
| 1348 | `Status::Blocked => "[B]",` | **Keep** | Status display |
| 1360 | `Status::Blocked => Color::DarkGray,` | **Keep** | Status color |

**User-facing labels** in TUI detail pane:
- "Blocked by:" → "After:" (or keep "Blocked by:" for UX clarity — **decision needed**)
- "Blocks:" → "Before:" (or keep "Blocks:" for UX clarity)

**Recommendation:** Change to "After:" / "Before:" in the detail pane labels, since users will see `--after` in CLI help.

### 7.2 `src/tui/app.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 39 | `Status::Blocked => 3,` | **Keep** | Status ordering |
| 590 | comment: `no blocked_by` | `no after` | Comment |
| 601 | comment: `blocked_by parent_id` | `after parent_id` | Comment |
| 606 | `if task.blocked_by.is_empty() {` | `if task.after.is_empty() {` | Field access |
| 609 | `for blocker_id in &task.blocked_by {` | `for pred_id in &task.after {` | Variable |
| 773 | `Status::Blocked => 3,` | **Keep** | Status ordering |
| 787 | `task.blocked_by.is_empty()` | `task.after.is_empty()` | Field access |
| 790 | `for blocker_id in &task.blocked_by {` | `for pred_id in &task.after {` | Variable |
| 1111 | `Status::Blocked => counts.blocked += 1,` | **Keep** or rename `counts.blocked` → `counts.waiting` | Logic |
| 1542–1626 | tests with `Status::Blocked` | **Keep** | Test |

### 7.3 `src/tui/graph_layout.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 257 | `for blocker_id in &task.blocked_by {` | `for pred_id in &task.after {` | Variable |
| 571 | `for blocker_id in &task.blocked_by {` | `for pred_id in &task.after {` | Variable |
| 1336 | `Status::Blocked => "B",` | **Keep** | Status abbreviation |
| 1346 | helper: `blocked_by: Vec<&str>` | `after: Vec<&str>` | Test helper |
| 1350 | `blocked_by: blocked_by.into_iter()...` | `after: after.into_iter()...` | Test helper |

---

## 8. Other Source Files

### 8.1 `src/trace_function.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 103 | `pub blocked_by: Vec<String>,` | `pub after: Vec<String>,` | Struct field (TraceFunctionTask) |
| 398 | `blocked_by: template.blocked_by.clone(),` | `after: template.after.clone(),` | Field init |
| 418+ | comments: `blocked_by references` | `after references` | Comment |
| 437–441 | `for dep in &task.blocked_by {` ... `"blocked_by '{}'"` | `for dep in &task.after {` ... `"after '{}'"` | Variable + string |
| 458–466 | `"Circular blocked_by dependency"` | `"Circular after dependency"` | Error string |
| 472–474 | `t.blocked_by.iter().any(...)` | `t.after.iter().any(...)` | Field access |
| 537–573 | tests with `blocked_by` field | `after` field | Test |
| 935, 974+ | tests | Test |

Add `#[serde(alias = "blocked_by")]` on the struct field for backward compat with existing trace function YAML files.

### 8.2 `src/federation.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 575 | `"blocked" => crate::graph::Status::Blocked,` | **Keep** | Status parsing |
| 1446 | `crate::graph::Status::Blocked` | **Keep** | Test assertion |

**No rename needed** — this file only references `Status::Blocked`, which stays.

### 8.3 `src/matrix_commands.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 328, 528 | `Status::Blocked` | **Keep** | Status match |
| 556 | `t.blocked_by.iter().all(...)` | `t.after.iter().all(...)` | Field access |

### 8.4 `src/parser.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 413 | comment: `Acquire exclusive lock (LOCK_EX) - blocks until available` | **Keep** — this is about file locking, not task blocking | No change |

### 8.5 `src/service/executor.rs`

| Lines | Old | New | Type |
|-------|-----|-----|------|
| 414 | `"wg add \"title\" --blocked-by {{task_id}}"` | `"wg add \"title\" --after {{task_id}}"` | String literal (agent instructions) |
| 573–574 | `blocks: vec![], blocked_by: vec![],` | `before: vec![], after: vec![],` | Field init |

---

## 9. Test Files

All 13 test files contain `blocked_by` / `blocks` field references in task construction and assertions. These are **entirely mechanical renames**.

| File | Approx lines affected | Notes |
|------|----------------------|-------|
| `tests/integration_replay_exhaustive.rs` | ~60 | Heaviest test file |
| `tests/integration_auto_assignment.rs` | ~15 | |
| `tests/integration_trace_functions.rs` | ~10 | |
| `tests/integration_cross_repo_dispatch.rs` | ~10 | Also has `--blocked-by` CLI string |
| `tests/integration_loops.rs` | ~10 | |
| `tests/integration_service_coordinator.rs` | ~5 | |
| `tests/integration_cli_workflows.rs` | ~5 | `--blocked-by` CLI string |
| `tests/integration_error_paths.rs` | ~15 | `test_orphan_blocked_by_reference` rename |
| `tests/integration_check_context.rs` | ~5 | `check_detects_orphan_blocked_by` rename |
| `tests/integration_service.rs` | ~5 | |
| `tests/integration_cli_commands.rs` | ~5 | |
| `tests/integration_loop_workflow.rs` | ~10 | `--blocked-by` CLI string |
| `tests/integration_trace_replay.rs` | ~10 | |

**For CLI integration tests**: Tests using `--blocked-by` as a CLI flag should be updated to `--after`, but since `--blocked-by` is kept as an alias, old tests would still pass. Recommendation: update tests to use `--after` to exercise the new flag; optionally add a few tests that verify `--blocked-by` alias still works.

---

## 10. Documentation & Config

### 10.1 Files requiring updates (48 doc files found)

**Priority 1 — User-facing docs:**

| File | Changes needed |
|------|---------------|
| `README.md` | `--blocked-by` → `--after` in examples; `blocked_by` → `after` in field descriptions |
| `docs/COMMANDS.md` | `--blocked-by` → `--after`; `--add-blocked-by` → `--add-after`; `--remove-blocked-by` → `--remove-after` |
| `docs/README.md` | Same CLI flag updates |
| `docs/AGENT-GUIDE.md` | "blocked" runtime state description — review |
| `.claude/skills/wg/SKILL.md` | `--blocked-by` → `--after` in all examples |
| `CLAUDE.md` | `--blocked-by` → `--after` in the "For Spawned Agents" section |

**Priority 2 — Design/research docs:**

| File | Changes needed |
|------|---------------|
| `docs/design/cycle-aware-graph.md` | Already discusses the rename; update any remaining `blocked_by` references |
| `docs/design/cross-repo-communication.md` | `--blocked-by` → `--after` |
| `docs/design/trace-functions.md` | `blocked_by` → `after` in field references |
| `docs/research/cycle-detection-algorithms.md` | `blocked_by` → `after` |
| `docs/research/organizational-patterns.md` | `blocked_by` → `after` |
| `docs/research/cyclic-processes.md` | `blocked_by` → `after` |
| `docs/cycle-support-audit.md` | "blocked" state references — review |
| `docs/fix-dag-terminology.md` | Already about this rename |
| `docs/design-cyclic-workgraph.md` | `blocked_by` → `after` |
| `docs/test-specs/trace-replay-test-spec.md` | `blocked_by` → `after`, `blocks` → `before` |

**Priority 3 — Archive/review docs (low priority, update opportunistically):**

All files under `docs/archive/` and `docs/research/` — update `blocked_by` → `after` where practical.

**Priority 4 — Manual (.typ files):**

| File | Changes needed |
|------|---------------|
| `docs/manual/workgraph-manual.typ` | `blocked_by` → `after` in examples |
| `docs/manual/02-task-graph.typ` | Field definitions |
| `docs/manual/04-coordination.typ` | CLI examples |
| `docs/manual/01-overview.typ` | Overview references |

---

## 11. Status::Blocked Decision

**Decision: KEEP `Status::Blocked` unchanged.**

Rationale:
- `Status::Blocked` is a runtime state (a task is explicitly set to blocked status, distinct from being implicitly "waiting" because predecessors aren't done).
- It's rarely used — agents don't set `Status::Blocked`; it's for manual human intervention ("I'm putting this on hold because X").
- The design doc says: "'Blocking' becomes a derived runtime state, not a relationship type." This is exactly what `Status::Blocked` already represents.
- Changing it to `Status::Waiting` would cause a serde break in existing graphs where tasks have `status: "blocked"`.
- The `wg blocked` and `wg why-blocked` commands are about runtime state queries, not edge traversal.

**What stays unchanged:**
- `Status::Blocked` enum variant
- `"blocked"` string in serialization/deserialization
- `Status::Blocked` matches in all display/color/status code
- `wg blocked` command name
- `wg why-blocked` command name
- `Commands::Blocked` and `Commands::WhyBlocked` enum variants

**What changes:**
- `ProjectSummary.blocked` field → `ProjectSummary.waiting` (this counts implicitly blocked tasks, not `Status::Blocked` tasks)
- Comments that conflate "blocked" (runtime state) with "blocked_by" (relationship): update to clearly distinguish

---

## 12. Function Renames

| Module | Old Name | New Name | Callers |
|--------|----------|----------|---------|
| `src/query.rs` | `blocked_by()` | `pending_predecessors()` | `commands/blocked.rs`, `commands/done.rs`, tests |
| `src/query.rs` | `is_blocker_satisfied()` | `is_predecessor_satisfied()` | `query.rs::ready_tasks_with_peers()`, tests |
| `src/query.rs` | `build_reverse_index()` | Keep name (generic enough) | Various |
| `src/commands/blocked.rs` | module name | Keep (`blocked` is runtime state) | `main.rs` |
| `src/commands/why_blocked.rs` | module name | Keep | `main.rs` |

**Variable naming conventions:**

| Old | New | Context |
|-----|-----|---------|
| `blocker_id` | `pred_id` | Loop variables iterating over `after` |
| `blockers` | `predecessors` | Collections of predecessor tasks |
| `blocker` | `predecessor` | Single predecessor task |
| `blocked` | `dependent` or `successor` | Task that depends on another |
| `blocked_count` | `waiting_count` | Summary counter |

---

## 13. Backward Compatibility Summary

### Serde (serialized data)

| Field | Serializes as | Deserializes from |
|-------|--------------|-------------------|
| `Task.after` | `"after"` | `"after"` or `"blocked_by"` (alias) |
| `Task.before` | `"before"` | `"before"` or `"blocks"` (alias) |
| `TraceFunctionTask.after` | `"after"` | `"after"` or `"blocked_by"` (alias) |
| `ExportedTask.after` | `"after"` | `"after"` or `"blocked_by"` (alias) |
| `ExportedTask.before` | `"before"` | `"before"` or `"blocks"` (alias) |

**Old `.workgraph/graph.jsonl` files** with `"blocked_by"` and `"blocks"` keys will deserialize correctly via aliases. New writes will use `"after"` and `"before"`.

### CLI

| Old flag | New flag | Backward compat |
|----------|----------|-----------------|
| `--blocked-by` | `--after` | `alias = "blocked-by"` (hidden) |
| `--add-blocked-by` | `--add-after` | `alias = "add-blocked-by"` (hidden) |
| `--remove-blocked-by` | `--remove-after` | `alias = "remove-blocked-by"` (hidden) |

### JSON output

These keys change in JSON output. External consumers may need updating:
- `wg list --json`: `"blocked_by"` → `"after"`
- `wg show --json`: `"blocked_by"` → `"after"`, `"blocks"` → `"before"`
- `wg status --json`: `"blocked"` → `"waiting"` (summary field)
- `wg why-blocked --json`: `"blocked_by"` key in tree nodes → `"after"`
- `wg ready --json`: `"blocked_by"` → `"after"`
- `wg trace export`: `"blocked_by"` → `"after"`, `"blocks"` → `"before"`
- `wg check` orphan relation strings: `"blocked_by"` → `"after"`, `"blocks"` → `"before"`

---

## 14. Parallelization Guide

This work can be split across agents as follows:

### Batch 1: Core (must go first)
1. **`src/graph.rs`** — Task struct + TaskHelper + Deserialize impl + remove_node + comments
2. **`src/query.rs`** — Function renames + field renames + ProjectSummary

### Batch 2: Commands (parallelizable after Batch 1)
Each file is independent and can be done by a separate agent:
- `src/main.rs` (CLI definitions + dispatch)
- `src/commands/add.rs`
- `src/commands/edit.rs`
- `src/commands/show.rs`
- `src/commands/done.rs`
- `src/commands/service.rs`
- `src/commands/replay.rs`
- `src/commands/trace_instantiate.rs`
- `src/commands/viz.rs`
- `src/commands/critical_path.rs`
- `src/commands/forecast.rs`
- `src/commands/analyze.rs` + `bottlenecks.rs`
- `src/commands/blocked.rs` + `why_blocked.rs`
- `src/commands/trace_export.rs` + `coordinate.rs` + `status.rs`
- `src/commands/list.rs` + `ready.rs` + `graph.rs`
- Remaining command files (loops, notify, spawn, evolve, trace, gc, etc.)

### Batch 3: Non-command source (parallelizable after Batch 1)
- `src/check.rs`
- `src/tui/mod.rs` + `app.rs` + `graph_layout.rs`
- `src/trace_function.rs`
- `src/matrix_commands.rs`
- `src/service/executor.rs`
- `src/commands/quickstart.rs` (help text strings)

### Batch 4: Tests (parallelizable after Batch 1)
Each test file can be done independently:
- All 13 `tests/integration_*.rs` files

### Batch 5: Documentation (parallelizable, independent of code)
- Priority 1 docs: README, COMMANDS, SKILL, CLAUDE.md, AGENT-GUIDE
- Priority 2 docs: Design and research docs
- Priority 3 docs: Archive docs
- Priority 4 docs: .typ manual files

### Verification
After all batches: `cargo build && cargo test` must pass.

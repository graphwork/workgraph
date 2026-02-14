# CLI Structure & Basic Commands Review

**Scope:** `src/main.rs` (2,014 lines), `src/commands/mod.rs` (79 lines), and 18 command files (~2,150 lines combined). Total ~4,243 lines.

## 1. Architecture Overview

### CLI Parser (`main.rs`)

The CLI uses `clap` with `#[derive(Parser)]` and a single flat `Commands` enum containing **67 top-level variants** (65 without feature-gated Matrix commands). This is a *very* large enum for a single file.

**Structure:**
```
Cli (global args: --dir, --json, --help, --help-all, --alphabetical)
├── Commands enum (67 variants, ~780 lines of arg definitions)
├── Subcommand enums (6 groups):
│   ├── ResourceCommands (add, list)
│   ├── ActorCommands (add, list)
│   ├── SkillCommands (list, task, find, install)
│   ├── AgencyCommands (init, stats)
│   ├── RoleCommands (add, list, show, edit, rm, lineage)
│   ├── MotivationCommands (add, list, show, edit, rm, lineage)
│   ├── AgentCommands (create, list, show, rm, lineage, performance, run)
│   ├── ServiceCommands (start, stop, status, reload, pause, resume, install, tick, daemon)
│   └── MatrixCommands (listen, send, status, login, logout) [feature-gated]
├── print_help() (130 lines - custom help with usage-based ordering)
├── command_name() (72 lines - enum-to-string mapping)
└── main() (535 lines - match dispatch)
```

### Command Routing (`main()`)

The `main()` function at line 1467 is a single giant `match` block that:
1. Destructures each `Commands` variant
2. Calls the corresponding `commands::*::run()` function
3. Passes all args individually (no intermediate structs)

The `command_name()` function at line 1392 is a parallel `match` that maps every variant to a string for usage tracking — must be updated manually whenever commands change.

### Module Registry (`commands/mod.rs`)

A simple flat list of 63 `pub mod` declarations plus two utility functions (`graph_path`, `notify_graph_changed`). No grouping or organization.

## 2. Command Pattern Analysis

### Shared Boilerplate Pattern

Nearly every command follows this exact pattern:
```rust
pub fn run(dir: &Path, ...) -> Result<()> {
    let path = graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }
    let mut graph = load_graph(&path).context("Failed to load graph")?;
    let task = graph.get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;
    // ... modify task ...
    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);
    println!("...");
    Ok(())
}
```

This pattern appears verbatim in: `done.rs`, `submit.rs`, `approve.rs`, `reject.rs`, `fail.rs`, `abandon.rs`, `retry.rs`, `claim.rs`, `reclaim.rs`, `edit.rs`, `log.rs`, `list.rs`, `ready.rs`, `blocked.rs`, `check.rs`.

**Lines duplicated across files for just the boilerplate (load-guard + load + save + notify):** ~6-8 lines per file × 15 files = ~100 lines of pure duplication.

### State Transition Commands

These commands move a task through its lifecycle:

| Command | From Status | To Status | Clears assigned? | Adds log? | Captures output? |
|---------|------------|-----------|-------------------|-----------|-----------------|
| `claim` | Open/Blocked/Failed | InProgress | No (sets it) | No | No |
| `unclaim` | Any | Open | Yes | No | No |
| `done` | Open/InProgress | Done | No | No | Yes |
| `submit` | InProgress | PendingReview | No | Yes | Yes |
| `approve` | PendingReview | Done | No | Yes | No |
| `reject` | PendingReview | Open | Yes | Yes | No |
| `fail` | Open/InProgress | Failed | No | No | Yes |
| `abandon` | Open/InProgress | Abandoned | No | No | No |
| `retry` | Failed | Open | Yes | No | No |
| `reclaim` | InProgress | InProgress | Swaps | Yes | No |

### Inconsistencies Found

1. **Blocker checks are inconsistent:**
   - `done`, `submit`, `approve` check `query::blocked_by()` before transitioning
   - `fail`, `abandon`, `claim`, `unclaim`, `reject` do **not** check blockers
   - `reject` doesn't need to (it was already PendingReview), but `claim` not checking means you can claim a task that still has open blockers

2. **Output capture inconsistency:**
   - `done` and `fail` capture output via `capture_task_output()`
   - `submit` also captures output (makes sense — it's the agent's completion signal)
   - `approve` does NOT capture output (reasonable — reviewer hasn't done work)
   - `abandon` does NOT capture output (could be useful for debugging)

3. **Log entry inconsistency:**
   - `submit`, `approve`, `reject`, `reclaim` add structured `LogEntry` items
   - `done`, `fail`, `abandon`, `retry`, `claim`, `unclaim` do **not** add log entries
   - This means the task log doesn't record when a task was completed, failed, or retried — only submit/approve/reject events

4. **Status validation strictness varies:**
   - `done` accepts any non-Done status (Open, InProgress, etc.) but checks blockers
   - `submit` requires exactly InProgress
   - `claim` allows Open, Blocked, Failed (anything except InProgress and Done)
   - `unclaim` has NO status check — can unclaim a Done task, resetting it to Open

5. **`unclaim` is too permissive:** It unconditionally sets `status = Open` and `assigned = None` regardless of current status. You can `unclaim` a Done task and it becomes Open again. This is likely a bug.

6. **`done` doesn't require InProgress:** You can `wg done` a task that was never claimed. The task goes from Open → Done without ever being InProgress, which skips the whole claim workflow.

7. **`fail` increments `retry_count`:** Semantically confusing. The "retry count" goes up on failure, not on retry. `retry.rs` doesn't increment it. So `retry_count` is actually "failure count."

8. **Parameter naming: `id` vs `task_id`:** Some commands use `id` as the parameter name (most), some use `task_id` (submit, approve, reject, reclaim). The CLI arg is always just `id`.

## 3. Argument Handling

### `add` command (15 parameters to `run()`)
This function takes 15 individual parameters. It's the most complex arg signature:
```rust
pub fn run(dir, title, id, description, blocked_by, assign, hours, cost,
           tags, skills, inputs, deliverables, max_retries, model, verify)
```

### `edit` command (11 parameters)
Similar verbosity:
```rust
pub fn run(dir, task_id, title, description, add_blocked_by, remove_blocked_by,
           add_tag, remove_tag, model, add_skill, remove_skill)
```

### `Config` command (28 fields!)
The worst offender. The `Config` variant in the `Commands` enum has **28 fields**. The dispatch logic in `main()` takes 60 lines just for Config (lines 1807-1889). This is because Config conflates show/init/update/matrix into one command with mutually exclusive flags.

### `edit` vs `add` field coverage
`edit` can modify: title, description, blocked_by, tags, model, skills
`edit` cannot modify: assign, hours, cost, inputs, deliverables, max_retries, verify, exec

This is a gap — if you need to change an estimate or verification criteria on an existing task, you can't use `wg edit`.

## 4. main.rs Size Analysis

**Breakdown by section:**
| Section | Lines | % |
|---------|-------|---|
| Imports + Cli struct | 1-37 | 2% |
| Commands enum | 39-826 | 39% |
| Subcommand enums | 828-1249 | 21% |
| `print_help()` | 1251-1389 | 7% |
| `command_name()` | 1392-1465 | 4% |
| `main()` dispatch | 1467-2014 | 27% |

Nearly 40% of the file is just argument definitions. The dispatch block and help system add another 38%.

## 5. Recommendations

### High Priority

**R1. Fix `unclaim` status validation.**
`unclaim` should reject Done/Abandoned tasks or at minimum warn. Currently it silently resets any task to Open.

**R2. Add log entries to all state transitions.**
`done`, `fail`, `abandon`, `retry`, `claim`, `unclaim` should all add log entries. The task history is incomplete without them.

**R3. Rename `retry_count` to `failure_count`.**
It's incremented on failure, not on retry. The current name is misleading.

### Medium Priority

**R4. Extract boilerplate into a helper.**
A `with_task_mut(dir, id, |task| { ... })` helper would eliminate ~100 lines of duplicated load/save/notify boilerplate across 15+ command files.

```rust
// Potential helper in commands/mod.rs
pub fn with_task_mut<F>(dir: &Path, id: &str, f: F) -> Result<()>
where F: FnOnce(&mut Task, &WorkGraph) -> Result<()>
```

**R5. Split `Config` into subcommands.**
Instead of 28 mutually exclusive flags, use `wg config show`, `wg config set --key value`, `wg config matrix --homeserver ...`. This matches the pattern already used by `Resource`, `Actor`, `Service`, etc.

**R6. Split `main.rs`.**
The file could be split by moving:
- All subcommand enums into their respective modules (e.g., `ServiceCommands` into `commands/service.rs`)
- `print_help()` into `src/help.rs`
- `command_name()` could be derived via a proc macro or `strum` crate
- The `Commands` enum itself stays in `main.rs` but becomes smaller if subcommands are promoted

**R7. Expand `edit` to cover all task fields.**
Missing: assign, hours, cost, inputs, deliverables, max_retries, verify, exec. Users must currently edit graph.jsonl manually to change these.

### Low Priority

**R8. Consider merging overlapping commands.**
Some commands could be consolidated:
- `blocked` + `why-blocked` → `wg blocked <id> [--why]`
- `agents` + `dead-agents` → `wg agents [--dead] [--cleanup]`
- `viz` + `dag` + `graph` → `wg viz [--format dot|ascii|mermaid]` (partially done already since `dag` delegates to `viz`)

**R9. Standardize parameter naming.**
Pick either `id` or `task_id` everywhere in command implementations. Currently mixed.

**R10. Use arg structs instead of positional parameters.**
For `add` (15 params) and `edit` (11 params), pass a struct instead of individual values:
```rust
pub struct AddArgs { title: String, id: Option<String>, ... }
pub fn run(dir: &Path, args: AddArgs) -> Result<()>
```

**R11. Eliminate `command_name()` manual mapping.**
Use `clap`'s built-in `Command::get_name()` at dispatch time, or derive a `Display`/`AsRef<str>` impl on `Commands` via `strum_macros::EnumString`. The current manual mapping is fragile — adding a command requires updating it separately.

## 6. What's Working Well

- **Consistent use of `anyhow`** for error handling throughout
- **`--json` flag** propagated through most read commands for machine consumption
- **Custom help system** with usage-based ordering is a nice UX touch
- **`notify_graph_changed()`** consistently called after mutations (good daemon integration)
- **Test coverage** in `claim.rs`, `reclaim.rs`, and `edit.rs` is thorough
- **ID auto-generation** in `add.rs` is practical and handles collisions
- **Verification workflow** (submit → approve/reject) is well-designed
- **Output capture** on done/fail/submit feeds the evaluation system cleanly

## 7. Summary

The CLI works well functionally but has grown organically to 67+ commands, creating maintenance burden. The main pain points are:

1. **`main.rs` is too large** (2,014 lines) — mostly arg definitions and dispatch
2. **State transition commands are inconsistent** in validation and logging
3. **`Config` command is overloaded** with 28 flags
4. **Boilerplate duplication** across ~15 command files
5. **`unclaim` has a bug** allowing Done tasks to be reset
6. **`edit` is incomplete** — can't modify half of a task's fields

The highest-impact improvement would be R4 (extract boilerplate) combined with R2 (consistent logging), as these touch the most code and directly affect correctness.

# Research: TUI evaluation-state coloring (pink vs green)

**Question (paraphrased):** Does the TUI still use light-pink to indicate "task is being evaluated", given that there's now a light-green "being evaluated" indicator somewhere?

**Short answer: BOTH still exist. They are not redundant — they color different aspects of the same lifecycle event:**

- **Pink (ANSI 256-color 219, ≈ RGB 255/175/215)** colors the **phase annotation label** (`[∴ evaluating]`) that appears next to a task whose `.evaluate-X` scaffolding task is *currently running*.
- **Chartreuse / light-green (ANSI 256-color 154, ≈ RGB 140/230/80)** colors the **task's own status text** when the task's `Status` field is `PendingEval` (the soft-done state where the agent finished and is awaiting the evaluator's verdict).

Visually, a task in the middle of being evaluated will render roughly like:

```
my-task  (pending-eval)        [∴ evaluating]
└── chartreuse green ──┘       └── pink ──┘
   (status of the task)        (active scaffolding)
```

---

## 1. Where the pink color is applied

Pink is applied to **phase annotation labels** — `[⊞ assigning]`, `[∴ evaluating]`, `[∴ validating]`, `[verifying]`, `[placing]` — when the corresponding system task (`.assign-X`, `.evaluate-X`, etc.) is active.

Single source of truth (used by both `wg viz` CLI and the TUI viz tab):

- **`src/commands/viz/ascii.rs:323-341`** — in `format_node`, when an annotation contains any of `placing`, `assigning`, `evaluating`, `validating`, `verifying`, the annotation text is wrapped in `\x1b[38;5;219m...\x1b[0m`:
  ```
  let is_agency_phase = use_color && annotations.get(id).is_some_and(|a| {
      a.text.contains("placing") || a.text.contains("assigning")
          || a.text.contains("evaluating") || a.text.contains("validating")
          || a.text.contains("verifying")
  });
  let phase_info = if is_agency_phase {
      annotations.get(id).map(|a| format!(" \x1b[38;5;219m{}\x1b[0m", a.text)).unwrap_or_default()
  } else { phase_info };
  ```
- **`src/commands/viz/mod.rs:194-203`** — `compute_phase_annotation` produces those annotation strings (e.g. `[∴ evaluating]` for any non-`.assign-` non-`.verify-` system task such as `.evaluate-X` or `.flip-X`).
- **`src/commands/viz/mod.rs:183-188`** — `is_pipeline_active`: a system task counts as "active" (and therefore an annotation is emitted) if its status is `InProgress | PendingValidation | PendingEval`.

Tests covering pink:
- **`src/tui/viz_viewer/render.rs:10778` `test_pink_agency_phase_text`** — asserts `[assigning]` and `[∴ evaluating]` annotations get `\x1b[38;5;219m` when ANSI is enabled.
- **`src/tui/viz_viewer/render.rs:10967` `test_pink_agency_phase_preserves_in_trace`** — pink survives upstream-trace edge highlighting.
- **`src/commands/viz/ascii.rs:5223`** — measures `visible_len("\x1b[38;5;219m[assigning]\x1b[0m")`.

Adjacent (same color, related semantics — Log tab "oversight" markers; same family, not directly an evaluation indicator):
- **`src/tui/viz_viewer/render.rs:13431-13468` / commit `7f6075543`** — Log-tab agent-marker `⊞` lines use the same pink (Color::Indexed(219)) "to align oversight/validation/feedback visual semantics".

The Magenta hits in `src/tui/viz_viewer/render.rs` (e.g. lines 1466, 1652, 1669, 4848, 5075, 7137, 7528, 7675, 7704, 8912) are **NOT** evaluation coloring — they color upstream-edge tracing, sent-message accent, AgentSpawned activity, etc. That's a different "magenta" channel from the agency pink (ANSI 219).

## 2. Where the chartreuse / light-green color is applied

Chartreuse colors the **task's own status text/fill** when `Status::PendingEval` (i.e. the parent task that has been reported done and is gated on the `.evaluate-X` verdict).

- **`src/commands/viz/ascii.rs:245`** — status_color for `Status::PendingEval` → `\x1b[38;5;154m` (chartreuse). Used by both `wg viz` CLI and the TUI viz tab.
- **`src/commands/viz/graph.rs:89`** — same `\x1b[38;5;154m` chartreuse for `wg viz --layout graph`.
- **`src/commands/viz/dot.rs:31`** — Graphviz output: `Status::PendingEval => "style=filled, fillcolor=chartreuse"`.
- **`src/commands/trace.rs:883, 1117`** — `wg trace`: `Status::PendingEval => "\x1b[38;5;154m"`.
- **`src/tui/viz_viewer/state.rs:280`** — TUI **flash animation** color when a task transitions into `PendingEval`: `(140, 230, 80) // chartreuse: between yellow (in-progress) and green (done)`.

The intent is documented inline at `state.rs:280` and `dot.rs:31`: chartreuse sits visually between yellow (in-progress) and green (done), encoding the "soft-done, awaiting evaluator" semantic.

## 3. Other green/red/yellow status indicators (NOT evaluation-specific, for completeness)

The TUI detail view's "Task Lifecycle" panel (`src/tui/viz_viewer/render.rs:5456-5489`) renders the assign → execute → evaluate phases with status icons keyed off `phase.status`:
- Done → green ✓
- InProgress → **yellow** ●
- Failed → red ✗
- other → dark-gray ○

So in the lifecycle panel an "evaluating" phase that is currently running shows as **yellow ●**, not pink and not chartreuse. (Pink and chartreuse only appear in the graph/viz views — not in this detail-view phase strip.)

## 4. Do pink and chartreuse-green coexist? Or has one replaced the other?

**They coexist and are complementary**, not redundant.

| color | what it colors | when it appears | code |
|-------|----------------|-----------------|------|
| **pink** ANSI 219 (≈255,175,215) | the `[∴ evaluating]` annotation label appended to the parent task's line | whenever the parent task has an active `.evaluate-X` (or `.flip-X`) scaffolding task | `src/commands/viz/ascii.rs:337` |
| **chartreuse** ANSI 154 (≈140,230,80) | the parent task's own status text/fill | whenever the parent task's `Status` is `PendingEval` | `src/commands/viz/ascii.rs:245`, `dot.rs:31`, `graph.rs:89`, `state.rs:280` |

The two can be on screen simultaneously and on the same line — chartreuse on `(pending-eval)`, pink on the trailing `[∴ evaluating]` annotation. Neither replaced the other. Pink was never the task-status color; chartreuse was never the annotation-label color.

History confirms they were added in different commits for different concerns:
- `a4f591261` / `fdc36e0a1` (`feat: PendingEval state — eval is the dependency-unblock gate`) — introduced the `PendingEval` status and its chartreuse coloring.
- `7f6075543` (`feat: change log metadata color from light blue to pink (ANSI 219)`) — extended the existing pink agency-phase color to log-tab oversight markers; no removal of pink from agency phase annotations.
- `5c98ea598` (`revert: remove TUI agency pipeline display, restore simple phase annotations`) — confirms the simple `[∴ evaluating]` annotation (still pink) is the current state, after a richer agency-pipeline view was reverted.

## 5. Recommendation (research output, no code change)

If the user finds the dual coloring confusing — i.e. wants only one signal for "this task is being evaluated" — the cleanest collapse would be either:

- (A) **Drop the pink annotation when the parent is already `PendingEval`** (since chartreuse already says it). The parent line still shows `[∴ evaluating]` text, just not in pink — pink is redundant there because chartreuse on the status word is unambiguous.
- (B) **Drop the chartreuse status color** and rely only on the pink annotation label. Loses the at-a-glance status distinction in `wg viz` / Graphviz / trace output that has nothing to do with annotations.

Recommend (A) if a change is desired. But this is research-only — no code changes were made.

## Validation checklist

- [x] Report lists every TUI/viz location where evaluation color is applied, with file:line refs.
- [x] Report names the exact colors: pink (ANSI 219, RGB ~255/175/215) and chartreuse (ANSI 154, RGB ~140/230/80).
- [x] Report states clearly: BOTH coexist; neither replaced the other.
- [x] Report distinguishes the trigger conditions: pink for the active-scaffolding annotation label, chartreuse for the parent task's `Status::PendingEval`.
- [x] No code changes — research only.

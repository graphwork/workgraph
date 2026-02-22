# Fix: Remove incorrect DAG terminology from codebase

## Problem

Workgraph is **not** a DAG (directed acyclic graph). It explicitly supports cycles via loop edges (`loops_to`, back-edges, iteration guards). Despite this, the codebase and documentation refer to the graph as a "DAG" in numerous places — including the core struct's own doc comment. This causes AI agents and contributors to incorrectly describe workgraph as a DAG, which misrepresents a fundamental design property.

The correct term is **directed graph** or **task graph**. The `after` edges form the forward dependency structure, and `loops_to` edges create deliberate cycles for iterative workflows (write-review-revise, polling, retries, sprint cycles).

## Scope

Every instance of "DAG," "directed acyclic graph," or "acyclic" used to describe the workgraph's data model needs to be corrected. References to acyclicity in the context of external library constraints (e.g., the `ascii-dag` crate requiring acyclic input) or algorithmic notes (e.g., "critical path only works on the acyclic portion") are **fine** — those describe implementation constraints, not the data model itself.

## Locations to fix

### Critical — these define the mental model

1. **`src/graph.rs:411`** — Doc comment on the `WorkGraph` struct:
   ```rust
   /// The work graph: a DAG of tasks and resources with embedded dependency edges.
   ```
   Fix: Remove "DAG" — call it a "directed task graph" or just "work graph."

2. **`docs/README.md:81`** — Main documentation hedges but still leads with DAG:
   > "Tasks form a directed graph through `after` relationships. While typically a DAG (directed acyclic graph), cycles are permitted for iterative/recurring work patterns."

   Fix: Don't frame it as "typically a DAG with exceptions." State plainly that it's a directed graph that supports cycles via loop edges. The `after` edges are always forward (acyclic), but `loops_to` edges create intentional cycles. This is a feature, not an exception.

3. **`docs/research/cyclic-processes.md:11`** — Research document states:
   > "Workgraph currently assumes a DAG (Directed Acyclic Graph) for task dependencies."

   Fix: This was written before loop edges were implemented. Update the opening to reflect that cycles are now supported, or add a note at the top that this is a historical research document and cycles have since been implemented.

### Important — naming and surveys

4. **`src/tui/dag_layout.rs`** — The entire file is named `dag_layout.rs`.

   Consider renaming to `graph_layout.rs` or `task_graph_layout.rs`. The file already handles cycles (it detects back-edges, strips them for the `ascii-dag` crate, and renders them separately). The name implies it only handles DAGs.

5. **`docs/dag-assumptions-survey.md`** — Survey of DAG assumptions in the codebase.

   This document is useful as a reference but its framing assumes the graph *should* be a DAG. Add a header note clarifying that cycles are a supported feature, and this document catalogs places where code was originally written with DAG assumptions that needed updating. Consider renaming to something like `cycle-support-audit.md`.

### Contextual — these are fine or need minor tweaks

6. **`src/tui/dag_layout.rs:276`** — Comment: `// Build the DAG and compute layout (now guaranteed acyclic)`. This is **correct in context** — at that point in the code, back-edges have been stripped and the input to the `ascii-dag` crate is indeed acyclic. But the comment could be clearer: "Build the layout (back-edges stripped, input is now acyclic for ascii-dag)."

7. **`src/tui/dag_layout.rs:1753, 1771, 1800, 1806`** — Test comments referencing acyclic graphs. These are testing the acyclic-input code path specifically, so the language is accurate. No change needed.

8. **`docs/design-cyclic-workgraph.md:113`** — Explains that `after` edges are acyclic while `loops_to` edges are separate. This is **correct and well-written**. No change needed.

9. **`docs/archive/reviews/review-core-graph.md:240`** — Archive review noting cycle handling. Historical document, low priority.

10. **`docs/archive/research/rust-ecosystem-research.md:135, 204, 234`** — Research comparing petgraph vs daggy. Historical document discussing library choices. Fine as-is since it's describing the *libraries*, not workgraph's model.

11. **`docs/archive/reviews/review-tui.md:37, 76`** — Reviews noting cycle detection in the TUI. Accurate description of implementation. Fine as-is.

12. **`docs/archive/research/task-format-research.md:97, 155`** — Describes Taskwarrior as a DAG. Accurate for Taskwarrior, not about workgraph.

13. **`docs/archive/research/beads-gastown-research.md:97`** — Describes another system's acyclic constraint. Not about workgraph.

14. **`Cargo.toml:36`** — `ascii-dag = "0.8"` dependency. This is the crate name, not a claim about workgraph. No change.

15. **`src/commands/viz.rs:24`** — `"dag"` as a CLI alias for ASCII output format. This is a user-facing command alias (`wg viz --format dag`). Consider whether this name is confusing, but it's low priority.

## Suggested replacement language

Instead of:
> "a DAG of tasks"

Use:
> "a directed task graph with dependency edges and optional loop edges for cyclic workflows"

Instead of:
> "typically a DAG, but cycles are permitted"

Use:
> "a directed graph where `after` edges express forward dependencies and `loops_to` edges enable iterative cycles (review loops, retries, recurring work)"

The key framing shift: cycles are a **first-class feature**, not an exception to a DAG model.

## Verification

After fixing, grep for `(?i)\bDAG\b|directed acyclic|acyclic` and confirm every remaining instance either:
- Refers to an external library constraint (ascii-dag crate)
- Describes a specific algorithmic property (critical path on acyclic subgraph)
- Appears in archive/research docs about other systems

No remaining instance should describe workgraph's own data model as a DAG or acyclic.

# Workgraph Manual

A conceptual manual for humans who want to understand and use workgraph and its agency system. This is not an API reference or CLI cheat-sheet — it is a book of tight, precise prose that builds understanding from first principles.

## Contents

1. **Glossary** — precise definitions for every term used in the manual
2. **System Overview** — what workgraph is, what the agency adds, and how they relate
3. **The Task Graph** — tasks, statuses, dependencies, structural cycles, readiness, and emergent patterns
4. **The Agency Model** — roles, motivations, agents, content-hash IDs, skills, and trust
5. **Coordination & Execution** — the service daemon, coordinator tick, dispatch cycle, and parallelism
6. **Evolution & Improvement** — evaluation, evolution strategies, lineage, and the autopoietic loop

## Compiling

The manual is written in [Typst](https://typst.app/). To compile:

```bash
typst compile docs/manual/workgraph-manual.typ
```

This produces `workgraph-manual.pdf` in the same directory.

To watch for changes and recompile automatically:

```bash
typst watch docs/manual/workgraph-manual.typ
```

## Source files

The unified manual (`workgraph-manual.typ`) is composed from five section files written independently and then harmonized:

| File | Section |
|------|---------|
| `01-overview.typ` | System Overview |
| `02-task-graph.typ` | The Task Graph |
| `03-agency.typ` | The Agency Model |
| `04-coordination.typ` | Coordination & Execution |
| `05-evolution.typ` | Evolution & Improvement |

The section files are retained as the working originals. The unified manual is the authoritative version with harmonized cross-references, consistent terminology, and a glossary.

# Development Notes

Operational notes, recurring patterns, and things to remember.

## Reusable Functions

Functions extracted from completed task graphs. Run `wg func list` for the full catalog.

| Function | Purpose | Usage |
|----------|---------|-------|
| `doc-sync` | Sync all key docs to current code state | `wg func apply doc-sync` |
| `tfp-pattern` | Trace-function protocol implementation pattern | `wg func apply tfp-pattern` |

The `doc-sync` function fans out: spec → 7 parallel doc updates (README, SKILL, quickstart, COMMANDS, AGENCY, AGENT-GUIDE/SERVICE, manual) → integrate → extract. Run it whenever features land and docs drift.

See `docs/KEY_DOCS.md` for the canonical list of documentation files to keep in sync.

## Build & Test

```
cargo install --path .          # rebuild global wg binary
wg service stop                 # stop before rebuilding
cargo test                      # run tests
typst compile docs/manual/workgraph-manual.typ   # rebuild manual PDF
typst compile docs/research/organizational-patterns.typ  # rebuild org patterns PDF
```

## Documentation: Typst → Markdown

**Typst (.typ) files are the ground truth.** Markdown versions exist for developers who prefer .md and for the website. Keep them in sync.

Markdown locations:
- `docs/manual/workgraph-manual.md` — full manual (glossary + chapters 01-05)
- `docs/research/organizational-patterns.md` — theory document
- `graphwork.github.io/` — website copies (same files)

To regenerate markdown after editing typst:

```bash
# pandoc can't handle all typst constructs (table.header, #quote, raw blocks,
# #text wrappers, #align). A preprocessing step extracts these into markdown
# tables/quotes/code blocks, then pandoc converts the remaining text.
#
# Simple chapters that pandoc handles directly:
pandoc -f typst -t gfm --wrap=none docs/manual/01-overview.typ -o out.md

# For files with tables/figures (chapters 02, 04, org patterns), or for the
# full manual (pandoc can't follow #include): use the converter script at
# scripts/typst-to-md.py (if available) or see the convert-typst-docs task
# logs for the preprocessing approach.
#
# The full manual is assembled from: glossary (from workgraph-manual.typ)
# + chapters 01-05, each converted separately, then concatenated.
```

After regenerating, copy the markdown to `graphwork.github.io/` as well.

## Service Operations

```
wg service start --max-agents 5   # start coordinator
wg service status                 # check health
wg agents                         # who's working
wg list --status open             # what's pending
wg unclaim <task>                 # clear stale assignment
```

## Model Defaults

- **Agents**: configurable via `wg config` or per-task `--model`
- **Evaluator**: haiku (lightweight, cheap — set by `wg agency init`)
- **Assigner**: haiku (same rationale)
- Hierarchy: task `--model` > executor model > coordinator model > `"default"`

## Common Pitfalls

- Forgot `cargo install --path .` after code changes — old binary runs
- `wg evaluate` requires `run` subcommand: `wg evaluate run <task-id>`
- `wg retry` must clear `assigned` field or coordinator skips the task
- `--output-format stream-json` requires `--verbose` with `--print` in Claude CLI

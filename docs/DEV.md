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

## Worktree Isolation

When multiple agents run concurrently, each gets its own git worktree to prevent build interference and file conflicts.

### How it works

Enable in config:
```toml
[coordinator]
worktree_isolation = true
merge_strategy = "squash"   # "merge", "squash", or "rebase"
```

The coordinator creates a worktree per agent at spawn time:
1. `git worktree add .wg-worktrees/<agent-id> -b wg/<agent-id>/<task-id> HEAD`
2. Symlinks `.workgraph` into the worktree (shared task state)
3. Agent runs inside `.wg-worktrees/<agent-id>/`
4. On completion: squash-merges branch back to main, removes worktree

### Directory structure

```
.wg-worktrees/           # in .gitignore
├── agent-42/             # full working tree for agent-42
│   ├── .git              # file pointing to .git/worktrees/agent-42
│   ├── .workgraph -> /abs/path/.workgraph   # symlink (shared state)
│   └── src/              # independent file copy
└── agent-43/             # another agent's worktree
```

Branch naming: `wg/<agent-id>/<task-id>` (e.g. `wg/agent-42/implement-auth`).

### Cleanup

```bash
git worktree list              # see all worktrees
git worktree prune             # remove stale admin entries
git worktree remove .wg-worktrees/<agent-id>   # remove specific worktree
```

On service startup, `cleanup_orphaned_worktrees()` in `src/commands/service/worktree.rs` scans `.wg-worktrees/` and removes worktrees whose agents are no longer alive. Stale worktrees older than a threshold can be pruned via `prune_stale_worktrees()`.

See `docs/WORKTREE-ISOLATION.md` for the full research report and design rationale.

## Agency Pipeline Setup

The agency system automates agent assignment, evaluation, and evolution. Enable the pipeline:

```bash
# Enable auto-assign and auto-evaluate
wg config --auto-assign true --auto-evaluate true

# Start the service — the coordinator creates assign-{task} and evaluate-{task}
# meta-tasks automatically
wg service start
wg add "Implement feature X" --skill rust
```

### FLIP pipeline

FLIP (Fidelity via Latent Intent Probing) validates task output by reconstructing the prompt from the output and comparing it to the original. It runs as part of the evaluation pipeline.

FLIP uses two model roles:
- **FlipInference** (standard tier, default: sonnet) — reconstructs the prompt
- **FlipComparison** (fast tier, default: haiku) — scores similarity

Low FLIP scores can trigger **Verification** tasks (premium tier, default: opus) for deeper review.

### Evolution

After accumulating evaluations, evolve the agency:

```bash
wg evolve run                                  # full cycle, all strategies
wg evolve run --strategy mutation --budget 3   # targeted changes
wg evolve run --dry-run                        # preview without applying
```

Strategies: `mutation`, `crossover`, `gap-analysis`, `retirement`, `tradeoff-tuning`, `all` (default).

See `docs/AGENCY.md` for the full agency system reference.

## Model Defaults

Models are routed per-role via the `DispatchRole` enum (`src/config.rs`). Each role maps to a quality tier:

| Tier | Default Model | Roles |
|------|---------------|-------|
| **fast** | haiku | Triage, FlipComparison, Assigner |
| **standard** | sonnet | TaskAgent, Evaluator, FlipInference, Evolver, Default |
| **premium** | opus | Creator, Verification |

Resolution hierarchy (highest priority first):
1. `task.model` / `task.provider` — per-task override
2. `[models.<role>].model` — role-specific config
3. `[models.<role>].tier` — role-to-tier override
4. `DispatchRole::default_tier()` — built-in tier mapping (table above)
5. `[models.default]` — project-wide default
6. `agent.model` — global fallback

```bash
# Set tier defaults
wg config --tier fast=haiku --tier standard=sonnet --tier premium=opus

# Override a role's tier
wg config --model-role task_agent --tier premium
```

See `docs/MODEL_REGISTRY.md` for the full registry design (multi-provider support, cost tracking, budget hooks).

## Common Pitfalls

- Forgot `cargo install --path .` after code changes — old binary runs
- `wg evaluate` requires `run` subcommand: `wg evaluate run <task-id>`
- `wg retry` must clear `assigned` field or coordinator skips the task
- `--output-format stream-json` requires `--verbose` with `--print` in Claude CLI
- **Worktree stale state**: concurrent agents without worktree isolation caused 38 stashes and cross-agent contamination in SPARK v3. Never run `git stash` in a multi-agent setup — commit or discard instead. Enable `worktree_isolation = true` when running 2+ agents.
- **Branch management with concurrent agents**: each worktree uses `wg/<agent-id>/<task-id>` branches. On agent death, orphaned worktrees and branches accumulate. Service startup auto-cleans orphans; run `git worktree prune` and `git branch -d wg/...` manually if needed.
- **FLIP false negatives on human-integration tasks**: FLIP scored notification/webhook tasks 0.29-0.46 despite correct implementations, because these tasks produce side effects (sending messages) rather than code artifacts FLIP can compare. Consider manual evaluation or outcome-based scoring for side-effect-heavy tasks.

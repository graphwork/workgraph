<!-- workgraph-managed -->
# Workgraph

Use workgraph for task management.

**At the start of each session, run `wg quickstart` in your terminal to orient yourself.**
Use `wg service start` to dispatch work — do not manually claim tasks.

## Development

The global `wg` command is installed via `cargo install`. After making changes to the code, run:

```
cargo install --path .
```

to update the global binary. Forgetting this step is a common source of "why isn't this working" issues when testing changes.

## Service Configuration

Configure the dispatcher's executor and model with `wg config --dispatcher-executor <type>` and `wg config --model <model>`. (Legacy `--coordinator-executor` is still accepted with a deprecation warning.) Supported executors: `claude` (default), `amplifier` (provides bundles and multi-agent delegation). Spawned agents receive `WG_EXECUTOR_TYPE` and `WG_MODEL` env vars indicating their runtime context.

## For All Agents (Including the Chat Agent)

CRITICAL: Do NOT use built-in TaskCreate/TaskUpdate/TaskList/TaskGet tools.
These are a separate system that does NOT interact with workgraph.
Always use `wg` CLI commands for all task management.

CRITICAL: Do NOT use the built-in **Task tool** (subagents). NEVER spawn Explore, Plan,
general-purpose, or any other subagent type. The Task tool creates processes outside
workgraph, which defeats the entire system. If you need research, exploration, or planning
done — create a `wg add` task and let the dispatcher pick it up.

ALL tasks — including research, exploration, and planning — should be workgraph tasks.

### Cycles

Workgraph is a directed graph, NOT a DAG — it supports cycles. For repeating workflows, create ONE cycle with `--max-iterations` instead of duplicating tasks for each pass. Use `wg done --converged` to stop the cycle when the work has converged. See `wg quickstart` for examples.

### Chat agent role

A **chat agent** is the persistent LLM session the user talks to — whether it's running inside the wg TUI or in the developer's terminal Claude Code session. The contract is the same in both places: the chat agent is a **thin task-creator**, not an implementer. It does ONLY:
- **Conversation** with the user
- **Inspection** via `wg show`, `wg viz`, `wg list`, `wg status` (graph state only — NOT source files)
- **Task creation** via `wg add` with descriptions, dependencies, and context
- **Monitoring** via `wg agents`, `wg service status`, `wg watch`

A chat agent NEVER writes code, implements features, or does research itself.
It NEVER reads source files, searches code, explores the codebase, or investigates implementations.
Everything gets dispatched through `wg add` and the dispatcher (`wg service start`) hands the task to a worker agent.

**Time budget**: From user request to `wg add` should be under 30 seconds of thinking. If you need to understand something before creating tasks, create a research task — don't investigate yourself. Uncertainty is a signal to delegate, not to explore.

### Task description requirements

Every **code task** description MUST include a `## Validation` section with concrete test criteria. Use `--verify` to attach machine-checkable criteria that agents see as a hard gate.

Template:
```
wg add "Implement feature X" --after <dep> \
  --verify "cargo test test_feature_x passes" \
  -d "## Description
<what to implement>

## Validation
- [ ] Failing test written first (TDD): test_feature_x_<scenario>
- [ ] Implementation makes the test pass
- [ ] cargo build + cargo test pass with no regressions
- [ ] <any additional acceptance criteria>"
```

Research/design tasks should specify what artifacts to produce and how to verify completeness instead of test criteria.

### Smoke gate (HARD GATE on `wg done`)

`wg done` runs every scenario in `tests/smoke/manifest.toml` whose `owners = [...]`
list contains the task id. Any FAIL blocks `wg done` with the broken scenario name.
Exit 77 from a scenario script = loud SKIP (e.g. endpoint unreachable) and does not block.

- Agents CANNOT bypass the gate. `--skip-smoke` is refused when `WG_AGENT_ID` is set
  unless a human exports `WG_SMOKE_AGENT_OVERRIDE=1`.
- Use `wg done <id> --full-smoke` locally to run every scenario, not just owned.
- The manifest is **grow-only**: when you fix a regression that smoke should have caught,
  add a permanent scenario under `tests/smoke/scenarios/` and list your task id in `owners`.
- Scenarios MUST run live against real binaries / endpoints. Do not stub. The original
  wave-1 smoke silently passed against a fake LLM and that is exactly how the wg-nex 404
  shipped to users.

## Glossary

- **dispatcher** — the daemon launched by `wg service start`; polls the graph and spawns worker agents on ready tasks. Replaces the old "coordinator" terminology for the daemon.
- **chat agent** — the persistent LLM session the user talks to (in the wg TUI or in a terminal Claude Code session). Same role contract in both places. Replaces the old "coordinator" / "orchestrator" terminology for the UI agent.
- **worker agent** — an LLM process spawned by the dispatcher to do a single workgraph task. Lives only as long as that task is in-progress.

The words "coordinator" and "orchestrator" are deprecated as role-nouns in this codebase. They may still appear in legacy graph data (e.g., `.coordinator-N` task ids — run `wg migrate chat-rename` to rewrite) and in deprecation aliases on CLI flags / config keys / IPC commands. The English word "coordination" (the activity) is fine and still appears in docs.

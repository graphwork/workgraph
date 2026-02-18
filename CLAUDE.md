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

Configure the coordinator's executor and model with `wg config coordinator.executor <type>` and `wg config coordinator.model <model>`. Supported executors: `claude` (default), `amplifier` (provides bundles and multi-agent delegation). Spawned agents receive `WG_EXECUTOR_TYPE` and `WG_MODEL` env vars indicating their runtime context.

## For Spawned Agents

CRITICAL: Do NOT use built-in TaskCreate/TaskUpdate/TaskList/TaskGet tools.
These are a separate system that does NOT interact with workgraph.
Always use `wg` CLI commands for all task management.
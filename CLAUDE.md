<!-- workgraph-managed -->
# Workgraph (project-specific guide)

This file is the **layer-2** project guide for agents working *on the
workgraph codebase itself*. It is NOT the universal chat-agent / worker-agent
contract — that is bundled inside the `wg` binary and emitted by:

```
wg agent-guide
```

Run `wg agent-guide` at session start (or read its output from a previous
session) to get the universal role contract: chat-vs-dispatcher-vs-worker
distinction, `## Validation` requirement, smoke-gate, cycle handling, git
hygiene, worktree isolation, "no built-in Task tool" rules, etc.

This file only covers things specific to the workgraph repo:

- How to use `wg` itself in this session
- How to develop and rebuild the `wg` binary
- Service configuration recipes (model / endpoint pairs)
- Agency-task model pinning (a workgraph-only quirk)

For project orientation, run `wg quickstart`.

---

## Use workgraph for task management

**At the start of each session, run `wg quickstart` in your terminal to orient yourself.**
Use `wg service start` to dispatch work — do not manually claim tasks.

## Development

The global `wg` command is installed via `cargo install`. After making changes to the code, run:

```
cargo install --path .
```

to update the global binary. Forgetting this step is a common source of "why isn't this working" issues when testing changes.

## Service Configuration

Pick a **(model, endpoint)** pair — wg derives the handler from the model spec's provider prefix:

- `wg config -m claude:opus` → claude CLI handler (no endpoint needed; CLI auths itself)
- `wg config -m codex:gpt-5.5` → codex CLI handler (no endpoint needed)
- `wg config -m nex:qwen3-coder -e http://127.0.0.1:8088` → in-process nex handler
- `wg config -m openrouter:anthropic/claude-opus-4-7` → in-process nex handler

The model prefix matches the handler / subcommand name (`claude` / `codex` / `nex`). The previous `local:` and `oai-compat:` prefixes for the in-process nex handler are deprecated aliases for `nex:`; they keep working for one release with a stderr warning, and `wg migrate config` rewrites them in existing config files.

The legacy `--executor` / `-x` flag and `[agent].executor` / `[dispatcher].executor` config keys are deprecated; they still work for one release with a deprecation warning, but the model spec is the single source of truth for which handler runs. Spawned agents continue to receive `WG_EXECUTOR_TYPE` and `WG_MODEL` env vars (handler kind + resolved model). See `src/dispatch/handler_for_model.rs` for the full mapping.

A fresh install with no `~/.wg/config.toml` already runs `claude:opus` via the
claude CLI handler — built-in defaults cover the common case. To commit choices
to disk run `wg config init --global` (minimal canonical claude-cli config) or
`wg setup` (interactive wizard). To clean up an old config with deprecated
keys or stale model strings, run `wg migrate config --dry-run` then
`wg migrate config --all`. See `docs/config-ux-design.md` for full details.

### Agency tasks run on claude CLI

`.evaluate-*`, `.flip-*`, and `.assign-*` tasks are short one-shot LLM
calls (scoring + assignment verdicts), not full worker agents. They are
pinned to `claude:haiku` running on the claude CLI — the same handler
worker agents use — and ignore project-level provider cascade from
`coordinator.model`. This keeps agency cheap and immune to "openrouter
configured but no key" silent failures. Power users who *want* a
non-Anthropic provider for these roles can override per-role via
`[models.evaluator]` / `[models.assigner]` etc. in config; explicit
overrides win, cascade does not. The agent registry records these as
`executor=claude` (the legacy `eval` / `assign` labels are gone — they
were always cosmetic).

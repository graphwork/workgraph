# External executor class: claude, codex, amplifier as a uniform family

Status: **proposed 2026-04-18**. Step 4 of
`nex-executor-improvements.md`. Documents the existing external-
executor infrastructure, the remaining hardcoded gap in the
coordinator path, and the plan to close it.

## What's already here

Task-agent spawns go through `ExecutorRegistry::load_config(name)`
(see `src/service/executor.rs:1436+`). Built-in defaults exist for
`claude`, `codex`, `shell`, and custom override TOML files can
live at `.workgraph/executors/<name>.toml`. One is already
checked in: `.workgraph/executors/amplifier.toml`. The schema:

```toml
[executor]
type = "<type>"
command = "<bin>"
args = [ "...", "{{template_var}}", "..." ]
working_dir = "{{working_dir}}"
timeout = 600

[executor.env]
KEY = "value"
```

A spawn consumes the config, applies the template variables
(`{{task_id}}`, `{{task_title}}`, `{{task_description}}`,
`{{task_context}}`, `{{working_dir}}`, `{{prompt}}`), and launches
the subprocess. From the daemon's perspective, claude, codex,
amplifier, and a shell command all look the same — the only
differences are the args template and env vars.

**This is the external-executor class.** It's in place. Task agents
already benefit. Users can add `crux.toml`, `forgecode.toml`, etc.
without touching Rust — just drop a TOML file in
`.workgraph/executors/`.

## What's not generalized yet

The **coordinator** path is where the abstraction leaks. Look at
`src/commands/service/coordinator_agent.rs::spawn_claude_process`:

```rust
let mut cmd = Command::new("claude");
cmd.args([
    "--print",
    "--input-format", "stream-json",
    "--output-format", "stream-json",
    "--verbose",
    "--dangerously-skip-permissions",
    "--system-prompt", &system_prompt,
]);
```

That hardcode is the reason `executor=claude` can only launch the
Claude CLI. If you want the coordinator to run via `codex exec`
instead, or via a future peer, you currently have to edit Rust
and add a new `spawn_codex_process` sibling.

The reason this one is hardcoded while task-agent isn't: the
coordinator needs **stream-json duplex IPC** (stdin for injection,
stdout for response parsing across turns). Task-agents are
one-shot — fire prompt → read output → exit — which fits
`ExecutorRegistry` cleanly. The coordinator's long-lived session
shape is a different protocol.

**So the real generalization is two-sided:**

1. A **spawn-by-config** surface for coordinators: read the
   executor config, launch the subprocess, but leave the IPC
   protocol as a trait that different executor types implement
   (stream-json for claude, `exec` for codex, file-based for the
   `wg nex --chat` coordinator we already have).
2. A **config field for each executor** describing which protocol
   it speaks, so the daemon picks the right supervisor / reader.

This is **not** a small change. It touches the
`native_coordinator_loop` path, the Claude-CLI branch, the
`wg nex` subprocess spawn — everywhere coordinator IPC happens.

## Proposed implementation, phased

### Phase 4a — bundle standard executor configs

Low-risk, high-visibility win: check in example
`.workgraph/executors/claude.toml` and `.workgraph/executors/codex.toml`
alongside the existing `amplifier.toml`, so users have concrete
templates to copy. Current `ExecutorRegistry::default_config`
already covers these, so the bundled TOMLs are documentation as
much as behavior — they exist to be read, diffed, and customized.

Scope: 2 TOML files, updated `wg init` to optionally lay them
down. Maybe 100 LOC in `commands/init.rs`.

### Phase 4b — coordinator protocol trait

Formalize the IPC-protocol dimension:

```rust
trait CoordinatorBackend {
    async fn spawn(&self, config: &ExecutorConfig, ...) -> Result<Handle>;
    async fn send(&self, handle: &mut Handle, msg: &str) -> Result<()>;
    async fn recv(&self, handle: &mut Handle) -> Result<String>;
    async fn shutdown(&self, handle: Handle) -> Result<()>;
}
```

Implementations:
- `ClaudeCliBackend` — stream-json over stdin/stdout (current
  hardcoded path, lifted into a struct).
- `NexSubprocessBackend` — spawns `wg nex --chat <ref>`,
  communicates via inbox/outbox files (current
  `nex_subprocess_coordinator_loop`, lifted).
- `CodexBackend` — TBD; depends on what stream protocol codex
  exposes for long-running sessions. If codex is one-shot-only,
  then coordinator-via-codex means restarting the subprocess on
  every turn (expensive but functional), or keep coordinator
  stuck on claude/nex and only use codex for task agents.
- `AmplifierBackend` — similar investigation needed.

Scope: ~500-800 LOC. Extracts existing code rather than adding
it. Net LOC probably neutral or negative.

### Phase 4c — dispatcher driven by config

Replace the current `if executor == "claude" { ... } else {
nex_subprocess_coordinator_loop(...) }` with a dispatch based on
the executor config's protocol field:

```toml
[executor]
type = "claude"
protocol = "stream-json"   # new
command = "claude"
args = [...]
```

Supervisor picks the backend matching `protocol`. Adding codex
coordinator support becomes "add `protocol = "codex-exec"` +
register a new backend impl," not "edit the agent_thread_main
branch tree."

Scope: ~200 LOC (config field + dispatcher rewrite).

## Priority and ordering

Phase 4a is worth doing immediately. It's pure documentation value
with trivial implementation.

Phases 4b + 4c are a medium project. Worth it when:
- We actually want to add codex or another backend as a
  coordinator (not just task agent).
- The `nex_subprocess_coordinator_loop` and Claude CLI path
  accumulate enough parallel maintenance cost to justify the
  extraction.

Until either of those triggers, the coordinator keeps two branches
and that's OK — the task-agent class is already generalized, which
is where most of the value of "pluggable executors" lives.

## Relation to self-bootstrap goal

`nex-executor-improvements.md` frames the overall work around:
make `wg nex` strong enough for workgraph to self-dispatch. Phase
4a directly helps (users can see claude/codex/amplifier configs
side-by-side, pick the right one for their billing/capability
situation). Phases 4b-4c are architectural consolidation — they
don't change what the executor can DO, just who can add new ones
without touching Rust. Lower priority than steps 1-3, which
materially widened what the executor can do (MCP tools, real
compaction, accurate pressure).

## What we are explicitly NOT doing

- Rewriting `nex_subprocess_coordinator_loop` to use the
  protocol-trait dispatcher before 4b is greenlit. Keep the
  existing path as-is; the extraction happens all-at-once, not
  piecemeal.
- Supporting every executor's native session-resume protocol
  (claude --resume, codex sessions). That's a layer beyond what
  Phase 4 aims at — it's about spawn/IPC, not state
  serialization.

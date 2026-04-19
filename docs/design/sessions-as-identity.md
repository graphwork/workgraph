# Sessions as identity — the unified handler/session model

**Status:** design. See `sessions-as-identity-rollout.md` for staged work plan.

## The principle

> The coordinator isn't a process. The coordinator is a *session*.
> The process is an ephemeral *handler* that embodies the session
> while it's running.

Every LLM-backed task in workgraph — coordinator, agent, evaluator,
interactive nex — is (a) a persistent *session* on disk, plus (b) a
currently-active *handler* process. The session is the identity; the
handler is the body. Bodies can die and be reborn; the session persists.

This is already true for our native `wg nex` runtime — `--resume`
restores conversation from the journal, sessions live at
`.workgraph/chat/<uuid>/`, aliases map human-readable names to UUIDs.
What's missing is the *enforcement*: a single-writer lock to guarantee
at-most-one handler per session, and the lifecycle conventions that
follow from it.

This document specifies the full model. The sibling
`sessions-as-identity-rollout.md` specifies how we get there.

## What collapses

Today workgraph has three parallel mental models for "thing an LLM is
doing":

1. **Coordinator.** Spawned by the daemon, identified by numeric id
   (`coordinator-0`), uses a hand-rolled loop in
   `commands/service/coordinator_agent.rs`, talks to the TUI via chat
   files.

2. **Task agent.** Spawned by the daemon or `wg claim`, identified by
   task id, uses `AgentLoop::run` (soon: `run_interactive` with
   `--autonomous`), streams to `.workgraph/chat/task-<id>/streaming`.

3. **Interactive nex.** Spawned by the user at a terminal, identified
   by a random UUID alias, uses `AgentLoop::run_interactive` with
   either `TerminalSurface` or `ChatSurfaceState`.

These are the same thing in different clothes. Unified model:

- Every LLM activity is a **task** in the graph
- Every task that has a handler has a **session** at `.workgraph/chat/<uuid>/`
- The task id IS the session alias (coordinator-0 → the alias for
  some UUID, soon replaced by direct UUID-referencing task ids)
- The **handler** is whichever process currently owns the session
  (enforced by a lock file)

## Data model

### Session

Lives at `.workgraph/chat/<uuid>/`.

```
<uuid>/
  conversation.jsonl     # full journal (replayable)
  session-summary.md     # compaction artefact for --resume
  inbox.jsonl            # user → handler queue
  outbox.jsonl           # handler → user finalized turns
  .streaming             # in-flight assistant text (dotfile, ephemeral)
  stream.jsonl           # structured per-turn telemetry (tool_start/end, etc.)
  trace.ndjson           # low-level event log
  .handler.pid           # NEW: currently-active handler, see §Lock below
```

The session registry at `.workgraph/chat/sessions.json` maps UUID →
`SessionMeta { aliases, kind, created, forked_from }`. Aliases like
`coordinator-0` or `task-foo` are symlinks in the chat dir pointing at
the canonical UUID dir.

### Task

A graph task with `executor_type` set to something that runs an LLM
(native, claude, codex, gemini, amplifier). The task's `chat_ref` (new
field, or derivable from task_id) resolves to a session UUID. When the
task transitions to `in-progress`, it means a handler has claimed the
session.

### Handler

The process currently inhabiting a session. Could be:
- `wg nex --chat <uuid> --resume` (native handler)
- `claude --resume <their-session>` (claude handler, via adapter)
- `codex --session <id>` (codex handler, via adapter)
- Any other CLI the executor map knows how to launch

The handler's PID is written to `<uuid>/.handler.pid` on startup and
removed on clean exit (§Lock).

## The lock

File: `.workgraph/chat/<uuid>/.handler.pid`. Contents:

```
<pid>\n<exec-start-iso8601>\n<handler-kind>\n
```

### Acquire

A new handler process that wants to own the session:

1. Open `.handler.pid` with `O_CREAT | O_EXCL | O_WRONLY`. If it
   succeeds, we own it. Write PID + start time + kind, fsync, close.
2. If `EEXIST`, read the existing file:
   - **PID exists in `/proc` (Linux) / `kill -0 pid == 0` (Unix):**
     the lock is live. Either refuse (First-handler-wins), signal the
     existing handler to exit (TUI-takeover), or fall back to observer
     (read-only tail).
   - **PID is dead:** lock is stale. Delete the file and retry the
     O_EXCL create.
3. On clean handler exit, remove the file.
4. On crash, the file stays. Next acquire sees a dead PID and recovers.

### Release

On `SIGTERM` / `SIGINT` / clean `/quit` / EndTurn-in-autonomous-mode,
the handler:
1. Removes `.handler.pid` before exiting.
2. (Optional) Writes a final `end` entry to `conversation.jsonl`.

`atexit` / `Drop` hooks cover clean exits. Crashes are recovered via
stale-PID detection on next acquire.

### Takeover

Takeover is **intent-triggered, not open-triggered.** Opening the TUI
on a session that already has a handler does NOT trigger takeover —
the TUI enters observer mode (see §Handoff policy below). Takeover
only fires when the user signals *active engagement* by sending a
message or explicitly requesting it.

When triggered, the flow is:
1. TUI writes the user's message to `inbox.jsonl` (same as always).
2. TUI also marks the lock file with a `release_after_turn=true`
   flag (or writes a dedicated `.handler.release-requested` marker
   next to it).
3. External handler, at its next turn boundary, drains inbox → sees
   both the message and the release marker.
4. Handler finishes any in-flight tool call (§Long tool calls in
   progress below), commits journal, removes the lock file, exits
   clean.
5. TUI detects lock release, acquires via O_EXCL, spawns its own
   PTY-backed handler with `--resume`.
6. New handler replays journal (which includes the user's queued
   message), responds via PTY.

**No SIGKILL escalation.** We wait as long as the current tool call
takes. Journal consistency beats UI responsiveness — if the user
wants to interrupt a long tool call, they use the separate cancel
action (§Non-message interventions), which is orthogonal to
takeover.

## Handoff policy

**Observing is free; sending a message is the signal for takeover.**

When the TUI opens on a session that already has a live handler, the
TUI enters **observer mode**. It renders the session (tailing the
PTY/streaming file) but does not signal, takeover, or otherwise
interact with the handler. The autonomous work continues unaffected.

### Observer mode

- TUI reads whatever the handler is writing (streaming file + journal
  for replay).
- User input box is available but sending a message triggers takeover
  (next section).
- Non-takeover user actions (scrolling, switching tabs, viewing task
  detail) never affect the handler.
- Read-only: Ctrl-C in the observer pane is the separate
  *cancel-current-turn* action (§Non-message interventions); it does
  not trigger takeover.

### Takeover trigger: sending a message

When the user hits send on a message in observer mode, the TUI runs
the full takeover sequence (§Takeover above). The handler reads the
message at its next turn boundary, exits cleanly, and the TUI spawns
its own handler which resumes with the message already in the inbox.

The rationale for this model vs. "TUI always wins on open":

- **No accidental interrupts.** Coming back to a long-running
  research task should just show what's happening, not preempt it.
- **Intent-driven.** Takeover only when the user actively wants to
  converse. Navigation is free.
- **Uses existing handler shutdown path.** Handlers already know how
  to drain inbox and exit at turn boundary. No new code paths.
- **Journal continuity.** The swap happens at a turn boundary, so
  both handlers agree on state.

### Long tool calls in progress

If the handler is mid-tool-call (e.g. a 30-minute research) when the
user sends, the user's message waits in the inbox until the tool call
completes. The handler's next turn boundary is after the tool
finishes. **We do not interrupt mid-tool-call for takeover.**

Journal consistency wins over UI responsiveness. A user who wants to
interrupt a long tool call uses the explicit cancel action
(§Non-message interventions) first, which is a separate operation
from takeover.

### Non-message interventions

Cancel-current-turn and slash-commands are not message sends:

- **Cancel (Ctrl-C in observer pane):** sends a cooperative-cancel
  signal to the handler. Handler aborts its in-flight tool at the
  next safe point (same behaviour as Ctrl-C inside a `wg nex`
  terminal session). Does NOT release the lock, does NOT trigger
  takeover. The user can then decide whether to send a new message
  (triggering takeover) or keep observing.
- **Slash commands:** only available post-takeover, via the owned
  PTY. Observer mode has no slash-command surface — slash commands
  would require direct PTY stdin, which observer mode doesn't have.

### Rapid-fire sends

If the user sends multiple messages before takeover completes (common
pattern: user types, hits send, types more, hits send again), both
messages queue in the inbox. The external handler drains both FIFO,
processes them, exits. TUI's new handler replays journal including
both. No special handling.

## The TUI view

**One right-panel tab collapses five.** Chat, Log, Messages, Firehose,
and Output all become a single PTY view of the focused task's handler.

- **Focus a task in the graph →** right pane shows
  `wg spawn-task <task-id>` in a PTY. If the task has no handler yet,
  `spawn-task` spawns one and acquires the lock. If a handler already
  runs, takeover.
- **Input in the pane →** PTY stdin → handler. Native nex sees rustyline;
  claude sees its own input box; each handler behaves per its own UX.
- **The TUI renders bytes.** It doesn't interpret handler-specific
  structure. vt100 emulation + tui-term gives faithful rendering of
  whatever the CLI draws.

The "current coordinator" concept dissolves. There's no active
coordinator — there are N tasks, you focus whichever one.

## Heterogeneous executors

Different LLM-backed tasks use different CLIs. We unify identity and
lifecycle; we preserve handler-specific UI.

| Layer | Unified across executors | Heterogeneous |
|---|---|---|
| Session identity (UUID, alias, dir) | ✓ | |
| Handler-PID lock | ✓ | |
| Task lifecycle (ready → in-progress → done) | ✓ | |
| Working dir / worktree | ✓ | |
| PTY transport to the TUI | ✓ | |
| Terminal UI inside the PTY | | ✓ |
| Resume mechanics | | ✓ |
| Native tool surface | | ✓ |

### The `wg spawn-task` abstraction

A single CLI entry point that resolves a task id to its handler
command:

```
wg spawn-task <task-id>       # blocks, owns the lock, PTY-friendly
wg spawn-task <task-id> --no-lock   # tail-mode (no acquire)
```

Dispatches per executor:

- **native:** `wg nex --chat <uuid> --resume --role <task-role>`
- **claude:** `claude --resume <claude-session-id>` (session-id stored
  in task metadata; first run creates it)
- **codex:** `codex --session <id>` or equivalent
- **gemini:** TBD (check current CLI flags)
- **amplifier:** `wg amplifier-run <uuid>`

The TUI PTYs `wg spawn-task`, never the underlying CLI. When a CLI
vendor changes flags or adds resume support, we change one adapter in
`commands/spawn_task.rs`, not the TUI.

### Tool parity via MCP

`wg nex` has wg_add, wg_done, wg_fail, etc. built in. Claude/Codex
don't. An MCP server (`wg-mcp`) ships the same tool surface over MCP.
Handlers that support MCP (claude does, codex does, gemini: verify)
get tool parity.

For CLIs without MCP, a wrapper process proxies tool calls back to
`wg` via IPC. Ugly but functional fallback.

### Resume fallback

Not every CLI has session resume. For those, `wg spawn-task` prepends
a context preamble (the journal's session-summary) as the first user
message on re-invocation. Same trick `wg nex --resume` uses when there's
no journal but a summary exists. Lossy but workable.

## Daemon role

Daemon stops being the coordinator-owner. New role:

- **Supervisor.** Watch sessions with `in-progress` tasks and no live
  handler (stale or crashed). Respawn the handler.
- **Scheduler.** Continue walking ready tasks, spawning handlers for
  them up to `max_agents`.
- **Event sink.** Notification backends (Matrix, Telegram) still hook
  into lifecycle events emitted by handlers.

Daemon is opt-in. Running workgraph without a daemon works: the TUI
(or `wg spawn-task` at a terminal) owns any handler you care about.
Daemon is for users who want autonomous-when-I'm-away behavior.

## Decisions on initial open questions

(These were flagged as open during the design discussion. Resolved
here so the rollout can proceed deterministically. Listed in the
order they were raised.)

1. **Finished task focus.** *Read-only by default for terminal
   states (`done` / `failed` / `abandoned`); hot otherwise.* The
   PTY replays journal and shows the final screen. A dedicated
   keybind (e.g. `r`) flips to hot/resumable mode — same underlying
   `wg nex --chat X --resume`, just drops the read-only guard. The
   status field already captures terminal-vs-live, so drive the
   default off of it.

2. **Never-started task focus.** *Require explicit start.* Focus on a
   task with no journal (status `open`) shows a "press `s` to start"
   placeholder in the right pane. Poking around the graph is
   non-destructive. Hitting `s` spawns the handler, acquires the lock,
   transitions the task to `in-progress`.

3. **Remote worktree sessions.** *Hybrid — handler runs in the
   worktree, chat files live in the main repo's `.workgraph/chat/`.*
   The handler opens its chat files via absolute path into the main
   `.workgraph/`, while its working directory is the worktree. TUI in
   the main repo sees one canonical chat dir; worktree isolation of
   code edits is preserved; no cross-worktree session resolution
   needed.

4. **Task-id migration.** *Auto-migrate on first startup post-upgrade;
   keep the old `.coordinator-N` id as a permanent alias.* No user
   action required. Scripts / git-history links that hardcode
   `.coordinator-0` continue to work via alias resolution forever.
   New coordinators post-migration get `.chat-<uuid>` directly.

5. **TUI takeover semantics.** *Observe by default, takeover
   triggered by sending a message, wait for current tool call to
   complete.* See §Handoff policy above for the full model. No
   SIGKILL escalation — journal consistency beats UI responsiveness.
   Explicit cancel is a separate non-takeover action.

## Non-goals

- **Homogenizing CLI UIs.** Claude's box-drawing won't look like nex's
  box-drawing. That's fine. Users see "claude's UI when on a claude
  task" — honest and simple.
- **Single-binary consolidation.** We're not rewriting claude/codex
  as Rust crates or linking them in-process. PTY keeps them
  subprocess-boundary clean.
- **Live multi-user per session.** One handler at a time. Two users
  want to watch? Second user gets tail-mode. Collaborative editing of
  the same session is out of scope.

## Related prior art in this repo

- `nex-as-coordinator.md` — introduced the ConversationSurface trait
  as the plug point for nex serving every role. Landed as
  commits 737d223f / f92c7b8a / d7f8b5cb / ecdc1252 (3a).
- `chat_sessions.rs` — session UUID registry, aliasing,
  `register_coordinator_session` helper.
- `commands/tui_pty.rs` — PTY-embed `wg nex` in ratatui, commit
  b5642aea (3b). The infrastructure the TUI Chat tab will build on.

# Sessions as identity — rollout plan

**Companion to `sessions-as-identity.md`.** That document defines the
target; this one defines how we get there. Each phase is an
independently shippable commit-chain with explicit validation.

## Principle of staging

Every phase satisfies three constraints:

1. **Shippable.** At the end of the phase, main still works. No
   half-migrated state. A user who installs `wg` between phases gets
   a functioning system.
2. **Reversible.** Each phase lands as its own commit(s) so it can be
   reverted without taking later phases with it.
3. **Validated live.** Before declaring a phase done, its behaviour
   is smoke-tested with the actual binary against the actual workflows
   it's supposed to change. "tests pass + builds clean" is necessary,
   not sufficient (see `feedback_verify_exhaustively_before_claiming_done`).

## The seven phases

Each phase lists: *what it ships*, *what changes for users*, *validation*.

### Phase 1: Handler-PID lock (foundation)

**Ships:** `.workgraph/chat/<uuid>/.handler.pid` acquire/release/takeover
logic in `chat_sessions.rs`. New functions:

- `SessionLock::acquire(dir, kind) -> Result<SessionLock>` — O_EXCL
  create with PID + start time + handler kind. Detects stale locks
  (dead PID) and recovers.
- `SessionLock::release(self)` — removes the file. Idempotent.
- `SessionLock::try_takeover(dir, timeout) -> Result<SessionLock>` —
  SIGTERM, poll, escalate to SIGKILL after 5s, then acquire.
- `Drop` impl that calls release.
- Signal handler registration (`SIGTERM`, `SIGINT`) in `wg nex` and
  coordinator subprocess — both trigger turn-boundary-safe shutdown.

**User-visible change:** none directly. Two concurrent
`wg nex --chat <same-ref>` invocations now reliably refuse the second
one instead of racing on the inbox.

**Validation:**
- Unit: acquire twice → second errors; acquire after kill -9 of first
  → succeeds (stale recovery); takeover sends SIGTERM and waits.
- Live: run `wg nex --chat coord-0 --autonomous` with a long-running
  prompt, then in another terminal run `wg nex --chat coord-0 --resume`.
  The second should either refuse or takeover cleanly (per flag), not
  race on inbox.jsonl.
- Inspect: `.handler.pid` exists during a live session, is removed on
  clean /quit, persists on SIGKILL (recovered on next start).

**Size:** ~300 LOC (session_lock.rs new module + integration in `wg nex`).

### Phase 2: `wg spawn-task` abstraction

**Ships:** a new subcommand `wg spawn-task <task-id>` that looks up
the task's executor type + chat ref, dispatches to the right handler
command, acquires the lock, replaces-process-via-exec (so the handler
inherits stdin/stdout/stderr cleanly for PTY use). Per-executor
adapters:

- `adapter_native(task)` → `wg nex --chat <uuid> --resume --role <role>`
- `adapter_claude(task)` → `claude --resume <claude-session-id>` with
  workgraph MCP server launched as a sidecar
- `adapter_codex(task)` → equivalent for codex
- `adapter_gemini(task)` → TBD, probably stub with a "not yet supported"
  error until the CLI is verified
- `adapter_amplifier(task)` → defers to existing amplifier runner

**User-visible change:** `wg spawn-task <id>` becomes the canonical way
to interact with any task's handler from a terminal. `wg nex --chat`
keeps working (it's what the native adapter calls).

**Validation:**
- Live: `wg spawn-task <native-task-id>` in a terminal — native nex UI
  appears, conversation works, exits clean.
- Live: `wg spawn-task <claude-task-id>` — claude UI appears, MCP
  sidecar responds to wg_add calls, exits clean.
- Lock: `wg spawn-task X` twice → second blocks or takes over per flag.

**Size:** ~400 LOC, mostly adapter wiring + docs. Claude/Codex adapters
land behind `#[cfg]` gates or feature flags so they don't fail to
build when the CLI isn't installed.

### Phase 3: TUI Chat tab → PTY (observer + takeover-on-send)

**Ships:** the Chat tab in `wg tui` becomes a PtyPane whose behaviour
depends on whether the focused task's session has a live handler.

- New field `task_panes: HashMap<TaskId, PtyPane>` on VizApp (reuse
  the work from the pre-design Nex-tab attempt, rerouted to Chat).
- **Observer mode (default when an external handler already owns
  the session):** TUI tails the PTY/streaming file, renders faithfully,
  does NOT signal the handler, does NOT acquire the lock. User can
  see live activity without interfering.
- **Owned mode (TUI acquired the lock — either because nothing else
  owned it, or after a takeover):** key events forward to PTY stdin
  directly. User types into real nex / claude / whichever handler.
- **Takeover trigger:** sending a message via the TUI's input box in
  observer mode. TUI writes to inbox, marks release-requested, waits
  for the external handler to drain-and-exit at its next turn
  boundary, then acquires the lock and spawns its own handler.
- **Never-started tasks:** right pane shows "press `s` to start"
  placeholder. `s` spawns via `wg spawn-task`.
- **Terminal-state tasks (`done`/`failed`/`abandoned`):** read-only
  replay by default. Keybind (`r`) flips to resumable.

**User-visible change:** `wg tui` → focus any LLM task → live
PTY-rendered session. Observe autonomous work without interrupting
it; send a message to engage, and the TUI takes over cleanly. Tool
boxes, streaming, progress — all faithful because the PTY renders
whatever the handler draws.

**Validation:**
- **Observer fidelity.** Start `wg spawn-task <coord> --autonomous`
  in a terminal running a multi-tool workflow, then open `wg tui`
  and focus the same coordinator. TUI renders the live output; the
  terminal handler is unaffected; no takeover happens.
- **Takeover on send.** From the above observer state, type a
  message in the TUI and hit send. Confirm: message lands in
  `inbox.jsonl`; handler drains, finishes its current turn, exits;
  TUI's new handler replays journal, processes message, responds
  via PTY directly.
- **Long tool call wait.** Same setup, but the handler is mid-30s
  bash call when the user sends. Takeover must wait for the tool to
  finish before releasing — verify the release marker is respected
  at turn boundary, not mid-tool.
- **Cancel (non-takeover).** Ctrl-C in observer pane: handler
  aborts its in-flight tool; lock stays held; observer keeps
  watching. Separate action from takeover.
- **Cold focus.** Focus a never-started task. Right pane shows "press
  `s`". Hit `s`: handler spawns, task goes `in-progress`, PTY owned
  directly by TUI.
- **Terminal focus.** Focus a `done` task: read-only replay, no
  input accepted; hit `r`: now typeable.
- **Rapid-fire sends during takeover.** Send message 1, wait 1s,
  send message 2 before takeover completes. Both end up in inbox,
  both processed by the new handler FIFO.

**Size:** ~500 LOC new + retires large chunks of the file-tail chat
renderer in `state.rs` / `render.rs`. Net could be negative.

### Phase 4: Collapse Log / Messages / Firehose / Output tabs

**Ships:** removal of the `Log`, `Messages`, `Firehose`, `Output` tabs.
All the signals they surfaced are already visible in the PTY view:

- Log was per-task session log → nex/claude print that to stderr,
  which lands in the PTY.
- Messages was cross-task user-↔-agent turns → each task's inbox/outbox
  is visible in its own PTY.
- Firehose was aggregated agent stream → rarely used; if needed later,
  can be rebuilt as a split-pane view over multiple PTYs.
- Output was live agent stdout → same as Log from the PTY's view.

What survives: Chat (PTY), Detail, Agency, Config, Files, CoordLog
(renamed "Activity"?), Dashboard.

**User-visible change:** fewer tabs to navigate. Any user who habitually
switches between Log and Chat for the same task will find both views
in the single Chat PTY. CoordLog may be renamed to reflect that it's
now cross-task activity, not coordinator-specific.

**Validation:** for each removed tab, confirm the information it showed
is reachable from the PTY. Where it isn't (e.g., Firehose aggregation),
decide: rebuild in Phase 5+, or accept the loss.

**Size:** -1500 LOC estimated (lots of rendering code deletes).

### Phase 5: Task-id migration `.coordinator-N` → `.chat-<uuid>`

**Ships:** graph-storage migration that renames coordinator tasks from
`.coordinator-0` / `.coordinator-1` to `.chat-<uuid-short>` while
preserving the old id as a permanent alias (so links from prior git
history still resolve). `chat_sessions.json` gets an `legacy_alias`
field noting the old numeric id.

- One-time migration on first startup post-upgrade: scan graph for
  `.coordinator-*` tasks, find matching UUID in sessions.json, rewrite
  task id, add alias mapping.
- `wg show .coordinator-0` continues to resolve (via alias).
- New coordinators created post-migration get `.chat-<uuid>` ids
  directly.

**User-visible change:** task ids in `wg list`, `wg show`, etc. change
format for coordinators. Scripts that hardcode `.coordinator-0` keep
working via alias resolution.

**Validation:**
- Migration runs cleanly on an existing `.workgraph/graph.jsonl` with
  `.coordinator-0` + `.coordinator-1` — both get renamed + aliased.
- `wg show .coordinator-0` still resolves post-migration.
- New `wg tui` session create-coordinator flow produces `.chat-<uuid>`.

**Size:** ~200 LOC migration + tests + one-time-flag marker in sessions.json.

### Phase 6: Daemon → supervisor mode

**Ships:** daemon no longer owns the coordinator-spawning codepath.
New role:
- **Respawn:** every N seconds, scan sessions for stale lockfiles
  where a task is still `in-progress`. Respawn the handler via
  `wg spawn-task`.
- **Schedule:** continues walking ready tasks. Spawns via
  `wg spawn-task` instead of the current hand-rolled claude-CLI invoke.
- **Notify:** still hooks lifecycle events for Matrix/Telegram etc.

Old hand-rolled coordinator loop in
`commands/service/coordinator_agent.rs::native_coordinator_loop`
is deleted. Good riddance.

**User-visible change:** `wg service start` behaves the same from
outside. Inside, it's now executor-agnostic — no more claude-CLI
hardcoded, no more per-executor special-casing in the service layer.

**Validation:**
- `wg service start`, create a coordinator task, confirm handler
  spawns and conversation works.
- Kill the handler manually with SIGKILL. Within N seconds, daemon
  detects stale lock, respawns. Conversation resumes.
- `wg service stop` cleanly shuts down all tracked handlers
  (SIGTERM → wait → SIGKILL).

**Size:** net negative LOC. `native_coordinator_loop` is ~1400 lines;
supervisor logic is ~300.

### Phase 7: Executor adapters for Claude / Codex / Gemini

**Ships:** the claude/codex/gemini adapter implementations promised
in Phase 2 but stubbed. For each:

- Locate the CLI binary (configurable path).
- Map workgraph chat-ref → CLI session-id. Store the mapping in
  session metadata so re-spawning resumes cleanly.
- Launch `wg-mcp` sidecar (already exists) and pass its connection
  string to the CLI via the CLI's MCP config mechanism.
- For CLIs without native session-resume, inject journal-derived
  preamble.

**User-visible change:** you can now set `--executor claude` on a
task, focus it in the TUI, and interact with claude's UI through the
PTY. Tools work via MCP.

**Validation:**
- Create a `--executor claude` task, focus in TUI, type a prompt that
  requires a `wg_add` — verify the MCP-routed tool call lands and
  creates a real task.
- Same for codex, gemini (if present).
- Kill the claude CLI mid-session, re-spawn via `wg spawn-task` —
  verify resume works (via native session-id or preamble fallback).

**Size:** ~200 LOC per adapter, plus MCP plumbing shared across them
(~300 LOC). Feature-gated so a user without claude installed can
still build workgraph.

## Dependency graph

```
Phase 1 (lock)  ─────────┐
                         ├──> Phase 2 (spawn-task)  ─────┐
                         │                              ├──> Phase 3 (TUI PTY)
                         │                              │
                         │                              ├──> Phase 4 (tab collapse)
                         │                              │
                         └───────────────────────────── ┼──> Phase 6 (daemon → supervisor)
                                                        │
                                                        └──> Phase 7 (claude/codex/gemini adapters)

Phase 5 (task-id migration) can land anywhere after Phase 1. Suggested: between 3 and 4 (after the TUI-PTY payoff is validated, before the tab cleanup).
```

Phases 1–3 is the critical path for the user-visible "coordinator chat
is nex in a PTY" vision. 4–6 clean up. 7 opens the executor choice.

## Risk register

| Risk | Phase | Mitigation |
|---|---|---|
| Lock file on NFS / network FS doesn't do O_EXCL atomically | 1 | Document local-FS-only. Fallback flock() if O_EXCL is unavailable. |
| SIGTERM during a tool call leaves the session journal half-written | 1, 3 | Signal handler defers exit to next turn boundary, which is already the "safe snapshot" point in the loop. |
| Claude CLI session-resume disagrees with our chat-ref mapping | 7 | Store the mapping, log mismatches, fall back to preamble injection. |
| TUI takeover loses the daemon's in-flight work | 3 | Takeover is message-triggered, not open-triggered. Handler finishes current tool call, drains inbox, commits journal, exits clean at turn boundary — no SIGKILL. A runaway tool that never completes would block takeover indefinitely; the user's explicit-cancel action (Ctrl-C in observer pane) is the escape hatch. |
| Phase 4 removes a tab someone actually used | 4 | Before deletion, survey the repo's own CLAUDE.md / docs for references. Announce deprecation in one release before removing. |
| Migration (Phase 5) breaks git history links | 5 | Keep `.coordinator-N` as permanent alias — never actually rename in the graph, just map. |

## Deferred / out-of-scope

- **Multi-user simultaneous session viewing.** One handler, one owner.
  Observers can tail the streaming file but can't type.
- **Distributed sessions (session UUID across machines).** We assume
  local FS. Future work, not here.
- **Handler hot-swap mid-turn.** We wait for a turn boundary. Mid-turn
  takeover is hard and not needed for the vision.
- **Unifying CLI UIs.** Each CLI keeps its own look. We unify
  identity + lifecycle; UI heterogeneity is a feature, not a bug.

## Definition of done (for the whole rollout)

- `wg tui` → focus any LLM-backed task → live PTY of its handler.
- `wg spawn-task <task-id>` from any terminal owns the session or
  takes over from an existing owner.
- `.coordinator-N` is an alias, not a first-class id.
- Daemon is the optional supervisor, not the owner.
- Claude/Codex/Gemini tasks are first-class: PTY-focused, tool-parity
  via MCP, executor-agnostic from the graph's perspective.
- `native_coordinator_loop` is deleted.

At that point, the vision is implemented. Every piece of LLM
conversation in workgraph is a session + a handler, and the TUI is
the unified viewer.

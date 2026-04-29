# Chat Agent Persistence: tmux wrapper vs targeted codex fix vs custom detach

**Task:** design-chat-agent
**Date:** 2026-04-29
**Status:** design — implementation deferred to follow-up task

---

## Problem statement

The user wants tmux-like behavior for chat agents: if the wg TUI exits (clean
quit, panic, host SSH disconnect, terminal close), the chat agent process
should survive and be reattachable from a fresh `wg tui` session. Currently:

- Killing the TUI kills every chat agent PTY child.
- Claude *appears* to recover on the next `wg tui` because its on-disk session
  log is replayed by `claude --resume <uuid>`.
- Codex *appears* broken when interrupted mid-turn — the user reports
  `if i exit while it's working, codex breaks`.

This design doc answers three questions:

- **Part A.** Why does claude survive TUI exit while codex breaks?
- **Part B.** Which persistence strategy do we adopt? (A/B/C/D)
- **Part C.** What does `auto-resume` actually mean for our scope?

The deliverable is a concrete implementation plan + smoke scenarios that a
follow-up task can execute without re-investigation.

---

## Part A — claude vs codex asymmetry

### Code-level facts (from this branch)

1. **Both chat agents are spawned by the same code path.** Both go through
   `PtyPane::spawn_in` in `src/tui/pty_pane.rs:119-314`. There is no special
   signal handling, no `setsid`, no `setpgid`. The child inherits the TUI's
   process group and session.

2. **`PtyPane::Drop` unconditionally kills the child.** See
   `src/tui/pty_pane.rs:551-558`:

   ```rust
   impl Drop for PtyPane {
       fn drop(&mut self) {
           let _ = self.child.kill();
           // ...
       }
   }
   ```

   This runs in two cases:
   - Clean TUI exit → `task_panes` HashMap is dropped → every PtyPane drops →
     every child killed.
   - Panic → `restore_terminal()` runs (mod.rs:75-78) but the panic still
     unwinds and the HashMap is dropped → same result.

   It does NOT run when the TUI process is signaled (SIGKILL, SIGHUP from a
   closed terminal). In that case the TUI dies without unwinding; the children
   become orphans, get reparented to PID 1, and continue running until they
   themselves SIGHUP-out (controlling-tty closure).

3. **Same `Drop::kill` for both claude and codex.** The spawn-site dispatch
   in `src/tui/viz_viewer/state.rs:13039-13183` chooses different `(bin,
   args, cwd)` per executor but feeds them all to the same `PtyPane::spawn_in`.
   So at the TUI-exit moment the kill behavior is identical.

4. **The asymmetry is on the resume side, not the kill side.** Look at
   `state.rs:13089-13177`:
   - **claude** resumes via `--resume <uuid>` against a session log under
     `~/.claude/projects/<slug>/<uuid>.jsonl`. Claude appends complete
     messages to this log; partial in-flight assistant tokens are lost on
     kill but the log itself stays well-formed.
   - **codex** resumes via either `resume <session-id>` (daemon-persisted in
     `.codex-session-id`) or `resume --last`. Codex's rollouts under
     `~/.codex/sessions/` include tool-call records that may be partial
     when killed mid-turn (request envelope written, response not). On
     resume, codex sees an unterminated tool-call and may error / rewrite /
     drop output.

5. **The `--no-alt-screen` codex flag is a separate fix.** It was added in
   commit `258d79fde` (fix-pass-no, 2026-04-XX) to fix *rendering*
   issues — alt-screen content never enters our scrollback and stacks
   animation frames. It does not address process *survival*. Even with
   `--no-alt-screen`, the codex child is killed on TUI exit.

6. **Claude/codex daemon handlers have signal logic that DOES NOT apply
   here.** `src/commands/claude_handler.rs` has SIGTERM forwarding;
   `src/commands/codex_handler.rs` does not. But these handlers run the
   *daemon's* autonomous chat agent (background dispatch path), not the
   *TUI's* interactive PTY chat. The TUI spawns the literal `claude` /
   `codex` binaries directly — the wg handler logic is bypassed.

### Reproduction recipe (cannot be executed by a worker agent)

This worker agent is not running an interactive terminal, so the empirical
TUI-exit repro must be executed by a human or by a follow-up scenario test.
The recipe below is concrete and assertion-driven so any future agent (or the
implementation task) can run it:

```bash
# Repro 1: claude mid-turn TUI exit
$ wg tui            # tab 1: chat-0 with claude (default)
# In another terminal:
$ pgrep -af 'claude.*--session-id'   # capture chat child PID = $CL_PID
$ ps -o pid,ppid,pgid,sid,stat,cmd -p $CL_PID    # confirm same pgid as TUI
# In TUI: type "Write a 500-line essay about X" — wait for streaming to start.
# In another terminal: kill -KILL $TUI_PID  (host crash simulation)
$ ps -p $CL_PID                       # ASSERTION: process exits within 5s
$ ls ~/.claude/projects/<slug>/       # ASSERTION: session jsonl is well-formed
$ wg tui                              # reopen
# ASSERTION: chat-0 reattaches with full prior turn visible (incomplete tail OK)

# Repro 2: codex mid-turn TUI exit
$ wg tui            # tab 1: chat-1 with codex
$ pgrep -af 'codex'                   # CX_PID
# In TUI: type "Run ls and explain" — wait for tool call to start.
# kill -KILL $TUI_PID
$ ps -p $CX_PID                       # ASSERTION as above
$ ls ~/.codex/sessions/                # ASSERTION: rollout file present
$ jq '.records[-1]' ~/.codex/sessions/<latest>.jsonl
# ASSERTION: last record is complete (request) — tail integrity check
$ wg tui
# OBSERVE: does codex resume cleanly? does it abort mid-stream? does it
# replay a corrupted tool-call?
```

Hypothesized result based on code reading + user report:

- Claude: child dies, session log intact, resume works (degraded mid-turn but
  usable).
- Codex: child dies, rollout has unterminated tool-call, `resume --last`
  either rejects the rollout or replays into an inconsistent state.

### Conclusion of Part A

The asymmetry is **not** signal-handling — it is **resume-log integrity**.
Claude's session-log format is append-tolerant; codex's rollout format is
not. **Therefore option D (targeted codex SIGHUP fix) cannot solve the user's
real complaint.** A SIGHUP/SIGTERM handler on the codex CLI side cannot
reach in and fix a half-written rollout; the only way to keep the rollout
well-formed is to keep the codex process *alive* across TUI exits, so that
codex itself flushes its turn cleanly before any kill happens.

This eliminates D and pushes us toward A/B/C — a real persistence layer.

---

## Part B — persistence strategy comparison

### Option A: wrap every chat agent in tmux

Spawn:
```
tmux new-session -d -s wg-chat-<chat_ref> -- <bin> <args>...
```
TUI's PtyPane wraps `tmux attach -t wg-chat-<chat_ref> -d`, so when the TUI
dies the *attach client* dies but the tmux server keeps the chat process
alive. Reopening the TUI reattaches.

**Pros:**
- Battle-tested; the persistence path is what `tmux attach` does every day.
- Free reattach: `tmux attach -t wg-chat-<ref>` works from any terminal — even
  outside `wg tui`, useful for debugging.
- Free scrollback: tmux's own buffer. May fix unrelated PTY-emulator issues.
- We already use tmux for the outer multi-user `wg tui` wrapping
  (`docs/design/terminal-wrapping-strategy.md`) and for `wg server`. The dep
  is already a project assumption; `setup.rs:2156-2159` warns when missing.
- tmux acts as a vt100 normalizer between the vendor CLI and our emulator —
  our emulator only needs to handle whatever tmux renders, which is much
  cleaner than what vendor CLIs emit directly.

**Cons:**
- Hard dep on tmux being installed (mitigation: graceful fallback to current
  direct-spawn behavior + warning when tmux missing).
- Two layers of PTY: tmux's server-side PTY (vendor CLI inside) → tmux client
  → our PtyPane PTY → ratatui. Latency is real but bounded (microseconds per
  byte, no syscall amplification — tmux uses shared-memory IPC between
  client/server).
- TERM env inside the chat is `tmux-256color` / `screen-256color` instead of
  `xterm-256color`. Some vendor CLIs adjust capabilities; observed in
  practice this is fine because tmux-256color is well-supported.
- Lifecycle management: orphan tmux sessions when chats are abandoned. Need a
  cleanup hook.

### Option B: lighter detach utility (dtach / abduco)

Same shape as A but with `dtach -A wg-chat-<ref>.sock <bin>` instead of tmux.

**Pros:** smaller dep, no scrollback layer, simpler.

**Cons:**
- Less common; users may not have it installed and won't recognize the
  failure mode.
- No scrollback, no normalization — we still hit raw vendor-CLI escape
  sequences in our emulator. This means we don't get the
  rendering-cleanup benefit that A gives us "for free".
- We already pay for tmux as a dep (server, multi-user, setup wizard). Adding
  a *second* opaque PTY-detach utility for one feature is dependency
  duplication.

### Option C: custom detached-process supervision

Spawn chat agents via `setsid()` (we already do this for worker agents in
`src/commands/spawn/execution.rs:646-653`) plus a Unix socket per chat for
input/output forwarding. The TUI connects/disconnects via socket; the chat
process keeps running.

**Pros:** native, no external dep.

**Cons:**
- Reinvents tmux's reattach protocol from scratch.
- Have to design + test our own framing, flow control, signal forwarding
  (SIGWINCH for resize), output buffering on disconnect.
- The buffering policy alone is non-trivial: ring buffer? size limit?
  per-chat or global?
- We get nothing free — all the rendering quirks the vendor CLIs hit
  against our emulator stay our problem.

### Option D: targeted codex SIGHUP/exit fix

Eliminated by Part A: codex's rollout corruption requires the *process* to
survive the kill, not just the signal. There is no codex-side fix that keeps
the rollout well-formed across an external SIGKILL.

### Recommendation: **A (tmux wrapper)** with graceful fallback

- We already have tmux as a soft project dep (server, multi-user, setup).
- Free reattach + free rendering normalization are real wins, not just
  hypothetical.
- B's "smaller dep" argument is undercut by the fact that tmux is *already
  there*.
- C's complexity is real and untested; we'd be designing a tmux clone.
- D doesn't solve the user's reported problem.

When tmux is not installed: fall back to direct-spawn (current behavior) +
a one-time warning at chat-spawn time advising the user to install tmux for
chat persistence. Do not silently degrade.

---

## Part C — what does auto-resume mean?

Three possible scopes, ordered by cost:

1. **Survive only.** Process keeps running, output buffered, no automatic
   reattach. User would need to call `wg chat reattach <ref>` or similar.
2. **Survive + auto-reattach.** Next `wg tui` opens chat tabs that re-bind to
   their existing tmux sessions. (Default behavior with option A.)
3. **Survive + multi-attach.** Multiple `wg tui` instances can simultaneously
   view the same chat; tmux supports this natively via `tmux attach -t
   <name>` from each.

**Recommend scope #2 (survive + auto-reattach).** Multi-attach is a cheap
side-effect of using tmux but we should not ship it as a documented feature
in the first cut — it has surprising consequences (two users typing into
the same chat) that need separate UX thought. The implementation should not
*prevent* multi-attach, it just shouldn't advertise it.

---

## Implementation plan

### File scope (single follow-up task — no parallelism, sequential edits)

1. **`src/tui/pty_pane.rs`** — add `PtyPane::spawn_via_tmux(...)`
   constructor that:
   - Checks `tmux` availability via `which tmux` (cached for session).
   - Computes `session_name = format!("wg-chat-{}-{}", project_tag,
     chat_ref)` (mirrors the existing `wg-{project_name}` namespace from
     `src/commands/server.rs:119`).
   - Calls `tmux has-session -t <name>` to detect existing sessions.
   - If new: `tmux new-session -d -s <name> -e KEY=VAL ... -- <bin>
     <args>...` to start detached, then `tmux attach -t <name> -d` (the `-d`
     detaches any other clients first, ensuring single-attach semantics for
     the first cut).
   - If existing: just `tmux attach -t <name> -d`.
   - Returns a `PtyPane` whose `child` is the *attach client*. Drop kills only
     the client; the tmux server keeps the chat process alive.
   - Adds an explicit `kill_underlying_session()` method for the explicit
     "user wants to discard this chat" path, which calls `tmux kill-session
     -t <name>`. **Drop must NOT call this** — the whole point is survival.

2. **`src/tui/viz_viewer/state.rs`** — at the chat spawn site
   (`maybe_auto_enable_chat_pty` / `consume_pending_chat_pty_spawn` around
   lines 12916-13289):
   - When tmux is available, route `claude` / `codex` / `native` chat spawns
     through `PtyPane::spawn_via_tmux`. Native (`wg nex --resume`) benefits
     equally; the design is executor-agnostic.
   - When tmux is missing: existing direct-spawn path + log warning to
     stderr once per TUI session. The fallback path is unchanged code.
   - The pending-spawn deferral pattern (lines 13218-13289) stays the same —
     tmux opens the inner pty at a hardcoded size of its own; on first attach
     SIGWINCH resizes both the tmux pane and the chat process. Verified once
     in a smoke scenario.

3. **`src/tui/viz_viewer/state.rs`** (chat-archive path) — when a chat tab
   is closed/archived/deleted, call the new `kill_underlying_session()`
   so we don't accumulate orphan tmux sessions across many chats. Existing
   archive paths are at `event.rs:3203` (`task_panes.remove`) and similar.

4. **`src/commands/chat_cmd.rs`** (or a new `src/commands/chat_attach.rs`) —
   add `wg chat attach <chat-ref>` for terminal-side attach without TUI:
   shells out to `tmux attach -t wg-chat-<project>-<chat_ref>`. This is a
   user-facing convenience but also gives smoke scenarios a non-interactive
   handle to assert against. Optional for v1; recommended.

5. **`src/commands/setup.rs`** — bump tmux from "needed for `wg server`" to
   "needed for chat persistence" in the detector output (lines 2156-2159).
   Not a hard install requirement, just clearer messaging.

6. **No changes** to `src/commands/claude_handler.rs`,
   `src/commands/codex_handler.rs`, or any vendor-CLI-specific logic. The
   TUI spawn path is the only thing that changes. Daemon-side autonomous
   handlers continue to use their own session-lock mechanism.

### Lifecycle invariants

- One tmux session per `(project, chat_ref)` pair. Name is deterministic.
- Session lifetime = chat lifetime, NOT TUI lifetime. The session is created
  on first chat spawn and killed only when the chat is explicitly archived
  / deleted / abandoned.
- If a tmux session exists but its chat task has been deleted from the graph,
  it is an orphan. A startup sweep at TUI launch (`fn ensure_user_coordinator`
  at `state.rs:13294` is the right hook point) lists `tmux list-sessions`
  and kills any `wg-chat-<project>-*` session whose chat_ref no longer
  resolves to a live task.
- Drop on the attach-client PtyPane never kills the server-side session.
  This is the new invariant; it inverts the current `Drop::kill` contract
  for tmux-wrapped panes specifically. Plain (non-chat) PtyPanes elsewhere
  in the codebase keep their existing kill-on-drop semantics.

### Concurrency / IPC

No new IPC protocol needed. Tmux's control protocol is already a well-defined
channel; we don't speak it (we just shell out to `tmux` for has-session /
new-session / attach / kill-session). Forwarding stdin / stdout is exactly
the existing PtyPane pipe to the `tmux attach` client.

### Failure modes to handle

1. **tmux not installed** — fall back to direct spawn + warn once.
2. **tmux session exists from prior crashed run** — `has-session` true,
   `attach -d` reattaches cleanly. This is the desired path.
3. **tmux session exists but its chat process died** — `attach` shows an
   "[exited]" pane and exits. Detect via `tmux list-panes -t <name> -F
   '#{pane_dead}'` before attach; if dead, kill the session and start fresh.
4. **User runs `tmux kill-server`** — we lose all chat sessions in one shot.
   Acceptable; same blast radius as `pkill claude`.
5. **Two `wg tui` instances opened simultaneously** — `attach -d` causes the
   first to detach when the second attaches. Single-attach property maintained
   by default. (Multi-attach would drop the `-d`.)

---

## Smoke scenarios (added to `tests/smoke/manifest.toml`)

All scenarios live under `tests/smoke/scenarios/` as bash scripts that exit
0=PASS, 77=SKIP (e.g. `tmux not installed`), nonzero=FAIL. Each must use
real binaries against the local repo install. Owner is the implementation
task id (TBD).

1. **`chat_persists_across_tui_exit_claude.sh`**
   - Start dispatcher, create chat-0 with claude, send a message, wait for
     reply.
   - Capture claude PID via `pgrep -f claude.*session-id` and TUI PID.
   - SIGTERM the TUI process.
   - ASSERT claude PID is alive 2s later.
   - ASSERT `tmux has-session -t wg-chat-*-chat-0` exit 0.
   - ASSERT TUI restart reattaches; `wg chat history chat-0` shows the prior
     reply.

2. **`chat_persists_across_tui_exit_codex.sh`**
   - Same as #1 but with codex executor. The user's reported bug
     reproduction.
   - ASSERT codex PID alive after TUI kill.
   - ASSERT `~/.codex/sessions/<latest>.jsonl` last record is complete (no
     unterminated tool call).

3. **`chat_persists_mid_tool_call_codex.sh`**
   - Start codex chat, prompt with `Run pwd and tell me the answer`.
   - Wait for the tool-call request to be visible in the rollout file but
     before the response is recorded (poll the rollout).
   - SIGKILL the TUI (not SIGTERM — simulates host crash).
   - ASSERT codex PID is alive 3s later (tmux survives SIGKILL of attach
     client).
   - ASSERT codex completes the tool-call (rollout grows past the request).
   - Restart TUI, ASSERT chat is reattached and shows the tool result.

4. **`chat_no_orphan_tmux_after_archive.sh`**
   - Create chat-0, confirm `wg-chat-*-chat-0` tmux session exists.
   - Archive / delete chat-0 via TUI or `wg sweep`.
   - ASSERT tmux session is killed within 2s.

5. **`chat_orphan_tmux_swept_on_tui_start.sh`**
   - Manually create a tmux session named `wg-chat-<project>-chat-99` running
     `sleep 9999`. (No matching task in graph.)
   - Start `wg tui`.
   - ASSERT the orphan session is killed during startup sweep.

6. **`chat_falls_back_when_tmux_missing.sh`**
   - Use a `PATH` that excludes tmux (`PATH=/usr/bin env -i ... wg tui`
     or a tmpdir-shadowed PATH).
   - ASSERT chat-0 spawn succeeds with a one-time stderr warning containing
     `tmux not installed` / `chat persistence disabled`.
   - ASSERT TUI exit kills the chat (current behavior preserved).

7. **`chat_attach_command_works.sh`** (only if `wg chat attach` is shipped
   in v1)
   - Create chat-0 in a TUI, exit TUI.
   - From a fresh terminal, run `wg chat attach chat-0`.
   - ASSERT terminal attaches to the still-running chat process; sending
     `q` then enter (or chat-specific quit) works; assertions on captured
     output match the prior conversation tail.

All seven scenarios must FAIL on `main` (pre-implementation) and PASS after
the implementation task lands. CI: smoke gate runs them on `wg done <impl-
task>`; manifest `owners` lists the implementation task id.

---

## Out of scope for v1

- **Multi-attach UX.** Tmux supports it; we do not advertise or test it.
- **Cross-host persistence.** Tmux session lives on whatever host the user
  ran `wg tui` from; SSH'ing in from another host hits the same session if
  on the same box, otherwise gets a new chat. This matches user expectations.
- **Rollback / replay UI.** Tmux has its own scrollback; tying it to the
  graph's chat history is a separate concern (already handled by
  `chat.rs` / `chat_sessions.rs`).
- **Suspend (Ctrl-Z) of vendor CLIs.** Tmux complicates SIGTSTP forwarding;
  punt for v1, document.
- **Codex `--no-alt-screen` interaction.** Already shipped in `fix-pass-no`
  and is independent of this work — keep both flags.

---

## Summary

- The claude/codex asymmetry on TUI exit is **rollout integrity**, not
  signal handling. Codex cannot be fixed in isolation (option D rejected).
- Choose **option A: tmux wrapper** with graceful fallback when tmux is
  missing. Project already depends on tmux for adjacent features; the
  reattach + rendering benefits are immediate and free.
- Scope of "auto-resume" is **survive + auto-reattach** (not multi-attach).
- Implementation is bounded to `pty_pane.rs`, the chat spawn site in
  `state.rs`, the chat archive path, and a small `setup.rs` messaging tweak.
- Seven smoke scenarios cover the regression surface; the impl task owns
  them in the smoke manifest.

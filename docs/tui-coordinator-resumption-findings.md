# TUI Coordinator Resumption — Investigation Findings

## 1. Flow Diagram (ASCII)

### Session ID lifecycle for the `claude` executor (primary path):

```
TUI START
    │
    ├─ new VizApp() ──► restore_tui_state()                   [state.rs:11297]
    │                   reads .workgraph/tui-state.json         [state.rs:1665]
    │                   restores active_coordinator_id + tab    [state.rs:11298-11318]
    │
    ├─ ensure_user_coordinator()                                [state.rs:11659]
    │   may create new coordinator task in graph
    │   may override active_coordinator_id                      [state.rs:11714-11718]
    │
    └─ maybe_auto_enable_chat_pty()                             [state.rs:11346]
        │
        ├─ Resolve task_id = ".coordinator-{N}"                 [state.rs:11363]
        │  Resolve chat_ref = "coordinator-{N}"                 [state.rs:11364]
        │
        ├─ chat_dir = chat_dir_for_ref(wg_dir, chat_ref)       [state.rs:11385]
        │  (resolves alias via sessions.json → UUID dir)        [chat.rs:86-91]
        │
        ├─ Check session_lock::read_holder(chat_dir)            [state.rs:11386-11388]
        │  If daemon holds lock → request_release + SIGTERM     [state.rs:11411-11454]
        │
        ├─ [claude executor path]                               [state.rs:11515-11571]
        │   project_root = workgraph_dir.parent()               [state.rs:11531-11534]
        │   session_name = "wg-{project_tag}-{chat_ref}"        [state.rs:11545]
        │   has_prior = claude_has_session_for(&project_root)   [state.rs:11555]
        │   │                                                   [state.rs:14002-14018]
        │   │  (checks ~/.claude/projects/<cwd-slug>/*.jsonl)
        │   │  BUG: checks ANY session, not the named one ──────────────► ROOT CAUSE 1
        │   │
        │   ├─ has_prior=true:
        │   │   args = ["--continue", "-n", session_name, ...]  [state.rs:11561-11562]
        │   │   BUG: --continue resumes MOST RECENT session ───────────► ROOT CAUSE 2
        │   │   not the session named by -n
        │   │
        │   └─ has_prior=false:
        │       args = ["-n", session_name, "--system-prompt",..][state.rs:11556-11568]
        │       (fresh session — works correctly)
        │
        └─ PtyPane::spawn_in(bin, args, env, cwd)               [state.rs:11619-11626]


TUI EXIT
    │
    └─ save_all_chat_state()                                    [event.rs:150]
        ├─ writes chat-history-{N}.jsonl for each coordinator   [state.rs:10445-10461]
        └─ save_tui_state(wg_dir, active_id, tab)              [state.rs:10463-10467]
            writes .workgraph/tui-state.json                    [state.rs:1655-1662]
            (ONLY persists coordinator id + tab — no Claude session UUID)
```

### Session ID lifecycle for the `native` executor (wg nex):

```
TUI START → maybe_auto_enable_chat_pty()
    │
    ├─ Resolve chat_ref = "coordinator-{N}"
    ├─ chat_dir = chat_dir_for_ref(wg_dir, chat_ref)
    │  (resolves through sessions.json registry)
    ├─ Checks conversation.jsonl in chat_dir for resume
    └─ Spawns: wg nex --role coordinator [-m model] [-e endpoint]
       (interactive mode — no --chat, writes to stdout via PTY)
       Does NOT persist a session UUID that maps back to coordinator-N
```

### Session ID lifecycle for the daemon path (wg service start):

```
coordinator_agent.rs → supervisor loop
    │
    ├─ register_coordinator_session(dir, N)                     [coordinator_agent.rs:636]
    │  Installs aliases: "coordinator-N" + bare "N"             [chat_sessions.rs:306-328]
    │  Returns UUID (idempotent across restarts)
    │
    └─ Spawns: wg spawn-task .coordinator-{N}                   [coordinator_agent.rs:674-675]
       → resolve_handler() strips ".coordinator-" prefix        [spawn_task.rs:168]
       → chat_ref = "coordinator-{N}"                           [spawn_task.rs:168-169]
       → resume = conversation.jsonl exists in chat_dir         [spawn_task.rs:195]
```


## 2. Every File/Field That Stores a Coordinator Session Identifier

| Storage location | Field/key | What it holds | Scope |
|---|---|---|---|
| `.workgraph/tui-state.json` | `active_coordinator_id` | `u32` coordinator number (e.g. `0`, `1`) | Per-workgraph, TUI focus only |
| `.workgraph/tui-state.json` | `right_panel_tab` | Tab name string | Per-workgraph, TUI focus only |
| `.workgraph/chat/sessions.json` | `sessions[uuid].aliases[]` | `"coordinator-N"`, `"N"` | Per-workgraph, all executors |
| `.workgraph/chat/<uuid>/` | Directory existence | Chat session data dir (inbox, outbox, conversation, lock) | Per-workgraph, native executor |
| `.workgraph/chat-history-{N}.jsonl` | TUI chat history | TUI-layer chat messages (not Claude/nex session state) | Per-workgraph, TUI display only |
| `.workgraph/graph.jsonl` | `task.session_id` on `.coordinator-{N}` | Claude CLI session UUID (from stream Init event) | Per-workgraph, spawn/resume only |
| `.workgraph/service/coordinator-state-{N}.json` | Various coordinator runtime fields | Enabled, paused, model, executor overrides | Per-workgraph, daemon only |
| `~/.claude/projects/<cwd-slug>/*.jsonl` | Claude CLI's own session files | Claude's internal conversation data | Per-project-dir, Claude-global |
| In-memory `VizApp.coordinator_chats` | `HashMap<u32, ChatState>` | Per-coordinator chat messages, cursor, history | TUI runtime only |
| In-memory `VizApp.task_panes` | `HashMap<String, PtyPane>` keyed by `.coordinator-{N}` | Live PTY process | TUI runtime only |

## 3. Every Code Path That WRITES a Session Identifier

| # | File:line | What it writes | Trigger |
|---|---|---|---|
| W1 | `src/tui/viz_viewer/state.rs:1655-1662` | `tui-state.json` — active coordinator id + tab | TUI exit via `save_all_chat_state()` |
| W2 | `src/tui/viz_viewer/state.rs:11545` | `session_name = "wg-{project}-coordinator-{N}"` (ephemeral, passed as `-n` arg) | `maybe_auto_enable_chat_pty()` for claude executor |
| W3 | `src/chat_sessions.rs:153-193` | `sessions.json` — UUID + aliases + kind | `create_session()` / `ensure_session()` |
| W4 | `src/chat_sessions.rs:306-328` | `sessions.json` — `coordinator-N` + `N` aliases | `register_coordinator_session()` (daemon startup) |
| W5 | `src/chat_sessions.rs:493-514` | `sessions.json` — adds alias to existing entry | `add_alias()` |
| W6 | `src/commands/service/triage.rs:369-371` | `graph.jsonl` — `task.session_id` from stream Init event | Triage detects session_id from output |
| W7 | `src/commands/retry.rs:63` | `graph.jsonl` — clears `task.session_id = None` | `wg retry` |
| W8 | `src/commands/requeue.rs:59` | `graph.jsonl` — clears `task.session_id = None` | `wg requeue` |
| W9 | `src/stream_event.rs:277-280` | Parses `session_id` from Claude CLI `system` event JSON | Stream event translation |
| W10 | `src/tui/viz_viewer/state.rs:11272` | `active_coordinator_id` (in-memory) | `switch_coordinator()` |
| W11 | `src/tui/viz_viewer/state.rs:11706` | Creates new coordinator task in graph | `ensure_user_coordinator()` → `create_coordinator()` |

## 4. Every Code Path That READS a Session Identifier

| # | File:line | What it reads | Purpose |
|---|---|---|---|
| R1 | `src/tui/viz_viewer/state.rs:11298` | `tui-state.json` → `active_coordinator_id` | Restore TUI focus on startup |
| R2 | `src/tui/viz_viewer/state.rs:11555` | `claude_has_session_for()` checks `~/.claude/projects/<slug>/*.jsonl` | Decide `--continue` vs fresh session |
| R3 | `src/tui/viz_viewer/state.rs:11363-11364` | Derives `task_id` + `chat_ref` from `active_coordinator_id` | Build PTY spawn args |
| R4 | `src/tui/viz_viewer/state.rs:11385-11388` | `chat_dir_for_ref()` + `read_holder()` | Check if daemon holds session lock |
| R5 | `src/tui/viz_viewer/state.rs:6475` | `task.session_id.is_some()` | Show session info in Detail panel |
| R6 | `src/chat.rs:86-91` | `chat_sessions::resolve_ref()` → UUID dir | Resolve chat ref to filesystem dir |
| R7 | `src/commands/spawn_task.rs:168-169` | Strips `.coordinator-` prefix → `coordinator-{N}` | Build chat_ref for handler dispatch |
| R8 | `src/commands/spawn_task.rs:194-195` | `conversation.jsonl` existence in chat_dir | Decide `--resume` flag |
| R9 | `src/commands/spawn/execution.rs:216` | `task.session_id` | Claude `--resume <session_id>` for wait-resume |
| R10 | `src/commands/service/coordinator.rs:506,632` | `task.session_id` | Coordinator status display |
| R11 | `src/tui/viz_viewer/state.rs:11229-11246` | `load_persisted_chat_history_paginated()` from `chat-history-{N}.jsonl` | Restore TUI chat messages on coordinator switch |

## 5. Identified Root Causes (Ranked by Likelihood)

### Root Cause 1 (CRITICAL): `--continue` resumes the wrong session

**Evidence:** `src/tui/viz_viewer/state.rs:11561-11562`

When `has_prior` is true, the TUI spawns:
```
claude --continue -n wg-<project>-coordinator-<N> --dangerously-skip-permissions
```

The Claude CLI's `--continue` flag resumes **the most recently used session** in the project directory. It does NOT resume the session named by `-n`. The `-n` flag names the session for display in Claude's `/resume` picker — but when combined with `--continue`, the behavior is:

1. `--continue` finds the most recent session (any session, not necessarily one named `wg-...-coordinator-N`)
2. If the most recent session was from coordinator-0, but the user is now on coordinator-1, coordinator-1 incorrectly resumes coordinator-0's conversation
3. If the daemon ran its own session between TUI uses, `--continue` resumes the daemon's session instead

**Severity:** This is the primary cause of "sessions don't survive TUI restart." The session naming via `-n` is cosmetic — it doesn't control which session `--continue` picks.

### Root Cause 2 (HIGH): `claude_has_session_for()` checks ANY project session, not the named one

**Evidence:** `src/tui/viz_viewer/state.rs:14002-14018`

```rust
fn claude_has_session_for(cwd: &std::path::Path) -> bool {
    // ...
    let slug = cwd_str.replace('/', "-");
    let dir = home.join(".claude").join("projects").join(slug);
    entries.flatten().any(|e| e.path().extension().is_some_and(|x| x == "jsonl")
        && e.metadata().map(|m| m.len() > 0).unwrap_or(false))
}
```

This function checks if *any* `.jsonl` file exists in `~/.claude/projects/<slug>/`, not whether a session with the specific name `wg-<project>-coordinator-<N>` exists. Consequences:

- If coordinator-0 was used but coordinator-1 is new, `has_prior` returns true for coordinator-1 because coordinator-0's session file exists in the same project dir
- coordinator-1 then gets `--continue` instead of a fresh session, resuming coordinator-0's conversation

### Root Cause 3 (MEDIUM): No per-coordinator CWD for claude executor

**Evidence:** `src/tui/viz_viewer/state.rs:11531-11534`

The `claude` executor spawns all coordinators with the same CWD: `project_root` (parent of workgraph_dir). Claude's `--continue` looks up sessions scoped to the CWD. With all coordinators sharing the same CWD, there is no way to isolate their session histories — `--continue` picks the globally most-recent session regardless of which coordinator tab triggered it.

The `native` executor doesn't have this problem because it uses the `chat_sessions.json` registry (UUID-backed) to resolve sessions. The `codex` executor also uses CWD for session isolation but scopes it to `chat_dir` per coordinator (which is correct but different from claude's approach).

### Root Cause 4 (LOW): Missing Claude session UUID persistence

The `task.session_id` field on the graph captures the Claude session UUID from stream Init events (written by triage at `src/commands/service/triage.rs:369-371`). However, the TUI's `maybe_auto_enable_chat_pty` never reads this field. Even if the UUID were available, the claude CLI's interactive mode (`claude -n <name>`) has no `--resume <uuid>` flag — only `--continue` (most recent) or `--resume` (interactive picker, not scriptable).

### Root Cause 5 (LOW): TUI state persistence doesn't include session UUID

`tui-state.json` only stores `active_coordinator_id: u32` and `right_panel_tab: String`. It does NOT store:
- The Claude session UUID that was active for each coordinator
- Whether the session was fresh or continued
- The session name that was passed via `-n`

This means even if we could use `--resume <uuid>`, the TUI has no record of which UUID to resume.


## 6. Reproduction Recipe

### Prerequisites
- A workgraph project with `[coordinator] executor = "claude"` in `.workgraph/config.toml`
- Claude CLI installed and authenticated (`claude` on PATH)

### Steps

1. **Start the TUI with a fresh workgraph:**
   ```bash
   wg init my-test-project && cd my-test-project
   wg config --coordinator-executor claude
   wg tui
   ```

2. **Interact with coordinator-0:**
   - The Chat tab should spawn an embedded Claude session
   - Send a message: "Remember the word PINEAPPLE"
   - Wait for Claude to acknowledge

3. **Close the TUI:**
   - Press `q` or Ctrl-C to exit

4. **Reopen the TUI:**
   ```bash
   wg tui
   ```

5. **Verify the coordinator tab is restored:**
   - The correct coordinator tab should be focused (from tui-state.json — this part works)

6. **Verify session resumption FAILS:**
   - The Claude session in the Chat tab should NOT remember "PINEAPPLE"
   - Instead, it either:
     - (a) Starts a fresh session (if `claude_has_session_for` returns false — rare after first use)
     - (b) Resumes a different session than the one from step 2 (if other Claude activity happened in the same project dir between steps 3 and 4)

7. **Multi-coordinator failure (more severe):**
   ```bash
   # While in TUI, press '+' to create coordinator-1
   # Switch to coordinator-1, interact with it
   # Switch back to coordinator-0
   # Close TUI, reopen
   # Both coordinators will attempt --continue, but BOTH will resume
   # the SAME session (whichever was most recent in Claude's store)
   ```


## 7. Recommended Fix Sketch

### Option A: Use `--resume <session-id>` instead of `--continue` (Preferred)

**Layer:** `src/tui/viz_viewer/state.rs` — `maybe_auto_enable_chat_pty()` claude executor path

**Approach:**
1. After spawning a Claude PTY session, capture the session UUID from Claude's `system` event (emitted on session start as the first JSONL line to stdout). Store it in a new field in `tui-state.json` or in `coordinator-state-{N}.json`.
2. On TUI restart, read the stored UUID and pass `--resume <uuid>` instead of `--continue`.
3. If no stored UUID exists (first launch), start fresh with `-n <name> --system-prompt <prompt>`.
4. Remove `claude_has_session_for()` entirely — the per-coordinator UUID check replaces it.

**Complications:**
- The Claude CLI's `--resume` flag in interactive mode launches an interactive picker, not a direct-resume-by-UUID. The `--resume <uuid>` form only works with `--print` (non-interactive). This is a blocker for the PTY-embedded path.
- Alternative: use `--continue` with a **per-coordinator CWD** (Option B) to get per-coordinator session isolation.

### Option B: Per-coordinator CWD for Claude executor

**Layer:** `src/tui/viz_viewer/state.rs` — `maybe_auto_enable_chat_pty()` claude executor path

**Approach:**
1. Instead of using `project_root` as the CWD for all claude-executor coordinators, use a per-coordinator directory: `chat_dir` (already computed at line 11385).
2. This scopes Claude's `--continue` to per-coordinator session history.
3. Tradeoff: each coordinator needs its own Claude Code trust approval, which degrades first-launch UX. Mitigate by symlinking `.claude/settings.json` or using `--trust`.

### Option C: Hybrid — session UUID in `tui-state.json` + `--continue` with per-coord CWD

**Layer:** Both state persistence and spawn path

**Approach:**
1. Extend `PersistedTuiState` to include a `HashMap<u32, String>` mapping coordinator ID → Claude session UUID.
2. On spawn, if a UUID is stored, create a per-coordinator tmpdir, copy/symlink the UUID's session file there, and spawn Claude with that CWD + `--continue`.
3. This gives us per-coordinator isolation without per-coordinator trust prompts.

### Recommendation

**Option B is simplest and most robust.** The CWD trick lets Claude's own session management do the right thing. The trust-prompt issue can be addressed by passing `--dangerously-skip-permissions` (already done) — if Claude auto-trusts dirs with that flag, the UX cost is zero. If not, `--trust` can be added.

Regardless of which option is chosen, `claude_has_session_for()` must be scoped per-coordinator (check the coordinator-specific CWD, not the shared project root).

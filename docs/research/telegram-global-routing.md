# Global Telegram Bot: Shared Inbound Routing Across Repos

Design document for a single Telegram bot shared by multiple `wg service` daemons across different workgraph repos.

---

## Table of Contents

1. [Architecture Decision: Polling Model](#1-architecture-decision-polling-model)
2. [Routing State Schema](#2-routing-state-schema)
3. [Outbound Message Format](#3-outbound-message-format)
4. [Inbound Routing Algorithm](#4-inbound-routing-algorithm)
5. [Freestanding Message Routing](#5-freestanding-message-routing)
6. [Task-to-User Messaging (CLI Interface)](#6-task-to-user-messaging-cli-interface)
7. [Sequence Diagrams](#7-sequence-diagrams)
8. [Global Config](#8-global-config)

---

## 1. Architecture Decision: Polling Model

### Decision: File-lock leader election among service daemons

**Rejected alternatives:**

- **Global telegram-bridge daemon (separate process):** Adds operational complexity — users must remember to start a separate bridge daemon alongside `wg service`. If it crashes, all repos lose Telegram. Creates a management burden for a tool that emphasizes simplicity.

- **Webhook mode:** Requires exposing a port (ngrok, Cloudflare tunnel, or public IP). Most workgraph users run on laptops/dev machines where exposing a port is unacceptable. Also conflicts with multiple repos wanting the same webhook endpoint.

**Chosen approach: Poll-lock leader election**

The first `wg service` daemon to start acquires an exclusive file lock on `~/.config/workgraph/telegram-poll.lock`. That daemon becomes the **poll leader** — it runs the `getUpdates` long-polling loop and routes inbound messages to the correct repo via the shared routing state file. All other daemons are **outbound-only** — they can send messages but do not poll.

**Mechanics:**

1. On startup, each service daemon attempts `flock(LOCK_EX | LOCK_NB)` on `~/.config/workgraph/telegram-poll.lock`.
2. If the lock is acquired → this daemon is the poll leader. It spawns the `getUpdates` loop.
3. If the lock is already held → this daemon operates outbound-only.
4. If the poll leader exits (daemon stops, crashes), the OS releases the flock. The next daemon to attempt the lock wins. Daemons should re-attempt lock acquisition periodically (every 60s) so leadership transfers naturally.
5. The lock file contains the PID of the current leader for diagnostics: `echo $$ > telegram-poll.lock` before acquiring.

**Why this works:**

- Zero extra processes — poll leadership is embedded in the existing `wg service` daemon.
- Automatic failover — OS-level flock release on process exit.
- No coordination protocol — file locks are atomic and well-understood.
- Multiple repos can still send messages concurrently (outbound doesn't need the lock).
- The `getUpdates` offset is persisted in routing state (see §2), so a new leader continues from where the old one left off.

**Trade-off:** If the poll-leader repo's service daemon is stopped, there's a brief gap (up to 60s) before another daemon acquires the lock. This is acceptable for a notification system — messages queue on Telegram's servers and are fetched when the new leader starts polling.

---

## 2. Routing State Schema

### Location

`~/.config/workgraph/telegram-routing.json`

### Schema

```json
{
  "version": 1,
  "getUpdates_offset": 123456789,
  "last_active_project": "/home/user/project-alpha/.workgraph",
  "projects": {
    "/home/user/project-alpha/.workgraph": {
      "repo_name": "project-alpha",
      "last_activity": "2026-03-11T14:30:00Z",
      "pid": 12345
    },
    "/home/user/project-beta/.workgraph": {
      "repo_name": "project-beta",
      "last_activity": "2026-03-11T14:25:00Z",
      "pid": 12346
    }
  },
  "message_map": {
    "4401": {
      "project_dir": "/home/user/project-alpha/.workgraph",
      "task_id": "fix-auth-bug",
      "event_type": "task_failed",
      "timestamp": "2026-03-11T14:30:00Z"
    },
    "4402": {
      "project_dir": "/home/user/project-beta/.workgraph",
      "task_id": "deploy-staging",
      "event_type": "approval",
      "timestamp": "2026-03-11T14:25:00Z"
    }
  },
  "pending_replies": {
    "fix-auth-bug:/home/user/project-alpha/.workgraph": {
      "telegram_message_id": 4403,
      "task_id": "fix-auth-bug",
      "project_dir": "/home/user/project-alpha/.workgraph",
      "question": "Which auth provider should I use?",
      "asked_at": "2026-03-11T14:31:00Z",
      "timeout_seconds": 3600
    }
  }
}
```

### Field definitions

| Field | Type | Description |
|-------|------|-------------|
| `version` | `u32` | Schema version for forward-compat. Currently `1`. |
| `getUpdates_offset` | `i64` | Telegram offset for `getUpdates` continuity across leader transitions. |
| `last_active_project` | `String` | Absolute path to `.workgraph` dir of the most-recently-active project. Used for freestanding message routing. |
| `projects` | `Map<String, ProjectEntry>` | Registered projects keyed by `.workgraph` dir path. |
| `projects[].repo_name` | `String` | Short display name derived from repo directory (e.g., `project-alpha`). |
| `projects[].last_activity` | `String` | ISO 8601 timestamp of last outbound notification from this project. |
| `projects[].pid` | `u32` | PID of the service daemon for this project. Used to detect stale entries (check if PID is alive). |
| `message_map` | `Map<String, MessageEntry>` | Maps Telegram `message_id` (string key) → originating project and task. |
| `message_map[].project_dir` | `String` | Which project sent this message. |
| `message_map[].task_id` | `String` | Which task this message is about. |
| `message_map[].event_type` | `String` | What kind of notification (for context in routing). |
| `message_map[].timestamp` | `String` | When the outbound message was sent. |
| `pending_replies` | `Map<String, PendingReply>` | Tasks waiting for a user reply. Keyed by `task_id:project_dir`. |
| `pending_replies[].telegram_message_id` | `i64` | The Telegram message to watch for replies to. |
| `pending_replies[].question` | `String` | The question text (for display/diagnostics). |
| `pending_replies[].asked_at` | `String` | When the question was sent. |
| `pending_replies[].timeout_seconds` | `u64` | How long to wait before timing out. |

### Locking

The routing state file is accessed by multiple daemons concurrently (one writing outbound mappings, one reading for inbound routing). Use **advisory file locking** (`flock`) on the JSON file itself:

- Reads: `LOCK_SH` (shared lock) — multiple daemons can read simultaneously.
- Writes: `LOCK_EX` (exclusive lock) — only one daemon writes at a time.
- All locks are **non-blocking with retry** (3 attempts, 100ms sleep between).

### Garbage collection

The `message_map` grows unbounded. Entries older than 7 days should be pruned on each write. This is a simple filter during serialization.

The `projects` map should prune entries whose `pid` is no longer alive (checked via `kill(pid, 0)` / `/proc/$pid` existence).

---

## 3. Outbound Message Format

### Decision: Project-prefixed messages with task ID

Every outbound Telegram message includes a project identifier prefix so the user always knows which repo a notification came from.

### Format

**Plain text:**
```
[project-alpha] ❌ failed: fix-auth-bug — Build Frontend
Exit code 1: cargo test failed
```

**HTML (for Telegram's HTML parse mode):**
```html
<b>[project-alpha]</b> ❌ <b>failed</b>: <code>fix-auth-bug</code> — Build Frontend
Exit code 1: cargo test failed
```

### Structure

```
[{repo_name}] {emoji} {event_label}: {task_id} — {task_title}
{optional_detail}
```

Where:
- `repo_name` — from `projects[].repo_name` in routing state (derived from directory name, e.g., `basename $(dirname $project_dir)`)
- `emoji` — existing emoji mapping from `dispatch.rs` (`📋 ❌ 🚫 🔐 🚨`)
- `event_label` — `ready`, `failed`, `blocked`, `approval needed`, `URGENT`
- `task_id` — the workgraph task ID (in monospace/code format for Telegram)
- `task_title` — human-readable task title
- `detail` — optional extra context (failure reason, etc.)

### Implementation change

Modify `dispatch::format_event()` to accept an optional `repo_name: Option<&str>` parameter. When present, prepend `[{repo_name}]` to both plain and HTML output. The service daemon passes the repo name from the routing state.

---

## 4. Inbound Routing Algorithm

When the poll leader receives a Telegram update, it follows this algorithm:

```
fn route_inbound(update: TelegramUpdate, state: &RoutingState) -> RouteAction:
    if update.is_callback_query():
        // Button press — always has a reply_to message
        msg_id = update.callback_query.message.message_id
        if entry = state.message_map.get(msg_id):
            return Deliver(entry.project_dir, entry.task_id, CallbackAction(update.data))
        else:
            return Drop("callback for unknown message")

    if update.is_reply():
        // User replied to a specific message
        replied_to_id = update.message.reply_to_message.message_id

        // Check pending_replies first (task asking user a question)
        for (key, pending) in state.pending_replies:
            if pending.telegram_message_id == replied_to_id:
                return DeliverReply(pending.project_dir, pending.task_id, update.text)

        // Check message_map (user replying to a notification)
        if entry = state.message_map.get(replied_to_id):
            return Deliver(entry.project_dir, entry.task_id, TextMessage(update.text))

        return Drop("reply to unknown message")

    // Freestanding message (not a reply)
    return route_freestanding(update.text, state)
```

### Delivery mechanism

When the poll leader determines the target `(project_dir, task_id)`, it delivers the message by writing directly to the workgraph message queue:

```rust
// Poll leader delivers inbound message to the target project
workgraph::messages::send_message(
    &project_dir,           // e.g., "/home/user/project-alpha/.workgraph"
    &task_id,               // e.g., "fix-auth-bug"
    &update.text,           // message body
    "telegram:username",    // sender (prefixed with channel)
    "normal",               // priority
)?;
```

This works because the `.workgraph/messages/` directory is accessible to any process on the machine. The poll leader doesn't need IPC to the target repo's daemon — it writes directly to the message file using the same `flock`-based `send_message()` that `wg msg send` uses.

For callback queries (button presses), the poll leader also writes to the message queue with the action ID in the body (e.g., `action:approve:fix-auth-bug`).

---

## 5. Freestanding Message Routing

### Decision: Last-active project (primary) + `/project` command (fallback)

When the user sends a message that isn't a reply to any tracked notification:

**Primary: Route to last-active project.** The `last_active_project` field in routing state tracks which project most recently sent an outbound notification. Freestanding messages go to this project's coordinator task.

**Fallback: `/project <name>` command.** The user can explicitly switch context:
- `/project alpha` — sets `last_active_project` to the project matching "alpha"
- `/projects` — lists all active projects with their last activity time

**Why this combination:**

- **Option A (last-active) alone** is the right default 80% of the time — the user is probably responding to whatever they last saw. But it fails if the user wants to interact with a different project.
- **Option B (/project prefix) alone** is too much friction — every message needs a prefix.
- **Option C (inline keyboard picker)** adds latency and visual noise. Good for disambiguation but bad as a primary flow.
- **Option D (broadcast to all coordinators)** is chaotic — most coordinators would ignore the message, wasting tokens.

**Routing algorithm for freestanding messages:**

```
fn route_freestanding(text: &str, state: &RoutingState) -> RouteAction:
    // Check for /project command
    if text.starts_with("/project "):
        name = text[9..].trim()
        if project = find_project_by_name(state, name):
            state.last_active_project = project.dir
            return Info("Switched to project: {name}")
        else:
            return Info("Unknown project. Active projects: {list}")

    if text == "/projects":
        return Info(format_project_list(state))

    // Route to last-active project's coordinator
    if let Some(project_dir) = state.last_active_project:
        if is_pid_alive(state.projects[project_dir].pid):
            return DeliverToCoordinator(project_dir, text)

    // No active project — show picker
    active = state.projects.values().filter(|p| is_pid_alive(p.pid))
    if active.count() == 0:
        return Info("No active workgraph projects.")
    elif active.count() == 1:
        return DeliverToCoordinator(active[0].dir, text)
    else:
        return ShowProjectPicker(active, text)  // inline keyboard
```

`DeliverToCoordinator` sends the message to the coordinator task (typically the task with no dependencies, or a task tagged `coordinator`). Implementation: look for the active `InProgress` task in the project's graph whose `assigned` agent is the coordinator, or fall back to writing to a well-known `_coordinator` message queue.

---

## 6. Task-to-User Messaging (CLI Interface)

### `wg msg send --user "question"` — synchronous (blocking) mode

**Invocation:**
```bash
wg msg send <task-id> --user "Which auth provider should I use: OAuth2 or SAML?"
```

Or the shorthand:
```bash
wg ask "Which auth provider should I use: OAuth2 or SAML?"
```
(Infers the task ID from `$WG_TASK_ID` env var, set by the coordinator when spawning agents.)

**Behavior:**

1. **Send:** The CLI sends a Telegram message to the configured `chat_id`:
   ```
   [project-alpha] 💬 fix-auth-bug asks:
   Which auth provider should I use: OAuth2 or SAML?
   ```
   The message includes inline keyboard buttons if the question has structured options (detected via `--options "OAuth2,SAML"`).

2. **Register:** The CLI writes a `pending_replies` entry in `telegram-routing.json` with the Telegram `message_id`, `task_id`, `project_dir`, and timeout.

3. **Wait:** The CLI polls `wg msg read <task-id>` for a message from `telegram:*` sender. Poll interval: 2 seconds. This is a tight loop — the agent's process blocks here.

4. **Receive:** When the poll leader routes the user's reply back to the task's message queue (see §4), the CLI picks it up and prints it to stdout. The agent sees the user's response and continues.

5. **Timeout:** After `timeout_seconds` (default 3600s = 1 hour), the CLI returns with exit code 1 and a timeout message. The agent should handle this gracefully (e.g., pick a default, or `wg fail`).

6. **Cleanup:** On receive or timeout, remove the `pending_replies` entry.

### `wg msg send --user "info"` — asynchronous (fire-and-forget) mode

```bash
wg msg send <task-id> --user --async "FYI: deployment completed successfully"
```

Sends the Telegram message and returns immediately. No reply tracking. Useful for status updates that don't need a response.

### CLI flag summary

| Flag | Description |
|------|-------------|
| `--user` | Route this message to the human via Telegram (instead of to the task's agent message queue) |
| `--async` | Don't wait for a reply (fire-and-forget) |
| `--timeout <seconds>` | How long to wait for a reply (default: 3600) |
| `--options "A,B,C"` | Present structured choices as inline keyboard buttons |

### How the reply flows back

See sequence diagram §7.4. The poll leader receives the user's reply, matches it against `pending_replies` by `telegram_message_id`, and writes the reply text to the task's message queue via `send_message()`. The blocking `wg ask` command picks it up on its next poll cycle.

---

## 7. Sequence Diagrams

### 7.1 Outbound Alert (Task Failed)

```
Service Daemon (project-alpha)        Routing State File          Telegram API
         │                                    │                        │
         │  1. task "fix-auth" fails          │                        │
         │  2. format_event() with            │                        │
         │     repo_name="project-alpha"      │                        │
         │                                    │                        │
         │  3. LOCK_EX routing state          │                        │
         │──────────────────────────────────>  │                        │
         │                                    │                        │
         │  4. sendMessage(chat_id,           │                        │
         │     "[project-alpha] ❌ failed:     │                        │
         │      fix-auth — Build Frontend")   │                        │
         │─────────────────────────────────────────────────────────────>│
         │                                    │                        │
         │  5. response: message_id=4401      │                        │
         │<─────────────────────────────────────────────────────────────│
         │                                    │                        │
         │  6. write message_map[4401] =      │                        │
         │     {project_dir, task_id, ...}    │                        │
         │──────────────────────────────────>  │                        │
         │                                    │                        │
         │  7. update last_active_project     │                        │
         │     update projects[].last_activity│                        │
         │──────────────────────────────────>  │                        │
         │                                    │                        │
         │  8. UNLOCK routing state           │                        │
         │──────────────────────────────────>  │                        │
```

### 7.2 Reply Routing (User Replies to Notification)

```
Telegram API          Poll Leader Daemon          Routing State       Target Project
     │                       │                         │             (project-alpha)
     │                       │                         │                    │
     │  1. getUpdates        │                         │                    │
     │  reply_to_msg=4401    │                         │                    │
     │  text="Try OAuth2"    │                         │                    │
     │<──────────────────────│                         │                    │
     │                       │                         │                    │
     │                       │  2. LOCK_SH state       │                    │
     │                       │────────────────────────>│                    │
     │                       │                         │                    │
     │                       │  3. lookup message_map  │                    │
     │                       │     [4401] →            │                    │
     │                       │     project-alpha,      │                    │
     │                       │     fix-auth            │                    │
     │                       │<────────────────────────│                    │
     │                       │                         │                    │
     │                       │  4. UNLOCK state        │                    │
     │                       │────────────────────────>│                    │
     │                       │                         │                    │
     │                       │  5. send_message(       │                    │
     │                       │     project-alpha/      │                    │
     │                       │     .workgraph,         │                    │
     │                       │     "fix-auth",         │                    │
     │                       │     "Try OAuth2",       │                    │
     │                       │     "telegram:user")    │                    │
     │                       │────────────────────────────────────────────>│
     │                       │                         │                    │
     │                       │                         │        6. message  │
     │                       │                         │        appears in  │
     │                       │                         │        fix-auth's  │
     │                       │                         │        queue       │
     │                       │                         │                    │
     │                       │                         │        7. agent    │
     │                       │                         │        picks up    │
     │                       │                         │        via wg msg  │
     │                       │                         │        read        │
```

### 7.3 Freestanding Message (User Sends Unprompted Message)

```
Telegram API          Poll Leader Daemon          Routing State       Target Project
     │                       │                         │             (last-active)
     │                       │                         │                    │
     │  1. getUpdates        │                         │                    │
     │  no reply_to          │                         │                    │
     │  text="How is the     │                         │                    │
     │   deploy going?"      │                         │                    │
     │<──────────────────────│                         │                    │
     │                       │                         │                    │
     │                       │  2. LOCK_SH state       │                    │
     │                       │────────────────────────>│                    │
     │                       │                         │                    │
     │                       │  3. not a reply →       │                    │
     │                       │     check /project cmd  │                    │
     │                       │     → not a command     │                    │
     │                       │                         │                    │
     │                       │  4. get last_active     │                    │
     │                       │     _project →          │                    │
     │                       │     project-alpha       │                    │
     │                       │<────────────────────────│                    │
     │                       │                         │                    │
     │                       │  5. check PID alive     │                    │
     │                       │     → yes               │                    │
     │                       │                         │                    │
     │                       │  6. UNLOCK state        │                    │
     │                       │────────────────────────>│                    │
     │                       │                         │                    │
     │                       │  7. deliver to          │                    │
     │                       │     coordinator task    │                    │
     │                       │     in project-alpha    │                    │
     │                       │────────────────────────────────────────────>│
     │                       │                         │                    │
     │                       │                         │        8. coord    │
     │                       │                         │        receives    │
     │                       │                         │        user msg    │
```

### 7.4 Task Asking User a Question (Synchronous `wg ask`)

```
Agent (fix-auth)     wg CLI        Telegram API     Routing State    Poll Leader     User
     │                 │                │                │               │             │
     │  1. wg ask      │                │                │               │             │
     │  "OAuth2 or     │                │                │               │             │
     │   SAML?"        │                │                │               │             │
     │────────────────>│                │                │               │             │
     │                 │                │                │               │             │
     │                 │  2. sendMessage │                │               │             │
     │                 │  "[project-α]  │                │               │             │
     │                 │  💬 fix-auth   │                │               │             │
     │                 │   asks: ..."   │                │               │             │
     │                 │───────────────>│                │               │             │
     │                 │                │                │               │             │
     │                 │  3. msg_id=4403│                │               │             │
     │                 │<───────────────│                │               │             │
     │                 │                │                │               │             │
     │                 │  4. LOCK_EX    │                │               │             │
     │                 │  write pending │                │               │             │
     │                 │  _replies      │                │               │             │
     │                 │───────────────────────────────>│               │             │
     │                 │                │                │               │             │
     │                 │  5. poll loop: │                │               │             │
     │                 │  wg msg read   │                │               │             │
     │                 │  fix-auth      │                │               │             │
     │                 │  (every 2s)    │                │               │             │
     │                 │                │                │               │             │
     │                 │                │  6. user sees  │               │             │
     │                 │                │  question on   │               │             │
     │                 │                │  phone         │               │             │
     │                 │                │────────────────────────────────────────────>│
     │                 │                │                │               │             │
     │                 │                │  7. user       │               │             │
     │                 │                │  replies       │               │             │
     │                 │                │  "Use OAuth2"  │               │             │
     │                 │                │  (reply to     │               │             │
     │                 │                │   msg 4403)    │               │             │
     │                 │                │<────────────────────────────────────────────│
     │                 │                │                │               │             │
     │                 │                │  8. getUpdates │               │             │
     │                 │                │───────────────────────────────>│             │
     │                 │                │                │               │             │
     │                 │                │                │  9. match     │             │
     │                 │                │                │  pending      │             │
     │                 │                │                │  _replies     │             │
     │                 │                │                │<──────────────│             │
     │                 │                │                │               │             │
     │                 │                │                │  10. deliver  │             │
     │                 │                │                │  to fix-auth  │             │
     │                 │                │                │  msg queue    │             │
     │                 │                │                │───────────────│             │
     │                 │                │                │               │             │
     │                 │  11. poll      │                │               │             │
     │                 │  picks up msg  │                │               │             │
     │                 │  "Use OAuth2"  │                │               │             │
     │                 │                │                │               │             │
     │  12. stdout:    │                │                │               │             │
     │  "Use OAuth2"   │                │               │               │             │
     │<────────────────│                │                │               │             │
     │                 │                │                │               │             │
     │  13. agent      │  14. cleanup   │                │               │             │
     │  continues      │  pending_reply │                │               │             │
     │  with answer    │───────────────────────────────>│               │             │
```

---

## 8. Global Config

### No per-repo bot tokens required

The Telegram bot token and chat ID live exclusively in the global config:

```toml
# ~/.config/workgraph/notify.toml

[routing]
default = ["telegram"]

[telegram]
bot_token = "123456:ABC-DEF..."
chat_id = "12345678"
```

Per-repo `.workgraph/notify.toml` files may override routing rules (e.g., which event types trigger notifications) but **never** need to specify `bot_token` or `chat_id`. The service daemon resolves Telegram config by:

1. Check per-repo `.workgraph/notify.toml` for a `[telegram]` section.
2. If absent, fall back to `~/.config/workgraph/notify.toml`.
3. The `bot_token` and `chat_id` from whichever file is found are used.

This is already how `NotifyConfig::load()` works (see `config.rs:111-119`). No changes needed for config resolution.

### Routing state is always global

The `telegram-routing.json` file is always at `~/.config/workgraph/telegram-routing.json`. This is never per-repo — it's the shared coordination point.

### Per-repo display name

Each repo registers itself in the routing state's `projects` map when its service daemon starts. The `repo_name` is derived automatically from the directory name:

```rust
let repo_name = project_dir
    .parent()  // .workgraph → project root
    .and_then(|p| p.file_name())
    .map(|n| n.to_string_lossy().to_string())
    .unwrap_or_else(|| "unknown".to_string());
```

Users can override this with a `display_name` field in `.workgraph/config.toml` if their directory name isn't descriptive.

---

## Implementation Notes

### Files to modify

| File | Change |
|------|--------|
| `src/notify/telegram.rs` | Add `register_outbound()` method that writes to `telegram-routing.json` after sending. Add `poll_leader_loop()` that does `getUpdates` + inbound routing. |
| `src/notify/dispatch.rs` | Add `repo_name` parameter to `format_event()`. |
| `src/commands/service/mod.rs` | On startup: attempt poll lock, register project in routing state. In `try_dispatch_notifications()`: call `register_outbound()` after sending. |
| `src/commands/telegram.rs` | Refactor `run_listen()` to use the new poll-leader logic instead of standalone polling. |
| `src/cli.rs` / `src/main.rs` | Add `wg ask` subcommand. Add `--user` and `--async` flags to `wg msg send`. |
| New: `src/notify/telegram_routing.rs` | Routing state structs, load/save with flock, GC logic. |

### Testing strategy

- Unit tests: routing state serialization/deserialization, GC of stale entries, inbound routing algorithm (reply matching, freestanding routing, `/project` command parsing).
- Integration test: two service daemons registering in routing state, one acquiring poll lock.
- Manual test: send notification from two repos, reply to each, verify routing.

### Migration

No migration needed — `telegram-routing.json` is created on first use. Existing `notify.toml` configs continue to work unchanged.

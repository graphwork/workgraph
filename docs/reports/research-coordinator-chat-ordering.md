# Research: Coordinator Chat Message Ordering and Delivery

**Task:** `research-coordinator-chat`
**Date:** 2026-04-02
**Status:** Complete

---

## 1. Current Message Flow

```
User types in TUI chat
       │
       ▼
send_chat_message()                    [src/tui/viz_viewer/state.rs:9957]
  ├─ Adds user message to self.chat.messages (display list) immediately
  ├─ Saves chat history to disk
  ├─ Sets awaiting_response = true, last_request_id = <tui-...>
  └─ Spawns background thread: `wg chat <text> --coordinator N`
              │
              ▼
run_send()                             [src/commands/chat.rs:51]
  ├─ Generates request_id (chat-...)
  ├─ Sends IPC UserChat to daemon
  │         │
  │         ▼
  │   handle_connection()              [src/commands/service/ipc.rs:530]
  │     └─ IpcRequest::UserChat        [src/commands/service/ipc.rs:420]
  │       ├─ append_chat_inbox()       [src/commands/service/ipc.rs:1112]
  │       │    └─ Writes to .workgraph/chat/{cid}/inbox.jsonl
  │       ├─ Sets urgent_wake = true
  │       └─ Pushes cid to pending_coordinator_ids
  │
  └─ Blocks on wait_for_response_for() [src/chat.rs:445]
       (polls outbox.jsonl every 200ms for matching request_id, 120s timeout)
              │
              ▼
Daemon main loop wakes (urgent_wake)   [src/commands/service/mod.rs:2609]
  └─ route_chat_to_all_agents()        [src/commands/service/mod.rs:2660]
       └─ route_chat_to_agent()        [src/commands/service/mod.rs:1100]
         ├─ Reads inbox since coordinator cursor
         ├─ agent.send_message(request_id, content) → mpsc channel
         └─ Advances coordinator cursor
              │
              ▼
agent_thread_main() inner loop         [src/commands/service/coordinator_agent.rs:604]
  ├─ rx.recv_timeout(5s) → gets ChatRequest
  ├─ build_coordinator_context()       [context injection]
  ├─ Writes stream-json to Claude CLI stdin
  └─ collect_response()                [src/commands/service/coordinator_agent.rs:1147]
       ├─ Reads stdout events (text, tool_use, tool_result, turn_complete)
       ├─ Writes streaming partial text to .streaming file
       └─ On TurnComplete:
            ├─ append_outbox_full_for() [src/chat.rs:401]
            └─ clear_streaming()
              │
              ▼
TUI poll_chat_messages()               [src/tui/viz_viewer/state.rs:9756]
  ├─ Reads .streaming file → updates streaming_text
  ├─ Reads outbox since outbox_cursor
  ├─ Pushes new coordinator messages to self.chat.messages
  ├─ Clears awaiting_response on ANY new outbox message
  └─ Updates outbox_cursor
```

### Key Cursors and State

| Cursor/State | Location | Purpose |
|---|---|---|
| `.coordinator-cursor` | `.workgraph/chat/{cid}/` | Last inbox message ID processed by coordinator agent |
| `.cursor` | `.workgraph/chat/{cid}/` | Last outbox message ID read by CLI/TUI |
| `outbox_cursor` | TUI in-memory (`self.chat.outbox_cursor`) | Last outbox ID polled by TUI |
| `awaiting_response` | TUI in-memory (`self.chat.awaiting_response`) | Whether TUI is waiting for a response |
| `last_request_id` | TUI in-memory (`self.chat.last_request_id`) | The request_id of the pending message |

---

## 2. Problem 1: User Messages Float Above Incoming Chunks

### Symptom
When the user sends a message while the coordinator is actively processing/delivering a response, the user's message appears ABOVE the chunk of messages being delivered, rather than at the correct chronological position.

### Root Cause

**File:** `src/tui/viz_viewer/state.rs:9978`

In `send_chat_message()` at line 9978, the user message is pushed **immediately** to `self.chat.messages`:

```rust
// Add user message to display immediately.
self.chat.messages.push(ChatMessage {
    role: ChatRole::User,
    text: text.clone(),
    ...
});
```

This happens **before** the background `wg chat` command runs and before IPC reaches the daemon. Meanwhile, `poll_chat_messages()` at line 9789 **also appends** coordinator messages to the end of `self.chat.messages`:

```rust
self.chat.messages.push(ChatMessage {
    role: ChatRole::Coordinator,
    text: msg.content.clone(),
    ...
});
```

The sequence when the user sends during an active response:

1. Coordinator response R1 is still being generated (streaming)
2. User types and sends message M2 → pushed to `self.chat.messages` immediately (line 9978)
3. R1 finishes → `poll_chat_messages()` appends R1 to `self.chat.messages` (line 9789)
4. Display order: `[..., M2, R1]` — **M2 appears above R1**

The user's message should appear **after** R1 (the response to their previous message) in the display, since the coordinator hasn't seen M2 yet.

### Additional Factor

The `awaiting_response` flag is cleared on **any** new outbox message (line 9835-9839):
```rust
if self.chat.awaiting_response {
    self.chat.awaiting_response = false;
    ...
}
```

This uses no request_id matching. The comment at line 9833 acknowledges this:
```rust
// The TUI request_id ("tui-...") differs from wg chat's ("chat-..."),
// so we clear on any new outbox message rather than matching by ID.
```

### Proposed Fix (Minimal)

In `send_chat_message()`, instead of pushing the user message immediately, store it in a **pending buffer** (e.g., `self.chat.pending_user_messages`). In `poll_chat_messages()`, after appending all new coordinator messages (line 9789-9802), flush the pending buffer to the display list. This ensures user messages always appear **after** the coordinator messages that were already in-flight.

Alternatively, attach timestamps to all messages and sort the display list by timestamp after each poll. The user message gets `chrono::Utc::now()` (line 9987), and coordinator messages carry their `msg.timestamp` from the outbox. A stable sort by timestamp would place them in chronological order.

The simplest surgical fix: in `send_chat_message()`, if `awaiting_response` is true, defer the push to a `Vec<ChatMessage>` field like `self.chat.queued_user_messages`. Then at the top of `poll_chat_messages()`, after appending coordinator messages, drain the queue:

```rust
// After appending coordinator messages (after line 9802):
for queued in self.chat.queued_user_messages.drain(..) {
    self.chat.messages.push(queued);
}
```

---

## 3. Problem 2: Race Condition — Missed Response Trigger

### Symptom
When the user sends a message while a response to a previous message is being delivered, the second message gets "lost" — no response is generated for it.

### Root Cause

**File:** `src/tui/viz_viewer/state.rs:10004` and `src/tui/viz_viewer/state.rs:9835`

The race condition involves three interacting mechanisms:

**Step 1:** User sends message M1. TUI sets:
```rust
self.chat.awaiting_response = true;       // line 10004
self.chat.last_request_id = Some(rid1);   // line 10006
```
And spawns `wg chat M1` in a background thread with `CommandEffect::ChatResponse(rid1)`.

**Step 2:** While M1 is being processed by the coordinator, user sends M2. TUI sets:
```rust
self.chat.awaiting_response = true;       // line 10004 (already true)
self.chat.last_request_id = Some(rid2);   // line 10006 — OVERWRITES rid1
```
And spawns `wg chat M2` with `CommandEffect::ChatResponse(rid2)`.

**Step 3:** R1 (response to M1) arrives in the outbox. `poll_chat_messages()` detects it:
```rust
if self.chat.awaiting_response {          // line 9835
    self.chat.awaiting_response = false;  // line 9836 — CLEARED
    self.chat.last_request_id = None;     // line 9838 — CLEARED
}
```

**Step 4:** The `wg chat M1` background command completes (success). `drain_commands()` processes `CommandEffect::ChatResponse(rid1)`:
```rust
// line 7971-7978
self.poll_chat_messages();  // No new messages — R1 already consumed
// awaiting_response is already false, so nothing further happens
```

**Step 5:** The `wg chat M2` background command completes (success). `drain_commands()` processes `CommandEffect::ChatResponse(rid2)`:
```rust
// line 7971-7978
self.poll_chat_messages();  // Finds R2 if it arrived, or finds nothing
```

The **critical race** is between steps 3 and 5. If R1 and R2 arrive close together:
- `poll_chat_messages()` at step 3 picks up R1, clears `awaiting_response`
- The `wg chat M2` command at step 5 calls `poll_chat_messages()` which picks up R2

This path actually **works** in most cases. The true "lost message" scenario is different:

**The actual lost-trigger scenario:**

The `wg chat` CLI at `src/commands/chat.rs:123` calls `wait_for_response_for()` which polls the outbox for a matching `request_id`. The `wg chat M1` blocks until it finds `request_id=chat-{M1}` in the outbox. Meanwhile, `wg chat M2` **also** blocks waiting for `request_id=chat-{M2}`.

The coordinator agent processes messages **sequentially** through an `mpsc::channel` (`src/commands/service/coordinator_agent.rs:606`). It processes M1, writes R1 to outbox, then processes M2, writes R2 to outbox. Both `wg chat` processes eventually return.

However, the race is in the **daemon's routing layer** at `src/commands/service/mod.rs:1100-1143`:

```rust
fn route_chat_to_agent(...) -> Result<usize> {
    let inbox_cursor = chat::read_coordinator_cursor_for(dir, coordinator_id)?;
    let new_messages = chat::read_inbox_since_for(dir, coordinator_id, inbox_cursor)?;
    ...
    for msg in &new_messages {
        agent.send_message(msg.request_id.clone(), msg.content.clone())?;
    }
    // Advance cursor past ALL messages
    if let Some(last) = new_messages.last() {
        chat::write_coordinator_cursor_for(dir, coordinator_id, last.id)?;
    }
}
```

This is called **once per urgent_wake**. If both M1 and M2 arrive before the daemon processes the wake, both get routed in a single call — this works fine. But if M1's IPC triggers urgent_wake, the daemon routes M1 and advances the cursor, then M2's IPC triggers a **second** urgent_wake — this also works because `route_chat_to_agent` re-reads the inbox from the updated cursor.

**The real race is in the TUI's `awaiting_response` state machine**, not in the daemon:

When R1 arrives, `poll_chat_messages()` clears `awaiting_response`. If M2 was already sent (background thread spawned), the TUI **stops polling actively** for R2. The TUI only polls chat messages when:
1. `awaiting_response` is true (line 5389, 5596, 5754)  
2. `right_panel_tab == RightPanelTab::Chat` (line 5511, 5596)
3. File watcher detects outbox change (line 5520-5524)

If the user is on the Chat tab (condition 2), polling continues on the 1-second refresh interval (line 5596). If not, R2 gets picked up only when the file watcher fires (condition 3) or when the user switches back to the Chat tab.

**The actual "lost message" scenario occurs when:**
1. User sends M2 while coordinator is processing M1
2. `awaiting_response` was already true from M1, so M2's `send_chat_message` sets it again (no-op since it's already true)
3. R1 arrives → `awaiting_response` cleared
4. `wg chat M2` background command hasn't completed yet
5. R2 arrives in outbox while the user is NOT on the Chat tab
6. `awaiting_response` is false, so the fast streaming poll path (100ms, line 5832) is inactive
7. No fs_watcher event fires (or it's debounced)
8. R2 sits unpolled until the next 1-second refresh cycle happens to poll chat

This isn't truly "lost" — it's **delayed delivery with no indication**. But there's a worse scenario:

9. The user sends M2 and R1 hasn't arrived yet
10. `send_chat_message` sets `last_request_id = rid2` (overwriting rid1)
11. `wg chat M1` completes (success) → `CommandEffect::ChatResponse(rid1)` 
12. `drain_commands` at line 7935: calls `poll_chat_messages()` which picks up R1
13. `poll_chat_messages` clears `awaiting_response` (line 9836) — **but M2 is still in flight**
14. `wg chat M2` completes later → `CommandEffect::ChatResponse(rid2)`
15. `drain_commands` calls `poll_chat_messages()` again — picks up R2 if present

So the flow actually works correctly in most cases because `CommandEffect::ChatResponse` always calls `poll_chat_messages()`. The true failure mode is when:
- The coordinator agent **crashes** during M2 processing
- R1's arrival clears `awaiting_response`
- `wg chat M2` times out (120s) or errors
- The ChatResponse error handler at line 7966 checks `self.chat.last_request_id == Some(rid2)` but `last_request_id` was already cleared by R1's poll

**The fundamental issue**: `awaiting_response` is a **singleton boolean** tracking a **single pending request**, but the system can have **multiple concurrent requests in flight** (one per `wg chat` background thread).

### Proposed Fix (Minimal)

Replace the singleton `awaiting_response` / `last_request_id` with a **set of pending request IDs**:

```rust
// In ChatState, replace:
//   pub awaiting_response: bool,
//   pub last_request_id: Option<String>,
// With:
pub pending_request_ids: HashSet<String>,
```

Then:
- `send_chat_message` inserts the new request_id into the set
- `poll_chat_messages` removes matched request_ids from the set (match by outbox `request_id`)
- `awaiting_response` becomes `!self.chat.pending_request_ids.is_empty()`
- `CommandEffect::ChatResponse(rid)` error handler removes just `rid` from the set

This is a 3-file change touching only the TUI state, not the daemon or chat storage.

---

## 4. Summary of Root Causes

| Problem | Root Cause | Location |
|---|---|---|
| **P1: Message ordering** | User message pushed to display list immediately; coordinator response appended later → wrong order | `src/tui/viz_viewer/state.rs:9978` (immediate push) and `src/tui/viz_viewer/state.rs:9789` (append on poll) |
| **P2: Missed response** | Singleton `awaiting_response` boolean tracks only one request; second send overwrites first's tracking state | `src/tui/viz_viewer/state.rs:10004-10006` (overwrite) and `src/tui/viz_viewer/state.rs:9835-9839` (clear on any response) |

Both problems are **TUI-side display/state issues**, not daemon or storage bugs. The daemon and coordinator agent handle concurrent messages correctly via the mpsc channel queue and cursor-based inbox reading.

---

## 5. Proposed Fixes (Summary)

### Fix for P1: Deferred display insertion

**Scope:** `src/tui/viz_viewer/state.rs` only

Add a `queued_user_messages: Vec<ChatMessage>` field to `ChatState`. In `send_chat_message()`, when `awaiting_response` is true, push to the queue instead of the main messages list. In `poll_chat_messages()`, after appending coordinator messages, drain the queue. When `awaiting_response` is false, push directly as before (current behavior for non-overlapping sends).

**Files to modify:** `src/tui/viz_viewer/state.rs` (struct definition + 2 methods)

### Fix for P2: Multi-request tracking

**Scope:** `src/tui/viz_viewer/state.rs` only

Replace `awaiting_response: bool` + `last_request_id: Option<String>` with `pending_request_ids: HashSet<String>`. Update all sites that read/write these fields. The `awaiting_response` concept becomes a derived property: `!pending_request_ids.is_empty()`.

**Files to modify:** `src/tui/viz_viewer/state.rs` (struct definition + ~8 call sites)

### Combined estimate: ~50 lines changed, 1 file

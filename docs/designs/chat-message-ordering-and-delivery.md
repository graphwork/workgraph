# Design: Chat Message Ordering and Delivery Fixes

**Task:** `design-chat-message`
**Date:** 2026-04-02
**Based on:** `research-coordinator-chat` ([research report](../reports/research-coordinator-chat-ordering.md))

---

## Overview

Two TUI-side bugs in the coordinator chat system require surgical fixes:

1. **P1 — Message ordering:** User messages appear above in-flight coordinator responses
2. **P2 — Race condition:** Singleton `awaiting_response` boolean cannot track multiple concurrent requests, causing missed response delivery

Both problems live entirely in `src/tui/viz_viewer/state.rs`. The daemon, coordinator agent, inbox/outbox storage, and IPC layer are all correct.

---

## Fix 1: Deferred User Message Display Insertion

### Problem

When the user sends a message while the coordinator is mid-response, `send_chat_message()` (line 9978) pushes the user message to `self.chat.messages` **immediately**. The coordinator's response, which was generated before the user's new message was even sent, arrives later via `poll_chat_messages()` (line 9789) and gets appended **after** the user message. The display shows `[User M2, Coordinator R1]` when chronologically it should be `[Coordinator R1, User M2]`.

### Design

Introduce a **deferred insertion queue** for user messages sent while a response is in flight.

#### Data structure change

Add one field to `ChatState` (line 985):

```rust
pub struct ChatState {
    // ... existing fields ...
    /// User messages waiting to be placed after in-flight coordinator responses complete.
    pub queued_user_messages: Vec<ChatMessage>,
}
```

Initialize to `Vec::new()` in `Default for ChatState` (line 1077).

#### Changes to `send_chat_message()` (line 9957)

**Current** (line 9978): Always pushes user message to `self.chat.messages` immediately.

**New logic**: Check whether any request is in flight. If so, queue; otherwise push directly.

```rust
// In send_chat_message(), replace the immediate push at line 9978:
let user_msg = ChatMessage {
    role: ChatRole::User,
    text: text.clone(),
    full_text: None,
    attachments: att_names,
    edited: false,
    inbox_id: None,
    user: Some(workgraph::current_user()),
    target_task: None,
    msg_timestamp: Some(chrono::Utc::now().to_rfc3339()),
    read_at: None,
    msg_queue_id: None,
};

if !self.chat.pending_request_ids.is_empty() {
    // A response is in flight — defer display until coordinator messages land.
    self.chat.queued_user_messages.push(user_msg);
} else {
    // No in-flight requests — display immediately (current behavior).
    self.chat.messages.push(user_msg);
}
```

**Rationale:** When no request is in flight, the user sees their message instantly (preserving the current snappy feel). When a response is in flight, the user message is held until after the coordinator response arrives — matching the actual processing order.

#### Changes to `poll_chat_messages()` (line 9756)

After appending coordinator messages (after line 9802), flush the deferred queue:

```rust
// After the `for msg in &new_msgs { ... }` loop (after line 9802):
// Flush deferred user messages — they now appear after the coordinator
// response that was in flight when the user sent them.
for queued in self.chat.queued_user_messages.drain(..) {
    self.chat.messages.push(queued);
}
```

This must happen **before** `poll_interleaved_messages()` (line 9805) and **before** `save_chat_history_with_skip()` (line 9808) so that the flushed messages are included in persistence.

#### Feedback: What the user sees during deferral

While a message is queued, the user might think their send failed. To address this, the queued message should still appear in the chat display **but visually distinguished** — e.g., shown with a dim/italic style and a "pending" indicator. This is a rendering concern in `render.rs`, not a state logic issue.

**Minimal approach (recommended):** Always push to `self.chat.messages` immediately as today (keeping visual feedback instant), but **also** track which messages are deferred. Then in `poll_chat_messages()`, when coordinator messages arrive, **reorder**: move the deferred user messages after the newly arrived coordinator messages.

```rust
// Alternative: track deferred message indices instead of a separate queue.
// In send_chat_message():
self.chat.messages.push(user_msg);  // always push for immediate display
if !self.chat.pending_request_ids.is_empty() {
    let idx = self.chat.messages.len() - 1;
    self.chat.deferred_user_indices.push(idx);
}

// In poll_chat_messages(), after appending coordinator messages:
// Reorder: move deferred user messages to after the new coordinator messages.
if !self.chat.deferred_user_indices.is_empty() && !new_msgs.is_empty() {
    // Extract deferred messages (in reverse index order to avoid shifting).
    let mut deferred: Vec<ChatMessage> = Vec::new();
    for &idx in self.chat.deferred_user_indices.iter().rev() {
        if idx < self.chat.messages.len() {
            deferred.push(self.chat.messages.remove(idx));
        }
    }
    deferred.reverse();
    self.chat.messages.extend(deferred);
    self.chat.deferred_user_indices.clear();
}
```

**Chosen approach: Index-tracking reorder** — This preserves the instant visual feedback the user currently gets (message appears immediately), while ensuring correct final ordering once the coordinator response arrives. The message "slides down" past the coordinator response, which is the natural and expected behavior.

### Sequence Diagram — Corrected Flow (P1)

```
User              TUI State               Coordinator
  │                  │                        │
  │  send M1         │                        │
  ├─────────────────►│ push M1 to messages    │
  │                  │ add rid1 to pending_ids │
  │                  │ spawn wg-chat M1       │
  │                  │───────────────────────►│
  │                  │                        │ processing M1...
  │  send M2         │                        │
  ├─────────────────►│ push M2 to messages    │
  │                  │ track idx as deferred   │
  │                  │ add rid2 to pending_ids │
  │                  │ spawn wg-chat M2       │
  │                  │───────────────────────►│
  │                  │                        │
  │                  │    R1 arrives in outbox │
  │                  │◄───────────────────────│
  │                  │ poll: append R1         │
  │                  │ reorder: move M2 after R1
  │                  │ remove rid1 from set    │
  │                  │                        │
  │  Display:        │                        │
  │  [..., M1, R1, M2]                       │
  │                  │    R2 arrives in outbox │
  │                  │◄───────────────────────│
  │                  │ poll: append R2         │
  │                  │ remove rid2 from set    │
  │                  │                        │
  │  Display:        │                        │
  │  [..., M1, R1, M2, R2]                   │
```

### Functions Modified (P1)

| Function | File:Line | Change |
|---|---|---|
| `ChatState` struct | `state.rs:985` | Add `deferred_user_indices: Vec<usize>` field |
| `Default for ChatState` | `state.rs:1077` | Initialize `deferred_user_indices: Vec::new()` |
| `send_chat_message()` | `state.rs:9957` | After push, if pending requests exist, record index in `deferred_user_indices` |
| `poll_chat_messages()` | `state.rs:9756` | After appending coordinator messages, reorder deferred messages to end |

**Total: 4 functions, ~15 lines added.**

### Markdown Rendering Preservation

This fix does not change `ChatMessage` fields, `ChatRole` variants, or any rendering logic. Messages have exactly the same structure as before. The only change is their **position** in the `Vec<ChatMessage>` — they get reordered. The markdown rendering pipeline in `render.rs` iterates `self.chat.messages` in order, so it will render the corrected order identically.

---

## Fix 2: Multi-Request Tracking

### Problem

`ChatState` uses a singleton `awaiting_response: bool` and `last_request_id: Option<String>` to track one pending request. When a second message is sent before the first response arrives:

1. `last_request_id` is overwritten (line 10006) — the first request ID is lost
2. When R1 arrives, `awaiting_response` is cleared (line 9836) — the system forgets M2 is still in flight
3. Streaming poll drops from 100ms to 1000ms, delayed delivery for R2
4. Error handler at line 7966 checks `last_request_id == Some(rid)` — after overwrite, error on rid1 is silently ignored

### Design

Replace the singleton state with a **set of pending request IDs**.

#### Data structure change

In `ChatState` (line 985):

```rust
pub struct ChatState {
    // REMOVE these two fields:
    //   pub awaiting_response: bool,
    //   pub last_request_id: Option<String>,
    //   pub awaiting_since: Option<std::time::Instant>,

    // ADD:
    /// Set of request IDs for in-flight chat requests.
    /// `awaiting_response` is derived: `!pending_request_ids.is_empty()`.
    pub pending_request_ids: std::collections::HashSet<String>,
    /// When the first pending request was added (for spinner elapsed time).
    /// Cleared when the set empties.
    pub awaiting_since: Option<std::time::Instant>,
}
```

Add a derived accessor method:

```rust
impl ChatState {
    /// Whether any chat request is in flight.
    pub fn awaiting_response(&self) -> bool {
        !self.pending_request_ids.is_empty()
    }
}
```

#### Site-by-site changes

Every read/write of `awaiting_response` and `last_request_id` must be updated. Here is the complete catalog:

##### 1. `Default for ChatState` (line 1077)

```rust
// REMOVE:
//   awaiting_response: false,
//   last_request_id: None,
// REPLACE WITH:
pending_request_ids: std::collections::HashSet::new(),
```

(`awaiting_since: None` remains unchanged.)

##### 2. `send_chat_message()` (line 10004–10006)

```rust
// REMOVE:
//   self.chat.awaiting_response = true;
//   self.chat.awaiting_since = Some(std::time::Instant::now());
//   self.chat.last_request_id = Some(request_id.clone());
// REPLACE WITH:
if self.chat.pending_request_ids.is_empty() {
    self.chat.awaiting_since = Some(std::time::Instant::now());
}
self.chat.pending_request_ids.insert(request_id.clone());
```

**Rationale:** `awaiting_since` tracks when the spinner started. It should only reset when transitioning from idle to awaiting (i.e., when the first request enters the set).

##### 3. `poll_chat_messages()` (line 9758 — streaming read guard)

```rust
// CHANGE:
//   if self.chat.awaiting_response {
// TO:
if self.chat.awaiting_response() {
```

##### 4. `poll_chat_messages()` (line 9835–9839 — clear awaiting on response)

```rust
// REMOVE:
//   if self.chat.awaiting_response {
//       self.chat.awaiting_response = false;
//       self.chat.awaiting_since = None;
//       self.chat.last_request_id = None;
//       self.chat.streaming_text.clear();
//   }

// REPLACE WITH:
// Remove request IDs that match responses we just received.
// Outbox messages carry a `request_id` field we can match on.
for msg in &new_msgs {
    if let Some(ref rid) = msg.request_id {
        self.chat.pending_request_ids.remove(rid);
    }
}
// If all requests are now answered, clear streaming state.
if self.chat.pending_request_ids.is_empty() {
    self.chat.awaiting_since = None;
    self.chat.streaming_text.clear();
}
```

**Important caveat:** The outbox `request_id` uses the `chat-{...}` prefix (generated by `wg chat` CLI), while the TUI generates `tui-{...}` IDs. These are different. The research report notes this at line 9833. Two approaches to handle this:

**Option A — Clear on any new outbox message (keep current behavior):**

```rust
// After appending coordinator messages, if we received any, remove
// one pending request ID (FIFO — oldest first).
if !new_msgs.is_empty() && !self.chat.pending_request_ids.is_empty() {
    // Remove the oldest pending request (insertion order via BTreeSet or
    // just pop an arbitrary element from HashSet — since responses arrive
    // in order, any removal tracks the right count).
    if let Some(first) = self.chat.pending_request_ids.iter().next().cloned() {
        self.chat.pending_request_ids.remove(&first);
    }
}
if self.chat.pending_request_ids.is_empty() {
    self.chat.awaiting_since = None;
    self.chat.streaming_text.clear();
}
```

**Option B — Thread the TUI request ID through `wg chat` (precise matching):**

Pass `--request-id <tui-rid>` to the `wg chat` CLI so the outbox entry carries the TUI's request ID. This enables exact matching but requires a small change to `src/commands/chat.rs`.

**Chosen approach: Option A** — It maintains the current invariant (one coordinator response retires one pending request) without touching the daemon or CLI. The ordering guarantee comes from the coordinator agent's sequential processing via mpsc channel: requests are processed in order, so FIFO retirement is correct.

##### 5. `CommandEffect::ChatResponse` error handler (line 7966–7969)

```rust
// REMOVE:
//   if self.chat.last_request_id.as_deref() == Some(&request_id) {
//       self.chat.awaiting_response = false;
//       self.chat.awaiting_since = None;
//       self.chat.last_request_id = None;
//   }

// REPLACE WITH:
self.chat.pending_request_ids.remove(&request_id);
if self.chat.pending_request_ids.is_empty() {
    self.chat.awaiting_since = None;
}
```

**Why this is better:** Previously, only the *last* request ID could be cleared on error. If the user sent M1, then M2, and M1 errored, the error was silently ignored (because `last_request_id` was already overwritten to rid2). Now every request ID is tracked independently.

##### 6. `maybe_refresh()` streaming fast-path (line 5389)

```rust
// CHANGE:
//   if self.chat.awaiting_response {
// TO:
if self.chat.awaiting_response() {
```

##### 7. `maybe_refresh()` fs_changed outbox check (line 5511)

```rust
// CHANGE:
//   if self.right_panel_tab == RightPanelTab::Chat || self.chat.awaiting_response {
// TO:
if self.right_panel_tab == RightPanelTab::Chat || self.chat.awaiting_response() {
```

##### 8. `maybe_refresh()` 1-second tick (line 5596)

```rust
// CHANGE:
//   if self.chat.awaiting_response || self.right_panel_tab == RightPanelTab::Chat {
// TO:
if self.chat.awaiting_response() || self.right_panel_tab == RightPanelTab::Chat {
```

##### 9. `has_timed_ui_elements()` (line 5754)

```rust
// CHANGE:
//   if self.chat.awaiting_response {
// TO:
if self.chat.awaiting_response() {
```

##### 10. `poll_interval()` spinner speed (line 5831)

```rust
// CHANGE:
//   if self.chat.awaiting_response {
// TO:
if self.chat.awaiting_response() {
```

##### 11. `interrupt_coordinator()` (line 10753)

```rust
// CHANGE:
//   self.chat.awaiting_response = false;
// TO:
self.chat.pending_request_ids.clear();
self.chat.awaiting_since = None;
```

##### 12. `event.rs` — Ctrl+C interrupt checks (lines 905, 1082, 1606)

```rust
// CHANGE all three occurrences:
//   if app.chat.awaiting_response {
// TO:
if app.chat.awaiting_response() {
```

##### 13. `render.rs` — 4 occurrences (lines 2750, 2898, 3212, 3271)

```rust
// CHANGE all four occurrences:
//   app.chat.awaiting_response
// TO:
app.chat.awaiting_response()
```

### Sequence Diagram — Corrected Flow (P2)

```
User              TUI State                    Coordinator Agent
  │                  │                              │
  │  send M1         │                              │
  ├─────────────────►│ pending_ids = {rid1}         │
  │                  │ spawn wg-chat M1 ──────────►│
  │                  │                              │ processing M1...
  │  send M2         │                              │
  ├─────────────────►│ pending_ids = {rid1, rid2}   │
  │                  │ spawn wg-chat M2 ──────────►│ queued in mpsc
  │                  │                              │
  │                  │    R1 arrives in outbox       │
  │                  │◄─────────────────────────────│
  │                  │ poll: append R1               │
  │                  │ pending_ids = {rid2}          │ (one removed)
  │                  │ awaiting_response() = true    │ ← still polling!
  │                  │ streaming poll still 100ms    │
  │                  │                              │ processing M2...
  │                  │    R2 arrives in outbox       │
  │                  │◄─────────────────────────────│
  │                  │ poll: append R2               │
  │                  │ pending_ids = {}              │ (empty)
  │                  │ awaiting_response() = false   │
  │                  │ clear streaming, spinner      │
```

### Error Scenario — M1 Fails, M2 Succeeds

```
User              TUI State                    Coordinator Agent
  │                  │                              │
  │  send M1         │                              │
  ├─────────────────►│ pending_ids = {rid1}         │
  │                  │ spawn wg-chat M1 ──────────►│
  │  send M2         │                              │
  ├─────────────────►│ pending_ids = {rid1, rid2}   │
  │                  │ spawn wg-chat M2 ──────────►│
  │                  │                              │
  │                  │ wg-chat M1 fails (timeout)   │
  │                  │ ChatResponse(rid1) error      │
  │                  │ remove rid1 from set          │
  │                  │ pending_ids = {rid2}          │ ← still tracking M2!
  │                  │ show error toast for M1       │
  │                  │                              │
  │                  │    R2 arrives in outbox       │
  │                  │◄─────────────────────────────│
  │                  │ poll: append R2               │
  │                  │ pending_ids = {}              │
  │                  │ display: M1, [error], M2, R2  │
```

Previously, M1's error was silently ignored because `last_request_id` had been overwritten to rid2. Now both requests are independently tracked.

### Functions Modified (P2)

| # | Function | File:Line | Change |
|---|---|---|---|
| 1 | `ChatState` struct | `state.rs:985` | Replace `awaiting_response` + `last_request_id` with `pending_request_ids: HashSet<String>` |
| 2 | `Default for ChatState` | `state.rs:1077` | Initialize `pending_request_ids: HashSet::new()` |
| 3 | `send_chat_message()` | `state.rs:10004` | Insert request_id into set instead of singleton assignment |
| 4 | `poll_chat_messages()` | `state.rs:9758, 9835` | Derive `awaiting_response()`, retire one ID per response |
| 5 | `drain_commands()` ChatResponse handler | `state.rs:7966` | Remove specific request_id from set on error |
| 6 | `interrupt_coordinator()` | `state.rs:10753` | Clear entire set |
| 7 | `maybe_refresh()` | `state.rs:5389, 5511, 5596` | Change field access to method call |
| 8 | `has_timed_ui_elements()` | `state.rs:5754` | Change field access to method call |
| 9 | `poll_interval()` | `state.rs:5831` | Change field access to method call |
| 10 | event.rs Ctrl+C handlers | `event.rs:905, 1082, 1606` | Change field access to method call |
| 11 | render.rs display checks | `render.rs:2750, 2898, 3212, 3271` | Change field access to method call |

**Total: ~11 call sites across 3 files. Net code change: ~25 lines modified, ~5 lines added.**

---

## Edge Cases

### Rapid-fire messages (3+ messages before any response)

- **P1 fix:** Each message after the first gets its index tracked in `deferred_user_indices`. When the first coordinator response arrives, all deferred messages are reordered to the end. Subsequent responses arrive in order — each `poll_chat_messages()` call only reorders messages deferred *at that point*, which is `[]` after the first drain.
- **P2 fix:** `pending_request_ids` grows to 3+ entries. Each response retires one entry. Streaming/spinner stay active until the set is fully drained.

### Agent crash mid-response

- The `wg chat` CLI has a 120-second timeout (`wait_for_response_for()` in `src/chat.rs:445`).
- After timeout, `CommandEffect::ChatResponse(rid)` fires with `result.success = false`.
- **P2 fix:** The specific request_id is removed from the set. Other in-flight requests are unaffected.
- **P1 fix:** The deferred user message for the failed request is already in `self.chat.messages` (just at a potentially wrong position). The error message from the ChatResponse handler gets appended after it, which is acceptable.

### Empty messages

- `send_chat_message()` already guards against empty text (line 9959: `if text.trim().is_empty() && !has_attachments { return; }`). No change needed.

### Coordinator interrupt (Ctrl+C)

- `interrupt_coordinator()` clears the entire pending set and streaming text.
- **P1 fix:** Any deferred user messages should be flushed to the display on interrupt. Add to `interrupt_coordinator()`:

```rust
// Flush deferred messages on interrupt so they don't vanish.
let deferred_indices: Vec<usize> = self.chat.deferred_user_indices.drain(..).collect();
// (Messages are already in self.chat.messages; just clear the tracking.)
```

Since messages are already in `self.chat.messages` (index-tracking approach), clearing `deferred_user_indices` is sufficient — the messages stay visible.

### Switching coordinators while messages are in flight

- Switching coordinators calls `init_chat_state()` which creates a fresh `ChatState`. In-flight `wg chat` background commands will still complete and fire `CommandEffect::ChatResponse`, but the request IDs won't be in the new coordinator's set — they'll be no-ops. The old coordinator's responses may arrive in the outbox but won't be polled (different coordinator ID). This is safe.

### Chat history persistence across restart

- `save_chat_history_with_skip()` persists `self.chat.messages` to disk as JSONL.
- On reload, `pending_request_ids` starts empty and `deferred_user_indices` starts empty.
- Messages already in the history file are in their final positions. No deferred-reorder is needed on reload.
- **Backward compatible:** The persisted format (`PersistedChatMessage`) is unchanged. Old chat logs load identically.

### Multiple coordinator agents processing the same inbox

- Not applicable — each coordinator ID has exactly one agent thread processing its inbox sequentially via mpsc channel.

---

## Risk Assessment

### What could break?

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Reorder leaves message in wrong position after edge-case timing | Low | Minor (cosmetic) | The reorder is deterministic: deferred messages always go to end after coordinator messages. Incorrect only if the user's mental model differs from actual processing order — but that's the point of the fix. |
| `awaiting_response()` method not caught at all call sites | Medium | Medium (UI glitch: no spinner, or stuck spinner) | Complete catalog of all 15 call sites provided above. Compiler will catch missing field access changes (`awaiting_response` field no longer exists → compilation error). |
| FIFO retirement removes wrong request ID | Low | Low (off-by-one in set count, self-corrects) | Coordinator agent processes messages in FIFO order via mpsc. Outbox entries arrive in the same order. One-to-one retirement is guaranteed by the sequential processing model. |
| `deferred_user_indices` becomes stale if messages are removed (e.g., by edit) | Low | Low (minor position error) | Chat edit mode is blocked for consumed messages. Un-consumed messages at the end of the list won't have their indices shift because they're the most recent entries. |
| New `HashSet` import conflicts or adds overhead | None | None | `std::collections::HashSet` is already used elsewhere in the file (line 9852: `std::collections::HashSet`). Zero new dependencies. |

### What does NOT change?

- Daemon IPC handling — no changes to `ipc.rs` or `service/mod.rs`
- Coordinator agent message processing — no changes to `coordinator_agent.rs`
- Inbox/outbox storage format — no changes to `.workgraph/chat/` structure
- Chat CLI — no changes to `src/commands/chat.rs`
- Markdown rendering — no changes to `render.rs` rendering logic (only `awaiting_response` → `awaiting_response()` accessor changes)
- Persisted chat format — `PersistedChatMessage` struct is unchanged

---

## Implementation Summary

### Files to modify

| File | Changes |
|---|---|
| `src/tui/viz_viewer/state.rs` | Struct change, Default impl, `send_chat_message()`, `poll_chat_messages()`, `drain_commands()` ChatResponse handler, `interrupt_coordinator()`, `maybe_refresh()`, `has_timed_ui_elements()`, `poll_interval()` |
| `src/tui/viz_viewer/event.rs` | 3 sites: `awaiting_response` → `awaiting_response()` |
| `src/tui/viz_viewer/render.rs` | 4 sites: `awaiting_response` → `awaiting_response()` |

**Total: 3 files, ~11 functions, ~45 lines changed.**

### Implementation order

1. **P2 first** (multi-request tracking) — This changes the data structure that P1's fix depends on (`pending_request_ids.is_empty()` replaces `awaiting_response`).
2. **P1 second** (deferred display insertion) — Builds on P2's multi-request tracking to know when responses are in flight.

### Testing strategy

- Unit test: Send two messages, simulate two outbox responses arriving in order. Assert final display order is `[M1, R1, M2, R2]`.
- Unit test: Send M1, send M2, simulate M1 error. Assert `pending_request_ids` contains only rid2. Assert M2's response still arrives correctly.
- Unit test: Send M1 while no response is in flight. Assert message is pushed immediately (no deferral — regression test for current behavior).
- Manual test: Open TUI chat, send a message, immediately send a second message while the coordinator spinner is active. Verify both responses appear in order.

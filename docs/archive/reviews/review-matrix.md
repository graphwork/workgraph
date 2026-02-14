# Review: Matrix Integration

## Overview

Workgraph has **two parallel Matrix client implementations** selected via Cargo feature flags:

| Feature | Module | Dependencies | Lines | Default? |
|---------|--------|-------------|-------|----------|
| `matrix` | `src/matrix/` | `matrix-sdk` 0.16 (E2EE, SQLite) | 1,562 | No |
| `matrix-lite` | `src/matrix_lite/` | `reqwest`, `urlencoding` | 1,125 | **Yes** |

Supporting command files (`src/commands/notify.rs` at 387 lines, `src/commands/matrix.rs` at 241 lines) are shared via `#[cfg]` gates that alias types depending on the active feature.

**Total: 3,315 lines across 8 files.**

---

## Implementation Details

### `matrix` (Full SDK) - `src/matrix/`

**`src/matrix/mod.rs`** (602 lines) - Full Matrix client wrapping `matrix-sdk`:
- SQLite-backed state store for session and crypto persistence
- E2EE key verification (SAS emoji comparison) with auto-accept handlers
- Session restore from disk or fresh login (password or access token)
- Room join/create, text and HTML message sending
- Message handler registration via `add_event_handler` + mpsc channel
- Sync: `sync_once()`, `sync_loop()`, deprecated `start_sync_thread()`
- Custom UUID generation for device IDs (`uuid_v4_simple()` - not cryptographically secure, uses PID XOR nanos)

**`src/matrix/commands.rs`** (447 lines) - Command parser with extensive tests:
- Parses: `claim`, `done`, `fail`, `input`/`log`/`note`, `unclaim`/`release`, `status`, `ready`, `help`
- Supports prefixes: `wg`, `!wg`, `/wg` (case-insensitive)
- Actor parsing: `as <actor>`, `--actor <actor>`, `for <actor>`
- 20 unit tests covering all commands, prefixes, edge cases

**`src/matrix/listener.rs`** (513 lines) - Background message listener:
- Listens to configured rooms, parses commands, executes against the graph
- Graph operations: claim, done, fail, input, unclaim, status, ready
- Uses `tokio::select!` for concurrent sync + message processing
- Directly calls `load_graph`/`save_graph` (bypasses CLI commands)

### `matrix-lite` (Lightweight HTTP) - `src/matrix_lite/`

**`src/matrix_lite/mod.rs`** (538 lines) - Minimal HTTP-only client:
- Uses `reqwest` directly against Matrix Client-Server API v3
- No E2EE, no SQLite - caches access token and sync token as plain files
- Login with password or access token, join room, send text/HTML messages
- Sync via long-polling with manual JSON response parsing
- `sync_once_with_filter()` pushes messages through a `MessageFilter` to mpsc channel
- Convenience functions: `send_notification()`, `send_notification_to_room()`
- Stub `VerificationEvent` enum for API compatibility (unused)

**`src/matrix_lite/commands.rs`** (162 lines) - **Copy-paste of** `src/matrix/commands.rs`:
- Identical `MatrixCommand` enum, `parse()`, `strip_prefix()`, `is_known_command()`, `parse_command()`, `parse_actor_arg()`, `help_text()`
- File header explicitly says: *"This is a copy of the commands module to avoid feature-flag complexity."*
- No tests (all tests are in the full version)

**`src/matrix_lite/listener.rs`** (425 lines) - **Near-identical copy of** `src/matrix/listener.rs`:
- Same `ListenerConfig`, same `MatrixListener` struct (but uses `HashSet<String>` instead of `HashSet<OwnedRoomId>`)
- Same command execution methods: `execute_claim`, `execute_done`, `execute_fail`, `execute_input`, `execute_unclaim`, `execute_status`, `execute_ready`
- Same `extract_localpart` helper (but takes `&str` instead of `&OwnedUserId`)
- Same `run_listener` entrypoint

### Shared Command Files

**`src/commands/matrix.rs`** (241 lines) - CLI subcommands:
- `wg matrix listen` - starts the message listener
- `wg matrix send` - one-shot message send
- `wg matrix status` - show config/connection info
- `wg matrix login` / `logout` - credential management
- Uses `#[cfg]` to import from `matrix` or `matrix_lite` with type aliases

**`src/commands/notify.rs`** (387 lines) - Task notification system:
- `wg notify <task-id>` - sends formatted task details to a Matrix room
- Rich formatting with status emojis, HTML, action hints
- Uses `#[cfg]` to select the right `MatrixClient`
- Well-tested (6 unit tests)

---

## Why Two Implementations Exist

The `matrix-lite` feature was created to **avoid the heavy `matrix-sdk` dependency**:

- `matrix-sdk` 0.16 with `e2e-encryption` + `sqlite` pulls in: `vodozemac` (Olm/Megolm), `sqlcipher`/`rusqlite`, `eyeball`/`eyeball-im`, OpenSSL bindings, and hundreds of transitive deps
- This significantly increases compile time and binary size
- For workgraph's use case (sending notifications, basic command/response in rooms), E2EE is rarely needed

The `matrix-lite` default was chosen to keep the default build fast and simple, with `matrix` available as an opt-in for encrypted rooms.

---

## Problems and Issues

### 1. Code Duplication (Critical)

**609 lines are copy-pasted** between the two implementations:

| File | Full Version | Lite Version | Duplicated |
|------|-------------|-------------|-----------|
| `commands.rs` | 447 lines | 162 lines | ~162 lines (all of lite) |
| `listener.rs` | 513 lines | 425 lines | ~400 lines (all executor methods) |
| `mod.rs` (struct) | `IncomingMessage` | `IncomingMessage` | ~47 lines (struct + enum) |

The command parser has **zero dependency on either SDK** - it's pure string parsing. There is no reason for it to be duplicated.

The listener's command execution methods (`execute_claim`, `execute_done`, etc.) are identical in both - they only use `load_graph`/`save_graph` and have no SDK dependency.

### 2. No Shared Trait or Interface

The two `MatrixClient` types have overlapping APIs but no shared trait:
- Both have `new()`, `is_logged_in()`, `user_id()`, `join_room()`, `send_message()`, `send_html_message()`, `sync_once()`
- But different signatures: `user_id()` returns `Option<&OwnedUserId>` vs `Option<&str>`, `join_room()` returns `Result<Room>` vs `Result<()>`
- Consumer code (`notify.rs`, `matrix.rs`) uses `#[cfg]` type aliases to switch

### 3. UUID Generation is Weak

`src/matrix/mod.rs:581-589` generates device IDs using `PID ^ nanos` formatted as hex. This is predictable and could collide. Should use `uuid` crate or `getrandom`.

### 4. Graph Operations Bypass CLI/Locking

Both listeners directly call `load_graph()`/`save_graph()` without file locking. The rest of workgraph uses `flock`-based graph locking (added in commit `8279a35`). Concurrent graph modifications from Matrix commands and CLI could corrupt the graph file.

### 5. Lite Version Has No Tests for Commands

`matrix_lite/commands.rs` has no tests despite being a separate copy. If the two versions diverge, bugs could go unnoticed.

### 6. `VerificationEvent` Stub in Lite

`matrix_lite/mod.rs:475-483` has a stub `VerificationEvent` enum that's never used. It exists only for "API compatibility" but nothing in the lite path references it.

### 7. Dead Code in Full SDK Version

- `start_sync_thread()` is deprecated and should be removed
- `sync_loop()` is never called (listener uses `sync_once()` pattern)

---

## Recommendations

### Keep `matrix-lite` as Default, Keep `matrix` as Opt-in

The two-feature approach is sound. The full SDK is genuinely heavy, and most workgraph users won't need E2EE. The problem is the implementation, not the design.

### Consolidate Shared Code (High Priority)

**Extract a `src/matrix_common/` module** (or just `src/matrix_commands.rs`) with:

1. **Command parser** - `MatrixCommand`, `parse()`, `help_text()`, all helpers. This has zero SDK dependencies and should exist exactly once. Both features can import it.

2. **Command executor** - `execute_claim()`, `execute_done()`, `execute_fail()`, `execute_input()`, `execute_unclaim()`, `execute_status()`, `execute_ready()`. These only depend on `load_graph`/`save_graph`/`chrono` - no Matrix SDK. Extract into a struct that takes `workgraph_dir: &Path` and returns `String` responses.

3. **`IncomingMessage`** - Define once with string-typed fields (the lite version's approach). The full SDK listener can convert from `OwnedRoomId`/`OwnedUserId` at the boundary.

**Estimated reduction: ~560 lines deleted, commands tested once.**

### Add Graph Locking to Listeners (High Priority)

Both listeners should use the same `flock`-based locking that the CLI uses when modifying the graph. Without this, Matrix commands can race with CLI operations.

### Define a Minimal Client Trait (Medium Priority)

```rust
#[async_trait]
pub trait MatrixTransport {
    async fn send_message(&self, room_id: &str, message: &str) -> Result<()>;
    async fn send_html_message(&self, room_id: &str, plain: &str, html: &str) -> Result<()>;
    async fn join_room(&self, room_id: &str) -> Result<()>;
    fn is_logged_in(&self) -> bool;
    fn user_id(&self) -> Option<&str>;
}
```

This would let `notify.rs` and `matrix.rs` use dynamic dispatch instead of `#[cfg]` type aliases. It's not strictly necessary but would simplify the consumer code.

### Minor Cleanups

- Remove deprecated `start_sync_thread()` and unused `sync_loop()`
- Remove `VerificationEvent` stub from lite version
- Replace `uuid_v4_simple()` with `getrandom` or similar
- Add `#[cfg(test)]` tests to the shared command module

### Estimated Effort

| Change | Lines Changed | Effort |
|--------|-------------|--------|
| Extract shared commands + executor | ~560 deleted, ~30 added | Medium |
| Add graph locking to listeners | ~40 changed | Small |
| Define `MatrixTransport` trait | ~80 added, ~60 changed | Medium |
| Remove dead code | ~40 deleted | Trivial |
| Total | Net -550 lines | ~2-3 hours |

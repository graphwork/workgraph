# Coordinator Add/Remove Modal — Manual Smoke Test

## Prerequisites
- `wg service start` running with at least one active coordinator
- TUI open via `wg watch` (or `wg viz`)

## Test 1: Add Coordinator (+ key)

1. Focus the **Chat** tab in the right panel.
2. Press **`+`**.
3. **Expected:** A text prompt overlay appears asking for a coordinator name.
4. Type a name (e.g., "test-coord") and press **Enter**.
5. **Expected:** A new coordinator tab appears in the tab bar. The service receives a `SpawnCoordinator` IPC request.
6. Leave the name empty and press **Enter**.
7. **Expected:** An unnamed coordinator is created (empty name is accepted).
8. Press **Esc** instead of Enter.
9. **Expected:** The prompt closes without creating a coordinator.

## Test 2: Remove Coordinator (- key)

1. Switch to a non-default coordinator tab (coordinator ID > 0).
2. Press **`-`** while the Chat tab is focused.
3. **Expected:** A choice dialog appears with three options:
   - **(a) Archive** — "Mark as done — work complete"
   - **(s) Stop** — "Pause coordinator — resume later"
   - **(x) Abandon** — "Permanently discard"
4. Use **Up/Down** arrows to navigate options; the highlight moves.
5. Press **Enter** on "Archive".
6. **Expected:** The coordinator is archived via `service archive-coordinator <id>` IPC. The coordinator tab disappears and the active coordinator switches to coordinator 0.
7. Repeat with a different coordinator, select "Stop".
8. **Expected:** The coordinator is stopped via `service stop-coordinator <id>` IPC. Tab disappears, switches to coordinator 0.
9. Repeat with a different coordinator, select "Abandon".
10. **Expected:** The coordinator is permanently deleted. Tab disappears, switches to coordinator 0.
11. Press **Esc** during the choice dialog.
12. **Expected:** Dialog closes, no action taken.

## Test 3: Default Coordinator (ID 0) Protection

1. Switch to coordinator 0 (the default coordinator).
2. Press **`-`**.
3. **Expected:** Nothing happens — the choice dialog does not open for coordinator 0.

## Test 4: Mouse Click on Tab Close Button

1. If coordinator tabs render a close button (×), click it on a non-default coordinator.
2. **Expected:** The same choice dialog opens as pressing `-`.

## Test 5: Hotkey Selection in Choice Dialog

1. Open the choice dialog (press `-` on a non-default coordinator).
2. Press **`a`** (archive hotkey).
3. **Expected:** Archive action fires immediately without needing Enter.
4. Repeat with **`s`** (stop) and **`x`** (abandon).

## IPC Serialization (automated)

Covered by unit tests:
- `test_ipc_archive_coordinator_serialization` — verifies `ArchiveCoordinator { coordinator_id }` round-trips through JSON.
- `test_ipc_stop_coordinator_serialization` — verifies `StopCoordinator { coordinator_id }` round-trips through JSON.

Run: `cargo test test_ipc_archive_coordinator && cargo test test_ipc_stop_coordinator`

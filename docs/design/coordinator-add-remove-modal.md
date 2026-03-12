# Design: Coordinator Add/Remove Modal UX

## Overview

Replace the immediate-action `+`/`-` keys and `✕` close button with modal dialogs for coordinator creation and removal. The create dialog prompts for an optional name; the remove dialog offers Archive, Stop, Abandon, or Cancel.

## Current Behavior

| Trigger | Location | Current Action |
|---------|----------|----------------|
| `+` key | event.rs:1387-1388 | `app.create_coordinator(None)` — no dialog |
| `-` key | event.rs:1391-1395 | `app.delete_coordinator(cid)` — no confirmation, marks Abandoned |
| `✕` close button | event.rs:1686-1690 | Same as `-` key |
| `[+]` mouse click | event.rs:1676-1679 | Same as `+` key |

## Design

### 1. New `TextPromptAction` Variant: `CreateCoordinator`

Add to `TextPromptAction` (state.rs:602-608):

```rust
pub enum TextPromptAction {
    MarkFailed(String),
    SendMessage(String),
    EditDescription(String),
    AttachFile,
    CreateCoordinator,  // NEW
}
```

**Behavior:**
- `+` key and `[+]` mouse click → clear editor, set `InputMode::TextPrompt(TextPromptAction::CreateCoordinator)`
- The existing `draw_text_prompt` renderer handles this variant with title `"New coordinator — enter name (optional):"`
- **Enter with text** → `app.create_coordinator(Some(text))`
- **Enter with empty text** → `app.create_coordinator(None)` (unnamed, preserves current default)
- **Esc** → `InputMode::Normal`, no action

This reuses the existing `TextPrompt` infrastructure entirely. The only new code is:
1. One match arm in `draw_text_prompt` for the title string
2. One match arm in `handle_text_prompt_input` for the submit action
3. Changed `+` key / `[+]` click handlers to enter TextPrompt mode instead of calling create directly

**Empty-submit handling:** The current `handle_text_prompt_input` returns to Normal mode on empty submit without executing the action (event.rs:412-419). For `CreateCoordinator`, we need empty submit to **still create** an unnamed coordinator. Override the empty-text early return for this variant:

```rust
if text.trim().is_empty() {
    match action {
        TextPromptAction::CreateCoordinator => {
            app.create_coordinator(None);
            app.input_mode = InputMode::Normal;
            return;
        }
        TextPromptAction::AttachFile => { /* existing */ }
        _ => {
            app.input_mode = InputMode::Normal;
            return;
        }
    }
}
```

### 2. New `InputMode` Variant: `ChoiceDialog`

Add to `InputMode` (state.rs:574-591):

```rust
pub enum InputMode {
    Normal,
    Search,
    ChatInput,
    MessageInput,
    TaskForm,
    Confirm(ConfirmAction),
    TextPrompt(TextPromptAction),
    ConfigEdit,
    ChoiceDialog(ChoiceDialogState),  // NEW
}
```

New types (state.rs, near ConfirmAction):

```rust
/// State for the multi-choice dialog overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceDialogState {
    pub action: ChoiceDialogAction,
    /// Currently highlighted option index (0-based). Arrow keys change this.
    pub selected_index: usize,
}

/// What action the choice dialog is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChoiceDialogAction {
    /// Remove a coordinator — presents Archive / Stop / Abandon / Cancel.
    RemoveCoordinator(u32),  // coordinator_id
}
```

**Why a new InputMode instead of reusing Confirm:**
- `Confirm` is a binary y/n dialog — 2 fixed options, no navigation needed.
- `ChoiceDialog` has N selectable options with keyboard navigation (Up/Down/j/k) and per-option hotkeys. Different rendering, different event handling. Mixing them would add complexity to both.

### 3. Choice Dialog Options for `RemoveCoordinator`

When the user presses `-`, clicks `✕`, or otherwise triggers removal of coordinator `cid`:

```
┌─ Remove Coordinator 2 ─────────────────┐
│                                         │
│  [a] Archive — mark done, preserve      │
│  [s] Stop   — kill agent, can resume    │
│  [x] Abandon — remove permanently       │
│                                         │
│  [Esc] Cancel                           │
└─────────────────────────────────────────┘
```

The currently highlighted option has a colored background (yellow on dark). Arrow keys (Up/Down) and j/k move the highlight. Enter confirms the highlighted option. Hotkeys (a/s/x) act immediately without needing to navigate.

### 4. Event Handling: `handle_choice_dialog_input`

New function in event.rs (alongside `handle_confirm_input`):

```rust
fn handle_choice_dialog_input(app: &mut VizApp, code: KeyCode) {
    let state = match &app.input_mode {
        InputMode::ChoiceDialog(s) => s.clone(),
        _ => return,
    };

    match state.action {
        ChoiceDialogAction::RemoveCoordinator(cid) => {
            let num_options = 3; // archive, stop, abandon
            match code {
                // Navigation
                KeyCode::Up | KeyCode::Char('k') => {
                    let new_idx = if state.selected_index == 0 {
                        num_options - 1
                    } else {
                        state.selected_index - 1
                    };
                    app.input_mode = InputMode::ChoiceDialog(ChoiceDialogState {
                        selected_index: new_idx,
                        ..state
                    });
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let new_idx = (state.selected_index + 1) % num_options;
                    app.input_mode = InputMode::ChoiceDialog(ChoiceDialogState {
                        selected_index: new_idx,
                        ..state
                    });
                }

                // Confirm highlighted option
                KeyCode::Enter => {
                    match state.selected_index {
                        0 => archive_coordinator(app, cid),
                        1 => stop_coordinator(app, cid),
                        2 => app.delete_coordinator(cid), // existing abandon
                        _ => {}
                    }
                    app.input_mode = InputMode::Normal;
                }

                // Direct hotkeys
                KeyCode::Char('a') => {
                    archive_coordinator(app, cid);
                    app.input_mode = InputMode::Normal;
                }
                KeyCode::Char('s') => {
                    stop_coordinator(app, cid);
                    app.input_mode = InputMode::Normal;
                }
                KeyCode::Char('x') => {
                    app.delete_coordinator(cid);
                    app.input_mode = InputMode::Normal;
                }

                // Cancel
                KeyCode::Esc => {
                    app.input_mode = InputMode::Normal;
                }
                _ => {}
            }
        }
    }
}
```

Wire into the main dispatch (event.rs:184):

```rust
InputMode::ChoiceDialog(_) => handle_choice_dialog_input(app, code),
```

### 5. Trigger Points (Replace Immediate Actions)

**`-` key** (event.rs:1391-1396) — change from:
```rust
KeyCode::Char('-') if app.right_panel_tab == RightPanelTab::Chat => {
    let cid = app.active_coordinator_id;
    if cid != 0 {
        app.delete_coordinator(cid);
    }
}
```
To:
```rust
KeyCode::Char('-') if app.right_panel_tab == RightPanelTab::Chat => {
    let cid = app.active_coordinator_id;
    if cid != 0 {
        app.input_mode = InputMode::ChoiceDialog(ChoiceDialogState {
            action: ChoiceDialogAction::RemoveCoordinator(cid),
            selected_index: 0,
        });
    }
}
```

**`✕` close button** (event.rs:1686-1690) — same change: open dialog instead of calling `delete_coordinator` directly.

**`+` key** (event.rs:1387-1388) — change from:
```rust
KeyCode::Char('+') if app.right_panel_tab == RightPanelTab::Chat => {
    app.create_coordinator(None);
}
```
To:
```rust
KeyCode::Char('+') if app.right_panel_tab == RightPanelTab::Chat => {
    super::state::editor_clear(&mut app.text_prompt.editor);
    app.input_mode = InputMode::TextPrompt(TextPromptAction::CreateCoordinator);
}
```

**`[+]` mouse click** (event.rs:1676-1679) — same change as `+` key.

### 6. New IPC Commands

#### `ArchiveCoordinator`

Marks the coordinator task as `Done` (not `Abandoned`). History is preserved. The coordinator agent is shut down and removed from the daemon's `coordinator_agents` map.

Add to `IpcRequest` (ipc.rs):
```rust
ArchiveCoordinator { coordinator_id: u32 },
```

IPC handler `handle_archive_coordinator`:
```rust
fn handle_archive_coordinator(dir: &Path, coordinator_id: u32) -> IpcResponse {
    let graph_path = crate::commands::graph_path(dir);
    let mut graph = match workgraph::parser::load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return IpcResponse::error(&format!("Failed to load graph: {}", e)),
    };

    let task_id = format!(".coordinator-{}", coordinator_id);
    let task = match graph.get_task_mut(&task_id) {
        Some(t) => t,
        None => return IpcResponse::error(&format!("Coordinator task '{}' not found", task_id)),
    };

    task.status = workgraph::graph::Status::Done;
    task.log.push(workgraph::graph::LogEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        actor: Some("daemon".to_string()),
        message: format!("Coordinator {} archived via IPC", coordinator_id),
    });

    if let Err(e) = workgraph::parser::save_graph(&graph, &graph_path) {
        return IpcResponse::error(&format!("Failed to save graph: {}", e));
    }

    IpcResponse::success(serde_json::json!({
        "coordinator_id": coordinator_id,
        "task_id": task_id,
        "action": "archived",
    }))
}
```

The daemon processes this the same as `DeleteCoordinator`: adds `coordinator_id` to `delete_coordinator_ids`, which triggers coordinator agent shutdown in the main loop (mod.rs:1396-1404).

#### `StopCoordinator`

Kills the running agent (if any) and sets the coordinator task status to `Open` — meaning it can be resumed later when the user sends a message. The coordinator tab remains visible (not filtered out).

> **Note:** The graph `Status` enum (graph.rs:122-132) has no `Paused` variant. Using `Open` is the simplest approach: the coordinator task becomes available for re-activation. The daemon already filters out `Abandoned` and `Done` coordinators from `ListCoordinators` (ipc.rs:996-998), so `Open` coordinators are still listed. When the user sends a chat message to a stopped coordinator, the daemon's `pending_coordinator_ids` mechanism (mod.rs:1449-1452) will lazy-spawn a fresh coordinator agent for it.

Add to `IpcRequest` (ipc.rs):
```rust
StopCoordinator { coordinator_id: u32 },
```

IPC handler `handle_stop_coordinator`:
```rust
fn handle_stop_coordinator(dir: &Path, coordinator_id: u32) -> IpcResponse {
    let graph_path = crate::commands::graph_path(dir);
    let mut graph = match workgraph::parser::load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => return IpcResponse::error(&format!("Failed to load graph: {}", e)),
    };

    let task_id = format!(".coordinator-{}", coordinator_id);
    let task = match graph.get_task_mut(&task_id) {
        Some(t) => t,
        None => return IpcResponse::error(&format!("Coordinator task '{}' not found", task_id)),
    };

    task.status = workgraph::graph::Status::Open;
    task.log.push(workgraph::graph::LogEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        actor: Some("daemon".to_string()),
        message: format!("Coordinator {} stopped via IPC (can resume)", coordinator_id),
    });

    if let Err(e) = workgraph::parser::save_graph(&graph, &graph_path) {
        return IpcResponse::error(&format!("Failed to save graph: {}", e));
    }

    IpcResponse::success(serde_json::json!({
        "coordinator_id": coordinator_id,
        "task_id": task_id,
        "action": "stopped",
    }))
}
```

The daemon shuts down the coordinator agent for this `cid` (same `delete_coordinator_ids` path) but does **not** remove the coordinator from the TUI tab bar — because the task status is `Open`, `ListCoordinators` still includes it.

**Daemon-side cleanup (mod.rs:1396-1404):** The existing `delete_coordinator_ids` loop already handles agent shutdown. Both `ArchiveCoordinator` and `StopCoordinator` should push to this same vector. The difference is only in graph task status.

### 7. New `CommandEffect` Variants

Add to state.rs `CommandEffect` enum:

```rust
pub enum CommandEffect {
    // ... existing variants ...
    ArchiveCoordinator(u32),
    StopCoordinator(u32),
}
```

**`ArchiveCoordinator(cid)` effect handler:** Same as `DeleteCoordinator(cid)` — remove local chat state, switch to coordinator 0, refresh. The tab disappears because `Done` coordinators are filtered from list.

**`StopCoordinator(cid)` effect handler:** Refresh the graph (the tab stays because `Open` is not filtered). Optionally show a HUD notification "Coordinator N stopped". Do **not** remove local chat state — the user can resume the conversation.

### 8. App Methods

Add to `VizApp` (state.rs, near existing `create_coordinator` and `delete_coordinator`):

```rust
/// Archive a coordinator session (mark as Done). Coordinator 0 cannot be archived.
pub fn archive_coordinator(&mut self, cid: u32) {
    if cid == 0 { return; }
    let args = vec![
        "service".to_string(),
        "archive-coordinator".to_string(),
        cid.to_string(),
    ];
    self.exec_command(args, CommandEffect::ArchiveCoordinator(cid));
}

/// Stop a coordinator session (kill agent, set to Open). Coordinator 0 cannot be stopped.
pub fn stop_coordinator(&mut self, cid: u32) {
    if cid == 0 { return; }
    let args = vec![
        "service".to_string(),
        "stop-coordinator".to_string(),
        cid.to_string(),
    ];
    self.exec_command(args, CommandEffect::StopCoordinator(cid));
}
```

### 9. Rendering: `draw_choice_dialog`

New function in render.rs (alongside `draw_confirm_dialog`):

```rust
fn draw_choice_dialog(frame: &mut Frame, state: &ChoiceDialogState) {
    match &state.action {
        ChoiceDialogAction::RemoveCoordinator(cid) => {
            let title = format!(" Remove Coordinator {} ", cid);
            let options = [
                ("[a] Archive", "mark done, preserve history"),
                ("[s] Stop", "kill agent, can resume later"),
                ("[x] Abandon", "remove permanently"),
            ];

            let size = frame.area();
            let width = 44.min(size.width.saturating_sub(4));
            let height = (options.len() as u16 + 4).min(size.height.saturating_sub(2)); // +4 for border + padding + esc line
            let x = (size.width.saturating_sub(width)) / 2;
            let y = (size.height.saturating_sub(height)) / 2;
            let area = Rect::new(x, y, width, height);

            frame.render_widget(Clear, area);

            let block = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
            let inner = block.inner(area);
            frame.render_widget(block, area);

            let mut lines = Vec::new();
            for (i, (key, desc)) in options.iter().enumerate() {
                let is_selected = i == state.selected_index;
                let style = if is_selected {
                    Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let key_style = if is_selected {
                    Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(*key, key_style),
                    Span::styled(format!(" — {}", desc), style),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Esc] Cancel",
                Style::default().fg(Color::DarkGray),
            )));

            let paragraph = Paragraph::new(lines);
            frame.render_widget(paragraph, inner);
        }
    }
}
```

Wire into `draw()` (render.rs, after the Confirm dialog overlay at line 278-280):

```rust
if let InputMode::ChoiceDialog(ref state) = app.input_mode {
    draw_choice_dialog(frame, state);
}
```

### 10. Mouse Support for Choice Dialog

Store the rendered option areas in a `last_choice_dialog_areas: Vec<Rect>` field on `VizApp`. In the mouse handler, if `InputMode::ChoiceDialog` is active and the click falls within one of the option rects, trigger that option.

Alternatively (simpler): the choice dialog is small and keyboard-driven. Mouse clicks outside the dialog dismiss it (cancel). Mouse clicks inside are ignored for V1 — the hotkeys and arrow navigation are sufficient. Add mouse click support as a follow-up if needed.

**Recommended for V1:** Cancel-on-click-outside only. No option click targets. This avoids storing hit areas.

### 11. Paste Handling

Add to `handle_paste` (event.rs:200-245):

```rust
InputMode::ChoiceDialog(_) => {
    // Ignore paste in choice dialog — it's not a text input.
}
```

### 12. Edge Cases

#### Coordinator 0 Protection
- `cid == 0` check already exists in `delete_coordinator` (state.rs:6191-6192).
- Same guard applies to `archive_coordinator` and `stop_coordinator`.
- The `-` key handler already checks `cid != 0` before opening the dialog (event.rs:1393).
- The `✕` close button is not rendered for coordinator 0 (`close_start == close_end`, see render.rs coordinator tab rendering).

#### Agent Running vs Not Running
- **Archive:** Works regardless. If agent is running, the daemon kills it during `delete_coordinator_ids` cleanup. If not running, just changes graph status.
- **Stop:** Same — `delete_coordinator_ids` cleanup is safe whether or not an agent is running. If no agent is running, only the graph status changes.
- **Abandon:** Existing behavior, works regardless.

#### Concurrent State
- The choice dialog captures `cid` at open time. If the coordinator is removed by another mechanism before the user confirms, the IPC handler returns an error (coordinator task not found). The TUI should handle this gracefully — just close the dialog and refresh.

#### Dialog Stacking
- Only one modal can be active at a time (InputMode is a single enum variant). Opening the choice dialog prevents other modals from appearing until it's dismissed. This is the existing pattern — no change needed.

#### Resumed Coordinator State
- When a stopped (`Open`) coordinator receives a new chat message, a fresh coordinator agent is spawned. The agent reads existing chat history from the outbox, so conversation context is preserved. The agent may need to re-read its system prompt, but this is standard coordinator agent startup behavior.

## Files That Need Changes

| File | Changes |
|------|---------|
| `src/tui/viz_viewer/state.rs` | Add `ChoiceDialogState`, `ChoiceDialogAction` enums. Add `CreateCoordinator` to `TextPromptAction`. Add `ArchiveCoordinator(u32)`, `StopCoordinator(u32)` to `CommandEffect`. Add `archive_coordinator()`, `stop_coordinator()` methods to `VizApp`. |
| `src/tui/viz_viewer/event.rs` | Add `handle_choice_dialog_input()`. Wire into main dispatch. Change `+`/`-`/`✕`/`[+]` handlers to open dialogs. Add `CreateCoordinator` match arms in `handle_text_prompt_input`. Handle paste for `ChoiceDialog`. |
| `src/tui/viz_viewer/render.rs` | Add `draw_choice_dialog()`. Wire into `draw()`. Add title for `TextPromptAction::CreateCoordinator` in `draw_text_prompt`. Import new types. |
| `src/commands/service/ipc.rs` | Add `ArchiveCoordinator`, `StopCoordinator` to `IpcRequest`. Add `handle_archive_coordinator()`, `handle_stop_coordinator()` handlers. Wire into `handle_request()` dispatch. Push to `delete_coordinator_ids` for both. Add serialization tests. |
| `src/commands/service/mod.rs` | Add `"archive-coordinator"` and `"stop-coordinator"` subcommand routing (if command dispatch goes through CLI arg parsing). |
| `src/main.rs` | Add `service archive-coordinator` and `service stop-coordinator` CLI subcommands (parallel to existing `service delete-coordinator`). |

## Implementation Order

1. **IPC layer** (ipc.rs, mod.rs, main.rs): Add `ArchiveCoordinator` and `StopCoordinator` commands. Test with unit tests.
2. **State types** (state.rs): Add new enums and methods.
3. **Create dialog** (event.rs, render.rs, state.rs): Add `TextPromptAction::CreateCoordinator` handling.
4. **Remove dialog** (event.rs, render.rs): Add `ChoiceDialog` input mode, rendering, and event handling.
5. **Wire triggers** (event.rs): Replace immediate `+`/`-`/`✕`/`[+]` handlers with dialog openers.

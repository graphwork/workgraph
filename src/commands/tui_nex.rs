//! `wg tui-nex` — minimal ratatui-based interactive REPL for the native executor.
//!
//! Phase 4 MVP for the nex REPL: gives `wg nex` a proper two-pane UI
//! (messages pane + input pane) instead of stdin/stderr. Designed as a
//! self-contained command that doesn't touch the existing `wg tui`
//! viz_viewer — both can coexist, and a future session can merge this
//! UX into the main TUI's chat panel.
//!
//! ## Architecture
//!
//! The TUI runs a ratatui event loop on the main thread. The agent loop
//! runs on a tokio task. They communicate through two channels:
//!
//! - `UiToAgent`: user input lines go from the TUI to the agent task
//! - `AgentToUi`: streaming text, tool calls, and turn boundaries go
//!   from the agent task to the TUI
//!
//! The agent task uses a lightweight adapter around `AgentLoop` that
//! streams each response to the channel instead of stderr. Ctrl-C in
//! the TUI sends a signal that cancels the current turn.
//!
//! ## What's in (MVP)
//!
//! - Two-pane layout: scrollable messages area + single-line input
//! - Streaming response rendering (tokens appear as they arrive)
//! - Tool call and result display (compact summary lines)
//! - Ctrl-C cancels an in-flight turn without exiting
//! - Esc or Ctrl-Q exits cleanly
//! - `--model` / `--endpoint` flags match `wg nex`
//!
//! ## What's not in (follow-ups)
//!
//! - Slash commands (`/help`, `/clear`, `/bg`, etc.)
//! - Markdown rendering / syntax highlighting
//! - History persistence / search
//! - Multi-line input with scrollback
//! - Mouse support
//!
//! These can land incrementally on top of the channel architecture.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;

use workgraph::config::{Config, DispatchRole};
use workgraph::executor::native::client::{
    ContentBlock, Message, MessagesRequest, Role, StopReason,
};
use workgraph::executor::native::provider::{Provider, create_provider_ext};
use workgraph::executor::native::tools::ToolRegistry;
use workgraph::models::ModelRegistry;

/// Message from the UI to the agent task.
enum UiToAgent {
    /// User submitted a prompt. `interrupt=true` additionally cancels
    /// the in-flight turn (Ctrl-Enter); `interrupt=false` is queued
    /// and delivered cleanly at the next turn boundary (plain Enter
    /// while agent is working).
    UserInput { text: String, interrupt: bool },
    /// Single Ctrl-C — cooperative cancel of the in-flight turn.
    Cancel,
    /// Double Ctrl-C within DOUBLE_TAP_WINDOW — SIGKILL the subprocess
    /// tree (bash children, headless chrome, curl, etc.) and return
    /// to idle. nohup/disown'd children survive by Unix semantics.
    HardCancel,
    /// User is quitting — stop the agent task.
    Quit,
}

/// Message from the agent task to the UI.
#[derive(Clone, Debug)]
enum AgentToUi {
    /// Streaming token chunk arrived.
    Token(String),
    /// The assistant finished a turn (text accumulated so far is final).
    TurnEnded,
    /// A tool call started.
    ToolStart { name: String, summary: String },
    /// A tool call completed.
    ToolEnd {
        name: String,
        chars: usize,
        is_error: bool,
    },
    /// The agent encountered a fatal error.
    Error(String),
    /// Info/status line.
    Info(String),
}

/// A single displayable line in the messages pane.
#[derive(Clone, Debug)]
enum DisplayLine {
    User(String),
    /// In-progress assistant text being streamed.
    AssistantStreaming(String),
    /// Finalized assistant text from a completed turn.
    Assistant(String),
    ToolCall {
        name: String,
        summary: String,
    },
    ToolResult {
        name: String,
        chars: usize,
        is_error: bool,
    },
    Info(String),
    Error(String),
}

/// TUI application state.
struct App {
    display: Vec<DisplayLine>,
    input: String,
    cursor_pos: usize,
    /// Current streaming turn's accumulated text, if any.
    streaming_buf: String,
    /// True while the agent is processing a turn. Does NOT disable
    /// input — the composing buffer is always editable, Enter always
    /// works. The flag controls whether Enter sends immediately
    /// (idle) or queues for next turn boundary (working). See Stage E
    /// in docs/design/native-executor-run-loop.md.
    awaiting_response: bool,
    scroll_offset: u16,
    should_quit: bool,
    model_label: String,
    /// Timestamp of the last Ctrl-C press. Used to detect double-taps
    /// for hard-cancel (SIGKILL subprocess tree) vs. single-tap
    /// cooperative cancel.
    last_ctrl_c: Option<std::time::Instant>,
}

impl App {
    fn new(model_label: String) -> Self {
        Self {
            display: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            streaming_buf: String::new(),
            awaiting_response: false,
            scroll_offset: 0,
            should_quit: false,
            model_label,
            last_ctrl_c: None,
        }
    }

    fn handle_agent_event(&mut self, ev: AgentToUi) {
        match ev {
            AgentToUi::Token(t) => {
                self.streaming_buf.push_str(&t);
                // Replace or append an AssistantStreaming line.
                if let Some(last) = self.display.last_mut()
                    && matches!(last, DisplayLine::AssistantStreaming(_))
                {
                    *last = DisplayLine::AssistantStreaming(self.streaming_buf.clone());
                    return;
                }
                self.display
                    .push(DisplayLine::AssistantStreaming(self.streaming_buf.clone()));
            }
            AgentToUi::TurnEnded => {
                if !self.streaming_buf.is_empty() {
                    // Finalize the streaming line.
                    if let Some(last) = self.display.last_mut()
                        && matches!(last, DisplayLine::AssistantStreaming(_))
                    {
                        *last = DisplayLine::Assistant(self.streaming_buf.clone());
                    }
                    self.streaming_buf.clear();
                }
                self.awaiting_response = false;
            }
            AgentToUi::ToolStart { name, summary } => {
                // Finalize any in-progress streaming text before the tool line.
                if !self.streaming_buf.is_empty() {
                    if let Some(last) = self.display.last_mut()
                        && matches!(last, DisplayLine::AssistantStreaming(_))
                    {
                        *last = DisplayLine::Assistant(self.streaming_buf.clone());
                    }
                    self.streaming_buf.clear();
                }
                self.display.push(DisplayLine::ToolCall { name, summary });
            }
            AgentToUi::ToolEnd {
                name,
                chars,
                is_error,
            } => {
                self.display.push(DisplayLine::ToolResult {
                    name,
                    chars,
                    is_error,
                });
            }
            AgentToUi::Error(msg) => {
                self.display.push(DisplayLine::Error(msg));
                self.awaiting_response = false;
            }
            AgentToUi::Info(msg) => {
                self.display.push(DisplayLine::Info(msg));
            }
        }
    }
}

/// Bundled state returned by [`build_session`] — keeps the clippy
/// `type_complexity` lint happy and makes the call site readable.
struct TuiNexSession {
    client: Box<dyn Provider>,
    tools: ToolRegistry,
    supports_tools: bool,
    system_prompt: String,
    effective_model: String,
}

/// Build the runtime provider and tool registry for the tui-nex session.
fn build_session(
    workgraph_dir: &Path,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<TuiNexSession> {
    let config = Config::load_or_default(workgraph_dir);
    let effective_model = model
        .map(String::from)
        .or_else(|| std::env::var("WG_MODEL").ok())
        .unwrap_or_else(|| config.resolve_model_for_role(DispatchRole::TaskAgent).model);

    let working_dir = std::env::current_dir().unwrap_or_default();

    let registry =
        ToolRegistry::default_all_with_config(workgraph_dir, &working_dir, &config.native_executor);

    let client = create_provider_ext(workgraph_dir, &effective_model, None, endpoint, None)?;

    let model_registry = ModelRegistry::load(workgraph_dir).unwrap_or_default();
    let supports_tools = model_registry.supports_tool_use(&effective_model);

    let system_prompt = format!(
        "You are an AI assistant in a TUI terminal session. You have tools for reading \
         and writing files, running shell commands, web search, summarizing, and delegating.\n\
         \n\
         Working directory: {}",
        working_dir.display()
    );

    Ok(TuiNexSession {
        client,
        tools: registry,
        supports_tools,
        system_prompt,
        effective_model,
    })
}

/// Run the agent task. Reads user input from `rx_input`, sends events
/// to `tx_output`. Terminates when `rx_input` is closed or a Quit
/// message arrives.
async fn run_agent_task(
    client: Box<dyn Provider>,
    tools: ToolRegistry,
    system_prompt: String,
    supports_tools: bool,
    mut rx_input: mpsc::UnboundedReceiver<UiToAgent>,
    tx_output: mpsc::UnboundedSender<AgentToUi>,
) {
    let mut messages: Vec<Message> = Vec::new();
    let tool_defs = if supports_tools {
        tools.definitions()
    } else {
        vec![]
    };
    // Inputs submitted via Enter while a turn is in flight land here
    // rather than being dropped on the floor. Drained FIFO between turns.
    let mut pending_inputs: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    'outer: loop {
        // Pick the next user message: pending queue first, else block on
        // the channel.
        let user_text = if let Some(q) = pending_inputs.pop_front() {
            q
        } else {
            match rx_input.recv().await {
                Some(UiToAgent::UserInput { text, .. }) => text,
                Some(UiToAgent::Cancel) => continue, // nothing to cancel
                Some(UiToAgent::HardCancel) => {
                    // At idle, a hard cancel kills any lingering
                    // subprocess tree (e.g. leftover bash children)
                    // and returns to idle. Rare but harmless.
                    workgraph::service::kill_descendants(std::process::id());
                    let _ = tx_output.send(AgentToUi::Info(
                        "[hard-cancel] subprocess tree killed".to_string(),
                    ));
                    continue;
                }
                Some(UiToAgent::Quit) | None => break,
            }
        };

        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: user_text }],
        });

        // Inner turn loop: run until EndTurn or interrupted.
        loop {
            let request = MessagesRequest {
                model: client.model().to_string(),
                max_tokens: client.max_tokens(),
                system: Some(system_prompt.clone()),
                messages: messages.clone(),
                tools: tool_defs.clone(),
                stream: false,
            };

            // Stream callback: forward tokens to the UI.
            let tx = tx_output.clone();
            let on_text = move |text: String| {
                let _ = tx.send(AgentToUi::Token(text));
            };

            // Race the streaming call against a Cancel from the UI.
            let streaming_future = client.send_streaming(&request, &on_text);
            let cancel_future = async {
                loop {
                    match rx_input.recv().await {
                        Some(UiToAgent::Cancel) => return UiToAgent::Cancel,
                        Some(UiToAgent::HardCancel) => return UiToAgent::HardCancel,
                        Some(UiToAgent::Quit) | None => return UiToAgent::Quit,
                        Some(UiToAgent::UserInput { text, interrupt }) => {
                            if interrupt {
                                // Ctrl-Enter during generation: abort
                                // in-flight work AND queue the message
                                // as the next user turn.
                                pending_inputs.push_back(text);
                                return UiToAgent::Cancel;
                            }
                            // Plain Enter during generation: queue, keep
                            // generating. The user is typing ahead.
                            pending_inputs.push_back(text);
                            let _ = tx_output
                                .send(AgentToUi::Info("[queued for next turn]".to_string()));
                            continue;
                        }
                    }
                }
            };

            let response = tokio::select! {
                biased;
                signal = cancel_future => {
                    match signal {
                        UiToAgent::Cancel => {
                            let _ = tx_output.send(AgentToUi::Info(
                                "[cancelled] in-flight turn aborted".to_string(),
                            ));
                        }
                        UiToAgent::HardCancel => {
                            workgraph::service::kill_descendants(std::process::id());
                            let _ = tx_output.send(AgentToUi::Info(
                                "[hard-cancel] subprocess tree killed".to_string(),
                            ));
                        }
                        _ => {
                            break 'outer;
                        }
                    }
                    let _ = tx_output.send(AgentToUi::TurnEnded);
                    break;
                }
                res = streaming_future => match res {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx_output.send(AgentToUi::Error(format!("{}", e)));
                        let _ = tx_output.send(AgentToUi::TurnEnded);
                        break;
                    }
                }
            };

            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            match response.stop_reason {
                Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                    let _ = tx_output.send(AgentToUi::TurnEnded);
                    break;
                }
                Some(StopReason::MaxTokens) => {
                    messages.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: "Your response was truncated. Please continue.".to_string(),
                        }],
                    });
                    continue;
                }
                Some(StopReason::ToolUse) => {
                    // Collect and execute tool calls sequentially for
                    // the MVP (matches delegate's mini-loop pattern).
                    let tool_use_blocks: Vec<_> = response
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, name, input } => {
                                Some((id.clone(), name.clone(), input.clone()))
                            }
                            _ => None,
                        })
                        .collect();

                    let mut results = Vec::new();
                    for (id, name, input) in &tool_use_blocks {
                        let summary = compact_tool_input_summary(input);
                        let _ = tx_output.send(AgentToUi::ToolStart {
                            name: name.clone(),
                            summary,
                        });
                        let output = tools.execute(name, input).await;
                        let _ = tx_output.send(AgentToUi::ToolEnd {
                            name: name.clone(),
                            chars: output.content.len(),
                            is_error: output.is_error,
                        });
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: output.content,
                            is_error: output.is_error,
                        });
                    }

                    messages.push(Message {
                        role: Role::User,
                        content: results,
                    });
                    // Loop back for the next turn with the tool results.
                    continue;
                }
            }
        }
    }
}

/// Produce a compact one-line summary of a tool-call input for display.
fn compact_tool_input_summary(input: &serde_json::Value) -> String {
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        format!("command={}", truncate(cmd, 60))
    } else if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
        format!("path={}", path)
    } else if let Some(pat) = input.get("pattern").and_then(|v| v.as_str()) {
        format!("pattern={}", truncate(pat, 50))
    } else if let Some(url) = input.get("url").and_then(|v| v.as_str()) {
        format!("url={}", truncate(url, 60))
    } else if let Some(src) = input.get("source").and_then(|v| v.as_str()) {
        format!("source={}", truncate(src, 60))
    } else {
        truncate(&input.to_string(), 60).to_string()
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..s.floor_char_boundary(max)]
    }
}

/// Render the app state to the terminal.
fn draw<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &App,
) -> std::io::Result<()> {
    terminal.draw(|f| -> () {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(f.area());

        let messages_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" messages — {} ", app.model_label));

        let lines: Vec<Line> = app
            .display
            .iter()
            .flat_map(|dl| render_display_line(dl))
            .collect();

        let messages_p = Paragraph::new(lines)
            .block(messages_block)
            .wrap(Wrap { trim: false })
            .scroll((app.scroll_offset, 0));
        f.render_widget(messages_p, chunks[0]);

        let input_title = if app.awaiting_response {
            " thinking... (Ctrl-C to cancel) ".to_string()
        } else {
            " input (Enter to send, Esc to quit) ".to_string()
        };
        let input_block = Block::default().borders(Borders::ALL).title(input_title);
        let input_p = Paragraph::new(app.input.as_str()).block(input_block);
        f.render_widget(input_p, chunks[1]);

        // Set cursor position inside the input box.
        if !app.awaiting_response {
            let cursor_x = chunks[1].x + 1 + app.cursor_pos as u16;
            let cursor_y = chunks[1].y + 1;
            f.set_cursor_position((cursor_x, cursor_y));
        }

        let hint = Line::from(vec![
            Span::styled("Ctrl-C", Style::default().fg(Color::Yellow)),
            Span::raw(" cancel turn  "),
            Span::styled("Esc/Ctrl-Q", Style::default().fg(Color::Yellow)),
            Span::raw(" quit  "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(" send"),
        ]);
        f.render_widget(Paragraph::new(hint), chunks[2]);
    })
    .map_err(|e| std::io::Error::other(format!("draw failed: {:?}", e)))?;
    Ok(())
}

fn render_display_line(dl: &DisplayLine) -> Vec<Line<'static>> {
    match dl {
        DisplayLine::User(text) => wrap_role("user", text, Color::Cyan),
        DisplayLine::Assistant(text) => wrap_role("assistant", text, Color::Green),
        DisplayLine::AssistantStreaming(text) => wrap_role("assistant", text, Color::Green),
        DisplayLine::ToolCall { name, summary } => vec![Line::from(vec![
            Span::styled("  → ", Style::default().fg(Color::Magenta)),
            Span::styled(name.clone(), Style::default().fg(Color::Magenta)),
            Span::raw("("),
            Span::styled(summary.clone(), Style::default().fg(Color::DarkGray)),
            Span::raw(")"),
        ])],
        DisplayLine::ToolResult {
            name,
            chars,
            is_error,
        } => {
            let (marker, color) = if *is_error {
                ("  ✗", Color::Red)
            } else {
                ("  ✓", Color::Green)
            };
            vec![Line::from(vec![
                Span::styled(marker, Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(name.clone(), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(" ({} chars)", chars),
                    Style::default().fg(Color::DarkGray),
                ),
            ])]
        }
        DisplayLine::Info(text) => vec![Line::from(Span::styled(
            text.clone(),
            Style::default().fg(Color::Yellow),
        ))],
        DisplayLine::Error(text) => vec![Line::from(Span::styled(
            text.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))],
    }
}

fn wrap_role(role_label: &str, text: &str, color: Color) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    out.push(Line::from(Span::styled(
        format!("{}:", role_label),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    for line in text.lines() {
        out.push(Line::from(Span::raw(format!("  {}", line))));
    }
    out.push(Line::from(Span::raw("")));
    out
}

/// Public entry point called from main.rs.
pub fn run(workgraph_dir: &Path, model: Option<&str>, endpoint: Option<&str>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    rt.block_on(run_async(workgraph_dir, model, endpoint))
}

async fn run_async(
    workgraph_dir: &Path,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<()> {
    let TuiNexSession {
        client,
        tools,
        supports_tools,
        system_prompt,
        effective_model,
    } = build_session(workgraph_dir, model, endpoint)?;

    // Set up terminal.
    enable_raw_mode().context("enable_raw_mode failed")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("terminal enter failed")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("terminal init failed")?;

    // Channels.
    let (tx_input, rx_input) = mpsc::unbounded_channel::<UiToAgent>();
    let (tx_output, mut rx_output) = mpsc::unbounded_channel::<AgentToUi>();

    // Spawn agent task.
    let agent_handle = tokio::spawn(run_agent_task(
        client,
        tools,
        system_prompt,
        supports_tools,
        rx_input,
        tx_output,
    ));

    let mut app = App::new(effective_model);
    app.display.push(DisplayLine::Info(format!(
        "wg tui-nex — interactive session with {}. Type a message and press Enter.",
        app.model_label
    )));

    // Main event loop.
    let result: Result<()> = loop {
        if let Err(e) = draw(&mut terminal, &app) {
            break Err(anyhow::anyhow!("draw failed: {}", e));
        }

        // Drain any pending agent events without blocking.
        while let Ok(ev) = rx_output.try_recv() {
            app.handle_agent_event(ev);
        }

        // Poll for input events with a short timeout so we re-render
        // when agent events arrive.
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let modifiers = key.modifiers;
                    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
                    let shift = modifiers.contains(KeyModifiers::SHIFT);
                    match key.code {
                        KeyCode::Esc => {
                            app.should_quit = true;
                        }
                        KeyCode::Char('q') if ctrl => {
                            app.should_quit = true;
                        }
                        KeyCode::Char('c') if ctrl => {
                            let now = std::time::Instant::now();
                            let is_double_tap = app
                                .last_ctrl_c
                                .map(|t| {
                                    now.duration_since(t)
                                        <= workgraph::executor::native::cancel::DOUBLE_TAP_WINDOW
                                })
                                .unwrap_or(false);

                            if app.awaiting_response {
                                if is_double_tap {
                                    let _ = tx_input.send(UiToAgent::HardCancel);
                                    app.last_ctrl_c = None;
                                } else {
                                    let _ = tx_input.send(UiToAgent::Cancel);
                                    app.last_ctrl_c = Some(now);
                                }
                            } else if is_double_tap {
                                // Two Ctrl-Cs at idle → quit (matches most
                                // REPLs where Ctrl-C on empty prompt is a
                                // no-op on the first and exit on the
                                // second).
                                app.should_quit = true;
                            } else {
                                app.last_ctrl_c = Some(now);
                            }
                        }
                        KeyCode::Enter => {
                            if !app.input.trim().is_empty() {
                                let text = std::mem::take(&mut app.input);
                                app.cursor_pos = 0;
                                app.display.push(DisplayLine::User(text.clone()));
                                let interrupt = ctrl;
                                if !app.awaiting_response {
                                    app.awaiting_response = true;
                                }
                                // Always send — the agent task queues mid-turn
                                // inputs and delivers them at the next turn
                                // boundary. See run_agent_task for the pending
                                // queue logic.
                                let _ = tx_input.send(UiToAgent::UserInput { text, interrupt });
                            } else if shift {
                                // Shift-Enter on empty input: harmless, lets
                                // the muscle-memory work when the user reaches
                                // for it by accident.
                            }
                        }
                        KeyCode::Backspace => {
                            if app.cursor_pos > 0 {
                                let new_pos = app.cursor_pos - 1;
                                app.input.remove(new_pos);
                                app.cursor_pos = new_pos;
                            }
                        }
                        KeyCode::Left => {
                            if app.cursor_pos > 0 {
                                app.cursor_pos -= 1;
                            }
                        }
                        KeyCode::Right => {
                            if app.cursor_pos < app.input.len() {
                                app.cursor_pos += 1;
                            }
                        }
                        KeyCode::Home => {
                            app.cursor_pos = 0;
                        }
                        KeyCode::End => {
                            app.cursor_pos = app.input.len();
                        }
                        KeyCode::PageUp => {
                            app.scroll_offset = app.scroll_offset.saturating_sub(5);
                        }
                        KeyCode::PageDown => {
                            app.scroll_offset = app.scroll_offset.saturating_add(5);
                        }
                        KeyCode::Char(c) => {
                            app.input.insert(app.cursor_pos, c);
                            app.cursor_pos += 1;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        if app.should_quit {
            let _ = tx_input.send(UiToAgent::Quit);
            break Ok(());
        }
    };

    // Tear down terminal regardless of success.
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = terminal.show_cursor();

    // Wait briefly for agent task to drain.
    let _ = tokio::time::timeout(Duration::from_secs(2), agent_handle).await;

    result
}

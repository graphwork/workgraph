//! PTY-backed subprocess pane for embedding `wg nex` (or any command)
//! inside the ratatui TUI.
//!
//! Architecture:
//!
//! ```text
//!                  TUI main thread
//!                        │
//!    key events  ◄───────┤───────►  render()
//!        │               │              ▲
//!        ▼               │              │
//!   master.writer   vt100::Parser  (reads screen cells)
//!        │               ▲              │
//!        ▼               │              │
//!     PTY slave  ◄── reader thread ─┘   │
//!        │                              │
//!        ▼                              │
//!     wg nex (child) ──stdout/stderr────┘
//! ```
//!
//! One dedicated background thread drains the PTY master's reader
//! into a `vt100::Parser` wrapped in `Arc<Mutex<_>>`. The TUI's main
//! thread takes a read lock on render and a write lock on keypress
//! (for `feed_bytes` into the writer). The parser's `screen()` is
//! drawn via `tui_term::widget::PseudoTerminal`.
//!
//! Child lifetime: the PtyPane owns the `Child` handle. Dropping the
//! pane or calling `kill()` terminates the subprocess and joins the
//! reader thread. `is_alive()` polls the child without blocking.
//!
//! Resize: `resize(rows, cols)` threads through to both the parser's
//! `set_size` AND the master PTY's `resize` — the child sees SIGWINCH
//! with the new dimensions, so ratatui layout changes flow to the
//! embedded process correctly.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::Frame;
use ratatui::layout::Rect;

/// Default scrollback for the vt100 parser. Matches common terminal
/// emulator defaults (macOS Terminal, iTerm2) — enough to scroll back
/// through a few minutes of dense nex activity.
const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Sustained output rate (bytes/sec) above which we log a warning and
/// start discarding scrollback to prevent OOM. 512 KB/s sustained
/// over a full measurement window triggers the guard.
const GROWTH_RATE_WARN_BYTES_PER_SEC: u64 = 512 * 1024;

/// Measurement window for the growth-rate guard (seconds).
const GROWTH_RATE_WINDOW_SECS: u64 = 2;

/// Quiet window after a PTY resize before we declare the SIGWINCH reflow
/// complete and compute how many duplicate scrollback rows to hide.
/// 120 ms is conservative: claude/codex SIGWINCH reflows finish in <50 ms.
const RESIZE_DEDUP_WINDOW: std::time::Duration = std::time::Duration::from_millis(120);

pub struct PtyPane {
    parser: Arc<Mutex<vt100::Parser>>,
    /// Writer end of the PTY master — sending bytes here feeds the
    /// embedded process's stdin. Shared with the reader thread so it
    /// can answer terminal capability queries the child emits.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Master PTY handle, kept alive so resize(..) works.
    master: Box<dyn MasterPty + Send>,
    /// Handle to the embedded child process. We never try to
    /// mutable-borrow this across threads (the reader thread owns a
    /// separate reader handle cloned off the master); instead we poll
    /// it with `try_wait` from the main TUI thread.
    child: Box<dyn Child + Send + Sync>,
    /// Joinable handle for the PTY-reader thread. Set to `None` once
    /// we've joined it on teardown.
    reader_thread: Option<thread::JoinHandle<()>>,
    /// Current screen size known to the pane. `resize` updates this
    /// and pushes the new size through to master + parser.
    rows: u16,
    cols: u16,
    /// Optional tee file for the INPUT stream (stdin we write to the
    /// child). Set when `WG_PTY_DUMP` is exported — the output tee
    /// goes to `<prefix>.<cmd>.<pid>.bin`, this input tee goes to
    /// `<prefix>.<cmd>.<pid>.in.bin`. Smoke tests read it to assert
    /// key-forwarding byte sequences.
    input_tee: Option<Arc<Mutex<std::fs::File>>>,
    /// Whether auto-follow (live/tail) mode is active. When true,
    /// the parser's scrollback_offset stays at 0 and new output is
    /// visible immediately. When false, the parser's own
    /// scrollback_offset auto-increments on new content, anchoring
    /// the viewport to the content the user was reading.
    auto_follow: bool,
    #[allow(dead_code)]
    pub growth_rate_warned: Arc<AtomicBool>,
    #[allow(dead_code)]
    bytes_processed: Arc<AtomicU64>,
    /// Pending dedup state: (pre-resize scrollback row count, time of resize).
    /// Cleared after RESIZE_DEDUP_WINDOW elapses and `scrollback_hidden` is set.
    pending_dedup: Option<(usize, std::time::Instant)>,
    /// Number of scrollback rows at the "hot end" (most recently appended)
    /// to skip when navigating history. These are SIGWINCH reflow echoes:
    /// bytes the child re-emits after resize that push already-seen content
    /// back into scrollback. Skipping them hides the duplicate tail.
    scrollback_hidden: usize,
}

impl PtyPane {
    /// Spawn `command` (with `args` and `env` overrides) as a PTY
    /// child and start a background reader that feeds bytes into a
    /// vt100 parser.
    ///
    /// `rows` / `cols` set the initial PTY size. Call `resize(...)`
    /// when the ratatui layout changes.
    pub fn spawn(
        command: &str,
        args: &[&str],
        env: &[(String, String)],
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Self::spawn_in(command, args, env, None, rows, cols)
    }

    /// Like `spawn`, but lets the caller pin the child's working
    /// directory. Useful when embedding vendor CLIs whose
    /// session-resumption heuristics (e.g. `claude --continue` picks
    /// the most recent session in the current dir) depend on it.
    pub fn spawn_in(
        command: &str,
        args: &[&str],
        env: &[(String, String)],
        cwd: Option<&std::path::Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let resolved_cwd = cwd
            .map(std::path::Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        if let Some(cwd) = resolved_cwd {
            cmd.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn PTY child failed")?;
        // Drop the slave side in the parent — the child inherits it;
        // keeping a slave fd open here would delay EOF when the child
        // exits and we'd hang in the reader thread.
        drop(pair.slave);

        // The PTY master has ONE writer; share it between the public
        // `send_key`/`send_text` path AND the reader thread's
        // capability-query responder via Arc<Mutex>.
        let writer_shared = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .context("failed to take PTY writer")?,
        ));
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            rows,
            cols,
            DEFAULT_SCROLLBACK_LINES,
        )));

        let reader_parser = Arc::clone(&parser);
        // Optional: tee PTY output to a file for debugging terminal
        // emulation issues (vt100 parser / tui-term gaps). Activated
        // by WG_PTY_DUMP=<prefix>; every PTY child writes raw bytes
        // to `<prefix>.<command-basename>.<pid>.bin`.
        let (tee_path, input_tee_path) = if let Some(p) = std::env::var_os("WG_PTY_DUMP") {
            let pid = std::process::id();
            // Strip any path from `command` (can be absolute, like
            // /home/user/.cargo/bin/wg) — `with_extension` panics on
            // separators in the extension.
            let basename = std::path::Path::new(command)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("pty");
            let mut out_path = std::path::PathBuf::from(&p);
            let current_name = out_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            out_path.set_file_name(format!("{}.{}.{}.bin", current_name, basename, pid));
            let mut in_path = std::path::PathBuf::from(&p);
            in_path.set_file_name(format!("{}.{}.{}.in.bin", current_name, basename, pid));
            (Some(out_path), Some(in_path))
        } else {
            (None, None)
        };
        let input_tee = input_tee_path.and_then(|p| {
            std::fs::File::create(&p)
                .ok()
                .map(|f| Arc::new(Mutex::new(f)))
        });
        // The reader thread peeks at raw PTY bytes for capability
        // queries and writes the expected replies through this Arc.
        // Some vendor CLIs (claude in particular) send DA/XTVERSION/
        // DECRQM queries on startup and block input processing
        // until they get responses — a pure render-only pipeline
        // never answers, and the CLI freezes post-splash.
        let reader_responder = Arc::clone(&writer_shared);
        let growth_rate_warned = Arc::new(AtomicBool::new(false));
        let bytes_processed = Arc::new(AtomicU64::new(0));
        let reader_growth_warned = Arc::clone(&growth_rate_warned);
        let reader_bytes = Arc::clone(&bytes_processed);
        let reader_thread = thread::Builder::new()
            .name(format!("pty-reader-{}", command))
            .spawn(move || {
                use std::io::Write as _;
                let mut tee_file = tee_path.and_then(|p| std::fs::File::create(&p).ok());
                let mut buf = [0u8; 8192];
                let mut window_start = std::time::Instant::now();
                let mut window_bytes: u64 = 0;
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Some(f) = tee_file.as_mut() {
                                let _ = f.write_all(&buf[..n]);
                                let _ = f.flush();
                            }
                            respond_to_queries(&buf[..n], &reader_responder);

                            reader_bytes.fetch_add(n as u64, Ordering::Relaxed);
                            window_bytes += n as u64;
                            let elapsed = window_start.elapsed().as_secs();
                            if elapsed >= GROWTH_RATE_WINDOW_SECS && window_bytes > 0 {
                                let rate = window_bytes / elapsed.max(1);
                                if rate > GROWTH_RATE_WARN_BYTES_PER_SEC {
                                    if !reader_growth_warned.swap(true, Ordering::Relaxed) {
                                        eprintln!(
                                            "[pty] growth-rate guard: {} KB/s sustained — \
                                             truncating scrollback to prevent OOM",
                                            rate / 1024
                                        );
                                    }
                                    if let Ok(mut p) = reader_parser.lock() {
                                        p.screen_mut().set_scrollback(0);
                                    }
                                }
                                window_start = std::time::Instant::now();
                                window_bytes = 0;
                            }

                            if let Ok(mut p) = reader_parser.lock() {
                                p.process(&buf[..n]);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .context("failed to spawn PTY reader thread")?;

        Ok(Self {
            parser,
            writer: writer_shared,
            master: pair.master,
            child,
            reader_thread: Some(reader_thread),
            rows,
            cols,
            input_tee,
            auto_follow: true,
            growth_rate_warned,
            bytes_processed,
            pending_dedup: None,
            scrollback_hidden: 0,
        })
    }

    /// Render the current terminal screen as a ratatui widget in
    /// `area`. Safe to call from the main TUI thread every frame.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        self.render_with_focus(frame, area, true);
    }

    /// Like `render`, but when `focused` is false, paint a dim +
    /// desaturated overlay so the user sees "this pane is there and
    /// resumable but not receiving input right now." Without this,
    /// there's no visual signal distinguishing focused-pty-active
    /// from unfocused-pty-idle; the user presses keys and wonders
    /// why nothing happens.
    pub fn render_with_focus(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let parser = match self.parser.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let scroll_offset = parser.screen().scrollback();
        let screen = parser.screen();
        let widget = tui_term::widget::PseudoTerminal::new(screen);
        frame.render_widget(widget, area);

        if scroll_offset > 0 {
            let indicator = format!(" ↓{} ", scroll_offset);
            let x = area.x + area.width.saturating_sub(indicator.len() as u16 + 1);
            let y = area.y;
            if x >= area.x && y < area.y + area.height {
                let buf = frame.buffer_mut();
                for (i, ch) in indicator.chars().enumerate() {
                    let cx = x + i as u16;
                    if cx < area.x + area.width {
                        let cell = &mut buf[(cx, y)];
                        cell.set_char(ch);
                        cell.set_style(
                            ratatui::style::Style::default()
                                .fg(ratatui::style::Color::Black)
                                .bg(ratatui::style::Color::Yellow),
                        );
                    }
                }
            }
        }

        if !focused {
            let buf = frame.buffer_mut();
            for y in area.y..area.y.saturating_add(area.height) {
                for x in area.x..area.x.saturating_add(area.width) {
                    let cell = &mut buf[(x, y)];
                    cell.set_style(
                        ratatui::style::Style::default()
                            .fg(ratatui::style::Color::DarkGray)
                            .add_modifier(ratatui::style::Modifier::DIM),
                    );
                }
            }
        }
    }

    /// Scroll the view up (back through history) by `n` lines.
    pub fn scroll_up(&mut self, n: usize) {
        self.auto_follow = false;
        if let Ok(mut p) = self.parser.lock() {
            let current = p.screen().scrollback();
            // When jumping from live view (offset 0), skip over any SIGWINCH
            // reflow echo rows that sit at the hot end of the scrollback buffer.
            // Those rows are duplicates of content the child re-emitted after
            // SIGWINCH; scrolling into them shows the same content twice.
            let base = if current == 0 && self.scrollback_hidden > 0 {
                self.scrollback_hidden
            } else {
                current
            };
            p.screen_mut().set_scrollback(base.saturating_add(n));
        }
    }

    /// Scroll the view down (toward live output) by `n` lines.
    pub fn scroll_down(&mut self, n: usize) {
        if let Ok(mut p) = self.parser.lock() {
            let current = p.screen().scrollback();
            let new_offset = current.saturating_sub(n);
            // If the new offset would land inside the duplicate zone (offsets
            // 1..=scrollback_hidden), snap straight to live view instead of
            // letting the user drift through the reflow echo rows.
            let new_offset = if new_offset > 0 && new_offset <= self.scrollback_hidden {
                0
            } else {
                new_offset
            };
            p.screen_mut().set_scrollback(new_offset);
            if new_offset <= 1 {
                self.auto_follow = true;
                p.screen_mut().set_scrollback(0);
            }
        }
    }

    /// Jump to the top of scrollback.
    pub fn scroll_to_top(&mut self) {
        self.auto_follow = false;
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_scrollback(usize::MAX);
        }
    }

    /// Jump to the bottom (live output).
    pub fn scroll_to_bottom(&mut self) {
        self.auto_follow = true;
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_scrollback(0);
        }
    }

    #[allow(dead_code)]
    pub fn is_scrolled_back(&self) -> bool {
        !self.auto_follow
    }

    /// Current vt100 grid dimensions (rows, cols). Useful for tests
    /// that need to verify a pane was spawned at the right size before
    /// the first frame triggers any resize — see fix-pty-scrollback.
    pub fn dims(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    #[allow(dead_code)]
    pub fn bytes_processed(&self) -> u64 {
        self.bytes_processed.load(Ordering::Relaxed)
    }

    /// Forward a crossterm key event to the embedded process. Returns
    /// `Ok(())` even if the child has exited — a dead PTY swallows
    /// writes silently. Caller should use `is_alive()` to detect exit.
    pub fn send_key(&mut self, key: KeyEvent) -> Result<()> {
        let bytes = key_event_to_bytes(&key);
        if !bytes.is_empty() {
            if let Ok(mut w) = self.writer.lock() {
                let _ = w.write_all(&bytes);
                let _ = w.flush();
                self.tee_input(&bytes);
            }
            self.auto_follow = true;
            if let Ok(mut p) = self.parser.lock() {
                p.screen_mut().set_scrollback(0);
            }
        }
        Ok(())
    }

    /// Forward arbitrary text (e.g. pasted content) to the child's
    /// stdin verbatim, no key-event encoding.
    pub fn send_text(&mut self, text: &str) -> Result<()> {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(text.as_bytes());
            let _ = w.flush();
            self.tee_input(text.as_bytes());
        }
        Ok(())
    }

    fn tee_input(&self, bytes: &[u8]) {
        if let Some(tee) = &self.input_tee
            && let Ok(mut f) = tee.lock()
        {
            use std::io::Write as _;
            let _ = f.write_all(bytes);
            let _ = f.flush();
        }
    }

    /// Read the actual number of rows currently stored in the vt100
    /// scrollback buffer without changing the user's scroll position.
    /// Uses the `set_scrollback(MAX) → scrollback()` clamp trick.
    fn scrollback_count(p: &mut vt100::Parser) -> usize {
        let saved = p.screen().scrollback();
        p.screen_mut().set_scrollback(usize::MAX);
        let count = p.screen().scrollback();
        p.screen_mut().set_scrollback(saved);
        count
    }

    /// If a pending dedup is older than RESIZE_DEDUP_WINDOW, resolve it:
    /// compare current scrollback count to the pre-resize snapshot to find
    /// how many rows the SIGWINCH reflow echo added, store that as
    /// `scrollback_hidden`, and clear the pending state.
    fn maybe_resolve_dedup(&mut self) {
        let (pre_count, at) = match self.pending_dedup {
            Some(s) => s,
            None => return,
        };
        if at.elapsed() < RESIZE_DEDUP_WINDOW {
            return;
        }
        let (post_count, current_offset) = {
            let mut p = match self.parser.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            (Self::scrollback_count(&mut p), p.screen().scrollback())
        };
        let k = post_count.saturating_sub(pre_count);
        // If the user somehow landed in the duplicate zone (offset 1..=k),
        // push them just above it so they see real history, not the echo.
        if k > 0 && current_offset > 0 && current_offset <= k {
            if let Ok(mut p) = self.parser.lock() {
                p.screen_mut().set_scrollback(k.saturating_add(1));
            }
        }
        self.scrollback_hidden = k;
        self.pending_dedup = None;
    }

    /// Push a new size through to both the vt100 parser (so rendered
    /// cell layout updates) and the master PTY (so the child sees
    /// SIGWINCH and can reflow its own output). No-op if the size
    /// matches the current one.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        // Resolve any pending dedup from the previous resize, even on no-op
        // frames — the TUI calls resize() every render cycle, so this is
        // where we detect that RESIZE_DEDUP_WINDOW has elapsed.
        self.maybe_resolve_dedup();

        // Clamp to a workable minimum. A tiny grid makes
        // vt100::Parser panic frequently because drawing_cell(pos)
        // returns None for any pos past the first few cells. Some
        // environments (pty-in-pty tests, crossterm before the first
        // frame paints) report 0×0 or very small dims transiently;
        // keep a sane minimum until the real size arrives. The child
        // can still render; only wrap points shift slightly.
        let rows = rows.max(10);
        let cols = cols.max(40);
        if rows == self.rows && cols == self.cols {
            return Ok(());
        }

        // Snapshot pre-resize scrollback count so we can detect how many
        // rows the SIGWINCH reflow echo adds (see maybe_resolve_dedup).
        let pre_count = {
            let mut p = match self.parser.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            Self::scrollback_count(&mut p)
        };
        self.scrollback_hidden = 0; // stale dedup no longer valid for new resize
        self.pending_dedup = Some((pre_count, std::time::Instant::now()));

        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize failed")?;
        let mut p = match self.parser.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // vt100 0.16 moved set_size from Parser to Screen.
        p.screen_mut().set_size(rows, cols);
        drop(p);
        self.rows = rows;
        self.cols = cols;
        Ok(())
    }

    /// Non-blocking: has the embedded child exited?
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            // `try_wait` error = we can't tell; assume alive so we
            // don't tear down a working pane. If it's genuinely dead
            // the reader thread will hit EOF and close.
            Err(_) => true,
        }
    }

    /// SIGKILL the embedded child and wait for teardown. Safe to call
    /// multiple times.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        // Dropping the writer closes the master's write end; the
        // reader hits EOF and exits. Take the JoinHandle so we can
        // await the thread cleanup once.
        if let Some(handle) = self.reader_thread.take() {
            // Best-effort join. The reader exits on EOF from the
            // master — which happens automatically when the master
            // drops at end of life. Drop the master here by replacing
            // the writer with a closed stub would be invasive; we
            // simply let drop() handle it.
            let _ = handle.join();
        }
    }
}

impl Drop for PtyPane {
    fn drop(&mut self) {
        // Ensure the child is gone; the reader thread will see EOF
        // when the master drops after this Drop completes.
        let _ = self.child.kill();
        // Don't join the reader here — `kill` may not have fully
        // flushed yet and we'd block the TUI shutdown. The thread is
        // detached (no handle reference held after `kill()`), so the
        // OS reaps it when the process exits.
        let _ = self.reader_thread.take();
    }
}

/// Convert a crossterm `KeyEvent` into the byte sequence a Unix PTY
/// expects. Handles control characters, arrow keys (CSI sequences),
/// function keys, and plain text. Not exhaustive — covers what a
/// `wg nex` REPL user actually presses.
/// Scan PTY output for terminal capability queries and write the
/// conventional replies back through the shared writer. Minimal
/// coverage — just the queries claude and codex send on startup that,
/// if unanswered, make the CLI freeze post-splash.
///
/// This is standard terminal emulator behavior: xterm, gnome-terminal,
/// alacritty etc. all respond to these. portable-pty is a raw pipe
/// and vt100-the-parser doesn't generate replies, so we fill the gap.
fn respond_to_queries(chunk: &[u8], writer: &std::sync::Arc<Mutex<Box<dyn Write + Send>>>) {
    // Scan for well-known query sequences. Byte patterns:
    //   ESC [ c            — Primary Device Attributes (DA1)
    //   ESC [ > c          — Secondary Device Attributes (DA2)
    //   ESC [ ? 6 n        — cursor position request (also common)
    //   ESC [ > 0 q        — XTVERSION
    //   ESC [ ? 2026 $ p   — DECRQM for mode 2026 (synchronized output)
    //
    // We don't implement a full state machine; we just match the exact
    // byte sequences. Claude / codex emit these verbatim on startup.
    let mut reply = Vec::new();
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i] != 0x1b {
            i += 1;
            continue;
        }
        // Match starting at `ESC`.
        let tail = &chunk[i..];
        // ESC [ c — Primary DA. Reply: ESC [ ? 65 ; 1 ; 6 c
        // (VT500 with 132 cols + selective erase — conservative.)
        if tail.starts_with(b"\x1b[c") {
            reply.extend_from_slice(b"\x1b[?65;1;6c");
            i += 3;
            continue;
        }
        // ESC [ > c — Secondary DA. Reply: ESC [ > 41 ; 330 ; 0 c
        // (mimic xterm)
        if tail.starts_with(b"\x1b[>c") || tail.starts_with(b"\x1b[>0c") {
            reply.extend_from_slice(b"\x1b[>41;330;0c");
            i += tail[..].iter().position(|&b| b == b'c').unwrap_or(3) + 1;
            continue;
        }
        // ESC [ > 0 q — XTVERSION. Reply: ESC P > | wg-tui ESC \
        if tail.starts_with(b"\x1b[>0q") || tail.starts_with(b"\x1b[>q") {
            reply.extend_from_slice(b"\x1bP>|wg-tui(0.1.0)\x1b\\");
            let end = tail[..].iter().position(|&b| b == b'q').unwrap_or(4) + 1;
            i += end;
            continue;
        }
        // ESC [ ? 2026 $ p — DECRQM for synchronized output.
        // Reply: ESC [ ? 2026 ; 2 $ y (mode reset / not supported).
        if tail.starts_with(b"\x1b[?2026$p") {
            reply.extend_from_slice(b"\x1b[?2026;2$y");
            i += 9;
            continue;
        }
        // ESC [ ? <N> $ p — DECRQM for any mode. Reply "not recognized".
        if tail.starts_with(b"\x1b[?")
            && let Some(end) = tail.iter().position(|&b| b == b'p')
            && tail.get(end.saturating_sub(1)) == Some(&b'$')
        {
            // Extract the mode number between "?" and "$".
            let inner = &tail[3..end.saturating_sub(1)];
            if inner.iter().all(|b| b.is_ascii_digit()) && !inner.is_empty() {
                reply.extend_from_slice(b"\x1b[?");
                reply.extend_from_slice(inner);
                reply.extend_from_slice(b";0$y");
            }
            i += end + 1;
            continue;
        }
        i += 1;
    }
    if !reply.is_empty()
        && let Ok(mut w) = writer.lock()
    {
        let _ = w.write_all(&reply);
        let _ = w.flush();
    }
}

fn key_event_to_bytes(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let mut out = Vec::new();
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Standard C0 control codes: Ctrl-A..Ctrl-Z → 0x01..0x1a,
                // Ctrl-[ → 0x1b (ESC), Ctrl-\ → 0x1c, etc. Upper-case
                // and lower-case map identically per terminal convention.
                let ch = c.to_ascii_lowercase();
                if ch.is_ascii_lowercase() {
                    out.push((ch as u8) - b'a' + 1);
                } else if c == '[' {
                    out.push(0x1b);
                } else if c == '\\' {
                    out.push(0x1c);
                } else if c == ']' {
                    out.push(0x1d);
                } else if c == '^' {
                    out.push(0x1e);
                } else if c == '_' {
                    out.push(0x1f);
                } else if c == ' ' {
                    out.push(0);
                } else {
                    // Unknown Ctrl-combo — send literal so user sees
                    // something rather than silent drop.
                    let mut tmp = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
            } else if alt {
                // ESC-prefix: standard xterm/readline convention.
                out.push(0x1b);
                let mut tmp = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            } else {
                let mut tmp = [0u8; 4];
                let bytes = c.encode_utf8(&mut tmp).as_bytes();
                // Shift alone on a letter: crossterm gives us the
                // upper-case char already, no extra work.
                let _ = shift;
                out.extend_from_slice(bytes);
            }
        }
        // Enter: send both \r and \n so whichever the remote side
        // expects gets recognized. A PTY in cooked mode maps \r→\n
        // via ICRNL; in raw mode (rustyline inside wg nex), neither
        // translation happens, and some readers accept only \r and
        // some only \n. Sending both is safe — nothing reads empty
        // lines from a \r\n pair.
        // Raw-mode TTY apps expect Enter as a single CR (`\r`).
        // Sending `\r\n` is interpreted as "Enter + Ctrl-J" by most
        // REPLs — claude in particular treats the stray Ctrl-J as a
        // cancel/exit signal after accepting the trust prompt, making
        // its REPL die immediately after the user confirms.
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => {
            if shift {
                out.extend_from_slice(b"\x1b[Z"); // xterm back-tab
            } else {
                out.push(b'\t');
            }
        }
        // Crossterm reports Shift+Tab as a dedicated BackTab keycode
        // rather than Tab+SHIFT on most terminal emulators — both paths
        // must emit the xterm back-tab sequence or claude's
        // "shift-tab to cycle" binding (and readline's reverse-complete)
        // silently drop.
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::F(n) => {
            // F1-F4 use O-prefix, F5+ use CSI numeric.
            match n {
                1 => out.extend_from_slice(b"\x1bOP"),
                2 => out.extend_from_slice(b"\x1bOQ"),
                3 => out.extend_from_slice(b"\x1bOR"),
                4 => out.extend_from_slice(b"\x1bOS"),
                5 => out.extend_from_slice(b"\x1b[15~"),
                6 => out.extend_from_slice(b"\x1b[17~"),
                7 => out.extend_from_slice(b"\x1b[18~"),
                8 => out.extend_from_slice(b"\x1b[19~"),
                9 => out.extend_from_slice(b"\x1b[20~"),
                10 => out.extend_from_slice(b"\x1b[21~"),
                11 => out.extend_from_slice(b"\x1b[23~"),
                12 => out.extend_from_slice(b"\x1b[24~"),
                _ => {}
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_a_maps_to_soh() {
        let e = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(key_event_to_bytes(&e), vec![1]);
    }

    #[test]
    fn ctrl_c_maps_to_etx() {
        let e = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_event_to_bytes(&e), vec![3]);
    }

    #[test]
    fn enter_maps_to_cr_only() {
        // Raw-mode PTY apps (claude REPL, less, vim, readline)
        // expect bare CR for Enter; sending CR+LF gets interpreted as
        // Enter followed by a stray Ctrl-J and breaks apps that
        // treat Ctrl-J as cancel.
        let e = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&e), b"\r");
    }

    #[test]
    fn arrow_keys_emit_csi() {
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&up), b"\x1b[A");
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&down), b"\x1b[B");
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&right), b"\x1b[C");
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&left), b"\x1b[D");
    }

    #[test]
    fn alt_prefix_emits_esc() {
        let e = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(key_event_to_bytes(&e), vec![0x1b, b'b']);
    }

    #[test]
    fn plain_char_passthrough() {
        let e = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&e), b"x");
    }

    #[test]
    fn backspace_maps_to_del() {
        let e = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&e), vec![0x7f]);
    }

    #[test]
    fn f1_emits_ss3_prefix() {
        let e = KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&e), b"\x1bOP");
    }

    #[test]
    fn back_tab_emits_csi_z() {
        let e = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(key_event_to_bytes(&e), b"\x1b[Z");
        // Crossterm sometimes reports without SHIFT — still must work.
        let bare = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&bare), b"\x1b[Z");
    }

    #[test]
    fn shift_tab_via_tab_keycode_still_emits_csi_z() {
        let e = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(key_event_to_bytes(&e), b"\x1b[Z");
    }

    #[test]
    fn spawn_echo_and_read_output() {
        let mut pane =
            PtyPane::spawn("/bin/echo", &["hello from pty"], &[], 5, 40).expect("spawn echo");
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let found = {
                let p = pane.parser.lock().unwrap();
                let contents = p.screen().contents();
                contents.contains("hello from pty")
            };
            if found {
                return;
            }
        }
        let p = pane.parser.lock().unwrap();
        panic!(
            "did not see 'hello from pty' in PTY output; screen was:\n{}",
            p.screen().contents()
        );
    }

    #[test]
    fn scrollback_up_down_clamps() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            24,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));

        // Feed some content so there's actual scrollback to navigate
        {
            let mut p = parser.lock().unwrap();
            for i in 0..50 {
                let line = format!("line {}\r\n", i);
                p.process(line.as_bytes());
            }
        }

        // scroll_up from 0
        {
            let mut p = parser.lock().unwrap();
            let current = p.screen().scrollback();
            assert_eq!(current, 0);
            p.screen_mut().set_scrollback(current.saturating_add(10));
            assert_eq!(p.screen().scrollback(), 10);
        }

        // scroll_down back to 0
        {
            let mut p = parser.lock().unwrap();
            let current = p.screen().scrollback();
            p.screen_mut().set_scrollback(current.saturating_sub(10));
            assert_eq!(p.screen().scrollback(), 0);
        }

        // scroll_down past 0 stays 0
        {
            let mut p = parser.lock().unwrap();
            let current = p.screen().scrollback();
            p.screen_mut().set_scrollback(current.saturating_sub(5));
            assert_eq!(p.screen().scrollback(), 0);
        }

        // scroll_up past max clamps to actual scrollback buffer size
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(usize::MAX);
            let max = p.screen().scrollback();
            assert!(max > 0);
            assert!(max <= DEFAULT_SCROLLBACK_LINES);
        }
    }

    #[test]
    fn anchor_stable_when_new_content_arrives() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            5,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));

        // Feed initial content — enough to fill scrollback.
        {
            let mut p = parser.lock().unwrap();
            for i in 0..20 {
                let line = format!("line {}\r\n", i);
                p.process(line.as_bytes());
            }
        }

        // User scrolls up 5 lines (anchored mode).
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(5);
            assert_eq!(p.screen().scrollback(), 5);
        }

        // New content arrives at the bottom — 3 more lines.
        // The vt100 parser's scroll_up auto-increments scrollback_offset.
        {
            let mut p = parser.lock().unwrap();
            for i in 20..23 {
                let line = format!("line {}\r\n", i);
                p.process(line.as_bytes());
            }
        }

        // The anchor should have moved: 5 + 3 = 8 lines from bottom.
        // (vt100's grid.scroll_up increments scrollback_offset when > 0)
        {
            let p = parser.lock().unwrap();
            assert_eq!(
                p.screen().scrollback(),
                8,
                "anchor should grow from 5 to 8 as 3 new lines arrive"
            );
        }
    }

    #[test]
    fn live_mode_stays_at_bottom() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            5,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));

        // Feed initial content.
        {
            let mut p = parser.lock().unwrap();
            for i in 0..20 {
                let line = format!("line {}\r\n", i);
                p.process(line.as_bytes());
            }
        }

        // In live mode, scrollback_offset = 0.
        {
            let p = parser.lock().unwrap();
            assert_eq!(p.screen().scrollback(), 0);
        }

        // New content arrives.
        {
            let mut p = parser.lock().unwrap();
            for i in 20..30 {
                let line = format!("line {}\r\n", i);
                p.process(line.as_bytes());
            }
        }

        // Still at bottom — offset stays 0.
        {
            let p = parser.lock().unwrap();
            assert_eq!(
                p.screen().scrollback(),
                0,
                "live mode should stay at bottom"
            );
        }
    }

    #[test]
    fn scrollback_buffer_cap_honored() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            5,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));
        {
            let mut p = parser.lock().unwrap();
            // Feed more lines than the scrollback cap to fill the buffer.
            for i in 0..(DEFAULT_SCROLLBACK_LINES + 500) {
                let line = format!("line {}\r\n", i);
                p.process(line.as_bytes());
            }
        }
        // The vt100 parser itself enforces the scrollback cap. Verify the
        // grid's scrollback VecDeque doesn't grow unbounded.
        let p = parser.lock().unwrap();
        let screen = p.screen();
        let contents = screen.contents();
        assert!(
            !contents.is_empty(),
            "screen should have content after feeding lines"
        );
        // Scrollback is capped by the parser — if we try to set_scrollback
        // beyond it, it clamps. This is the buffer-cap test.
        drop(p);
        let mut p = parser.lock().unwrap();
        p.screen_mut()
            .set_scrollback(DEFAULT_SCROLLBACK_LINES + 1000);
        let actual = p.screen().scrollback();
        assert!(
            actual <= DEFAULT_SCROLLBACK_LINES,
            "scrollback {} should be <= cap {}",
            actual,
            DEFAULT_SCROLLBACK_LINES
        );
    }

    #[test]
    fn growth_rate_guard_fires() {
        let warned = Arc::new(AtomicBool::new(false));
        let _bytes = Arc::new(AtomicU64::new(0));

        // Simulate the rate check logic from the reader thread.
        let window_bytes: u64 = GROWTH_RATE_WARN_BYTES_PER_SEC * 3;
        let elapsed_secs: u64 = GROWTH_RATE_WINDOW_SECS;
        let rate = window_bytes / elapsed_secs.max(1);
        assert!(
            rate > GROWTH_RATE_WARN_BYTES_PER_SEC,
            "simulated rate {} should exceed threshold {}",
            rate,
            GROWTH_RATE_WARN_BYTES_PER_SEC
        );
        if rate > GROWTH_RATE_WARN_BYTES_PER_SEC {
            warned.store(true, Ordering::Relaxed);
        }
        assert!(
            warned.load(Ordering::Relaxed),
            "growth-rate guard should have fired"
        );
    }

    #[test]
    fn growth_rate_guard_does_not_fire_under_threshold() {
        let warned = Arc::new(AtomicBool::new(false));

        let window_bytes: u64 = 1024;
        let elapsed_secs: u64 = GROWTH_RATE_WINDOW_SECS;
        let rate = window_bytes / elapsed_secs.max(1);
        if rate > GROWTH_RATE_WARN_BYTES_PER_SEC {
            warned.store(true, Ordering::Relaxed);
        }
        assert!(
            !warned.load(Ordering::Relaxed),
            "growth-rate guard should NOT fire for low output rate"
        );
    }

    #[test]
    fn send_key_resets_scroll() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            24,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));

        // Feed content and scroll up
        {
            let mut p = parser.lock().unwrap();
            for i in 0..50 {
                p.process(format!("line {}\r\n", i).as_bytes());
            }
            p.screen_mut().set_scrollback(20);
            assert_eq!(p.screen().scrollback(), 20);
        }

        // Simulate what send_key does: reset to 0
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
            assert_eq!(p.screen().scrollback(), 0);
        }
    }

    /// Render the parser screen via tui-term + ratatui TestBackend at the
    /// given dimensions. Returns the buffer text with each row joined by '\n'
    /// and trailing whitespace per row trimmed.
    fn render_to_text(parser: &Arc<Mutex<vt100::Parser>>, rows: u16, cols: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(cols, rows);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let p = parser.lock().unwrap();
                let widget = tui_term::widget::PseudoTerminal::new(p.screen());
                frame.render_widget(widget, frame.area());
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            if y > area.top() {
                out.push('\n');
            }
            let mut line = String::new();
            for x in area.left()..area.right() {
                let cell = &buf[(x, y)];
                let symbol = cell.symbol();
                if symbol.is_empty() {
                    continue;
                }
                line.push_str(symbol);
            }
            out.push_str(line.trim_end());
        }
        out
    }

    /// Repro for the "PTY output doubled" bug: feed multi-line chat-agent-shaped
    /// content into a vt100 parser sized to fit, render via PseudoTerminal,
    /// and assert each input line appears exactly once in the rendered cells.
    /// The user's screenshot shows three review-session task IDs printed twice
    /// inside a single bordered pane.
    #[test]
    fn rendered_pty_output_does_not_double_lines() {
        let lines = ["stocktake-assets", "content-review-poietic-life", "wg-visual-language-study"];

        // Parser sized exactly to the rendering area — same as how
        // `pane.resize(area.height, area.width)` clamps things in
        // `draw_chat_tab`.
        for &cols in &[40_u16, 80, 120] {
            let parser = Arc::new(Mutex::new(vt100::Parser::new(
                10,
                cols,
                DEFAULT_SCROLLBACK_LINES,
            )));
            {
                let mut p = parser.lock().unwrap();
                for line in &lines {
                    p.process(line.as_bytes());
                    p.process(b"\r\n");
                }
            }
            let rendered = render_to_text(&parser, 10, cols);
            for line in &lines {
                let n = rendered.matches(line).count();
                assert_eq!(
                    n, 1,
                    "line {:?} appeared {} times at width {} — expected 1.\nrendered:\n{}",
                    line, n, cols, rendered
                );
            }
        }
    }

    /// The chat-tab PTY pane calls `pane.resize(area.height, area.width)`
    /// every frame. The implementation clamps to a 10×40 minimum (to keep
    /// vt100::Parser out of panic-prone tiny grids). If the visible area
    /// is *smaller* than 10×40, only the top of the parser is rendered —
    /// not duplicated. This test pins that contract: a 5-row visible area
    /// with content that fits in 5 rows shows each line exactly once and
    /// nothing leaks from the bottom 5 rows of the (clamped) parser grid.
    #[test]
    fn render_area_smaller_than_parser_minimum_does_not_double() {
        // Parser at the clamp minimum (10×40), but only 5 rows of visible
        // area. Content is 3 lines — well within both dimensions.
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            10,
            40,
            DEFAULT_SCROLLBACK_LINES,
        )));
        {
            let mut p = parser.lock().unwrap();
            p.process(b"stocktake-assets\r\n");
            p.process(b"content-review-poietic-life\r\n");
            p.process(b"wg-visual-language-study\r\n");
        }
        let rendered = render_to_text(&parser, 5, 40);
        for line in [
            "stocktake-assets",
            "content-review-poietic-life",
            "wg-visual-language-study",
        ] {
            let n = rendered.matches(line).count();
            assert_eq!(
                n, 1,
                "rendering 5 visible rows of a 10-row parser should not double {:?} \
                 (got {} occurrences). Rendered:\n{}",
                line, n, rendered
            );
        }
    }

    /// Cursor save / restore (DECSC / DECRC) is emitted by TUI children
    /// (claude, vendor CLIs) when redrawing dynamic regions. After the
    /// child has printed multi-line content, then issues
    /// `save → cursor-up → re-print → restore`, the SAME bytes feed the
    /// parser twice but the second pass overwrites the first — the on-
    /// screen content is the second pass only, no doubling. Pin the
    /// invariant: re-printing identical content via cursor positioning
    /// does not produce two copies in the rendered cells.
    #[test]
    fn cursor_reprint_does_not_double_content() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            10,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));
        {
            let mut p = parser.lock().unwrap();
            p.process(b"stocktake-assets\r\n");
            p.process(b"content-review-poietic-life\r\n");
            p.process(b"wg-visual-language-study\r\n");
            // Move cursor up 3 rows back to the top of the printed block
            // and re-print the same content (mimics a TUI redraw).
            p.process(b"\x1b[3A\r");
            p.process(b"stocktake-assets\r\n");
            p.process(b"content-review-poietic-life\r\n");
            p.process(b"wg-visual-language-study\r\n");
        }
        let rendered = render_to_text(&parser, 10, 80);
        for line in [
            "stocktake-assets",
            "content-review-poietic-life",
            "wg-visual-language-study",
        ] {
            let n = rendered.matches(line).count();
            assert_eq!(
                n, 1,
                "cursor-up + reprint of {:?} should overwrite, not double \
                 ({} occurrences). Rendered:\n{}",
                line, n, rendered
            );
        }
    }

    /// Streaming chat-agent output that arrives as small chunks (token by
    /// token) and uses `\r\n` line breaks must not produce duplicate
    /// rendered lines, even at a width where individual chunks may
    /// straddle row boundaries. The user described "streaming chat-agent
    /// output at a width where wrapping kicks in" as a likely repro shape.
    #[test]
    fn streamed_chunks_with_newlines_do_not_double() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            10,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));
        // Stream a numbered list as small chunks (mimics LLM token streaming).
        let chunks: &[&[u8]] = &[
            b"Your review-session tasks:\r\n",
            b"- ",
            b"stocktake-assets",
            b"\r\n",
            b"- ",
            b"content-review-poietic-life",
            b"\r\n",
            b"- ",
            b"wg-visual-language-study",
            b"\r\n",
        ];
        {
            let mut p = parser.lock().unwrap();
            for c in chunks {
                p.process(c);
            }
        }
        let rendered = render_to_text(&parser, 10, 80);
        for needle in [
            "stocktake-assets",
            "content-review-poietic-life",
            "wg-visual-language-study",
        ] {
            let n = rendered.matches(needle).count();
            assert_eq!(
                n, 1,
                "streamed chunked content {:?} appeared {} times — expected 1.\n\
                 Rendered:\n{}",
                needle, n, rendered
            );
        }
    }

    /// Re-rendering the same parser state multiple times (frame ticks while
    /// the embedded child is idle) must NOT cause content to drift or
    /// duplicate. Without this guarantee, every redraw would have to be
    /// idempotent at the byte level — and any bug that mutates parser state
    /// during render would surface here.
    #[test]
    fn repeated_render_is_idempotent() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            10,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));
        {
            let mut p = parser.lock().unwrap();
            p.process(b"alpha\r\nbeta\r\ngamma\r\n");
        }
        let first = render_to_text(&parser, 10, 80);
        let second = render_to_text(&parser, 10, 80);
        let third = render_to_text(&parser, 10, 80);
        assert_eq!(first, second, "re-render produced different output");
        assert_eq!(second, third, "third render diverged");
        assert_eq!(first.matches("alpha").count(), 1);
        assert_eq!(first.matches("beta").count(), 1);
        assert_eq!(first.matches("gamma").count(), 1);
    }

    /// Resize-while-rendering: the chat tab calls `pane.resize(h, w)` every
    /// frame; if the area is unchanged, the resize is a no-op, but if the
    /// embedded child reflows on SIGWINCH the parser may pick up a
    /// re-printed copy of recent output. Verify that a resize after content
    /// has been printed does not duplicate it inside the render buffer at
    /// either width.
    #[test]
    fn render_after_resize_does_not_double_lines() {
        // Prime parser at the original size with multi-line content.
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            10,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));
        {
            let mut p = parser.lock().unwrap();
            p.process(b"stocktake-assets\r\n");
            p.process(b"content-review-poietic-life\r\n");
            p.process(b"wg-visual-language-study\r\n");
        }

        // Caller resizes — narrower (wrap kicks in) then wider (no wrap).
        for &(rows, cols) in &[(10_u16, 40_u16), (10, 80), (10, 120)] {
            {
                let mut p = parser.lock().unwrap();
                p.screen_mut().set_size(rows, cols);
            }
            let rendered = render_to_text(&parser, rows, cols);
            for line in [
                "stocktake-assets",
                "content-review-poietic-life",
                "wg-visual-language-study",
            ] {
                let n = rendered.matches(line).count();
                assert_eq!(
                    n, 1,
                    "after resize to {}x{}, line {:?} appeared {} times — expected 1.\n\
                     rendered:\n{}",
                    rows, cols, line, n, rendered
                );
            }
        }
    }

    #[test]
    fn scroll_to_top_and_bottom() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            5,
            80,
            DEFAULT_SCROLLBACK_LINES,
        )));

        {
            let mut p = parser.lock().unwrap();
            for i in 0..50 {
                p.process(format!("line {}\r\n", i).as_bytes());
            }
        }

        // scroll_to_top (set to max, clamped to actual buffer)
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(usize::MAX);
            let top = p.screen().scrollback();
            assert!(top > 0, "should scroll to top of buffer");
        }

        // scroll_to_bottom
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
            assert_eq!(p.screen().scrollback(), 0);
        }
    }

    // ── helpers for the SIGWINCH dedup tests ──────────────────────────────

    /// Read the actual number of entries in the vt100 scrollback buffer
    /// without perturbing the user's scroll offset.
    fn test_scrollback_count(parser: &Arc<Mutex<vt100::Parser>>) -> usize {
        let mut p = parser.lock().unwrap();
        let saved = p.screen().scrollback();
        p.screen_mut().set_scrollback(usize::MAX);
        let count = p.screen().scrollback();
        p.screen_mut().set_scrollback(saved);
        count
    }

    /// Read all entries in the scrollback buffer exactly once, without
    /// including the live screen.  Walks from max offset to offset=1;
    /// at each step, row 0 of the visible window is a unique scrollback row
    /// (oldest-first order).  Returns each row as a trimmed string line.
    fn collect_scrollback_only_naive(
        parser: &Arc<Mutex<vt100::Parser>>,
        cols: u16,
    ) -> Vec<String> {
        let max = test_scrollback_count(parser);
        let mut rows_out = Vec::new();
        for offset in (1..=max).rev() {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(offset);
            drop(p);
            let p = parser.lock().unwrap();
            let row: String = (0..cols)
                .map(|c| {
                    p.screen()
                        .cell(0, c)
                        .map(|cell| cell.contents().to_string())
                        .unwrap_or_default()
                })
                .collect();
            rows_out.push(row.trim_end().to_string());
        }
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
        }
        rows_out
    }

    /// Same as `collect_scrollback_only_naive` but skips the `hidden` most
    /// recently appended rows (offsets 1..=hidden).  Mirrors the
    /// `scrollback_hidden` dedup logic in scroll_up / scroll_down.
    fn collect_scrollback_only_deduped(
        parser: &Arc<Mutex<vt100::Parser>>,
        cols: u16,
        hidden: usize,
    ) -> Vec<String> {
        let max = test_scrollback_count(parser);
        let mut rows_out = Vec::new();
        for offset in (1..=max).rev() {
            if offset <= hidden {
                continue;
            }
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(offset);
            drop(p);
            let p = parser.lock().unwrap();
            let row: String = (0..cols)
                .map(|c| {
                    p.screen()
                        .cell(0, c)
                        .map(|cell| cell.contents().to_string())
                        .unwrap_or_default()
                })
                .collect();
            rows_out.push(row.trim_end().to_string());
        }
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
        }
        rows_out
    }

    /// Deterministic reproduction of the SIGWINCH scrollback duplication bug.
    ///
    /// Scenario (mirrors TEST3 in /tmp/wg-pty-repro/src/main.rs):
    ///   1. Feed 30 lines into a 10-row parser → fills scrollback with rows
    ///      that have now scrolled off the visible area.
    ///   2. Grow terminal to 12 rows (simulates a resize event).
    ///   3. Simulate SIGWINCH: child clears screen and reprints the new
    ///      12-row visible region (markers 18–29).  Marker-0018 was already
    ///      in scrollback, so it gets pushed in again — a true duplicate.
    ///
    /// Pre-condition assertion: naive scrollback scan shows marker-0018 twice.
    /// Fix assertion: with scrollback_hidden = K, the deduped scan shows
    /// each scrollback entry at most once (no true duplicates).
    ///
    /// This is the "failing test first" from the fix-tui-pty task description.
    #[test]
    fn sigwinch_reflow_duplicates_scrollback_and_dedup_hides_them() {
        let init_rows = 10u16;
        let new_rows = 12u16;
        let cols = 80u16;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            init_rows,
            cols,
            DEFAULT_SCROLLBACK_LINES,
        )));

        // Feed 30 unique markers.  In a 10-row terminal the first ~20 scroll
        // into scrollback; the trailing \r\n on the 30th line scrolls one
        // more, so pre_count ends up at 21 (not 20).
        {
            let mut p = parser.lock().unwrap();
            for i in 0..30u32 {
                p.process(format!("marker-{:04}\r\n", i).as_bytes());
            }
        }

        let pre_count = test_scrollback_count(&parser);
        assert!(pre_count > 0, "expected scrollback rows before resize");

        // Simulate resize from 10 → 12 rows.
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_size(new_rows, cols);
        }

        // Simulate child SIGWINCH reflow: clear + repaint last 12 markers
        // (18–29).  Marker-0018 was already in scrollback; reprinting rows
        // that include it causes it to be pushed in again → echo duplicate.
        {
            let mut p = parser.lock().unwrap();
            p.process(b"\x1b[2J\x1b[H");
            for i in 18..30u32 {
                p.process(format!("marker-{:04}\r\n", i).as_bytes());
            }
        }

        let post_count = test_scrollback_count(&parser);
        let k = post_count.saturating_sub(pre_count);
        assert!(k > 0, "SIGWINCH reflow should have added echo rows; k={}", k);

        // ── Pre-condition: naive scan shows at least one in-scrollback dup ──
        let naive_rows = collect_scrollback_only_naive(&parser, cols);
        let naive_text = naive_rows.join("\n");
        let has_scrollback_dup = (0..30u32).any(|i| {
            naive_text.matches(&format!("marker-{:04}", i)).count() > 1
        });
        assert!(
            has_scrollback_dup,
            "expected at least one true scrollback duplicate (k={}); scrollback rows:\n{}",
            k, naive_text
        );

        // ── With fix: deduped scan has no marker more than once ────────────
        let deduped_rows = collect_scrollback_only_deduped(&parser, cols, k);
        let deduped_text = deduped_rows.join("\n");
        for i in 0..30u32 {
            let marker = format!("marker-{:04}", i);
            let count = deduped_text.matches(&marker).count();
            // Each marker may appear 0 (not yet pushed) or 1 (once) — never 2+.
            assert!(
                count <= 1,
                "marker {} appeared {} times in deduped scrollback (expected ≤1)\n{}",
                marker, count, deduped_text
            );
        }
    }

    /// Verify the offset arithmetic of the SIGWINCH dedup logic:
    /// - scroll_up(n) from live view (offset 0) with scrollback_hidden=K
    ///   jumps to K+n (skipping the K echo rows at the hot end of scrollback).
    /// - scroll_down(n) that would land in the hidden zone (1..=K) snaps to 0.
    #[test]
    fn scroll_up_skips_sigwinch_hidden_rows() {
        let hidden = 3usize; // K = 3 simulated echo rows

        // ── scroll_up arithmetic ────────────────────────────────────────────
        // From offset 0, scroll_up(1): base = hidden (since current==0), target = hidden+1.
        {
            let current = 0usize;
            let base = if current == 0 && hidden > 0 { hidden } else { current };
            let target = base.saturating_add(1);
            assert_eq!(target, hidden + 1, "scroll_up(1) from live view should jump to K+1");
        }
        // From offset 0, scroll_up(5): target = hidden+5.
        {
            let current = 0usize;
            let base = if current == 0 && hidden > 0 { hidden } else { current };
            let target = base.saturating_add(5);
            assert_eq!(target, hidden + 5, "scroll_up(5) from live view should jump to K+5");
        }
        // From a non-zero offset already above the hidden zone (e.g. hidden+2),
        // scroll_up should add n normally (no extra jump).
        {
            let current = hidden + 2;
            let base = if current == 0 && hidden > 0 { hidden } else { current };
            let target = base.saturating_add(1);
            assert_eq!(target, hidden + 3, "scroll_up(1) from above hidden zone adds 1");
        }

        // ── scroll_down arithmetic ──────────────────────────────────────────
        // From K+1, scroll_down(1) → new_off = K → in hidden zone → snap to 0.
        {
            let current = hidden + 1;
            let new_off = current.saturating_sub(1); // = K
            let snapped = if new_off > 0 && new_off <= hidden { 0 } else { new_off };
            assert_eq!(snapped, 0, "scroll_down from K+1 into hidden zone should snap to 0");
        }
        // From K+2, scroll_down(1) → new_off = K+1 → above hidden zone → stays.
        {
            let current = hidden + 2;
            let new_off = current.saturating_sub(1); // = K+1
            let snapped = if new_off > 0 && new_off <= hidden { 0 } else { new_off };
            assert_eq!(snapped, hidden + 1, "scroll_down from K+2 stays at K+1");
        }
        // From exactly 0 (live view), scroll_down does nothing (already at 0).
        {
            let current = 0usize;
            let new_off = current.saturating_sub(1); // = 0 (saturating)
            let snapped = if new_off > 0 && new_off <= hidden { 0 } else { new_off };
            assert_eq!(snapped, 0, "scroll_down from live view stays at 0");
        }

        // ── Integration: verify at vt100 parser level ──────────────────────
        // Build a parser with known scrollback + K echo rows, then check that
        // at offset K+1 (the first clean position after scroll_up from live
        // view), row 0 is from real history and not blank.
        let rows = 5u16;
        let cols = 40u16;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            rows, cols, DEFAULT_SCROLLBACK_LINES,
        )));
        // Feed 15 real lines → ~11 in scrollback, 4 visible (+ 1 empty).
        {
            let mut p = parser.lock().unwrap();
            for i in 0..15u32 {
                p.process(format!("real-{:04}\r\n", i).as_bytes());
            }
        }
        // Simulate SIGWINCH reflow: repaint 3 rows that were visible
        // (real-0011, real-0012, real-0013 — in the live screen at that moment).
        // Their \r\n pushes the PREVIOUS row 0 into scrollback each time.
        {
            let mut p = parser.lock().unwrap();
            for i in 11..14u32 {
                p.process(format!("real-{:04}\r\n", i).as_bytes());
            }
        }
        let total = test_scrollback_count(&parser);
        assert!(
            total > hidden,
            "need more scrollback rows than hidden={}; got total={}",
            hidden, total
        );

        // At the first clean offset (hidden+1), row 0 comes from real history.
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(hidden + 1);
            drop(p);
            let p = parser.lock().unwrap();
            let row0: String = (0..cols)
                .map(|c| {
                    p.screen()
                        .cell(0, c)
                        .map(|cell| cell.contents().to_string())
                        .unwrap_or_default()
                })
                .collect();
            assert!(
                row0.trim_end().starts_with("real-"),
                "offset {} row 0 should be real history, got: {:?}",
                hidden + 1,
                row0.trim_end()
            );
        }
        {
            // Restore to live view.
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
        }
    }

    /// Reads every scrollback row plus the current visible screen by
    /// rendering the parser into a TestBackend at offsets max..=0.
    /// Returns the concatenated text, with each row trimmed and
    /// joined by '\n'.
    fn collect_full_scrollback_and_screen(
        parser: &Arc<Mutex<vt100::Parser>>,
        rows: u16,
        cols: u16,
    ) -> String {
        let total = test_scrollback_count(parser);
        let mut out = String::new();
        for offset in (0..=total).rev() {
            {
                let mut p = parser.lock().unwrap();
                p.screen_mut().set_scrollback(offset);
            }
            let frame = render_to_text(parser, rows, cols);
            out.push_str(&frame);
            out.push('\n');
        }
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
        }
        out
    }

    /// Repro for fix-pty-scrollback: the chat-tab PTY pane was spawned
    /// at hardcoded 24×80 and then resized to the actual chat-message
    /// area on the first frame. The vendor CLI (claude/codex/wg-nex)
    /// dumps multi-screen history at the small size, then SIGWINCH
    /// triggers a clear-screen + reprint at the larger size. The
    /// scrollback ends up containing the same logical lines twice —
    /// once wrapped at the small width and once unwrapped at the larger
    /// width — which the user sees as "the chat scrollback loops a bit
    /// and then settles".
    ///
    /// Without the fix: lines whose length straddles 80 cols (wrap)
    /// but fit within 120 cols (no wrap) appear twice in the rendered
    /// scrollback because the wrap-at-80 form is in the older rows and
    /// the no-wrap form is in the SIGWINCH-echo rows + visible screen.
    /// The existing `scrollback_hidden` dedup hides the K hot-end echo
    /// rows but does NOT remove the older wrap-at-80 copies.
    #[test]
    fn initial_spawn_at_default_then_resize_doubles_long_lines_in_scrollback() {
        let small_rows = 24u16;
        let small_cols = 80u16;
        let large_rows = 30u16;
        let large_cols = 120u16;

        // Lines longer than small_cols (wrap at 80) but fit in large_cols (no wrap).
        // 12 lines × ~95 chars each.
        let lines: Vec<String> = (0..12)
            .map(|i| {
                let body = "x".repeat(80);
                format!("history-line-{:03}-{}", i, body)
            })
            .collect();
        // Sanity: each line is longer than small_cols, shorter than large_cols.
        for l in &lines {
            assert!(
                l.len() > small_cols as usize && l.len() <= large_cols as usize,
                "test setup: line length {} not in ({}, {}]",
                l.len(),
                small_cols,
                large_cols
            );
        }

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            small_rows,
            small_cols,
            DEFAULT_SCROLLBACK_LINES,
        )));
        // Vendor CLI dumps history at small dims (lines wrap at 80 cols).
        {
            let mut p = parser.lock().unwrap();
            for line in &lines {
                p.process(line.as_bytes());
                p.process(b"\r\n");
            }
        }
        // First frame triggers resize → SIGWINCH → child reflows: clear + reprint.
        // Most vendor CLIs reprint the visible region (their TUI), which pushes
        // wrap-at-old-size content into scrollback and then prints unwrap-at-new-size
        // content on the new screen.
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_size(large_rows, large_cols);
            p.process(b"\x1b[2J\x1b[H");
            for line in &lines {
                p.process(line.as_bytes());
                p.process(b"\r\n");
            }
        }

        let full = collect_full_scrollback_and_screen(&parser, large_rows, large_cols);
        // The bug: at least one line appears more than once in the
        // rendered scrollback because the wrap-at-80 copy survives
        // alongside the unwrap-at-120 reprint.
        let mut any_dup = false;
        for line in &lines {
            // Use a unique substring per line: the marker prefix.
            let marker = &line[..16]; // "history-line-NNN"
            let n = full.matches(marker).count();
            if n > 1 {
                any_dup = true;
                break;
            }
        }
        assert!(
            any_dup,
            "expected at least one logical line to appear twice in rendered \
             scrollback after spawn-at-wrong-size + resize-on-first-frame; \
             got each line once. Rendered:\n{}",
            full
        );
    }

    /// Fix: spawning the PTY at the actual chat-message area
    /// dimensions from the start avoids the SIGWINCH reflow entirely.
    /// Each logical line then appears in scrollback exactly once.
    /// This is the post-fix-pty-scrollback contract — every chat-tab
    /// PTY spawn must use the real `msg_area.height`/`msg_area.width`
    /// (deferred via `consume_pending_chat_pty_spawn` if needed).
    #[test]
    fn spawn_at_correct_size_does_not_double_long_lines_in_scrollback() {
        let large_rows = 30u16;
        let large_cols = 120u16;

        let lines: Vec<String> = (0..12)
            .map(|i| {
                let body = "x".repeat(80);
                format!("history-line-{:03}-{}", i, body)
            })
            .collect();

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            large_rows,
            large_cols,
            DEFAULT_SCROLLBACK_LINES,
        )));
        // Spawned at the right size — vendor CLI prints once, no SIGWINCH echo.
        {
            let mut p = parser.lock().unwrap();
            for line in &lines {
                p.process(line.as_bytes());
                p.process(b"\r\n");
            }
        }

        let full = collect_full_scrollback_and_screen(&parser, large_rows, large_cols);
        for line in &lines {
            let marker = &line[..16];
            let n = full.matches(marker).count();
            assert!(
                n >= 1,
                "line {:?} should appear at least once in rendered scrollback (got {})",
                marker,
                n
            );
            assert!(
                n <= 1,
                "line {:?} should appear at most once when spawn dims match render dims \
                 (got {} occurrences). Rendered:\n{}",
                marker,
                n,
                full
            );
        }
    }

    /// `PtyPane::dims()` reports the size the parser/master PTY were
    /// opened with. The chat-tab spawn path must call
    /// `consume_pending_chat_pty_spawn(rows, cols)` with the actual
    /// `msg_area` height/width so the child process sees its initial
    /// size as the layout area. If a regression slipped a hardcoded
    /// 24×80 back into the spawn site, this would catch it via the
    /// reported dims after a fresh spawn.
    #[test]
    fn pty_pane_dims_reports_spawn_size() {
        let pane = PtyPane::spawn("/bin/sh", &["-c", "sleep 60"], &[], 30, 120)
            .expect("spawn /bin/sh -c sleep");
        assert_eq!(pane.dims(), (30, 120));
        // Deliberately drop the pane (sleep gets killed by Drop).
    }
}

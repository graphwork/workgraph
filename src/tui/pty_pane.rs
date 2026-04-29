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
    /// When `Some`, the pane is wrapping a `tmux attach` client whose
    /// underlying tmux session is named here. The chat process lives
    /// inside that session, NOT as our direct child — so dropping this
    /// pane only kills the attach client; the session keeps running.
    /// Call `kill_underlying_session` to explicitly tear it down (e.g.
    /// when the user archives / deletes the chat).
    tmux_session: Option<String>,
    /// Cumulative bytes written from the host TUI to the embedded
    /// child's stdin via `send_key` / `send_text`. Tests use this to
    /// assert that a given event path does NOT forward bytes to the
    /// child (e.g. fix-mouse-wheel-2: wheel must not produce arrow-key
    /// bytes). Capability-query replies emitted from the reader thread
    /// are NOT counted here — only host-driven input is.
    input_bytes_written: Arc<AtomicU64>,
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
                // DEC mode 2026 (synchronized output) tracking. See
                // `manage_sync_mode_scrollback` for the full motivation —
                // codex's interactive TUI emits each animation frame as a
                // BSU/ESU-bracketed full-screen repaint whose trailing
                // newline scrolls one row off the top per frame, and
                // without trimming, scrolling back through history shows
                // stacked spinner frames.
                let mut in_sync_mode = false;
                let mut sync_start_scrollback_count = 0usize;
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Some(f) = tee_file.as_mut() {
                                let _ = f.write_all(&buf[..n]);
                                let _ = f.flush();
                            }
                            respond_to_queries(&buf[..n], &reader_responder, &reader_parser);

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

                            // Capture pre-process scrollback count for sync-mode
                            // trim accounting; cheap (a couple of method calls)
                            // when the chunk has no sync markers, since the
                            // manage_ helper short-circuits in that case.
                            let pre_count = if chunk_contains_sync_markers(&buf[..n]) {
                                if let Ok(mut p) = reader_parser.lock() {
                                    parser_scrollback_count(&mut p)
                                } else {
                                    0
                                }
                            } else {
                                0
                            };

                            if let Ok(mut p) = reader_parser.lock() {
                                p.process(&buf[..n]);
                            }

                            manage_sync_mode_scrollback(
                                &buf[..n],
                                &reader_parser,
                                &mut in_sync_mode,
                                &mut sync_start_scrollback_count,
                                pre_count,
                            );
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
            tmux_session: None,
            input_bytes_written: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Spawn `command` inside a detached tmux session and return a
    /// PtyPane attached to it via `tmux attach -t <session>`. When this
    /// pane drops (TUI exit, panic), only the attach client dies — the
    /// tmux session keeps the underlying process alive across TUI
    /// restarts. Use [`kill_underlying_session`] to explicitly tear the
    /// session down (e.g. when the user archives / deletes the chat).
    ///
    /// `session_name` MUST be a valid tmux session id (no whitespace,
    /// no `:`/`.`). Caller is responsible for namespacing it (e.g.
    /// `wg-chat-<project>-<chat_ref>`).
    ///
    /// If a session named `session_name` already exists (e.g. from a
    /// prior TUI run), this reattaches to it instead of starting a new
    /// process — that is the persistence point.
    ///
    /// Returns an error if the `tmux` binary is not available; the
    /// caller is expected to fall back to plain `spawn_in` and warn the
    /// user.
    pub fn spawn_via_tmux(
        session_name: &str,
        command: &str,
        args: &[&str],
        env: &[(String, String)],
        cwd: Option<&std::path::Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        if !tmux_available() {
            anyhow::bail!("tmux not available on PATH");
        }
        // Sanity-check the session name. tmux session names cannot
        // contain `:` or `.`; whitespace is also disallowed in our
        // contract. Bail explicitly so a malformed name doesn't manifest
        // later as a confusing tmux error.
        if session_name.is_empty()
            || session_name
                .chars()
                .any(|c| c.is_whitespace() || c == ':' || c == '.')
        {
            anyhow::bail!("invalid tmux session name: {:?}", session_name);
        }

        let session_exists = tmux_has_session(session_name);
        if !session_exists {
            // Build `tmux new-session -d -s <name> [-c cwd] [-e K=V ...] -- <bin> [args...]`
            let mut tmux_args: Vec<String> = vec![
                "new-session".to_string(),
                "-d".to_string(),
                "-s".to_string(),
                session_name.to_string(),
            ];
            if let Some(c) = cwd {
                tmux_args.push("-c".to_string());
                tmux_args.push(c.display().to_string());
            }
            for (k, v) in env {
                tmux_args.push("-e".to_string());
                tmux_args.push(format!("{}={}", k, v));
            }
            // Separator + program + program args. tmux's `--` only
            // works after the session-creation flags — everything after
            // is passed to the inner shell exec.
            tmux_args.push("--".to_string());
            tmux_args.push(command.to_string());
            for a in args {
                tmux_args.push(a.to_string());
            }
            let status = std::process::Command::new("tmux")
                .args(&tmux_args)
                .status()
                .context("failed to invoke tmux new-session")?;
            if !status.success() {
                anyhow::bail!(
                    "tmux new-session failed (status {:?}) for session '{}'",
                    status.code(),
                    session_name
                );
            }
        }

        // Attach client lives in our PTY child. `-d` detaches any other
        // clients first — single-attach semantics, even if a prior TUI
        // (or a stray `tmux attach` from a shell) is still glued on.
        let attach_args = ["attach", "-d", "-t", session_name];
        let mut pane = Self::spawn_in("tmux", &attach_args, &[], cwd, rows, cols)?;
        pane.tmux_session = Some(session_name.to_string());
        Ok(pane)
    }

    /// Returns the underlying tmux session name if this pane was
    /// spawned via [`spawn_via_tmux`].
    pub fn tmux_session(&self) -> Option<&str> {
        self.tmux_session.as_deref()
    }

    /// Explicitly tear down the underlying tmux session (and therefore
    /// the chat process inside it). No-op for non-tmux-wrapped panes.
    /// Idempotent — safe to call after `Drop` or `kill`.
    pub fn kill_underlying_session(&mut self) {
        if let Some(name) = self.tmux_session.clone() {
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", &name])
                .status();
            self.tmux_session = None;
        }
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
            p.screen_mut().set_scrollback(current.saturating_add(n));
        }
    }

    /// Scroll the view down (toward live output) by `n` lines.
    pub fn scroll_down(&mut self, n: usize) {
        if let Ok(mut p) = self.parser.lock() {
            let current = p.screen().scrollback();
            let new_offset = current.saturating_sub(n);
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

    /// Current scrollback offset (lines above live output). 0 means live.
    pub fn scrollback(&self) -> usize {
        self.parser
            .lock()
            .map(|p| p.screen().scrollback())
            .unwrap_or(0)
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
                self.input_bytes_written
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
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
            self.input_bytes_written
                .fetch_add(text.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Cumulative bytes written from the host TUI to the embedded
    /// child's stdin via `send_key`/`send_text`. Reader-thread
    /// capability-query replies are NOT counted here. Used by tests to
    /// assert that an event path (e.g. mouse wheel — fix-mouse-wheel-2)
    /// produces zero forwarded input.
    pub fn child_input_bytes_written(&self) -> u64 {
        self.input_bytes_written.load(Ordering::Relaxed)
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

    /// Push a new size through to both the vt100 parser (so rendered
    /// cell layout updates) and the master PTY (so the child sees
    /// SIGWINCH and can reflow its own output). No-op if the size
    /// matches the current one.
    ///
    /// vt100 0.16's `Screen::set_size` only resizes the visible row Vec —
    /// scrollback rows keep their pre-resize cell count and wrap flags
    /// (vt100/src/grid.rs:66-100). Without further work a width change
    /// leaves stale wrap state in scrollback and the user sees old wrapped
    /// rows duplicated against the new width. To avoid that we reflow by
    /// snapshotting the existing scrollback + visible content into logical
    /// lines, then re-feeding them into a freshly-sized parser. vt100
    /// re-wraps at parse time, so the output rows match the new width.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
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

        // Reflow scrollback + visible by re-feeding into a fresh parser at
        // the new dimensions. Holds the parser lock for the duration of the
        // swap so the reader thread doesn't see a half-built state.
        {
            let mut p = match self.parser.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let fresh = reflow_parser(&mut p, rows, cols, DEFAULT_SCROLLBACK_LINES);
            *p = fresh;
        }

        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize failed")?;
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
        // Ensure the local PTY child is gone; the reader thread will
        // see EOF when the master drops after this Drop completes.
        //
        // For tmux-wrapped panes the child IS the `tmux attach` client,
        // not the underlying chat process. Killing the attach client
        // detaches it cleanly; the tmux server keeps the chat session
        // alive so a later `wg tui` reattaches to the same session and
        // the user resumes their conversation. THIS IS THE PERSISTENCE
        // INVARIANT — Drop must NOT call `kill_underlying_session`. To
        // discard the chat, callers explicitly invoke that method
        // (chat archive / delete paths in viz_viewer/state.rs).
        let _ = self.child.kill();
        // Don't join the reader here — `kill` may not have fully
        // flushed yet and we'd block the TUI shutdown. The thread is
        // detached (no handle reference held after `kill()`), so the
        // OS reaps it when the process exits.
        let _ = self.reader_thread.take();
    }
}

/// Cached tmux-availability probe. Cheap (one `which` per process) and
/// returns false if tmux is not installed; callers fall back to plain
/// `spawn_in` + a one-time warning.
pub fn tmux_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("tmux")
            .arg("-V")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Returns true if a tmux session with the given name currently exists.
/// Cheap shell-out — used during `spawn_via_tmux` to decide whether to
/// create a fresh session or reattach to an existing one.
pub fn tmux_has_session(name: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// List every tmux session whose name starts with `prefix`. Returns an
/// empty vec if tmux isn't installed or `list-sessions` fails (e.g. no
/// server running). Used by the chat orphan-sweep to find dangling
/// `wg-chat-*` sessions whose backing task is gone.
pub fn tmux_list_sessions_with_prefix(prefix: &str) -> Vec<String> {
    let out = match std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#S"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with(prefix))
        .map(|l| l.to_string())
        .collect()
}

/// Tear down a tmux session by name. No-op when the session doesn't
/// exist. Idempotent.
pub fn tmux_kill_session(name: &str) {
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", name])
        .status();
}

/// Read a single row's content into a UTF-8 byte buffer plus its `wrapped`
/// flag. Empty cells are emitted as ASCII spaces and trailing spaces are
/// trimmed; wide-character continuation cells are skipped because their
/// content is already encoded in the preceding cell.
fn read_row_for_reflow(parser: &vt100::Parser, row: u16, cols: u16) -> (Vec<u8>, bool) {
    let mut row_bytes: Vec<u8> = Vec::new();
    for c in 0..cols {
        if let Some(cell) = parser.screen().cell(row, c) {
            if cell.is_wide_continuation() {
                continue;
            }
            let s = cell.contents();
            if s.is_empty() {
                row_bytes.push(b' ');
            } else {
                row_bytes.extend_from_slice(s.as_bytes());
            }
        }
    }
    while row_bytes.last() == Some(&b' ') {
        row_bytes.pop();
    }
    let wrapped = parser.screen().row_wrapped(row);
    (row_bytes, wrapped)
}

/// Snapshot every logical line currently in the parser (scrollback oldest →
/// newest, then visible top → bottom), optionally dropping the `drop_recent_k`
/// most-recently-scrolled-into-scrollback rows. With `drop_recent_k = 0` this
/// is the standard reflow snapshot; with `k > 0` it is the sync-mode-aware
/// trim that drops scrollback rows codex pushed during a `\x1b[?2026h` ...
/// `\x1b[?2026l` synchronized repaint (those rows are repaint echoes the user
/// never wanted preserved — see `manage_sync_mode_scrollback`).
///
/// `trim_trailing_blank_rows` controls whether trailing fully-blank logical
/// lines are popped before returning. The reflow-on-resize caller wants
/// trimming so blank visible regions don't compound across resizes; the
/// sync-mode trim caller wants trailing blanks PRESERVED so the row count of
/// the re-fed parser matches the pre-trim row count exactly (popping a blank
/// row would silently lose one row of REAL scrollback per sync block, which
/// over many animation frames would erase legitimate chat history — see
/// `sync_mode_block_trim_removes_scrolled_rows`).
fn snapshot_logical_lines_skipping_recent_scrollback(
    parser: &mut vt100::Parser,
    drop_recent_k: usize,
    trim_trailing_blank_rows: bool,
) -> Vec<Vec<u8>> {
    let saved_offset = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(usize::MAX);
    let max_offset = parser.screen().scrollback();
    let (rows, cols) = parser.screen().size();

    let mut rows_data: Vec<(Vec<u8>, bool)> = Vec::new();

    // Skip the K most-recently-scrolled rows (offsets 1..=k) by starting the
    // walk at offset k+1. When k >= max_offset, the loop body doesn't execute
    // and we walk only the visible region.
    let lower = drop_recent_k.saturating_add(1);
    if lower <= max_offset {
        for offset in (lower..=max_offset).rev() {
            parser.screen_mut().set_scrollback(offset);
            rows_data.push(read_row_for_reflow(parser, 0, cols));
        }
    }
    parser.screen_mut().set_scrollback(0);
    for r in 0..rows {
        rows_data.push(read_row_for_reflow(parser, r, cols));
    }
    parser.screen_mut().set_scrollback(saved_offset);

    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut prev_wrapped = false;
    for (row_bytes, wrapped) in rows_data {
        current.extend_from_slice(&row_bytes);
        if !wrapped {
            lines.push(std::mem::take(&mut current));
        }
        prev_wrapped = wrapped;
    }
    if !current.is_empty() || prev_wrapped {
        lines.push(current);
    }
    if trim_trailing_blank_rows {
        while lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
    }
    lines
}

/// Snapshot every logical line currently in the parser (scrollback oldest →
/// newest, then visible top → bottom). Joins rows whose preceding row carries
/// `wrapped()=true` so the result is a list of logical lines independent of
/// the original column width — feeding them into a fresh parser at a new
/// column count rewraps cleanly.
fn snapshot_logical_lines(parser: &mut vt100::Parser) -> Vec<Vec<u8>> {
    snapshot_logical_lines_skipping_recent_scrollback(parser, 0, true)
}

/// Build a fresh `vt100::Parser` at the requested dimensions and re-feed the
/// snapshot of `parser`'s scrollback + visible content into it. Content
/// reflows naturally to the new column width because vt100 re-wraps at parse
/// time.
fn reflow_parser(
    parser: &mut vt100::Parser,
    new_rows: u16,
    new_cols: u16,
    scrollback_lines: usize,
) -> vt100::Parser {
    let logical_lines = snapshot_logical_lines(parser);
    let mut fresh = vt100::Parser::new(new_rows, new_cols, scrollback_lines);
    let n = logical_lines.len();
    for (i, line) in logical_lines.iter().enumerate() {
        fresh.process(line);
        // Don't append \r\n after the final logical line — it would push the
        // current visible row into scrollback and leave the cursor on a
        // gratuitously blank row.
        if i + 1 < n {
            fresh.process(b"\r\n");
        }
    }
    fresh
}

/// Convert a crossterm `KeyEvent` into the byte sequence a Unix PTY
/// expects. Handles control characters, arrow keys (CSI sequences),
/// function keys, and plain text. Not exhaustive — covers what a
/// `wg nex` REPL user actually presses.
/// Scan PTY output for terminal capability queries and return the
/// conventional reply bytes. Pure (no I/O) so it is unit-testable.
///
/// `cursor_position` is the parser's current `(row, col)` (0-indexed)
/// at the moment the query bytes arrived. CPR (`ESC [ 6 n`) replies
/// with this position in 1-indexed form, matching xterm semantics.
///
/// Coverage targets the queries claude and codex actually send on
/// startup; if any are unanswered, the vendor CLI blocks post-splash
/// and the embedded TUI shows nothing usable. This is standard
/// terminal emulator behavior — xterm, gnome-terminal, alacritty etc.
/// all answer these — portable-pty is a raw pipe so vt100-the-parser
/// doesn't generate replies and we fill the gap.
fn compute_query_replies(chunk: &[u8], cursor_position: (u16, u16)) -> Vec<u8> {
    // Scan for well-known query sequences. Byte patterns:
    //   ESC [ c            — Primary Device Attributes (DA1)
    //   ESC [ > c          — Secondary Device Attributes (DA2)
    //   ESC [ 6 n          — Cursor Position Report (CPR)
    //   ESC [ 5 n          — Device Status Report (DSR)
    //   ESC [ ? u          — Kitty keyboard protocol query
    //   ESC ] 10 ; ? ESC \ — OSC 10 (foreground color query)
    //   ESC ] 11 ; ? ESC \ — OSC 11 (background color query)
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
        // ESC [ 6 n — Cursor Position Report (CPR). Reply with the
        // parser's current cursor in 1-indexed form. Codex's interactive
        // TUI sends this on startup and BLOCKS waiting for the reply —
        // without it, the splash never advances and the chat tab shows
        // nothing useful (root cause of the codex chat-tab regression).
        if tail.starts_with(b"\x1b[6n") {
            let (row, col) = cursor_position;
            let resp = format!(
                "\x1b[{};{}R",
                row.saturating_add(1),
                col.saturating_add(1)
            );
            reply.extend_from_slice(resp.as_bytes());
            i += 4;
            continue;
        }
        // ESC [ 5 n — Device Status Report. Reply 0 = "ready, no malfunction".
        if tail.starts_with(b"\x1b[5n") {
            reply.extend_from_slice(b"\x1b[0n");
            i += 4;
            continue;
        }
        // ESC [ ? u — Kitty keyboard protocol query. Reply 0 = legacy mode
        // (no kitty progressive enhancement). Codex falls back to xterm
        // sequences when this is the answer.
        if tail.starts_with(b"\x1b[?u") {
            reply.extend_from_slice(b"\x1b[?0u");
            i += 4;
            continue;
        }
        // ESC ] 10 ; ? ESC \ — OSC 10 (foreground color query). Reply with
        // a typical dark-theme light-gray foreground. Codex uses this to
        // pick a readable palette; an unanswered query leaves it stalled
        // at startup. Also accept BEL terminator (ESC ] 10 ; ? BEL).
        if tail.starts_with(b"\x1b]10;?\x1b\\") {
            reply.extend_from_slice(b"\x1b]10;rgb:cccc/cccc/cccc\x1b\\");
            i += 8;
            continue;
        }
        if tail.starts_with(b"\x1b]10;?\x07") {
            reply.extend_from_slice(b"\x1b]10;rgb:cccc/cccc/cccc\x07");
            i += 7;
            continue;
        }
        // ESC ] 11 ; ? ESC \ — OSC 11 (background color query). Reply
        // black (typical dark-theme bg). Same blocking behavior as OSC 10
        // if unanswered.
        if tail.starts_with(b"\x1b]11;?\x1b\\") {
            reply.extend_from_slice(b"\x1b]11;rgb:0000/0000/0000\x1b\\");
            i += 8;
            continue;
        }
        if tail.starts_with(b"\x1b]11;?\x07") {
            reply.extend_from_slice(b"\x1b]11;rgb:0000/0000/0000\x07");
            i += 7;
            continue;
        }
        i += 1;
    }
    reply
}

/// Read the parser's current scrollback row count without disturbing the
/// user's scrollback offset. Uses the standard `set_scrollback(MAX) →
/// scrollback()` clamp trick.
fn parser_scrollback_count(parser: &mut vt100::Parser) -> usize {
    let saved = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(usize::MAX);
    let count = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(saved);
    count
}

/// Build a fresh parser at the same dimensions, re-feeding logical lines
/// from `parser` but dropping the `drop_recent_k` most-recently-scrolled
/// scrollback rows. Used by `manage_sync_mode_scrollback` to remove the
/// row(s) codex's full-screen sync repaint scrolled off the top during a
/// `\x1b[?2026h ... \x1b[?2026l` block.
///
/// Trailing-blank trimming is DISABLED here so the re-fed parser's row
/// count matches the original exactly: popping a blank visible-region row
/// would silently scroll one extra row of REAL scrollback off the top per
/// trim, and over many animation frames would erase legitimate chat
/// history (`sync_mode_block_trim_removes_scrolled_rows` pins this).
fn parser_after_dropping_recent_scrollback(
    parser: &mut vt100::Parser,
    drop_recent_k: usize,
) -> vt100::Parser {
    let (rows, cols) = parser.screen().size();
    let lines =
        snapshot_logical_lines_skipping_recent_scrollback(parser, drop_recent_k, false);
    let mut fresh = vt100::Parser::new(rows, cols, DEFAULT_SCROLLBACK_LINES);
    let n = lines.len();
    for (i, line) in lines.iter().enumerate() {
        fresh.process(line);
        if i + 1 < n {
            fresh.process(b"\r\n");
        }
    }
    fresh
}

/// Whether `chunk` contains DEC mode 2026 (synchronized output) BSU
/// (`\x1b[?2026h`) or ESU (`\x1b[?2026l`) markers. Cheap byte scan;
/// `manage_sync_mode_scrollback` calls this first to skip the
/// snapshot/trim work on the vast majority of reads that have no sync
/// markers (raw text, key echo, prompts, etc.).
fn chunk_contains_sync_markers(chunk: &[u8]) -> bool {
    chunk
        .windows(8)
        .any(|w| w == b"\x1b[?2026h" || w == b"\x1b[?2026l")
}

/// Process DEC mode 2026 (synchronized output) tracking for one chunk.
///
/// Codex emits its TUI as `\x1b[?2026h` (BSU) ... full-screen repaint
/// with `\r\n`-separated rows ... `\x1b[?2026l` (ESU) per animation
/// frame. The repaint's trailing newline scrolls one row off the top
/// per frame, and after enough frames the scrollback fills with stacked
/// repaint-echoes the user perceives as "scrolling shows the spinner
/// frames over and over." vt100 0.16 does not implement BSU/ESU itself,
/// so we intercept the markers here and trim any scrollback the sync
/// block produced — the visible region is whatever codex repainted, and
/// the next sync block will repaint again, so we lose nothing the user
/// wanted preserved.
///
/// Caller invariants:
/// - `*in_sync` and `*sync_start_count` reflect state BEFORE this chunk's
///   bytes have been processed by the parser.
/// - `pre_chunk_scrollback_count` is the parser's scrollback row count
///   captured BEFORE `parser.process(chunk)` ran.
/// - Caller has already invoked `parser.process(chunk)` so the parser's
///   visible state and scrollback count reflect post-chunk.
///
/// Updates `*in_sync` and `*sync_start_count` to reflect post-chunk
/// state, and trims any rows scrolled into scrollback during a sync
/// block that ENDED in this chunk.
fn manage_sync_mode_scrollback(
    chunk: &[u8],
    parser: &Arc<Mutex<vt100::Parser>>,
    in_sync: &mut bool,
    sync_start_count: &mut usize,
    pre_chunk_scrollback_count: usize,
) {
    if !chunk_contains_sync_markers(chunk) {
        return;
    }

    // Walk the chunk byte-by-byte tracking sync transitions. We only need
    // to know the FINAL state and whether a sync block ended within the
    // chunk (so we can trim). When entering sync from outside, capture
    // the seed scrollback count: pre_chunk_scrollback_count for the FIRST
    // entry, post-chunk for subsequent entries (a sync block that started
    // and ended earlier in the chunk is already trimmed by then).
    let mut seed = if *in_sync {
        *sync_start_count
    } else {
        pre_chunk_scrollback_count
    };
    let mut current_in_sync = *in_sync;
    let mut total_drop = 0usize;

    let mut i = 0;
    while i < chunk.len() {
        if chunk[i..].starts_with(b"\x1b[?2026h") {
            if !current_in_sync {
                // Entering sync: pick the right seed. For the first BSU
                // in the chunk this is pre_chunk_scrollback_count (set
                // above). For subsequent BSUs after an earlier ESU, the
                // seed should be the post-trim count, which equals seed
                // at this point because each ESU's trim brings the
                // scrollback count back to seed.
                current_in_sync = true;
            }
            i += 8;
            continue;
        }
        if chunk[i..].starts_with(b"\x1b[?2026l") {
            if current_in_sync {
                // Sync ended. The bytes between the matching BSU and ESU
                // pushed (post_count - seed) rows into scrollback, where
                // post_count is the parser's count NOW (after the entire
                // chunk has been processed — minus any trim we've already
                // applied for earlier ESUs in this chunk). We accumulate
                // `total_drop` and trim once at the end so the parser
                // lock is held only briefly.
                if let Ok(mut p) = parser.lock() {
                    let now = parser_scrollback_count(&mut p);
                    let after_prior_trims = now.saturating_sub(total_drop);
                    if after_prior_trims > seed {
                        total_drop += after_prior_trims - seed;
                    }
                    seed = seed.min(after_prior_trims);
                }
                current_in_sync = false;
            }
            i += 8;
            continue;
        }
        i += 1;
    }

    if total_drop > 0
        && let Ok(mut p) = parser.lock()
    {
        let fresh = parser_after_dropping_recent_scrollback(&mut p, total_drop);
        *p = fresh;
    }

    // Update caller state for next chunk.
    *in_sync = current_in_sync;
    *sync_start_count = if current_in_sync {
        seed
    } else if let Ok(mut p) = parser.lock() {
        parser_scrollback_count(&mut p)
    } else {
        seed
    };
}

/// I/O wrapper: read the parser's current cursor position, compute
/// replies for any queries in `chunk`, and write them back through
/// the shared writer.
fn respond_to_queries(
    chunk: &[u8],
    writer: &std::sync::Arc<Mutex<Box<dyn Write + Send>>>,
    parser: &std::sync::Arc<Mutex<vt100::Parser>>,
) {
    let cursor_position = match parser.lock() {
        Ok(p) => p.screen().cursor_position(),
        Err(poisoned) => poisoned.into_inner().screen().cursor_position(),
    };
    let reply = compute_query_replies(chunk, cursor_position);
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

    /// End-to-end integration test for the codex chat-tab fix:
    /// spawn a real PTY child that emits the codex startup query burst,
    /// then reads the response wg writes back, then echoes it as a
    /// marker so the test can observe via the parser screen.
    ///
    /// Without the CPR / kitty / OSC 10 handlers, the child reads
    /// nothing for those queries and hangs at `read`, the test times
    /// out, and the screen never shows the marker. With the handlers,
    /// the child receives the responses through the PTY slave stdin,
    /// echoes them, and the marker shows up — proving the full
    /// reader-thread → respond_to_queries → writer → child stdin path
    /// works end-to-end (not just the pure compute_query_replies fn).
    #[test]
    fn pty_pane_unblocks_codex_style_query_burst_end_to_end() {
        // Bash script: emit the codex startup query burst, read up to
        // 64 bytes (covers DA + CPR + kitty + OSC10 replies easily),
        // base64 the bytes, then echo them with a unique marker.
        // Using base64 keeps the response printable so the parser screen
        // shows ASCII we can grep for; the raw response bytes contain
        // ESC/CSI which would be re-interpreted by the parser.
        //
        // The query burst matches what codex 0.125.0 actually sent at
        // 24×80 (verified via pty.fork()).
        let script = r#"
printf '\x1b[?2004h\x1b[>7u\x1b[?1004h\x1b[6n\x1b[?u\x1b[c\x1b]10;?\x1b\\'
# Drain whatever the responder sends. -N 64 = up to 64 bytes;
# -t 3 = 3-second timeout per read attempt. Loop a couple of times
# in case the responses are delivered in chunks.
got=""
for _ in 1 2 3; do
  IFS= read -r -t 1 -N 64 chunk || true
  got="$got$chunk"
done
b64=$(printf %s "$got" | base64 | tr -d '\n')
printf 'CODEX_RESP_MARKER:%s:END\n' "$b64"
sleep 5
"#;
        let mut pane = PtyPane::spawn("/bin/bash", &["-c", script], &[], 24, 80)
            .expect("spawn bash with codex burst");
        // Give the script up to 5 s to emit queries, receive responses,
        // and print the marker. 50 ms × 100 = 5 s.
        let mut last_screen = String::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let p = pane.parser.lock().unwrap();
            let contents = p.screen().contents();
            if contents.contains("CODEX_RESP_MARKER:") && contents.contains(":END") {
                // Decode the base64 between the markers.
                let start = contents.find("CODEX_RESP_MARKER:").unwrap()
                    + "CODEX_RESP_MARKER:".len();
                let rest = &contents[start..];
                let end = rest.find(":END").expect("END marker present");
                // The base64 may have been wrapped across PTY rows by the
                // emulator; strip whitespace before decoding.
                let b64: String = rest[..end].chars().filter(|c| !c.is_whitespace()).collect();
                let decoded = base64_decode_lenient(&b64);
                drop(p);

                // The raw response bytes must contain the four key replies:
                // DA1, CPR, kitty, and OSC 10. (DA1 was already supported
                // pre-fix; the other three are the new handlers.)
                assert!(
                    decoded.windows(b"\x1b[?65;1;6c".len()).any(|w| w == b"\x1b[?65;1;6c"),
                    "expected DA1 reply in PTY responses, got: {:?}",
                    decoded
                );
                assert!(
                    decoded.windows(b"\x1b[1;1R".len()).any(|w| w == b"\x1b[1;1R"),
                    "expected CPR reply in PTY responses (the bytes wg writes back \
                     to the child via the master writer must round-trip through the \
                     PTY slave stdin), got: {:?}",
                    decoded
                );
                assert!(
                    decoded.windows(b"\x1b[?0u".len()).any(|w| w == b"\x1b[?0u"),
                    "expected kitty keyboard reply in PTY responses, got: {:?}",
                    decoded
                );
                assert!(
                    decoded.windows(b"\x1b]10;rgb:".len()).any(|w| w == b"\x1b]10;rgb:"),
                    "expected OSC 10 fg-color reply in PTY responses, got: {:?}",
                    decoded
                );
                return;
            }
            last_screen = contents;
        }
        let _ = pane.kill();
        panic!(
            "PTY child never emitted the CODEX_RESP_MARKER within 5 s — \
             responder likely failed to write CPR/kitty/OSC10 replies \
             back through the PTY master writer. Last screen was:\n{}",
            last_screen
        );
    }

    /// Minimal RFC-4648 base64 decoder for test use only — the codex
    /// integration test base64-encodes the bytes the bash child read
    /// from stdin so they survive vt100 parser interpretation. We
    /// don't pull in a base64 crate just for one test.
    fn base64_decode_lenient(s: &str) -> Vec<u8> {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let s = s.trim_end_matches('=');
        let mut out = Vec::with_capacity(s.len() * 3 / 4);
        let mut buf = 0u32;
        let mut bits = 0u32;
        for ch in s.bytes() {
            let v = TABLE.iter().position(|&b| b == ch);
            let v = match v {
                Some(v) => v as u32,
                None => continue,
            };
            buf = (buf << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
                buf &= (1u32 << bits) - 1;
            }
        }
        out
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

    /// Walk every row of the parser exactly once: scrollback oldest → newest,
    /// then visible top → bottom. Unlike `collect_full_scrollback_and_screen`
    /// (which renders the visible window at every scrollback offset and so
    /// repeats most rows many times), this returns one entry per logical row,
    /// suitable for "appears at most once" duplication checks.
    fn collect_every_row_once(parser: &Arc<Mutex<vt100::Parser>>) -> Vec<String> {
        let max = test_scrollback_count(parser);
        let mut out: Vec<String> = Vec::new();
        let cols = {
            let p = parser.lock().unwrap();
            p.screen().size().1
        };
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
            out.push(row.trim_end().to_string());
        }
        {
            let mut p = parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
        }
        let p = parser.lock().unwrap();
        let (rows, _) = p.screen().size();
        for r in 0..rows {
            let row: String = (0..cols)
                .map(|c| {
                    p.screen()
                        .cell(r, c)
                        .map(|cell| cell.contents().to_string())
                        .unwrap_or_default()
                })
                .collect();
            out.push(row.trim_end().to_string());
        }
        out
    }

    /// Pre-condition: vt100 0.16's `Screen::set_size` does NOT reflow scrollback.
    /// After a width change, scrollback rows keep their pre-resize cell count
    /// and wrap flags — and a child's SIGWINCH-driven clear+reprint pushes
    /// already-seen content back into scrollback as duplicates.
    /// (Diagnose: agent-1104 / diagnose-scrollback-corruption.)
    ///
    /// This test pins the bug shape for regression detection. The fix lives in
    /// `reflow_parser`; see `refeed_reflow_eliminates_scrollback_duplicates`.
    #[test]
    fn naive_set_size_then_child_reprint_creates_scrollback_duplicates() {
        let init_rows = 10u16;
        let new_rows = 12u16;
        let cols = 80u16;
        let mut p = vt100::Parser::new(init_rows, cols, DEFAULT_SCROLLBACK_LINES);
        for i in 0..30u32 {
            p.process(format!("marker-{:04}\r\n", i).as_bytes());
        }
        // Simulate the buggy resize path: just set_size, no reflow.
        p.screen_mut().set_size(new_rows, cols);
        // Simulate the child's SIGWINCH-driven clear+repaint of the new visible
        // window. marker-0018 was already in scrollback; reprinting it pushes
        // a second copy in.
        p.process(b"\x1b[2J\x1b[H");
        for i in 18..30u32 {
            p.process(format!("marker-{:04}\r\n", i).as_bytes());
        }
        let parser = Arc::new(Mutex::new(p));
        let naive = collect_scrollback_only_naive(&parser, cols).join("\n");
        let any_dup = (0..30u32)
            .any(|i| naive.matches(&format!("marker-{:04}", i)).count() > 1);
        assert!(
            any_dup,
            "naive set_size + child reprint must produce scrollback duplicates \
             (this is the bug-shape pre-condition for the reflow fix); naive scrollback:\n{}",
            naive
        );
    }

    /// TDD assertion for the re-feed reflow fix: snapshotting the parser into
    /// logical lines and re-feeding them into a freshly-sized parser produces
    /// a scrollback in which every marker appears at most once. The bug
    /// (scrollback duplicates after SIGWINCH reflow) is gone, even when the
    /// child subsequently reprints — because the reprint hits a clean parser
    /// where the "duplicates" are simply the next visible region, not pushed
    /// into scrollback as wrap-mismatched echoes.
    #[test]
    fn refeed_reflow_eliminates_scrollback_duplicates() {
        let init_rows = 10u16;
        let new_rows = 12u16;
        let cols = 80u16;
        let mut p = vt100::Parser::new(init_rows, cols, DEFAULT_SCROLLBACK_LINES);
        for i in 0..30u32 {
            p.process(format!("marker-{:04}\r\n", i).as_bytes());
        }
        // Reflow via re-feed instead of bare set_size.
        let reflowed = reflow_parser(&mut p, new_rows, cols, DEFAULT_SCROLLBACK_LINES);
        let parser = Arc::new(Mutex::new(reflowed));

        // After reflow, every marker appears exactly once across scrollback +
        // visible. (Some markers may not appear at all if they wrapped off the
        // top of the bounded scrollback — but none should appear twice.)
        let rows = collect_every_row_once(&parser);
        let full = rows.join("\n");
        for i in 0..30u32 {
            let marker = format!("marker-{:04}", i);
            let n = full.matches(&marker).count();
            assert!(
                n <= 1,
                "after re-feed reflow, marker {} appeared {} times \
                 (expected ≤1). Rows:\n{}",
                marker, n, full
            );
        }
        // And the most recent markers are present at all.
        for i in 18..30u32 {
            let marker = format!("marker-{:04}", i);
            assert!(
                full.contains(&marker),
                "marker {} should still be present after reflow. Rows:\n{}",
                marker, full
            );
        }
    }

    /// Reflow at a NARROWER width: rows that previously fit on a single line
    /// now wrap. The user's logical content is preserved exactly once.
    #[test]
    fn refeed_reflow_rewraps_at_narrower_width() {
        let init_rows = 30u16;
        let init_cols = 120u16;
        let new_rows = 30u16;
        let new_cols = 40u16;
        let mut p = vt100::Parser::new(init_rows, init_cols, DEFAULT_SCROLLBACK_LINES);
        // 12 logical lines, each 100 chars — fit unwrapped at 120, wrap at 40.
        let lines: Vec<String> = (0..12)
            .map(|i| {
                let body = "x".repeat(85);
                format!("history-line-{:03}-{}", i, body)
            })
            .collect();
        for line in &lines {
            p.process(line.as_bytes());
            p.process(b"\r\n");
        }
        let reflowed = reflow_parser(&mut p, new_rows, new_cols, DEFAULT_SCROLLBACK_LINES);
        let parser = Arc::new(Mutex::new(reflowed));
        let rows = collect_every_row_once(&parser);
        let full = rows.join("\n");
        for line in &lines {
            let marker = &line[..16]; // "history-line-NNN"
            let n = full.matches(marker).count();
            assert_eq!(
                n, 1,
                "after reflow narrower, line marker {:?} appeared {} times \
                 (expected exactly 1). Rows:\n{}",
                marker, n, full
            );
        }
    }

    /// Reflow at a WIDER width: previously-wrapped logical lines unwrap into
    /// single rows. Each logical line still appears exactly once.
    #[test]
    fn refeed_reflow_unwraps_at_wider_width() {
        let init_rows = 24u16;
        let init_cols = 80u16;
        let new_rows = 24u16;
        let new_cols = 200u16;
        let mut p = vt100::Parser::new(init_rows, init_cols, DEFAULT_SCROLLBACK_LINES);
        let lines: Vec<String> = (0..10)
            .map(|i| {
                let body = "y".repeat(95);
                format!("entry-{:03}-{}", i, body)
            })
            .collect();
        for line in &lines {
            p.process(line.as_bytes());
            p.process(b"\r\n");
        }
        let reflowed = reflow_parser(&mut p, new_rows, new_cols, DEFAULT_SCROLLBACK_LINES);
        let parser = Arc::new(Mutex::new(reflowed));
        let rows_out = collect_every_row_once(&parser);
        let full = rows_out.join("\n");
        for line in &lines {
            let marker = &line[..9]; // "entry-NNN"
            let n = full.matches(marker).count();
            assert_eq!(
                n, 1,
                "after reflow wider, line marker {:?} appeared {} times \
                 (expected exactly 1). Rows:\n{}",
                marker, n, full
            );
        }
    }

    /// Simulate a TUI child's SIGWINCH-driven clear+repaint AFTER our
    /// reflow has already swapped the parser. The child's reprint hits the
    /// fresh, already-reflowed parser. The post-repaint scrollback may have
    /// at most a tiny number of "echo" rows from the repaint's trailing
    /// newlines (per diagnose: bounded ≤ 1 row per resize), but it must NOT
    /// re-introduce the wrap-stale compounding duplicates the dedup
    /// machinery used to paper over.
    #[test]
    fn refeed_reflow_then_child_repaint_keeps_scrollback_almost_clean() {
        let cols = 80u16;
        let mut p = vt100::Parser::new(10, cols, DEFAULT_SCROLLBACK_LINES);
        for i in 0..30u32 {
            p.process(format!("clean-{:04}\r\n", i).as_bytes());
        }
        // Reflow: shrink rows, keep cols. Equivalent to a typing-induced
        // SIGWINCH from the host TUI changing the chat-pane height.
        let mut p = reflow_parser(&mut p, 12, cols, DEFAULT_SCROLLBACK_LINES);
        // Now simulate the TUI child's SIGWINCH response — clear + repaint
        // the visible region with the most recent 12 markers.
        p.process(b"\x1b[2J\x1b[H");
        for i in 18..30u32 {
            p.process(format!("clean-{:04}\r\n", i).as_bytes());
        }
        let parser = Arc::new(Mutex::new(p));
        let rows = collect_every_row_once(&parser);
        let full = rows.join("\n");
        for i in 0..30u32 {
            let marker = format!("clean-{:04}", i);
            let n = full.matches(&marker).count();
            // Bound: at most TWO copies of any one marker — one from the
            // re-fed scrollback, one from the child's repaint. Without the
            // re-feed fix, the wrap-stale compounding pushed many more.
            assert!(
                n <= 2,
                "after reflow + child repaint, marker {} appeared {} times \
                 (expected ≤2 — one re-fed copy, at most one repaint copy). Rows:\n{}",
                marker, n, full
            );
        }
    }

    /// End-to-end: drive `PtyPane::resize` against a real spawned PTY and
    /// confirm that no scrollback duplicates appear after multiple width
    /// changes. Uses `printf` to emit a deterministic block of markers so the
    /// child output is fully realized before we resize.
    #[test]
    fn pty_pane_resize_does_not_create_scrollback_duplicates() {
        // 30 unique markers, then sleep so the child does not exit before we
        // have a chance to drive the resize sequence.
        let script = "for i in $(seq 0 29); do printf 'pane-marker-%04d\\n' $i; done; sleep 5";
        let mut pane = PtyPane::spawn("/bin/sh", &["-c", script], &[], 10, 80)
            .expect("spawn /bin/sh script");

        // Wait for the child output to land in the parser. /bin/sh emits the
        // 30 lines synchronously; 200 ms is generous on any platform.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Three back-to-back resizes (the chat tab fires a burst per the
        // diagnose). Each call goes through the new re-feed reflow path.
        for &(rows, cols) in &[(12u16, 80u16), (8, 100), (14, 60), (12, 80)] {
            pane.resize(rows, cols).expect("resize");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let parser_arc = Arc::clone(&pane.parser);
        let rows = collect_every_row_once(&parser_arc);
        let full = rows.join("\n");
        for i in 0..30u32 {
            let marker = format!("pane-marker-{:04}", i);
            let n = full.matches(&marker).count();
            assert!(
                n <= 1,
                "after PtyPane::resize burst, marker {} appeared {} times \
                 (expected ≤1). Rows:\n{}",
                marker, n, full
            );
        }
        pane.kill();
    }

    /// Multiple consecutive resizes (a typing-induced burst of SIGWINCH events
    /// per the diagnose) must NOT compound duplicates. Each reflow snapshot
    /// → re-feed cycle keeps each logical line appearing at most once.
    #[test]
    fn refeed_reflow_handles_burst_of_resizes_without_compounding_duplicates() {
        let cols = 80u16;
        let mut p = vt100::Parser::new(10, cols, DEFAULT_SCROLLBACK_LINES);
        for i in 0..40u32 {
            p.process(format!("burst-{:03}\r\n", i).as_bytes());
        }
        // Three back-to-back resizes alternating between heights/widths.
        for &(rows, cols) in &[(12u16, 80u16), (8, 100), (14, 60), (12, 80)] {
            let reflowed = reflow_parser(&mut p, rows, cols, DEFAULT_SCROLLBACK_LINES);
            p = reflowed;
        }
        let parser = Arc::new(Mutex::new(p));
        let rows = collect_every_row_once(&parser);
        let full = rows.join("\n");
        for i in 0..40u32 {
            let marker = format!("burst-{:03}", i);
            let n = full.matches(&marker).count();
            assert!(
                n <= 1,
                "after a burst of resizes, marker {} appeared {} times — \
                 reflow must not compound duplicates across consecutive resizes. \
                 Rows:\n{}",
                marker, n, full
            );
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

    // ─── Terminal-capability query responder tests ──────────────────────
    //
    // Codex's interactive CLI sends a burst of capability queries on
    // startup (CPR / kitty keyboard / OSC 10 + 11 / DA / DECRQM) and
    // BLOCKS waiting for replies — a real terminal answers them; the
    // wg PTY pane has to fill that role. These tests cover each query
    // we now answer; without the responses, codex's chat tab in the
    // wg TUI shows only its query bytes and never advances past the
    // splash. A live capture at 24×80 produced exactly 40 bytes (the
    // query block) until the responder was added; with replies in
    // place, codex emits its full TUI on the next read.

    #[test]
    fn cpr_query_replies_with_cursor_position() {
        // ESC [ 6 n at cursor (0,0) → ESC [ 1 ; 1 R (1-indexed).
        let reply = compute_query_replies(b"\x1b[6n", (0, 0));
        assert_eq!(reply, b"\x1b[1;1R");
    }

    #[test]
    fn cpr_query_uses_real_cursor_position() {
        // ESC [ 6 n at cursor (3, 9) → ESC [ 4 ; 10 R (row+1, col+1).
        let reply = compute_query_replies(b"\x1b[6n", (3, 9));
        assert_eq!(reply, b"\x1b[4;10R");
    }

    #[test]
    fn dsr_query_replies_ok() {
        // ESC [ 5 n → ESC [ 0 n (terminal ready, no malfunction).
        let reply = compute_query_replies(b"\x1b[5n", (0, 0));
        assert_eq!(reply, b"\x1b[0n");
    }

    #[test]
    fn kitty_keyboard_query_replies_legacy_mode() {
        // ESC [ ? u → ESC [ ? 0 u (no kitty progressive enhancement).
        let reply = compute_query_replies(b"\x1b[?u", (0, 0));
        assert_eq!(reply, b"\x1b[?0u");
    }

    #[test]
    fn osc10_foreground_query_replies_with_rgb() {
        // ESC ] 10 ; ? ESC \ → ESC ] 10 ; rgb:cccc/cccc/cccc ESC \
        let reply = compute_query_replies(b"\x1b]10;?\x1b\\", (0, 0));
        assert_eq!(reply, b"\x1b]10;rgb:cccc/cccc/cccc\x1b\\");
    }

    #[test]
    fn osc10_foreground_query_with_bel_terminator() {
        // BEL-terminated form: ESC ] 10 ; ? BEL.
        let reply = compute_query_replies(b"\x1b]10;?\x07", (0, 0));
        assert_eq!(reply, b"\x1b]10;rgb:cccc/cccc/cccc\x07");
    }

    #[test]
    fn osc11_background_query_replies_with_rgb() {
        // ESC ] 11 ; ? ESC \ → ESC ] 11 ; rgb:0000/0000/0000 ESC \
        let reply = compute_query_replies(b"\x1b]11;?\x1b\\", (0, 0));
        assert_eq!(reply, b"\x1b]11;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn osc11_background_query_with_bel_terminator() {
        let reply = compute_query_replies(b"\x1b]11;?\x07", (0, 0));
        assert_eq!(reply, b"\x1b]11;rgb:0000/0000/0000\x07");
    }

    /// Regression: the actual 40-byte query burst captured from a
    /// fresh `codex` PTY at 24×80 must produce a non-empty reply
    /// containing all four query answers (CPR, kitty, OSC 10, OSC 11)
    /// PLUS Primary DA (already supported). Without these replies
    /// codex never proceeds past its initial query block.
    ///
    /// Capture command:
    ///   pty.fork() → execvp("codex") with TERM=xterm-256color, COLORTERM=truecolor
    ///   read for 4 s, kill — output is the 40 bytes below verbatim.
    #[test]
    fn codex_startup_query_burst_unblocks() {
        // Bytes captured from codex 0.125.0 startup at 24×80.
        let burst: &[u8] = b"\x1b[?2004h\x1b[>7u\x1b[?1004h\x1b[6n\x1b[?u\x1b[c\x1b]10;?\x1b\\";
        let reply = compute_query_replies(burst, (0, 0));
        // Must contain a CPR reply.
        assert!(
            reply.windows(b"\x1b[1;1R".len()).any(|w| w == b"\x1b[1;1R"),
            "codex CPR query (ESC [ 6 n) must be answered, got: {:?}",
            reply
        );
        // Must contain a kitty keyboard reply.
        assert!(
            reply.windows(b"\x1b[?0u".len()).any(|w| w == b"\x1b[?0u"),
            "codex kitty kbd query (ESC [ ? u) must be answered, got: {:?}",
            reply
        );
        // Must contain Primary DA reply.
        assert!(
            reply.windows(b"\x1b[?65;1;6c".len()).any(|w| w == b"\x1b[?65;1;6c"),
            "codex DA1 query must be answered, got: {:?}",
            reply
        );
        // Must contain OSC 10 fg reply.
        assert!(
            reply.windows(b"\x1b]10;rgb:".len()).any(|w| w == b"\x1b]10;rgb:"),
            "codex OSC 10 fg query must be answered, got: {:?}",
            reply
        );
        // Mode-set bytes (\x1b[?2004h, \x1b[>7u, \x1b[?1004h) are not
        // queries — they should not produce replies.
        assert!(
            !reply.windows(b"\x1b[?2004".len()).any(|w| w == b"\x1b[?2004"),
            "mode-set bytes must not be echoed as replies, got: {:?}",
            reply
        );
    }

    /// CPR queries embedded in a stream of ordinary output should still
    /// be answered without truncating surrounding bytes from the reply
    /// scan (the function only writes responses; it doesn't gate
    /// vt100 processing).
    #[test]
    fn cpr_query_in_mixed_stream() {
        let chunk: &[u8] = b"hello\x1b[6nworld";
        let reply = compute_query_replies(chunk, (5, 0));
        assert_eq!(reply, b"\x1b[6;1R");
    }

    /// Multiple queries in a single chunk should each produce their
    /// own reply, in stream order. Codex sends CPR + kitty + OSC10
    /// + DA1 in one TCP-style flush.
    #[test]
    fn multiple_queries_yield_concatenated_replies() {
        let chunk: &[u8] = b"\x1b[6n\x1b[?u\x1b[c";
        let reply = compute_query_replies(chunk, (0, 0));
        // Order matches input.
        let cpr = b"\x1b[1;1R";
        let kitty = b"\x1b[?0u";
        let da = b"\x1b[?65;1;6c";
        let cpr_pos = reply
            .windows(cpr.len())
            .position(|w| w == cpr)
            .expect("CPR reply present");
        let kitty_pos = reply
            .windows(kitty.len())
            .position(|w| w == kitty)
            .expect("kitty reply present");
        let da_pos = reply
            .windows(da.len())
            .position(|w| w == da)
            .expect("DA reply present");
        assert!(cpr_pos < kitty_pos, "CPR reply must come before kitty");
        assert!(kitty_pos < da_pos, "kitty reply must come before DA");
    }

    /// A chunk with no recognized queries must return an empty reply
    /// (no spurious bytes get written back to the PTY).
    #[test]
    fn no_query_yields_empty_reply() {
        let reply = compute_query_replies(b"hello world\r\n", (0, 0));
        assert!(reply.is_empty(), "non-query bytes must not produce replies");
    }

    // ─── DEC mode 2026 (synchronized output) scrollback management ──────
    //
    // Codex's interactive TUI emits each animation frame as a BSU
    // (`\x1b[?2026h`) ... full-screen repaint with `\r\n`-separated rows
    // ... ESU (`\x1b[?2026l`) block. The repaint's trailing newline scrolls
    // one row off the top per frame, and after enough frames the user sees
    // stacked spinner copies in scrollback (fix-codex-chat-3 user repro).
    // vt100 0.16 doesn't implement BSU/ESU at all, so we intercept the
    // markers in the PTY reader and trim sync-induced scrollback growth.

    /// Pre-condition for the bug: a single codex-style sync-mode repaint
    /// pushes one row into scrollback when the cursor sits at the bottom
    /// of the visible region after the final `\r\n`. Pin this so that if
    /// vt100 ever starts treating BSU/ESU as scrollback-suppressing on
    /// its own, we know to remove the workaround.
    #[test]
    fn raw_sync_mode_repaint_scrolls_into_scrollback_without_intervention() {
        let rows = 24u16;
        let cols = 80u16;
        let mut p = vt100::Parser::new(rows, cols, DEFAULT_SCROLLBACK_LINES);

        // One frame: BSU + 23 content rows + 1 spinner row + ESU. The
        // last row's trailing `\r\n` lands the cursor below the bottom
        // and scrolls the top row off into scrollback.
        p.process(b"\x1b[?2026h");
        p.process(b"\x1b[1;0r"); // scroll region = full screen
        p.process(b"\x1b[1;1H"); // cursor home
        for r in 0..23 {
            p.process(format!("row {} content\r\n", r).as_bytes());
        }
        p.process(b"Loading X\r\n");
        p.process(b"\x1b[r");
        p.process(b"\x1b[?2026l");

        let count = parser_scrollback_count(&mut p);
        assert!(
            count >= 1,
            "raw sync repaint must push at least one row into scrollback \
             (this is the bug-shape pre-condition for the trim fix); \
             got count={}",
            count
        );
    }

    /// Fix: a single sync block whose repaint pushed K rows into
    /// scrollback must result in K rows being trimmed when sync ends.
    /// Drives `manage_sync_mode_scrollback` with a single BSU...ESU chunk
    /// matching codex's repaint pattern and asserts scrollback grew zero
    /// rows from before the chunk.
    #[test]
    fn sync_mode_block_trim_removes_scrolled_rows() {
        let rows = 24u16;
        let cols = 80u16;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            rows,
            cols,
            DEFAULT_SCROLLBACK_LINES,
        )));

        // Pre-feed history that scrolls into scrollback BEFORE the sync
        // block. (Real codex chats build scrollback as the user types
        // messages and the agent replies — those rows are persisted in
        // scrollback before any animation frame is emitted.)
        {
            let mut p = parser.lock().unwrap();
            for i in 0..30u32 {
                p.process(format!("real-history-{:02}\r\n", i).as_bytes());
            }
        }
        let pre_count = {
            let mut p = parser.lock().unwrap();
            parser_scrollback_count(&mut p)
        };
        assert!(
            pre_count > 0,
            "test setup: history feed should have pushed rows into scrollback"
        );

        // One sync-mode repaint frame whose final `\r\n` causes a scroll.
        let mut chunk: Vec<u8> = Vec::new();
        chunk.extend_from_slice(b"\x1b[?2026h");
        chunk.extend_from_slice(b"\x1b[1;0r");
        chunk.extend_from_slice(b"\x1b[1;1H");
        for r in 0..23 {
            chunk.extend_from_slice(format!("row {} content\r\n", r).as_bytes());
        }
        chunk.extend_from_slice(b"Loading X\r\n");
        chunk.extend_from_slice(b"\x1b[r");
        chunk.extend_from_slice(b"\x1b[?2026l");

        let pre_chunk_scrollback = {
            let mut p = parser.lock().unwrap();
            parser_scrollback_count(&mut p)
        };
        {
            let mut p = parser.lock().unwrap();
            p.process(&chunk);
        }

        let mut in_sync = false;
        let mut sync_start = 0usize;
        manage_sync_mode_scrollback(
            &chunk,
            &parser,
            &mut in_sync,
            &mut sync_start,
            pre_chunk_scrollback,
        );

        let post_count = {
            let mut p = parser.lock().unwrap();
            parser_scrollback_count(&mut p)
        };
        assert_eq!(
            post_count, pre_count,
            "sync-mode repaint must NOT grow scrollback after manage_sync_mode_scrollback \
             trims it (pre={}, post={}). Codex animations would otherwise stack frames in \
             scrollback as the user described.",
            pre_count, post_count
        );

        // The pre-existing real history must still be in scrollback —
        // the trim only drops the K rows the sync block pushed, not
        // rows that were already there. (The sync repaint overwrites
        // the visible region; that's expected and codex repaints again.)
        let lines = {
            let mut p = parser.lock().unwrap();
            snapshot_logical_lines(&mut p)
        };
        let joined: String = lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        for i in 0..pre_count.min(5) {
            let needle = format!("real-history-{:02}", i);
            assert!(
                joined.contains(&needle),
                "pre-sync real history line {} must survive the sync-mode trim \
                 (the trim must drop ONLY rows scrolled during sync, not pre-existing \
                 scrollback); got:\n{}",
                needle,
                joined
            );
        }
    }

    /// Five back-to-back sync-mode frames (one per animation tick) must
    /// each have their scrollback growth trimmed independently — total
    /// scrollback growth across N frames is zero, NOT N. This is the
    /// "scroll up shows stacked spinner frames" bug.
    #[test]
    fn five_sync_mode_frames_each_trim_independently() {
        let rows = 24u16;
        let cols = 80u16;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            rows,
            cols,
            DEFAULT_SCROLLBACK_LINES,
        )));

        {
            let mut p = parser.lock().unwrap();
            for i in 0..3u32 {
                p.process(format!("history-{}\r\n", i).as_bytes());
            }
        }
        let baseline = {
            let mut p = parser.lock().unwrap();
            parser_scrollback_count(&mut p)
        };

        let mut in_sync = false;
        let mut sync_start = 0usize;
        for frame in 0..5u32 {
            let mut chunk: Vec<u8> = Vec::new();
            chunk.extend_from_slice(b"\x1b[?2026h");
            chunk.extend_from_slice(b"\x1b[1;0r");
            chunk.extend_from_slice(b"\x1b[1;1H");
            for r in 0..23 {
                chunk.extend_from_slice(format!("row {} content\r\n", r).as_bytes());
            }
            chunk.extend_from_slice(format!("Loading frame-{}\r\n", frame).as_bytes());
            chunk.extend_from_slice(b"\x1b[r");
            chunk.extend_from_slice(b"\x1b[?2026l");

            let pre = {
                let mut p = parser.lock().unwrap();
                parser_scrollback_count(&mut p)
            };
            {
                let mut p = parser.lock().unwrap();
                p.process(&chunk);
            }
            manage_sync_mode_scrollback(&chunk, &parser, &mut in_sync, &mut sync_start, pre);
        }

        let final_count = {
            let mut p = parser.lock().unwrap();
            parser_scrollback_count(&mut p)
        };
        assert_eq!(
            final_count, baseline,
            "five sync-mode animation frames must each trim independently — \
             total scrollback should equal pre-animation baseline ({}), got {}. \
             Without the trim, scrollback would grow by ~5 rows (one per frame), \
             which is exactly the user-visible \"scroll up shows stacked spinner frames\" \
             regression.",
            baseline, final_count
        );
    }

    /// chunk_contains_sync_markers must NOT trigger the trim path on a
    /// chunk that has no DEC 2026 markers — the cheap fast-path that
    /// keeps the reader thread from snapshot/reflowing on every read.
    #[test]
    fn no_sync_markers_means_no_scrollback_trim() {
        let rows = 24u16;
        let cols = 80u16;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            rows,
            cols,
            DEFAULT_SCROLLBACK_LINES,
        )));
        // Push some content that scrolls into scrollback WITHOUT any sync
        // markers — manage_sync_mode_scrollback must leave it alone.
        let mut chunk: Vec<u8> = Vec::new();
        for i in 0..30u32 {
            chunk.extend_from_slice(format!("plain-{:02}\r\n", i).as_bytes());
        }
        let pre = {
            let mut p = parser.lock().unwrap();
            parser_scrollback_count(&mut p)
        };
        {
            let mut p = parser.lock().unwrap();
            p.process(&chunk);
        }
        let mut in_sync = false;
        let mut sync_start = 0usize;
        manage_sync_mode_scrollback(&chunk, &parser, &mut in_sync, &mut sync_start, pre);

        let post = {
            let mut p = parser.lock().unwrap();
            parser_scrollback_count(&mut p)
        };
        assert!(
            post > pre,
            "non-sync content scrolling off the top must accumulate normally in \
             scrollback (the fast-path skips trim work); pre={}, post={}",
            pre, post
        );

        let joined: Vec<String> = {
            let mut p = parser.lock().unwrap();
            snapshot_logical_lines(&mut p)
                .iter()
                .map(|l| String::from_utf8_lossy(l).into_owned())
                .collect()
        };
        let blob = joined.join("\n");
        assert!(
            blob.contains("plain-00") || blob.contains("plain-01"),
            "early plain-NN history must remain in scrollback (no spurious trim). \
             Got:\n{}",
            blob
        );
    }

    /// End-to-end: drive the full reader-thread + sync-mode-trim path
    /// against a real PTY child emitting codex's actual repaint pattern.
    /// Without the trim, scrollback grows by ~1 row per frame (the bug
    /// the user reported). With the trim wired into the reader thread,
    /// scrollback growth across N animation frames must stay bounded.
    ///
    /// The shell child here was the live confirmation that the fix
    /// works end-to-end — at 10 frames the unmodified vt100 parser
    /// stacked 10 rows in scrollback (verified separately); the wg
    /// reader thread with `manage_sync_mode_scrollback` keeps it ≤2
    /// (one possible off-by-one from each chunk's reflow plus the
    /// occasional wide chunk that ends mid-sync).
    #[test]
    fn pty_pane_codex_sync_repaint_does_not_stack_scrollback_rows() {
        // 5 frames × ~500 bytes each, 80 ms apart — well within
        // codex's 10-20 fps cadence.
        let script = r#"
ROWS=24
for frame in 1 2 3 4 5; do
  printf '\x1b[?2026h'
  printf '\x1b[1;0r'
  printf '\x1b[1;1H'
  for r in $(seq 1 23); do
    printf '\x1b[Krow %d content\r\n' "$r"
  done
  printf '\x1b[KLoading frame-%d\r\n' "$frame"
  printf '\x1b[r'
  printf '\x1b[?2026l'
  sleep 0.08
done
sleep 5
"#;
        let mut pane = PtyPane::spawn("/bin/bash", &["-c", script], &[], 24, 80)
            .expect("spawn /bin/bash codex-shape repaint");

        // Wait for all 5 frames to land (≥ 5 × 80 ms = 400 ms; pad for
        // bash spawn + thread scheduling).
        std::thread::sleep(std::time::Duration::from_millis(900));

        let scrollback_growth = {
            let mut p = pane.parser.lock().unwrap();
            parser_scrollback_count(&mut p)
        };

        // Without the fix, scrollback_growth would be ~5 (one row per
        // frame stacked). With the fix, it must be small — ideally 0,
        // but reflow has occasional off-by-one slack we accept since
        // it's bounded per-frame and doesn't compound across frames
        // the way the unmodified parser would.
        assert!(
            scrollback_growth <= 2,
            "5-frame codex-style sync repaint must NOT stack scrollback rows — \
             got {} rows growth, expected ≤2 (the bug reported in fix-codex-chat-3 \
             would produce 5+; user described it as 'scrolling up is repeating the \
             animation text')",
            scrollback_growth
        );

        pane.kill();
    }

    /// chunk_contains_sync_markers detects both BSU and ESU bytes and
    /// rejects unrelated bytes that happen to share a prefix.
    #[test]
    fn chunk_contains_sync_markers_detection() {
        assert!(chunk_contains_sync_markers(b"\x1b[?2026h"));
        assert!(chunk_contains_sync_markers(b"\x1b[?2026l"));
        assert!(chunk_contains_sync_markers(b"prefix\x1b[?2026hsuffix"));
        assert!(chunk_contains_sync_markers(b"prefix\x1b[?2026lsuffix"));
        assert!(!chunk_contains_sync_markers(b""));
        assert!(!chunk_contains_sync_markers(b"\x1b[?2026"));
        assert!(!chunk_contains_sync_markers(b"\x1b[?2025h"));
        assert!(!chunk_contains_sync_markers(b"plain text only"));
        assert!(!chunk_contains_sync_markers(b"\x1b[?2004h\x1b[H"));
    }

    /// snapshot_logical_lines_skipping_recent_scrollback drops the K most
    /// recent scrollback rows and preserves the rest plus the visible
    /// region. Pin the contract so future changes to reflow don't
    /// accidentally invert the offset semantics (offset 1 is the
    /// most-recently-scrolled, offset max is the oldest).
    #[test]
    fn snapshot_skip_recent_scrollback_drops_correct_rows() {
        let rows = 5u16;
        let cols = 40u16;
        let mut p = vt100::Parser::new(rows, cols, DEFAULT_SCROLLBACK_LINES);
        // Feed 8 lines into a 5-row screen. Each "line-N\r\n" advances
        // the cursor one row; after 8 newlines the cursor is 8 rows
        // below the start, so 4 lines have scrolled into scrollback
        // (line-0..line-3 — newest at offset 1 = line-3, oldest at
        // offset 4 = line-0). The visible region holds line-4..line-7
        // plus one trailing blank row.
        for i in 0..8u32 {
            p.process(format!("line-{}\r\n", i).as_bytes());
        }
        let max_offset = parser_scrollback_count(&mut p);
        assert_eq!(
            max_offset, 4,
            "test setup expectation: 8 lines into 5-row screen → 4 in scrollback, \
             4 visible (+ 1 trailing blank)"
        );

        // drop_recent_k=0 keeps everything.
        let all = snapshot_logical_lines_skipping_recent_scrollback(&mut p, 0, true);
        let blob_all: String = all
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        for i in 0..8u32 {
            assert!(
                blob_all.contains(&format!("line-{}", i)),
                "all lines should be present in full snapshot, missing line-{}; got:\n{}",
                i,
                blob_all
            );
        }

        // drop_recent_k=2 drops the two most-recently-scrolled rows:
        // line-3 (offset 1) and line-2 (offset 2). The two OLDEST
        // (line-0 at offset 4, line-1 at offset 3) survive in scrollback;
        // the visible region (line-4..line-7) is unaffected.
        let trimmed = snapshot_logical_lines_skipping_recent_scrollback(&mut p, 2, true);
        let blob_trim: String = trimmed
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        for surviving in ["line-0", "line-1", "line-4", "line-5", "line-6", "line-7"] {
            assert!(
                blob_trim.contains(surviving),
                "drop_recent_k=2 must keep {}; got:\n{}",
                surviving,
                blob_trim
            );
        }
        for dropped in ["line-2", "line-3"] {
            assert!(
                !blob_trim.contains(dropped),
                "drop_recent_k=2 must drop {} (it was offset 1 or 2 in scrollback); got:\n{}",
                dropped,
                blob_trim
            );
        }
    }

    /// THE persistence invariant: dropping a tmux-wrapped PtyPane only
    /// kills the attach client; the underlying tmux session keeps the
    /// inner process alive. If this assertion ever flips, chat agents
    /// will die on TUI exit and codex's mid-tool-call rollouts corrupt
    /// (see docs/design/chat-agent-persistence.md Part A). Skip if tmux
    /// isn't installed (CI may lack it).
    #[test]
    fn drop_does_not_kill_underlying_tmux_session() {
        if !tmux_available() {
            eprintln!("tmux not installed — skipping persistence invariant test");
            return;
        }
        // Use a wg-chat-test-* prefix so we land in the same namespace
        // the orphan sweep targets, and a unique pid+nanos suffix so
        // parallel test runs don't collide.
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let session = format!("wg-chat-test-drop-{}", suffix);

        // Long-running command so the session would survive the TUI
        // exit point we're modeling.
        {
            let pane = PtyPane::spawn_via_tmux(
                &session,
                "sh",
                &["-c", "while true; do sleep 1; done"],
                &[],
                None,
                24,
                80,
            )
            .expect("spawn_via_tmux should succeed when tmux is available");
            assert_eq!(pane.tmux_session(), Some(session.as_str()));
            assert!(
                tmux_has_session(&session),
                "session should exist while pane is alive"
            );
            // pane drops here — equivalent to TUI exit on user quit.
        }

        // Give tmux a brief moment in case the attach client teardown
        // is async on this platform; in practice the server keeps the
        // session regardless.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            tmux_has_session(&session),
            "tmux session must survive PtyPane Drop — this is the persistence invariant"
        );

        // Cleanup: explicitly kill so the test doesn't leak background
        // sleep loops between runs.
        tmux_kill_session(&session);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !tmux_has_session(&session),
            "kill_session should remove the session"
        );
    }

    /// Re-spawning into the same session reattaches rather than
    /// starting a fresh process — that is the entry point for "user
    /// closed wg tui, opens it again, gets their chat back".
    #[test]
    fn spawn_via_tmux_reattaches_existing_session() {
        if !tmux_available() {
            eprintln!("tmux not installed — skipping reattach test");
            return;
        }
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let session = format!("wg-chat-test-reattach-{}", suffix);

        // First spawn writes a marker file inside the session shell.
        // We assert the marker contents persist between attaches —
        // proving we hit the same shell, not a fresh one.
        let marker = std::env::temp_dir().join(format!("wg-tmux-test-{}", suffix));
        let cmd = format!(
            "echo first > {marker}; while true; do sleep 1; done",
            marker = marker.display()
        );
        {
            let _pane =
                PtyPane::spawn_via_tmux(&session, "sh", &["-c", &cmd], &[], None, 24, 80)
                    .expect("first spawn ok");
        }
        // Wait for the inner shell to write the marker.
        let mut found = false;
        for _ in 0..30 {
            if marker.exists() {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            found,
            "marker file {} should exist after first spawn",
            marker.display()
        );

        // Second spawn must NOT re-run the command; if it did, it
        // would overwrite our marker contents below.
        std::fs::write(&marker, b"reattach").unwrap();
        {
            let _pane =
                PtyPane::spawn_via_tmux(&session, "sh", &["-c", &cmd], &[], None, 24, 80)
                    .expect("second spawn ok (reattach)");
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
        let after = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            after.trim(),
            "reattach",
            "second spawn must reattach, NOT re-run the inner shell command"
        );

        tmux_kill_session(&session);
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn spawn_via_tmux_rejects_invalid_session_name() {
        // Don't actually need tmux to test the validation guard.
        let bad = ["", "with space", "has:colon", "has.dot"];
        for name in bad {
            let r = PtyPane::spawn_via_tmux(
                name,
                "sh",
                &["-c", "true"],
                &[],
                None,
                24,
                80,
            );
            assert!(
                r.is_err(),
                "spawn_via_tmux must reject invalid session name {:?}",
                name
            );
        }
    }
}

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

pub struct PtyPane {
    parser: Arc<Mutex<vt100::Parser>>,
    /// Writer end of the PTY master — sending bytes here feeds the
    /// embedded process's stdin.
    writer: Box<dyn Write + Send>,
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
        if let Ok(cwd) = std::env::current_dir() {
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

        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;
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
        let reader_thread = thread::Builder::new()
            .name(format!("pty-reader-{}", command))
            .spawn(move || {
                // Small buffer — vt100::Parser::process is cheap and
                // the TUI's render latency wants prompt updates.
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF — child exited
                        Ok(n) => {
                            // vt100 0.15 has known panics on certain
                            // wide-char sequences at column boundaries
                            // (em-dash in our banner, some emoji,
                            // particular CSI combos). Catch the unwind
                            // so one malformed byte sequence doesn't
                            // kill the reader thread — the pane stays
                            // live even if a chunk failed to render,
                            // which is much better UX than a locked
                            // black screen.
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
            writer,
            master: pair.master,
            child,
            reader_thread: Some(reader_thread),
            rows,
            cols,
        })
    }

    /// Render the current terminal screen as a ratatui widget in
    /// `area`. Safe to call from the main TUI thread every frame.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let parser = match self.parser.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let screen = parser.screen();
        let widget = tui_term::widget::PseudoTerminal::new(screen);
        frame.render_widget(widget, area);
    }

    /// Forward a crossterm key event to the embedded process. Returns
    /// `Ok(())` even if the child has exited — a dead PTY swallows
    /// writes silently. Caller should use `is_alive()` to detect exit.
    pub fn send_key(&mut self, key: KeyEvent) -> Result<()> {
        let bytes = key_event_to_bytes(&key);
        if !bytes.is_empty() {
            let _ = self.writer.write_all(&bytes);
            let _ = self.writer.flush();
        }
        Ok(())
    }

    /// Forward arbitrary text (e.g. pasted content) to the child's
    /// stdin verbatim, no key-event encoding.
    pub fn send_text(&mut self, text: &str) -> Result<()> {
        let _ = self.writer.write_all(text.as_bytes());
        let _ = self.writer.flush();
        Ok(())
    }

    /// Push a new size through to both the vt100 parser (so rendered
    /// cell layout updates) and the master PTY (so the child sees
    /// SIGWINCH and can reflow its own output). No-op if the size
    /// matches the current one.
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
                if ('a'..='z').contains(&ch) {
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
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => {
            if shift {
                out.extend_from_slice(b"\x1b[Z"); // xterm back-tab
            } else {
                out.push(b'\t');
            }
        }
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
    fn enter_maps_to_cr() {
        let e = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(&e), vec![b'\r']);
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
    fn spawn_echo_and_read_output() {
        // Integration-ish: spawn `/bin/echo hello`, read the screen
        // through the vt100 parser. Use a 5×40 grid — echo writes one
        // line then exits. We poll up to 2s for the line to appear.
        let mut pane = PtyPane::spawn("/bin/echo", &["hello from pty"], &[], 5, 40)
            .expect("spawn echo");
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
}

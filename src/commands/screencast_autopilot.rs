//! Screencast autopilot: drives the TUI in a tmux session for recording.
//!
//! Launches the TUI at a fixed terminal size, sends keystrokes via tmux,
//! reads TUI state via the screen dump IPC socket, and records the session
//! to an asciinema v2 .cast file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::tui::viz_viewer::screen_dump;

// ── Configuration ────────────────────────────────────────────────────────────

/// Autopilot configuration.
#[allow(dead_code)]
pub struct AutopilotConfig {
    pub workgraph_dir: PathBuf,
    pub output: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub fps: u16,
    pub duration: f64,
    pub idle_time_limit: f64,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            workgraph_dir: PathBuf::from(".workgraph"),
            output: PathBuf::from("screencast.cast"),
            cols: 80,
            rows: 24,
            fps: 15,
            duration: 60.0,
            idle_time_limit: 2.0,
        }
    }
}

// ── Asciicast output ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct CastHeader {
    version: u32,
    width: u16,
    height: u16,
    timestamp: i64,
    env: CastEnv,
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_time_limit: Option<f64>,
}

#[derive(Serialize)]
struct CastEnv {
    #[serde(rename = "TERM")]
    term: String,
    #[serde(rename = "SHELL")]
    shell: String,
}

struct CastWriter {
    file: std::io::BufWriter<std::fs::File>,
    start: Instant,
    prev_content: String,
    frame_count: usize,
}

impl CastWriter {
    fn new(path: &Path, cols: u16, rows: u16, idle_time_limit: f64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = std::io::BufWriter::new(
            std::fs::File::create(path)
                .with_context(|| format!("cannot create output: {}", path.display()))?,
        );

        let header = CastHeader {
            version: 2,
            width: cols,
            height: rows,
            timestamp: chrono::Utc::now().timestamp(),
            env: CastEnv {
                term: "xterm-256color".to_string(),
                shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string()),
            },
            idle_time_limit: Some(idle_time_limit),
        };
        serde_json::to_writer(&mut file, &header)?;
        file.write_all(b"\n")?;
        file.flush()?;

        Ok(Self {
            file,
            start: Instant::now(),
            prev_content: String::new(),
            frame_count: 0,
        })
    }

    /// Capture a frame from tmux and write it if content changed.
    fn capture_frame(&mut self, session: &str) -> Result<bool> {
        let output = Command::new("tmux")
            .args(["capture-pane", "-t", session, "-e", "-p"])
            .output()
            .context("failed to run tmux capture-pane")?;

        let content = String::from_utf8_lossy(&output.stdout).to_string();
        if content == self.prev_content {
            return Ok(false);
        }

        let elapsed = self.start.elapsed().as_secs_f64();

        // Strip one trailing newline (tmux always appends one).
        let stripped = content.strip_suffix('\n').unwrap_or(&content);

        // Convert bare LF to CR+LF for correct terminal rendering.
        let fixed = bare_lf_to_crlf(stripped);

        // Full frame: clear screen + home cursor, then content.
        let frame_data = format!("\x1b[H\x1b[2J{}", fixed);

        // Write asciinema v2 entry: [timestamp, "o", "data"]
        write!(self.file, "[{:.6}, \"o\", ", elapsed)?;
        serde_json::to_writer(&mut self.file, &frame_data)?;
        self.file.write_all(b"]\n")?;
        self.file.flush()?;

        self.prev_content = content;
        self.frame_count += 1;
        Ok(true)
    }

    fn finish(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }
}

fn bare_lf_to_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 10);
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            out.push('\r');
        }
        out.push(b as char);
    }
    out
}

// ── Tmux session management ──────────────────────────────────────────────────

struct TmuxSession {
    name: String,
}

impl TmuxSession {
    fn create(name: &str, cols: u16, rows: u16, cwd: &Path, shell_cmd: &str) -> Result<Self> {
        // Kill any existing session with this name.
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .output();

        // Create detached session at exact dimensions.
        let status = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
                shell_cmd,
            ])
            .current_dir(cwd)
            .status()
            .context("failed to create tmux session")?;

        if !status.success() {
            bail!("tmux new-session failed with status {}", status);
        }

        // Force resize (belt and suspenders).
        let _ = Command::new("tmux")
            .args([
                "resize-window",
                "-t",
                name,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
            ])
            .output();

        // Disable tmux status bar for clean content area.
        let _ = Command::new("tmux")
            .args(["set-option", "-t", name, "status", "off"])
            .output();

        Ok(Self {
            name: name.to_string(),
        })
    }

    fn send_keys(&self, keys: &[&str]) -> Result<()> {
        let mut args = vec!["send-keys", "-t", &self.name];
        args.extend(keys);
        let _ = Command::new("tmux").args(&args).output()?;
        Ok(())
    }

    #[allow(dead_code)]
    fn send_text(&self, text: &str) -> Result<()> {
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &self.name, "-l", text])
            .output()?;
        Ok(())
    }

    fn kill(&self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        self.kill();
    }
}

// ── Screen state reader ──────────────────────────────────────────────────────

/// Structured state from the TUI screen dump.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ScreenState {
    pub text: String,
    pub width: u16,
    pub height: u16,
    pub active_tab: String,
    pub focused_panel: String,
    pub selected_task: Option<String>,
    pub input_mode: String,
    pub coordinator_id: u32,
}

impl ScreenState {
    /// Try to read screen state from the TUI dump socket.
    fn read(workgraph_dir: &Path) -> Option<Self> {
        let snap = screen_dump::client_dump(workgraph_dir).ok()?;
        Some(Self {
            text: snap.text,
            width: snap.width,
            height: snap.height,
            active_tab: snap.active_tab,
            focused_panel: snap.focused_panel,
            selected_task: snap.selected_task,
            input_mode: snap.input_mode,
            coordinator_id: snap.coordinator_id,
        })
    }

    /// Count how many task-like lines appear in the screen text.
    fn visible_task_count(&self) -> usize {
        // Tasks typically show as lines containing status indicators.
        self.text
            .lines()
            .filter(|l| {
                l.contains("in-progress")
                    || l.contains("done")
                    || l.contains("open")
                    || l.contains("failed")
                    || l.contains("blocked")
                    || l.contains("●")
                    || l.contains("◉")
                    || l.contains("○")
            })
            .count()
    }

    /// Check if the screen text contains a specific pattern.
    fn contains(&self, pattern: &str) -> bool {
        self.text.to_lowercase().contains(&pattern.to_lowercase())
    }
}

// ── Autopilot script ─────────────────────────────────────────────────────────

/// The autopilot brain: adaptive script that navigates the TUI.
struct Autopilot {
    session: TmuxSession,
    cast: CastWriter,
    config: AutopilotConfig,
    start_time: Instant,
}

impl Autopilot {
    fn new(session: TmuxSession, cast: CastWriter, config: AutopilotConfig) -> Self {
        Self {
            session,
            cast,
            config,
            start_time: Instant::now(),
        }
    }

    fn elapsed(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64()
    }

    fn remaining(&self) -> f64 {
        self.config.duration - self.elapsed()
    }

    fn is_over(&self) -> bool {
        self.remaining() <= 0.0
    }

    /// Sleep while capturing frames.
    fn sleep(&mut self, seconds: f64) -> Result<()> {
        let interval = 1.0 / self.config.fps as f64;
        let deadline = Instant::now() + std::time::Duration::from_secs_f64(seconds);
        while Instant::now() < deadline && !self.is_over() {
            self.cast.capture_frame(&self.session.name)?;
            let remaining = deadline - Instant::now();
            std::thread::sleep(remaining.min(std::time::Duration::from_secs_f64(interval)));
        }
        Ok(())
    }

    /// Send a key and capture a frame.
    fn send_key(&mut self, key: &str) -> Result<()> {
        self.session.send_keys(&[key])?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        self.cast.capture_frame(&self.session.name)?;
        Ok(())
    }

    /// Read the current TUI screen state (structured).
    fn read_screen(&self) -> Option<ScreenState> {
        ScreenState::read(&self.config.workgraph_dir)
    }

    /// Wait for the TUI to start (screen dump socket to become available).
    fn wait_for_tui(&mut self, timeout: f64) -> Result<bool> {
        let deadline = Instant::now() + std::time::Duration::from_secs_f64(timeout);
        while Instant::now() < deadline {
            self.cast.capture_frame(&self.session.name)?;
            if self.read_screen().is_some() {
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Ok(false)
    }

    /// Wait for screen text to contain a pattern.
    #[allow(dead_code)]
    fn wait_for_content(&mut self, pattern: &str, timeout: f64) -> Result<bool> {
        let deadline = Instant::now() + std::time::Duration::from_secs_f64(timeout);
        while Instant::now() < deadline && !self.is_over() {
            self.cast.capture_frame(&self.session.name)?;
            if let Some(state) = self.read_screen()
                && state.contains(pattern)
            {
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Ok(false)
    }

    /// Execute the adaptive autopilot script.
    fn run_script(&mut self) -> Result<()> {
        eprintln!(
            "[autopilot] starting adaptive script ({:.0}s budget)",
            self.config.duration
        );

        // Phase 1: Wait for TUI to start
        eprintln!("[autopilot] phase 1: waiting for TUI...");
        let tui_ready = self.wait_for_tui(15.0)?;
        if !tui_ready {
            bail!("TUI did not start within 15s (screen dump socket not available)");
        }
        eprintln!("[autopilot] TUI is ready");
        self.sleep(2.0)?;

        if self.is_over() {
            return Ok(());
        }

        // Phase 2: Explore task list view
        eprintln!("[autopilot] phase 2: exploring task list");
        self.explore_task_list()?;

        if self.is_over() {
            return Ok(());
        }

        // Phase 3: Switch to graph view and navigate
        eprintln!("[autopilot] phase 3: graph view navigation");
        self.explore_graph_view()?;

        if self.is_over() {
            return Ok(());
        }

        // Phase 4: Cycle through detail panes
        eprintln!("[autopilot] phase 4: detail panes");
        self.cycle_detail_panes()?;

        if self.is_over() {
            return Ok(());
        }

        // Phase 5: Return to task list, browse tasks
        eprintln!("[autopilot] phase 5: final browse");
        self.final_browse()?;

        // Phase 6: Exit
        eprintln!("[autopilot] phase 6: exit");
        self.send_key("q")?;
        self.sleep(1.0)?;

        Ok(())
    }

    /// Phase 2: Navigate down through the task list.
    fn explore_task_list(&mut self) -> Result<()> {
        let state = self.read_screen();
        let task_count = state.as_ref().map_or(5, |s| s.visible_task_count().max(3));
        let nav_count = task_count.min(8);

        // Navigate down through tasks.
        for _ in 0..nav_count {
            if self.is_over() {
                return Ok(());
            }
            self.send_key("Down")?;
            self.sleep(0.8)?;

            // Check if selected task is interesting (in-progress, etc.).
            if let Some(state) = self.read_screen()
                && let Some(ref task) = state.selected_task
                && state.text.contains("in-progress")
            {
                eprintln!("[autopilot]   interesting task: {}", task);
                self.sleep(1.5)?;
            }
        }

        self.sleep(1.0)?;

        // Navigate back up a few.
        for _ in 0..nav_count.min(3) {
            if self.is_over() {
                return Ok(());
            }
            self.send_key("Up")?;
            self.sleep(0.6)?;
        }

        self.sleep(1.0)?;
        Ok(())
    }

    /// Phase 3: Tab to graph view and arrow through nodes.
    fn explore_graph_view(&mut self) -> Result<()> {
        self.send_key("Tab")?;
        self.sleep(1.5)?;

        // Navigate through graph nodes.
        for i in 0..6 {
            if self.is_over() {
                return Ok(());
            }
            self.send_key("Down")?;
            self.sleep(0.8)?;

            // Every other node, show a detail pane.
            if i % 2 == 1 {
                let pane = (i / 2 % 4 + 1) as u8;
                self.send_key(&pane.to_string())?;
                self.sleep(1.5)?;
            }
        }

        self.sleep(1.0)?;
        Ok(())
    }

    /// Phase 4: Cycle through detail panes 1-4 on the current selection.
    fn cycle_detail_panes(&mut self) -> Result<()> {
        for pane in 1..=4 {
            if self.is_over() {
                return Ok(());
            }
            self.send_key(&pane.to_string())?;
            self.sleep(2.0)?;

            // Read what the detail pane shows.
            if let Some(state) = self.read_screen() {
                eprintln!(
                    "[autopilot]   pane {}: tab={}, task={:?}",
                    pane, state.active_tab, state.selected_task
                );
            }
        }

        self.sleep(1.0)?;
        Ok(())
    }

    /// Phase 5: Tab back to task list, drill into interesting tasks.
    fn final_browse(&mut self) -> Result<()> {
        self.send_key("Tab")?;
        self.sleep(1.5)?;

        // Go to top.
        self.send_key("Home")?;
        self.sleep(1.0)?;

        // Scan through tasks looking for activity.
        for _ in 0..5 {
            if self.is_over() {
                return Ok(());
            }
            self.send_key("Down")?;
            self.sleep(0.8)?;

            if let Some(state) = self.read_screen()
                && (state.contains("in-progress") || state.contains("done"))
            {
                // Linger on active/completed tasks.
                self.send_key("1")?;
                self.sleep(2.0)?;
            }
        }

        self.sleep(1.0)?;
        Ok(())
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

pub fn run(workgraph_dir: &Path, output: &Path, cols: u16, rows: u16, duration: f64) -> Result<()> {
    // Verify tmux is available.
    let tmux_check = Command::new("tmux").arg("-V").output();
    match tmux_check {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout);
            eprintln!("[autopilot] tmux: {}", version.trim());
        }
        _ => bail!("tmux is required but not found. Install it with: apt install tmux"),
    }

    let wg_dir =
        std::fs::canonicalize(workgraph_dir).unwrap_or_else(|_| workgraph_dir.to_path_buf());

    // The project root is one level up from .workgraph
    let project_dir = wg_dir.parent().unwrap_or(&wg_dir);

    let session_name = format!("wg-autopilot-{}", std::process::id());

    // Build the shell command to launch the TUI.
    let tui_cmd = format!(
        "cd '{}' && wg tui --show-keys --recording",
        project_dir.display()
    );

    eprintln!(
        "[autopilot] launching TUI in tmux session '{}' ({}x{})",
        session_name, cols, rows
    );
    eprintln!("[autopilot] cmd: {}", tui_cmd);

    let session = TmuxSession::create(&session_name, cols, rows, project_dir, &tui_cmd)?;

    // Give the TUI a moment to start.
    std::thread::sleep(std::time::Duration::from_secs(1));

    let cast = CastWriter::new(output, cols, rows, 2.0)?;

    let config = AutopilotConfig {
        workgraph_dir: wg_dir,
        output: output.to_path_buf(),
        cols,
        rows,
        duration,
        ..Default::default()
    };

    let mut autopilot = Autopilot::new(session, cast, config);
    let result = autopilot.run_script();

    // Finalize recording.
    autopilot.cast.finish()?;

    let duration = autopilot.elapsed();
    let frames = autopilot.cast.frame_count;
    eprintln!(
        "[autopilot] recording complete: {:.1}s, {} frames → {}",
        duration,
        frames,
        output.display()
    );

    result
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screencast_autopilot_bare_lf_to_crlf() {
        assert_eq!(bare_lf_to_crlf("hello\nworld"), "hello\r\nworld");
        assert_eq!(bare_lf_to_crlf("already\r\nfine"), "already\r\nfine");
        assert_eq!(bare_lf_to_crlf("no newlines"), "no newlines");
        assert_eq!(bare_lf_to_crlf("\nstart"), "\r\nstart");
        assert_eq!(bare_lf_to_crlf("end\n"), "end\r\n");
    }

    #[test]
    fn screencast_autopilot_cast_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cast");
        let writer = CastWriter::new(&path, 80, 24, 2.0).unwrap();
        drop(writer);

        let contents = std::fs::read_to_string(&path).unwrap();
        let header: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 80);
        assert_eq!(header["height"], 24);
    }

    #[test]
    fn screencast_autopilot_screen_state_contains() {
        let state = ScreenState {
            text: "Tasks: 5 open, 2 in-progress, 1 done".to_string(),
            ..Default::default()
        };
        assert!(state.contains("in-progress"));
        assert!(state.contains("IN-PROGRESS"));
        assert!(!state.contains("failed"));
    }

    #[test]
    fn screencast_autopilot_screen_state_task_count() {
        let state = ScreenState {
            text: "● task-a  open\n◉ task-b  in-progress\n○ task-c  done\nnot-a-task".to_string(),
            ..Default::default()
        };
        assert_eq!(state.visible_task_count(), 3);
    }
}

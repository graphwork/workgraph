use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{self, ClearType},
};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use workgraph::graph::Status;

use super::trace::{collect_descendants, format_duration, reconstruct_temporal, GraphSnapshot};

/// Run the trace animation TUI.
pub fn run(dir: &Path, root_id: &str, speed: f64) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;
    let _root = graph.get_task_or_err(root_id)?;

    let descendants = collect_descendants(root_id, &graph);
    let subgraph_ids: HashSet<&str> = descendants.iter().map(|t| t.id.as_str()).collect();

    let snapshots = reconstruct_temporal(dir, &subgraph_ids)?;
    if snapshots.is_empty() {
        println!("No operation history found for trace '{}'. Nothing to animate.", root_id);
        return Ok(());
    }

    // Compute total duration for the progress bar
    let first_ts = snapshots.first().unwrap().timestamp;
    let last_ts = snapshots.last().unwrap().timestamp;
    let total_secs = (last_ts - first_ts).num_seconds().max(1);

    // Enter raw mode
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = run_animation_loop(
        &mut stdout,
        &graph,
        &descendants,
        &subgraph_ids,
        &snapshots,
        first_ts,
        total_secs,
        speed,
    );

    // Restore terminal
    execute!(stdout, cursor::Show, terminal::EnterAlternateScreen)?;
    terminal::disable_raw_mode()?;
    // Leave alternate screen *after* disabling raw mode
    execute!(io::stdout(), terminal::LeaveAlternateScreen)?;

    result
}

#[allow(clippy::too_many_arguments)]
fn run_animation_loop(
    stdout: &mut io::Stdout,
    graph: &workgraph::graph::WorkGraph,
    descendants: &[&workgraph::graph::Task],
    subgraph_ids: &HashSet<&str>,
    snapshots: &[GraphSnapshot],
    first_ts: chrono::DateTime<chrono::Utc>,
    total_secs: i64,
    initial_speed: f64,
) -> Result<()> {
    let task_ids: HashSet<&str> = subgraph_ids.iter().copied().collect();
    let annotations = HashMap::new();
    let mut current_idx: usize = 0;
    let mut paused = false;
    let mut speed = initial_speed;

    // Render initial frame
    render_frame(
        stdout,
        graph,
        descendants,
        &task_ids,
        &annotations,
        &snapshots[current_idx],
        current_idx,
        snapshots.len(),
        first_ts,
        total_secs,
        speed,
        paused,
    )?;

    loop {
        // Calculate sleep duration based on gap to next snapshot
        let sleep_ms = if paused || current_idx >= snapshots.len() - 1 {
            50 // Just poll for input
        } else {
            let gap = (snapshots[current_idx + 1].timestamp - snapshots[current_idx].timestamp)
                .num_milliseconds() as f64;
            let scaled = (gap / speed).clamp(50.0, 5000.0);
            scaled as u64
        };

        // Poll for keyboard input
        if event::poll(Duration::from_millis(sleep_ms.min(100)))? {
            if let Event::Key(KeyEvent { code, .. }) = event::read()? {
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char(' ') => {
                        paused = !paused;
                        render_frame(
                            stdout,
                            graph,
                            descendants,
                            &task_ids,
                            &annotations,
                            &snapshots[current_idx],
                            current_idx,
                            snapshots.len(),
                            first_ts,
                            total_secs,
                            speed,
                            paused,
                        )?;
                    }
                    KeyCode::Right => {
                        if current_idx < snapshots.len() - 1 {
                            current_idx += 1;
                            render_frame(
                                stdout,
                                graph,
                                descendants,
                                &task_ids,
                                &annotations,
                                &snapshots[current_idx],
                                current_idx,
                                snapshots.len(),
                                first_ts,
                                total_secs,
                                speed,
                                paused,
                            )?;
                        }
                    }
                    KeyCode::Left => {
                        if current_idx > 0 {
                            current_idx -= 1;
                            render_frame(
                                stdout,
                                graph,
                                descendants,
                                &task_ids,
                                &annotations,
                                &snapshots[current_idx],
                                current_idx,
                                snapshots.len(),
                                first_ts,
                                total_secs,
                                speed,
                                paused,
                            )?;
                        }
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        speed *= 2.0;
                        render_frame(
                            stdout,
                            graph,
                            descendants,
                            &task_ids,
                            &annotations,
                            &snapshots[current_idx],
                            current_idx,
                            snapshots.len(),
                            first_ts,
                            total_secs,
                            speed,
                            paused,
                        )?;
                    }
                    KeyCode::Char('-') => {
                        speed = (speed / 2.0).max(1.0);
                        render_frame(
                            stdout,
                            graph,
                            descendants,
                            &task_ids,
                            &annotations,
                            &snapshots[current_idx],
                            current_idx,
                            snapshots.len(),
                            first_ts,
                            total_secs,
                            speed,
                            paused,
                        )?;
                    }
                    _ => {}
                }
            }
        } else if !paused && current_idx < snapshots.len() - 1 {
            // Advance to next snapshot
            current_idx += 1;
            render_frame(
                stdout,
                graph,
                descendants,
                &task_ids,
                &annotations,
                &snapshots[current_idx],
                current_idx,
                snapshots.len(),
                first_ts,
                total_secs,
                speed,
                paused,
            )?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_frame(
    stdout: &mut io::Stdout,
    graph: &workgraph::graph::WorkGraph,
    descendants: &[&workgraph::graph::Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
    snapshot: &GraphSnapshot,
    current_idx: usize,
    total_snapshots: usize,
    first_ts: chrono::DateTime<chrono::Utc>,
    total_secs: i64,
    speed: f64,
    paused: bool,
) -> Result<()> {
    // Build status overrides from snapshot
    let status_overrides: HashMap<&str, Status> = snapshot
        .statuses
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();

    // Generate the graph with overridden statuses
    let graph_output = super::viz::generate_graph_with_overrides(
        graph,
        descendants,
        task_ids,
        annotations,
        &status_overrides,
    );

    // Compute elapsed time
    let elapsed = (snapshot.timestamp - first_ts).num_seconds();
    let elapsed_str = format_duration(elapsed);
    let total_str = format_duration(total_secs);

    // Build progress bar
    let (term_width, _term_height) = terminal::size().unwrap_or((80, 24));
    let bar_width = (term_width as usize).saturating_sub(30).max(10);
    let progress = if total_secs > 0 {
        (elapsed as f64 / total_secs as f64).min(1.0)
    } else {
        1.0
    };
    let filled = (progress * bar_width as f64) as usize;
    let bar: String = format!(
        "[{}{}] {} / {}",
        "=".repeat(filled.min(bar_width)),
        " ".repeat(bar_width.saturating_sub(filled)),
        elapsed_str,
        total_str,
    );

    // Status line
    let pause_indicator = if paused { " PAUSED" } else { "" };
    let status_line = format!(
        "Step {}/{} | Speed: {:.0}x{} | q:quit space:pause ←→:step +/-:speed",
        current_idx + 1,
        total_snapshots,
        speed,
        pause_indicator,
    );

    // Count statuses in snapshot
    let done_count = snapshot
        .statuses
        .values()
        .filter(|s| **s == Status::Done)
        .count();
    let in_progress_count = snapshot
        .statuses
        .values()
        .filter(|s| **s == Status::InProgress)
        .count();
    let failed_count = snapshot
        .statuses
        .values()
        .filter(|s| **s == Status::Failed)
        .count();
    let summary = format!(
        "\x1b[32m{} done\x1b[0m  \x1b[33m{} in-progress\x1b[0m  \x1b[31m{} failed\x1b[0m  {} total",
        done_count,
        in_progress_count,
        failed_count,
        snapshot.statuses.len(),
    );

    // Clear screen and draw
    execute!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(ClearType::All),
    )?;

    write!(stdout, "\x1b[1mTrace Animation: {}\x1b[0m\r\n", snapshot.timestamp.format("%H:%M:%S"))?;
    write!(stdout, "{}\r\n\r\n", summary)?;

    for line in graph_output.lines() {
        write!(stdout, "{}\r\n", line)?;
    }

    write!(stdout, "\r\n{}\r\n", bar)?;
    write!(stdout, "{}\r\n", status_line)?;

    stdout.flush()?;

    Ok(())
}

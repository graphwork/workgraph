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
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
        &std::collections::HashSet::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Progress bar math ──

    #[test]
    fn test_progress_bar_calculation() {
        // Simulating the progress bar logic from render_frame
        let total_secs: i64 = 100;
        let elapsed: i64 = 50;
        let bar_width: usize = 40;

        let progress = (elapsed as f64 / total_secs as f64).min(1.0);
        assert!((progress - 0.5).abs() < f64::EPSILON);

        let filled = (progress * bar_width as f64) as usize;
        assert_eq!(filled, 20);

        let bar = format!(
            "[{}{}]",
            "=".repeat(filled.min(bar_width)),
            " ".repeat(bar_width.saturating_sub(filled)),
        );
        assert_eq!(bar.len(), bar_width + 2); // +2 for brackets
        assert!(bar.starts_with('['));
        assert!(bar.ends_with(']'));
    }

    #[test]
    fn test_progress_bar_at_boundaries() {
        // At start (elapsed = 0)
        let progress_start = (0_f64 / 100_f64).min(1.0);
        assert_eq!(progress_start, 0.0);

        // At end (elapsed = total)
        let progress_end = (100_f64 / 100_f64).min(1.0);
        assert_eq!(progress_end, 1.0);

        // Past end (clamped)
        let progress_past = (150_f64 / 100_f64).min(1.0);
        assert_eq!(progress_past, 1.0);
    }

    // ── Speed control ──

    #[test]
    fn test_speed_doubling() {
        let mut speed: f64 = 10.0;
        speed *= 2.0;
        assert_eq!(speed, 20.0);
        speed *= 2.0;
        assert_eq!(speed, 40.0);
    }

    #[test]
    fn test_speed_halving_clamped() {
        let mut speed: f64 = 4.0;
        speed = (speed / 2.0).max(1.0);
        assert_eq!(speed, 2.0);
        speed = (speed / 2.0).max(1.0);
        assert_eq!(speed, 1.0);
        // Cannot go below 1.0
        speed = (speed / 2.0).max(1.0);
        assert_eq!(speed, 1.0);
    }

    // ── Sleep duration scaling ──

    #[test]
    fn test_sleep_duration_scaling() {
        // Simulating the sleep calculation from run_animation_loop
        let gap_ms: f64 = 2000.0; // 2 seconds between snapshots
        let speed: f64 = 10.0;

        let scaled = (gap_ms / speed).clamp(50.0, 5000.0);
        assert_eq!(scaled, 200.0);
    }

    #[test]
    fn test_sleep_duration_clamp_min() {
        let gap_ms: f64 = 10.0; // Very small gap
        let speed: f64 = 100.0;
        let scaled = (gap_ms / speed).clamp(50.0, 5000.0);
        assert_eq!(scaled, 50.0); // Clamped to minimum
    }

    #[test]
    fn test_sleep_duration_clamp_max() {
        let gap_ms: f64 = 100_000.0; // Very large gap
        let speed: f64 = 1.0;
        let scaled = (gap_ms / speed).clamp(50.0, 5000.0);
        assert_eq!(scaled, 5000.0); // Clamped to maximum
    }

    // ── Status counting from snapshots ──

    #[test]
    fn test_snapshot_status_counting() {
        let mut statuses: HashMap<String, Status> = HashMap::new();
        statuses.insert("t1".to_string(), Status::Done);
        statuses.insert("t2".to_string(), Status::Done);
        statuses.insert("t3".to_string(), Status::InProgress);
        statuses.insert("t4".to_string(), Status::Failed);
        statuses.insert("t5".to_string(), Status::Open);

        let done_count = statuses.values().filter(|s| **s == Status::Done).count();
        let in_progress_count = statuses.values().filter(|s| **s == Status::InProgress).count();
        let failed_count = statuses.values().filter(|s| **s == Status::Failed).count();

        assert_eq!(done_count, 2);
        assert_eq!(in_progress_count, 1);
        assert_eq!(failed_count, 1);
        assert_eq!(statuses.len(), 5);
    }
}

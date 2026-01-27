use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use workgraph::graph::{Node, Status, Task};
use workgraph::parser::load_graph;

use super::graph_path;

fn archive_path(dir: &Path) -> std::path::PathBuf {
    dir.join("archive.jsonl")
}

/// Load archived tasks from the archive file
fn load_archive(archive_path: &Path) -> Result<Vec<Task>> {
    if !archive_path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(archive_path)
        .with_context(|| format!("Failed to open archive file: {:?}", archive_path))?;
    let reader = BufReader::new(file);
    let mut tasks = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let node: Node = serde_json::from_str(trimmed).with_context(|| {
            format!(
                "Failed to parse archive line {}: {}",
                line_num + 1,
                trimmed
            )
        })?;
        if let Node::Task(task) = node {
            tasks.push(task);
        }
    }

    Ok(tasks)
}

/// Parse a date string (YYYY-MM-DD) to DateTime<Utc>
fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("Invalid date format '{}', expected YYYY-MM-DD", s))?;
    Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

/// Check if a task falls within the date range
fn in_date_range(task: &Task, since: Option<&DateTime<Utc>>, until: Option<&DateTime<Utc>>) -> bool {
    let completed = task.completed_at.as_ref().and_then(|s| {
        DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.with_timezone(&Utc))
    });

    match (completed, since, until) {
        (Some(completed), Some(since), Some(until)) => completed >= *since && completed <= *until,
        (Some(completed), Some(since), None) => completed >= *since,
        (Some(completed), None, Some(until)) => completed <= *until,
        (None, Some(_), _) | (None, _, Some(_)) => false, // No timestamp, exclude if filtering
        (_, None, None) => true, // No filter, include
    }
}

/// Get the color for a task based on its status
fn status_color(status: &Status, is_archived: bool) -> &'static str {
    if is_archived {
        return "lightgray";
    }
    match status {
        Status::Done => "palegreen",
        Status::InProgress => "coral",        // Red/orange - active work, draws attention
        Status::Blocked => "khaki",           // Yellow - waiting
        Status::Open => "white",              // Ready to pick up
        Status::Failed => "salmon",           // Red-ish - needs attention
        Status::Abandoned => "lightgray",     // Grayed out
    }
}

/// Get the border style for special states
fn status_style(status: &Status, is_archived: bool) -> &'static str {
    if is_archived {
        return "filled,dashed";
    }
    match status {
        Status::InProgress => "filled,bold",  // Bold border for active work
        Status::Failed => "filled,bold",      // Bold for attention
        _ => "filled",
    }
}

pub fn run(dir: &Path, include_archive: bool, since: Option<&str>, until: Option<&str>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Parse date filters
    let since_dt = since.map(parse_date).transpose()?;
    let until_dt = until.map(parse_date).transpose()?;

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Collect tasks from main graph
    let mut all_tasks: Vec<(Task, bool)> = graph
        .tasks()
        .filter(|t| in_date_range(t, since_dt.as_ref(), until_dt.as_ref()))
        .map(|t| (t.clone(), false))
        .collect();

    // Load archived tasks if requested
    if include_archive {
        let arch_path = archive_path(dir);
        let archived = load_archive(&arch_path)?;
        for task in archived {
            if in_date_range(&task, since_dt.as_ref(), until_dt.as_ref()) {
                all_tasks.push((task, true));
            }
        }
    }

    // Print DOT format for visualization
    println!("digraph workgraph {{");
    println!("  rankdir=LR;");
    println!("  node [shape=box];");

    // Add legend
    println!();
    println!("  // Legend");
    println!("  subgraph cluster_legend {{");
    println!("    label=\"Legend\";");
    println!("    style=dashed;");
    println!("    fontsize=10;");
    println!("    legend_open [label=\"Open\", style=filled, fillcolor=white];");
    println!("    legend_progress [label=\"In Progress\", style=\"filled,bold\", fillcolor=coral];");
    println!("    legend_blocked [label=\"Blocked\", style=filled, fillcolor=khaki];");
    println!("    legend_done [label=\"Done\", style=filled, fillcolor=palegreen];");
    println!("    legend_failed [label=\"Failed\", style=\"filled,bold\", fillcolor=salmon];");
    if include_archive {
        println!("    legend_archived [label=\"Archived\", style=\"filled,dashed\", fillcolor=lightgray];");
    }
    println!("    legend_open -> legend_progress -> legend_blocked -> legend_done -> legend_failed [style=invis];");
    println!("  }}");
    println!();

    // Print task nodes
    for (task, is_archived) in &all_tasks {
        let color = status_color(&task.status, *is_archived);
        let style = status_style(&task.status, *is_archived);

        // Add assigned actor to label if claimed
        let label = if let Some(ref assigned) = task.assigned {
            format!("{}\\n{}\\n[{}]", task.id, task.title, assigned)
        } else {
            format!("{}\\n{}", task.id, task.title)
        };

        println!(
            "  \"{}\" [label=\"{}\", style=\"{}\", fillcolor={}];",
            task.id, label, style, color
        );
    }

    // Print actors
    for actor in graph.actors() {
        let name = actor.name.as_deref().unwrap_or(&actor.id);
        println!(
            "  \"{}\" [label=\"{}\", shape=ellipse, style=filled, fillcolor=lightblue];",
            actor.id, name
        );
    }

    // Print resources
    for resource in graph.resources() {
        let name = resource.name.as_deref().unwrap_or(&resource.id);
        println!(
            "  \"{}\" [label=\"{}\", shape=diamond, style=filled, fillcolor=lightyellow];",
            resource.id, name
        );
    }

    println!();

    // Print edges (only for tasks in our filtered set)
    let task_ids: std::collections::HashSet<_> = all_tasks.iter().map(|(t, _)| &t.id).collect();

    for (task, _) in &all_tasks {
        for blocked in &task.blocked_by {
            // Only draw edge if both tasks are in our set
            if task_ids.contains(blocked) {
                println!("  \"{}\" -> \"{}\" [label=\"blocks\"];", blocked, task.id);
            }
        }
        if let Some(ref assigned) = task.assigned {
            println!(
                "  \"{}\" -> \"{}\" [style=dashed, label=\"assigned\"];",
                task.id, assigned
            );
        }
        for req in &task.requires {
            if task_ids.contains(req) {
                println!(
                    "  \"{}\" -> \"{}\" [style=dotted, label=\"requires\"];",
                    task.id, req
                );
            }
        }
    }

    println!("}}");

    Ok(())
}

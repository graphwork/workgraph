use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;
use workgraph::graph::{Status, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;

/// Number of weeks to show by default
const DEFAULT_WEEKS: usize = 4;

/// Bar chart width in characters
const BAR_WIDTH: usize = 25;

/// Weekly velocity data
#[derive(Debug, Clone, Serialize)]
pub struct WeeklyVelocity {
    pub week_number: usize,
    pub tasks_completed: usize,
    pub hours_completed: f64,
    pub task_ids: Vec<String>,
}

/// Overall velocity summary
#[derive(Debug, Serialize)]
pub struct VelocitySummary {
    pub weeks: Vec<WeeklyVelocity>,
    pub average_tasks_per_week: f64,
    pub average_hours_per_week: f64,
    pub trend: String,
    pub open_tasks: usize,
    pub open_hours: f64,
    pub weeks_to_clear: Option<f64>,
    pub tasks_without_timestamp: usize,
}

/// Calculate completion velocity from the graph
pub fn calculate_velocity(graph: &WorkGraph, num_weeks: usize) -> VelocitySummary {
    let now = Utc::now();
    let mut weeks: Vec<WeeklyVelocity> = Vec::with_capacity(num_weeks);

    // Initialize weeks (week 1 is oldest, week N is most recent)
    for i in 0..num_weeks {
        weeks.push(WeeklyVelocity {
            week_number: i + 1,
            tasks_completed: 0,
            hours_completed: 0.0,
            task_ids: Vec::new(),
        });
    }

    let mut tasks_without_timestamp = 0;

    // Collect completed tasks with timestamps
    for task in graph.tasks() {
        if task.status != Status::Done {
            continue;
        }

        let completed_at = match &task.completed_at {
            Some(ts) => match DateTime::parse_from_rfc3339(ts) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(_) => {
                    tasks_without_timestamp += 1;
                    continue;
                }
            },
            None => {
                tasks_without_timestamp += 1;
                continue;
            }
        };

        // Calculate which week this completion falls into
        let days_ago = (now - completed_at).num_days();
        if days_ago < 0 {
            // Future date, skip
            continue;
        }

        let weeks_ago = (days_ago / 7) as usize;
        if weeks_ago >= num_weeks {
            // Too old, not in our window
            continue;
        }

        // weeks_ago == 0 means current week (most recent), which is week N (last in array)
        let week_index = num_weeks - 1 - weeks_ago;

        weeks[week_index].tasks_completed += 1;
        weeks[week_index].task_ids.push(task.id.clone());

        if let Some(ref estimate) = task.estimate {
            if let Some(hours) = estimate.hours {
                weeks[week_index].hours_completed += hours;
            }
        }
    }

    // Calculate averages
    let total_tasks: usize = weeks.iter().map(|w| w.tasks_completed).sum();
    let total_hours: f64 = weeks.iter().map(|w| w.hours_completed).sum();
    let average_tasks_per_week = if num_weeks > 0 {
        total_tasks as f64 / num_weeks as f64
    } else {
        0.0
    };
    let average_hours_per_week = if num_weeks > 0 {
        total_hours / num_weeks as f64
    } else {
        0.0
    };

    // Calculate trend (compare first half to second half)
    let trend = calculate_trend(&weeks);

    // Count open tasks and their hours
    let mut open_tasks = 0;
    let mut open_hours = 0.0;
    for task in graph.tasks() {
        if task.status == Status::Open || task.status == Status::InProgress {
            open_tasks += 1;
            if let Some(ref estimate) = task.estimate {
                if let Some(hours) = estimate.hours {
                    open_hours += hours;
                }
            }
        }
    }

    // Estimate weeks to clear
    let weeks_to_clear = if average_tasks_per_week > 0.0 && open_tasks > 0 {
        Some(open_tasks as f64 / average_tasks_per_week)
    } else {
        None
    };

    VelocitySummary {
        weeks,
        average_tasks_per_week,
        average_hours_per_week,
        trend,
        open_tasks,
        open_hours,
        weeks_to_clear,
        tasks_without_timestamp,
    }
}

/// Determine trend based on weekly data
fn calculate_trend(weeks: &[WeeklyVelocity]) -> String {
    if weeks.len() < 2 {
        return "insufficient data".to_string();
    }

    let mid = weeks.len() / 2;
    let first_half: usize = weeks[..mid].iter().map(|w| w.tasks_completed).sum();
    let second_half: usize = weeks[mid..].iter().map(|w| w.tasks_completed).sum();

    if first_half == 0 && second_half == 0 {
        return "no completions".to_string();
    }

    let first_avg = first_half as f64 / mid as f64;
    let second_avg = second_half as f64 / (weeks.len() - mid) as f64;

    // Calculate percentage change
    if first_avg == 0.0 {
        if second_avg > 0.0 {
            return "increasing".to_string();
        }
        return "stable".to_string();
    }

    let change = (second_avg - first_avg) / first_avg;

    if change > 0.2 {
        "increasing".to_string()
    } else if change < -0.2 {
        "decreasing".to_string()
    } else {
        "stable".to_string()
    }
}

/// Generate an ASCII bar for the given value
fn make_bar(value: usize, max_value: usize, below_average: bool) -> String {
    let bar_length = if max_value > 0 {
        (value as f64 / max_value as f64 * BAR_WIDTH as f64).round() as usize
    } else {
        0
    };

    let bar = "|".repeat(bar_length);
    let padding = " ".repeat(BAR_WIDTH - bar_length);

    if below_average && value > 0 {
        format!("[{}{}] [below average]", bar, padding)
    } else {
        format!("[{}{}]", bar, padding)
    }
}

pub fn run(dir: &Path, json: bool, weeks: Option<usize>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let num_weeks = weeks.unwrap_or(DEFAULT_WEEKS);
    let summary = calculate_velocity(&graph, num_weeks);

    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_human_output(&summary);
    }

    Ok(())
}

fn print_human_output(summary: &VelocitySummary) {
    println!("Completion Velocity (last {} days):\n", summary.weeks.len() * 7);

    // Find max for scaling bars
    let max_tasks = summary
        .weeks
        .iter()
        .map(|w| w.tasks_completed)
        .max()
        .unwrap_or(0);

    // Print each week
    for week in &summary.weeks {
        let below_average = week.tasks_completed > 0
            && (week.tasks_completed as f64) < summary.average_tasks_per_week * 0.7;

        let bar = make_bar(week.tasks_completed, max_tasks, below_average);

        let hours_str = if week.hours_completed > 0.0 {
            format!(" ({:.0}h)", week.hours_completed)
        } else {
            String::new()
        };

        println!(
            "  Week {}: {} tasks{} {}",
            week.week_number, week.tasks_completed, hours_str, bar
        );
    }

    println!();
    println!(
        "  Average: {:.1} tasks/week ({:.0}h/week)",
        summary.average_tasks_per_week, summary.average_hours_per_week
    );
    println!("  Trend: {}", summary.trend);

    // Projection
    if summary.open_tasks > 0 {
        println!();
        println!("At current velocity:");

        if let Some(weeks_to_clear) = summary.weeks_to_clear {
            let weeks_str = if weeks_to_clear < 1.0 {
                "less than a week".to_string()
            } else if weeks_to_clear < 2.0 {
                "~1 week".to_string()
            } else {
                format!("~{:.0} weeks", weeks_to_clear)
            };

            let hours_str = if summary.open_hours > 0.0 {
                format!(" ({:.0}h)", summary.open_hours)
            } else {
                String::new()
            };

            println!(
                "  Open tasks ({}){}: {} to clear",
                summary.open_tasks, hours_str, weeks_str
            );
        } else {
            println!(
                "  Open tasks ({}): unable to estimate (no recent completions)",
                summary.open_tasks
            );
        }
    }

    // Note about missing timestamps
    if summary.tasks_without_timestamp > 0 {
        println!();
        println!(
            "Note: {} completed task(s) have no timestamp and are not included in velocity calculation.",
            summary.tasks_without_timestamp
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use workgraph::graph::{Estimate, Node, Task};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
        }
    }

    fn make_done_task(id: &str, title: &str, days_ago: i64, hours: Option<f64>) -> Task {
        let completed_at = Utc::now() - Duration::days(days_ago);
        let mut task = make_task(id, title);
        task.status = Status::Done;
        task.completed_at = Some(completed_at.to_rfc3339());
        if let Some(h) = hours {
            task.estimate = Some(Estimate {
                hours: Some(h),
                cost: None,
            });
        }
        task
    }

    #[test]
    fn test_velocity_empty_graph() {
        let graph = WorkGraph::new();
        let summary = calculate_velocity(&graph, 4);

        assert_eq!(summary.weeks.len(), 4);
        assert_eq!(summary.average_tasks_per_week, 0.0);
        assert_eq!(summary.average_hours_per_week, 0.0);
        assert_eq!(summary.open_tasks, 0);
        assert!(summary.weeks_to_clear.is_none());
    }

    #[test]
    fn test_velocity_single_completion() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("t1", "Task 1", 2, Some(8.0))));

        let summary = calculate_velocity(&graph, 4);

        // Task completed 2 days ago should be in week 4 (most recent)
        assert_eq!(summary.weeks[3].tasks_completed, 1);
        assert_eq!(summary.weeks[3].hours_completed, 8.0);
        assert_eq!(summary.average_tasks_per_week, 0.25); // 1 task / 4 weeks
        assert_eq!(summary.average_hours_per_week, 2.0); // 8 hours / 4 weeks
    }

    #[test]
    fn test_velocity_multiple_weeks() {
        let mut graph = WorkGraph::new();

        // Week 4 (most recent, 0-6 days ago)
        graph.add_node(Node::Task(make_done_task("t1", "Task 1", 1, Some(4.0))));
        graph.add_node(Node::Task(make_done_task("t2", "Task 2", 3, Some(4.0))));

        // Week 3 (7-13 days ago)
        graph.add_node(Node::Task(make_done_task("t3", "Task 3", 8, Some(8.0))));

        // Week 2 (14-20 days ago)
        graph.add_node(Node::Task(make_done_task("t4", "Task 4", 15, Some(4.0))));
        graph.add_node(Node::Task(make_done_task("t5", "Task 5", 16, Some(4.0))));
        graph.add_node(Node::Task(make_done_task("t6", "Task 6", 17, Some(4.0))));

        // Week 1 (21-27 days ago)
        graph.add_node(Node::Task(make_done_task("t7", "Task 7", 22, Some(8.0))));

        let summary = calculate_velocity(&graph, 4);

        assert_eq!(summary.weeks[0].tasks_completed, 1); // Week 1
        assert_eq!(summary.weeks[1].tasks_completed, 3); // Week 2
        assert_eq!(summary.weeks[2].tasks_completed, 1); // Week 3
        assert_eq!(summary.weeks[3].tasks_completed, 2); // Week 4

        assert_eq!(summary.average_tasks_per_week, 7.0 / 4.0); // 1.75
    }

    #[test]
    fn test_velocity_with_open_tasks() {
        let mut graph = WorkGraph::new();

        // Some completed tasks
        graph.add_node(Node::Task(make_done_task("t1", "Task 1", 1, Some(4.0))));
        graph.add_node(Node::Task(make_done_task("t2", "Task 2", 8, Some(4.0))));

        // Some open tasks
        let mut open1 = make_task("t3", "Open 1");
        open1.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });
        graph.add_node(Node::Task(open1));

        let mut open2 = make_task("t4", "Open 2");
        open2.estimate = Some(Estimate {
            hours: Some(16.0),
            cost: None,
        });
        graph.add_node(Node::Task(open2));

        let summary = calculate_velocity(&graph, 4);

        assert_eq!(summary.open_tasks, 2);
        assert_eq!(summary.open_hours, 24.0);
        assert!(summary.weeks_to_clear.is_some());
    }

    #[test]
    fn test_velocity_missing_timestamps() {
        let mut graph = WorkGraph::new();

        // Task with timestamp
        graph.add_node(Node::Task(make_done_task("t1", "Task 1", 1, None)));

        // Task without timestamp
        let mut no_ts = make_task("t2", "Task 2");
        no_ts.status = Status::Done;
        graph.add_node(Node::Task(no_ts));

        let summary = calculate_velocity(&graph, 4);

        assert_eq!(summary.tasks_without_timestamp, 1);
        assert_eq!(summary.weeks[3].tasks_completed, 1);
    }

    #[test]
    fn test_trend_increasing() {
        let weeks = vec![
            WeeklyVelocity {
                week_number: 1,
                tasks_completed: 2,
                hours_completed: 8.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 2,
                tasks_completed: 2,
                hours_completed: 8.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 3,
                tasks_completed: 5,
                hours_completed: 20.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 4,
                tasks_completed: 6,
                hours_completed: 24.0,
                task_ids: vec![],
            },
        ];

        let trend = calculate_trend(&weeks);
        assert_eq!(trend, "increasing");
    }

    #[test]
    fn test_trend_decreasing() {
        let weeks = vec![
            WeeklyVelocity {
                week_number: 1,
                tasks_completed: 8,
                hours_completed: 32.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 2,
                tasks_completed: 7,
                hours_completed: 28.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 3,
                tasks_completed: 3,
                hours_completed: 12.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 4,
                tasks_completed: 2,
                hours_completed: 8.0,
                task_ids: vec![],
            },
        ];

        let trend = calculate_trend(&weeks);
        assert_eq!(trend, "decreasing");
    }

    #[test]
    fn test_trend_stable() {
        let weeks = vec![
            WeeklyVelocity {
                week_number: 1,
                tasks_completed: 4,
                hours_completed: 16.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 2,
                tasks_completed: 5,
                hours_completed: 20.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 3,
                tasks_completed: 4,
                hours_completed: 16.0,
                task_ids: vec![],
            },
            WeeklyVelocity {
                week_number: 4,
                tasks_completed: 5,
                hours_completed: 20.0,
                task_ids: vec![],
            },
        ];

        let trend = calculate_trend(&weeks);
        assert_eq!(trend, "stable");
    }

    #[test]
    fn test_make_bar() {
        // Full bar
        let bar = make_bar(10, 10, false);
        assert!(bar.contains("|||||||||||||||||||||||||")); // 25 bars

        // Half bar
        let bar = make_bar(5, 10, false);
        assert!(bar.starts_with("[||||||||||||"));

        // Below average marker
        let bar = make_bar(3, 10, true);
        assert!(bar.contains("[below average]"));

        // Empty bar
        let bar = make_bar(0, 10, false);
        assert!(bar.starts_with("[                         ]"));
    }

    #[test]
    fn test_velocity_json_serialization() {
        let summary = VelocitySummary {
            weeks: vec![WeeklyVelocity {
                week_number: 1,
                tasks_completed: 5,
                hours_completed: 20.0,
                task_ids: vec!["t1".to_string()],
            }],
            average_tasks_per_week: 5.0,
            average_hours_per_week: 20.0,
            trend: "stable".to_string(),
            open_tasks: 3,
            open_hours: 12.0,
            weeks_to_clear: Some(0.6),
            tasks_without_timestamp: 0,
        };

        let json = serde_json::to_string(&summary).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["average_tasks_per_week"], 5.0);
        assert_eq!(parsed["trend"], "stable");
        assert_eq!(parsed["weeks"][0]["tasks_completed"], 5);
    }

    #[test]
    fn test_velocity_old_completion_excluded() {
        let mut graph = WorkGraph::new();

        // Task completed 30 days ago (outside 4-week window)
        graph.add_node(Node::Task(make_done_task("t1", "Old Task", 30, Some(8.0))));

        // Task completed 3 days ago (inside window)
        graph.add_node(Node::Task(make_done_task("t2", "Recent Task", 3, Some(4.0))));

        let summary = calculate_velocity(&graph, 4);

        // Only recent task should be counted
        let total_tasks: usize = summary.weeks.iter().map(|w| w.tasks_completed).sum();
        assert_eq!(total_tasks, 1);
    }

    #[test]
    fn test_in_progress_counted_as_open() {
        let mut graph = WorkGraph::new();

        // In-progress task should count as open for velocity projections
        let mut in_prog = make_task("t1", "In Progress");
        in_prog.status = Status::InProgress;
        in_prog.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });
        graph.add_node(Node::Task(in_prog));

        let summary = calculate_velocity(&graph, 4);

        assert_eq!(summary.open_tasks, 1);
        assert_eq!(summary.open_hours, 8.0);
    }
}

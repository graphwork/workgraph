use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::path::Path;
use workgraph::graph::{Status, Task, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;

/// Age bucket categories
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgeBucket {
    LessThanOneDay,
    OneToSevenDays,
    OneToFourWeeks,
    OneToThreeMonths,
    MoreThanThreeMonths,
}

impl AgeBucket {
    fn label(&self) -> &'static str {
        match self {
            AgeBucket::LessThanOneDay => "< 1 day",
            AgeBucket::OneToSevenDays => "1-7 days",
            AgeBucket::OneToFourWeeks => "1-4 weeks",
            AgeBucket::OneToThreeMonths => "1-3 months",
            AgeBucket::MoreThanThreeMonths => "> 3 months",
        }
    }

    fn warning_level(&self) -> Option<&'static str> {
        match self {
            AgeBucket::OneToThreeMonths => Some("WARNING"),
            AgeBucket::MoreThanThreeMonths => Some("CRITICAL"),
            _ => None,
        }
    }

    fn from_days(days: i64) -> Self {
        if days < 1 {
            AgeBucket::LessThanOneDay
        } else if days < 7 {
            AgeBucket::OneToSevenDays
        } else if days < 28 {
            AgeBucket::OneToFourWeeks
        } else if days < 90 {
            AgeBucket::OneToThreeMonths
        } else {
            AgeBucket::MoreThanThreeMonths
        }
    }

    fn all() -> [AgeBucket; 5] {
        [
            AgeBucket::LessThanOneDay,
            AgeBucket::OneToSevenDays,
            AgeBucket::OneToFourWeeks,
            AgeBucket::OneToThreeMonths,
            AgeBucket::MoreThanThreeMonths,
        ]
    }
}

/// Information about a task's age
struct TaskAgeInfo<'a> {
    task: &'a Task,
    age_days: i64,
    bucket: AgeBucket,
}

/// Information about a stale in-progress task
struct StaleInProgressInfo<'a> {
    task: &'a Task,
    started_days_ago: i64,
}

/// JSON output structure for age distribution
#[derive(Debug, Serialize)]
struct AgeBucketJson {
    bucket: String,
    count: usize,
    warning_level: Option<String>,
}

/// JSON output structure for an old task
#[derive(Debug, Serialize)]
struct OldTaskJson {
    id: String,
    title: String,
    age_days: i64,
    assigned: Option<String>,
    blocked_by: Vec<String>,
}

/// JSON output structure for a stale task
#[derive(Debug, Serialize)]
struct StaleTaskJson {
    id: String,
    title: String,
    started_days_ago: i64,
    assigned: Option<String>,
}

/// JSON output structure
#[derive(Debug, Serialize)]
struct AgingOutput {
    distribution: Vec<AgeBucketJson>,
    oldest_tasks: Vec<OldTaskJson>,
    stale_in_progress: Vec<StaleTaskJson>,
    tasks_with_unknown_age: usize,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let now = Utc::now();

    // Collect open/in-progress tasks with their ages
    let (tasks_with_age, unknown_age_count) = collect_task_ages(&graph, &now);

    // Count tasks by bucket
    let distribution = count_by_bucket(&tasks_with_age);

    // Find oldest open tasks (top 5)
    let oldest_tasks = find_oldest_tasks(&tasks_with_age, 5);

    // Find stale in-progress tasks (started > 14 days ago)
    let stale_in_progress = find_stale_in_progress(&graph, &now, 14);

    if json {
        output_json(&distribution, &oldest_tasks, &stale_in_progress, unknown_age_count);
    } else {
        output_text(&distribution, &oldest_tasks, &stale_in_progress, unknown_age_count);
    }

    Ok(())
}

/// Collect all open/in-progress tasks with their ages
fn collect_task_ages<'a>(graph: &'a WorkGraph, now: &DateTime<Utc>) -> (Vec<TaskAgeInfo<'a>>, usize) {
    let mut tasks_with_age = Vec::new();
    let mut unknown_age_count = 0;

    for task in graph.tasks() {
        // Only consider open or in-progress tasks
        if task.status != Status::Open && task.status != Status::InProgress {
            continue;
        }

        if let Some(ref created_at_str) = task.created_at {
            if let Ok(created_at) = DateTime::parse_from_rfc3339(created_at_str) {
                let age = *now - created_at.with_timezone(&Utc);
                let age_days = age.num_days();
                let bucket = AgeBucket::from_days(age_days);

                tasks_with_age.push(TaskAgeInfo {
                    task,
                    age_days,
                    bucket,
                });
            } else {
                unknown_age_count += 1;
            }
        } else {
            unknown_age_count += 1;
        }
    }

    (tasks_with_age, unknown_age_count)
}

/// Count tasks by age bucket
fn count_by_bucket(tasks: &[TaskAgeInfo]) -> [(AgeBucket, usize); 5] {
    let mut counts = [0usize; 5];

    for task_info in tasks {
        let idx = match task_info.bucket {
            AgeBucket::LessThanOneDay => 0,
            AgeBucket::OneToSevenDays => 1,
            AgeBucket::OneToFourWeeks => 2,
            AgeBucket::OneToThreeMonths => 3,
            AgeBucket::MoreThanThreeMonths => 4,
        };
        counts[idx] += 1;
    }

    let buckets = AgeBucket::all();
    [
        (buckets[0], counts[0]),
        (buckets[1], counts[1]),
        (buckets[2], counts[2]),
        (buckets[3], counts[3]),
        (buckets[4], counts[4]),
    ]
}

/// Find the oldest N tasks
fn find_oldest_tasks<'a>(tasks: &'a [TaskAgeInfo<'a>], limit: usize) -> Vec<&'a TaskAgeInfo<'a>> {
    let mut sorted: Vec<_> = tasks.iter().collect();
    sorted.sort_by(|a, b| b.age_days.cmp(&a.age_days));
    sorted.truncate(limit);
    sorted
}

/// Find stale in-progress tasks (started more than threshold_days ago)
fn find_stale_in_progress<'a>(
    graph: &'a WorkGraph,
    now: &DateTime<Utc>,
    threshold_days: i64,
) -> Vec<StaleInProgressInfo<'a>> {
    let mut stale = Vec::new();
    let threshold = Duration::days(threshold_days);

    for task in graph.tasks() {
        if task.status != Status::InProgress {
            continue;
        }

        if let Some(ref started_at_str) = task.started_at {
            if let Ok(started_at) = DateTime::parse_from_rfc3339(started_at_str) {
                let age = *now - started_at.with_timezone(&Utc);
                if age > threshold {
                    stale.push(StaleInProgressInfo {
                        task,
                        started_days_ago: age.num_days(),
                    });
                }
            }
        }
    }

    // Sort by started_days_ago descending
    stale.sort_by(|a, b| b.started_days_ago.cmp(&a.started_days_ago));
    stale
}

/// Generate an ASCII bar for the distribution
fn make_bar(count: usize, max_count: usize, width: usize) -> String {
    if max_count == 0 {
        return " ".repeat(width);
    }

    let filled = if count > 0 {
        ((count as f64 / max_count as f64) * width as f64).ceil() as usize
    } else {
        0
    };

    let filled = filled.min(width);
    let empty = width - filled;

    format!("[{}{}]", "|".repeat(filled), " ".repeat(empty))
}

fn output_text(
    distribution: &[(AgeBucket, usize); 5],
    oldest_tasks: &[&TaskAgeInfo],
    stale_in_progress: &[StaleInProgressInfo],
    unknown_age_count: usize,
) {
    println!("Task Age Distribution:\n");

    // Find max count for bar scaling
    let max_count = distribution.iter().map(|(_, c)| *c).max().unwrap_or(0);

    // Print distribution
    for (bucket, count) in distribution {
        let bar = make_bar(*count, max_count, 25);
        let warning = bucket.warning_level().map(|w| format!(" [{}]", w)).unwrap_or_default();
        println!(
            "  {:>12}: {:>3} tasks  {}{}",
            bucket.label(),
            count,
            bar,
            warning
        );
    }

    if unknown_age_count > 0 {
        println!("\n  ({} tasks with unknown age - missing created_at)", unknown_age_count);
    }

    // Print oldest open tasks
    if !oldest_tasks.is_empty() {
        println!("\nOldest open tasks:");
        for (i, task_info) in oldest_tasks.iter().enumerate() {
            let assigned = task_info
                .task
                .assigned
                .as_ref()
                .map(|a| format!("@{}", a))
                .unwrap_or_else(|| "unassigned".to_string());

            let blocked_by = if task_info.task.blocked_by.is_empty() {
                "nothing".to_string()
            } else {
                task_info.task.blocked_by.join(", ")
            };

            println!(
                "  {}. {} ({} days) - {} - blocked by: {}",
                i + 1,
                task_info.task.id,
                task_info.age_days,
                assigned,
                blocked_by
            );
        }
    }

    // Print stale in-progress tasks
    if !stale_in_progress.is_empty() {
        println!("\nStale in-progress tasks (started > 14 days ago):");
        for stale_info in stale_in_progress {
            let assigned = stale_info
                .task
                .assigned
                .as_ref()
                .map(|a| format!("@{}", a))
                .unwrap_or_else(|| "unassigned".to_string());

            println!(
                "  - {} (started {} days ago) - {}",
                stale_info.task.id, stale_info.started_days_ago, assigned
            );
        }
    }
}

fn output_json(
    distribution: &[(AgeBucket, usize); 5],
    oldest_tasks: &[&TaskAgeInfo],
    stale_in_progress: &[StaleInProgressInfo],
    unknown_age_count: usize,
) {
    let distribution_json: Vec<AgeBucketJson> = distribution
        .iter()
        .map(|(bucket, count)| AgeBucketJson {
            bucket: bucket.label().to_string(),
            count: *count,
            warning_level: bucket.warning_level().map(|s| s.to_string()),
        })
        .collect();

    let oldest_json: Vec<OldTaskJson> = oldest_tasks
        .iter()
        .map(|task_info| OldTaskJson {
            id: task_info.task.id.clone(),
            title: task_info.task.title.clone(),
            age_days: task_info.age_days,
            assigned: task_info.task.assigned.clone(),
            blocked_by: task_info.task.blocked_by.clone(),
        })
        .collect();

    let stale_json: Vec<StaleTaskJson> = stale_in_progress
        .iter()
        .map(|stale_info| StaleTaskJson {
            id: stale_info.task.id.clone(),
            title: stale_info.task.title.clone(),
            started_days_ago: stale_info.started_days_ago,
            assigned: stale_info.task.assigned.clone(),
        })
        .collect();

    let output = AgingOutput {
        distribution: distribution_json,
        oldest_tasks: oldest_json,
        stale_in_progress: stale_json,
        tasks_with_unknown_age: unknown_age_count,
    };

    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;

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

    fn setup_test_graph() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let workgraph_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&workgraph_dir).unwrap();
        let graph_file = workgraph_dir.join("graph.jsonl");
        (tmp, graph_file)
    }

    #[test]
    fn test_age_bucket_from_days() {
        assert_eq!(AgeBucket::from_days(0), AgeBucket::LessThanOneDay);
        assert_eq!(AgeBucket::from_days(1), AgeBucket::OneToSevenDays);
        assert_eq!(AgeBucket::from_days(6), AgeBucket::OneToSevenDays);
        assert_eq!(AgeBucket::from_days(7), AgeBucket::OneToFourWeeks);
        assert_eq!(AgeBucket::from_days(27), AgeBucket::OneToFourWeeks);
        assert_eq!(AgeBucket::from_days(28), AgeBucket::OneToThreeMonths);
        assert_eq!(AgeBucket::from_days(89), AgeBucket::OneToThreeMonths);
        assert_eq!(AgeBucket::from_days(90), AgeBucket::MoreThanThreeMonths);
        assert_eq!(AgeBucket::from_days(365), AgeBucket::MoreThanThreeMonths);
    }

    #[test]
    fn test_age_bucket_labels() {
        assert_eq!(AgeBucket::LessThanOneDay.label(), "< 1 day");
        assert_eq!(AgeBucket::OneToSevenDays.label(), "1-7 days");
        assert_eq!(AgeBucket::OneToFourWeeks.label(), "1-4 weeks");
        assert_eq!(AgeBucket::OneToThreeMonths.label(), "1-3 months");
        assert_eq!(AgeBucket::MoreThanThreeMonths.label(), "> 3 months");
    }

    #[test]
    fn test_age_bucket_warning_levels() {
        assert_eq!(AgeBucket::LessThanOneDay.warning_level(), None);
        assert_eq!(AgeBucket::OneToSevenDays.warning_level(), None);
        assert_eq!(AgeBucket::OneToFourWeeks.warning_level(), None);
        assert_eq!(AgeBucket::OneToThreeMonths.warning_level(), Some("WARNING"));
        assert_eq!(AgeBucket::MoreThanThreeMonths.warning_level(), Some("CRITICAL"));
    }

    #[test]
    fn test_make_bar_empty() {
        let bar = make_bar(0, 10, 10);
        assert_eq!(bar, "[          ]");
    }

    #[test]
    fn test_make_bar_full() {
        let bar = make_bar(10, 10, 10);
        assert_eq!(bar, "[||||||||||]");
    }

    #[test]
    fn test_make_bar_half() {
        let bar = make_bar(5, 10, 10);
        assert_eq!(bar, "[|||||     ]");
    }

    #[test]
    fn test_make_bar_zero_max() {
        let bar = make_bar(0, 0, 10);
        assert_eq!(bar, "          "); // no brackets, just spaces
    }

    #[test]
    fn test_collect_task_ages_with_timestamp() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();
        let five_days_ago = now - Duration::days(5);

        let mut task = make_task("t1", "Task 1");
        task.created_at = Some(five_days_ago.to_rfc3339());
        graph.add_node(Node::Task(task));

        let (tasks, unknown) = collect_task_ages(&graph, &now);
        assert_eq!(tasks.len(), 1);
        assert_eq!(unknown, 0);
        assert_eq!(tasks[0].age_days, 5);
        assert_eq!(tasks[0].bucket, AgeBucket::OneToSevenDays);
    }

    #[test]
    fn test_collect_task_ages_without_timestamp() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let task = make_task("t1", "Task 1"); // no created_at
        graph.add_node(Node::Task(task));

        let (tasks, unknown) = collect_task_ages(&graph, &now);
        assert_eq!(tasks.len(), 0);
        assert_eq!(unknown, 1);
    }

    #[test]
    fn test_collect_task_ages_skips_done_tasks() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();
        let five_days_ago = now - Duration::days(5);

        let mut task = make_task("t1", "Task 1");
        task.status = Status::Done;
        task.created_at = Some(five_days_ago.to_rfc3339());
        graph.add_node(Node::Task(task));

        let (tasks, unknown) = collect_task_ages(&graph, &now);
        assert_eq!(tasks.len(), 0);
        assert_eq!(unknown, 0);
    }

    #[test]
    fn test_count_by_bucket() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // Create tasks in different buckets
        let mut t1 = make_task("t1", "Task 1");
        t1.created_at = Some((now - Duration::hours(12)).to_rfc3339()); // < 1 day

        let mut t2 = make_task("t2", "Task 2");
        t2.created_at = Some((now - Duration::days(3)).to_rfc3339()); // 1-7 days

        let mut t3 = make_task("t3", "Task 3");
        t3.created_at = Some((now - Duration::days(14)).to_rfc3339()); // 1-4 weeks

        let mut t4 = make_task("t4", "Task 4");
        t4.created_at = Some((now - Duration::days(60)).to_rfc3339()); // 1-3 months

        let mut t5 = make_task("t5", "Task 5");
        t5.created_at = Some((now - Duration::days(120)).to_rfc3339()); // > 3 months

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));
        graph.add_node(Node::Task(t5));

        let (tasks, _) = collect_task_ages(&graph, &now);
        let distribution = count_by_bucket(&tasks);

        assert_eq!(distribution[0].1, 1); // < 1 day
        assert_eq!(distribution[1].1, 1); // 1-7 days
        assert_eq!(distribution[2].1, 1); // 1-4 weeks
        assert_eq!(distribution[3].1, 1); // 1-3 months
        assert_eq!(distribution[4].1, 1); // > 3 months
    }

    #[test]
    fn test_find_oldest_tasks() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "Task 1");
        t1.created_at = Some((now - Duration::days(10)).to_rfc3339());

        let mut t2 = make_task("t2", "Task 2");
        t2.created_at = Some((now - Duration::days(30)).to_rfc3339());

        let mut t3 = make_task("t3", "Task 3");
        t3.created_at = Some((now - Duration::days(5)).to_rfc3339());

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let (tasks, _) = collect_task_ages(&graph, &now);
        let oldest = find_oldest_tasks(&tasks, 2);

        assert_eq!(oldest.len(), 2);
        assert_eq!(oldest[0].task.id, "t2"); // 30 days
        assert_eq!(oldest[1].task.id, "t1"); // 10 days
    }

    #[test]
    fn test_find_stale_in_progress() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // Task started 20 days ago (stale)
        let mut t1 = make_task("t1", "Task 1");
        t1.status = Status::InProgress;
        t1.started_at = Some((now - Duration::days(20)).to_rfc3339());

        // Task started 5 days ago (not stale)
        let mut t2 = make_task("t2", "Task 2");
        t2.status = Status::InProgress;
        t2.started_at = Some((now - Duration::days(5)).to_rfc3339());

        // Task is open, not in progress
        let mut t3 = make_task("t3", "Task 3");
        t3.started_at = Some((now - Duration::days(20)).to_rfc3339());

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let stale = find_stale_in_progress(&graph, &now, 14);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].task.id, "t1");
        assert_eq!(stale[0].started_days_ago, 20);
    }

    #[test]
    fn test_run_empty_graph() {
        let (_tmp, graph_file) = setup_test_graph();
        let graph = WorkGraph::new();
        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_with_tasks() {
        let (_tmp, graph_file) = setup_test_graph();
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "Task 1");
        t1.created_at = Some((now - Duration::days(10)).to_rfc3339());
        graph.add_node(Node::Task(t1));

        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_json_output() {
        let (_tmp, graph_file) = setup_test_graph();
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "Task 1");
        t1.created_at = Some((now - Duration::days(10)).to_rfc3339());
        graph.add_node(Node::Task(t1));

        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_no_workgraph_initialized() {
        let tmp = TempDir::new().unwrap();
        let workgraph_dir = tmp.path().join(".workgraph");
        // Don't create the directory

        let result = run(&workgraph_dir, false);
        assert!(result.is_err());
    }
}

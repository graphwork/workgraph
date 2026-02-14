use anyhow::{Context, Result};
use chrono::Utc;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use workgraph::graph::{Estimate, LoopEdge, Node, Status, Task, parse_delay};
use workgraph::parser::load_graph;

use super::graph_path;

/// Parse a guard expression string into a LoopGuard.
/// Formats: 'task:<id>=<status>' or 'always'
pub fn parse_guard_expr(expr: &str) -> Result<workgraph::graph::LoopGuard> {
    let expr = expr.trim();
    if expr.eq_ignore_ascii_case("always") {
        return Ok(workgraph::graph::LoopGuard::Always);
    }
    if let Some(rest) = expr.strip_prefix("task:") {
        if let Some((task_id, status_str)) = rest.split_once('=') {
            let status = match status_str.to_lowercase().as_str() {
                "open" => Status::Open,
                "in-progress" => Status::InProgress,
                "done" => Status::Done,
                "blocked" => Status::Blocked,
                "failed" => Status::Failed,
                "abandoned" => Status::Abandoned,
                "pending-review" => Status::PendingReview,
                _ => anyhow::bail!("Unknown status '{}' in guard expression", status_str),
            };
            return Ok(workgraph::graph::LoopGuard::TaskStatus {
                task: task_id.to_string(),
                status,
            });
        }
        anyhow::bail!(
            "Invalid guard format. Expected 'task:<id>=<status>', got '{}'",
            expr
        );
    }
    anyhow::bail!(
        "Invalid guard expression '{}'. Expected 'task:<id>=<status>' or 'always'",
        expr
    );
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    title: &str,
    id: Option<&str>,
    description: Option<&str>,
    blocked_by: &[String],
    assign: Option<&str>,
    hours: Option<f64>,
    cost: Option<f64>,
    tags: &[String],
    skills: &[String],
    inputs: &[String],
    deliverables: &[String],
    max_retries: Option<u32>,
    model: Option<&str>,
    verify: Option<&str>,
    loops_to: Option<&str>,
    loop_max: Option<u32>,
    loop_guard: Option<&str>,
    loop_delay: Option<&str>,
) -> Result<()> {
    let path = graph_path(dir);

    // Load existing graph to check for ID conflicts
    let graph = if path.exists() {
        load_graph(&path).context("Failed to load graph")?
    } else {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    };

    // Generate ID if not provided
    let task_id = match id {
        Some(id) => {
            if graph.get_node(id).is_some() {
                anyhow::bail!("Task with ID '{}' already exists", id);
            }
            id.to_string()
        }
        None => generate_id(title, &graph),
    };

    let estimate = if hours.is_some() || cost.is_some() {
        Some(Estimate { hours, cost })
    } else {
        None
    };

    // Build loop edges if --loops-to specified
    let loops_to_edges = if let Some(target) = loops_to {
        let max_iterations = loop_max
            .ok_or_else(|| anyhow::anyhow!("--loop-max is required when using --loops-to"))?;
        let guard = match loop_guard {
            Some(expr) => Some(parse_guard_expr(expr)?),
            None => None,
        };
        let delay = match loop_delay {
            Some(d) => {
                // Validate the delay parses correctly
                parse_delay(d).ok_or_else(|| {
                    anyhow::anyhow!("Invalid delay '{}'. Use format: 30s, 5m, 1h, 24h, 7d", d)
                })?;
                Some(d.to_string())
            }
            None => None,
        };
        vec![LoopEdge {
            target: target.to_string(),
            guard,
            max_iterations,
            delay,
        }]
    } else {
        if loop_max.is_some() || loop_guard.is_some() || loop_delay.is_some() {
            anyhow::bail!("--loop-max, --loop-guard, and --loop-delay require --loops-to");
        }
        vec![]
    };

    let task = Task {
        id: task_id.clone(),
        title: title.to_string(),
        description: description.map(String::from),
        status: Status::Open,
        assigned: assign.map(String::from),
        estimate,
        blocks: vec![],
        blocked_by: blocked_by.to_vec(),
        requires: vec![],
        tags: tags.to_vec(),
        skills: skills.to_vec(),
        inputs: inputs.to_vec(),
        deliverables: deliverables.to_vec(),
        artifacts: vec![],
        exec: None,
        not_before: None,
        created_at: Some(Utc::now().to_rfc3339()),
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries,
        failure_reason: None,
        model: model.map(String::from),
        verify: verify.map(String::from),
        agent: None,
        loops_to: loops_to_edges,
        loop_iteration: 0,
        ready_after: None,
    };

    // Append to file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open graph.jsonl")?;

    let json = serde_json::to_string(&Node::Task(task)).context("Failed to serialize task")?;
    writeln!(file, "{}", json).context("Failed to write task")?;
    super::notify_graph_changed(dir);

    println!("Added task: {} ({})", title, task_id);
    if let (Some(target), Some(max)) = (&loops_to, &loop_max) {
        println!("  Loop edge: → {} (max {} iterations)", target, max);
    }
    super::print_service_hint(dir);
    Ok(())
}

fn generate_id(title: &str, graph: &workgraph::WorkGraph) -> String {
    // Generate a slug from the title
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("-");

    let base_id = if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    };

    // Ensure uniqueness
    if graph.get_node(&base_id).is_none() {
        return base_id;
    }

    for i in 2..1000 {
        let candidate = format!("{}-{}", base_id, i);
        if graph.get_node(&candidate).is_none() {
            return candidate;
        }
    }

    // Fallback to timestamp
    format!(
        "task-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::WorkGraph;
    use workgraph::graph::{LoopGuard, Node, Status, Task};

    /// Helper: create a minimal task with the given ID for inserting into a WorkGraph.
    fn stub_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
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
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
        }
    }

    // ---- parse_guard_expr tests ----

    #[test]
    fn guard_always_lowercase() {
        let g = parse_guard_expr("always").unwrap();
        assert_eq!(g, LoopGuard::Always);
    }

    #[test]
    fn guard_always_mixed_case() {
        let g = parse_guard_expr("Always").unwrap();
        assert_eq!(g, LoopGuard::Always);
    }

    #[test]
    fn guard_always_uppercase() {
        let g = parse_guard_expr("ALWAYS").unwrap();
        assert_eq!(g, LoopGuard::Always);
    }

    #[test]
    fn guard_always_with_whitespace() {
        let g = parse_guard_expr("  always  ").unwrap();
        assert_eq!(g, LoopGuard::Always);
    }

    #[test]
    fn guard_task_status_done() {
        let g = parse_guard_expr("task:my-task=done").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "my-task".to_string(),
                status: Status::Done,
            }
        );
    }

    #[test]
    fn guard_task_status_open() {
        let g = parse_guard_expr("task:build-step=open").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "build-step".to_string(),
                status: Status::Open,
            }
        );
    }

    #[test]
    fn guard_task_status_failed() {
        let g = parse_guard_expr("task:deploy=failed").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "deploy".to_string(),
                status: Status::Failed,
            }
        );
    }

    #[test]
    fn guard_task_status_abandoned() {
        let g = parse_guard_expr("task:cleanup=abandoned").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "cleanup".to_string(),
                status: Status::Abandoned,
            }
        );
    }

    #[test]
    fn guard_task_status_in_progress() {
        let g = parse_guard_expr("task:long-running=in-progress").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "long-running".to_string(),
                status: Status::InProgress,
            }
        );
    }

    #[test]
    fn guard_task_status_blocked() {
        let g = parse_guard_expr("task:waiting=blocked").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "waiting".to_string(),
                status: Status::Blocked,
            }
        );
    }

    #[test]
    fn guard_task_status_pending_review() {
        let g = parse_guard_expr("task:pr-check=pending-review").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "pr-check".to_string(),
                status: Status::PendingReview,
            }
        );
    }

    #[test]
    fn guard_task_status_case_insensitive() {
        let g = parse_guard_expr("task:check=Done").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "check".to_string(),
                status: Status::Done,
            }
        );
    }

    #[test]
    fn guard_task_id_with_underscores() {
        let g = parse_guard_expr("task:my_task_id=done").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "my_task_id".to_string(),
                status: Status::Done,
            }
        );
    }

    #[test]
    fn guard_task_id_with_dashes() {
        let g = parse_guard_expr("task:my-task-id=open").unwrap();
        assert_eq!(
            g,
            LoopGuard::TaskStatus {
                task: "my-task-id".to_string(),
                status: Status::Open,
            }
        );
    }

    #[test]
    fn guard_unknown_status_errors() {
        let result = parse_guard_expr("task:foo=bogus");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Unknown status"), "got: {msg}");
    }

    #[test]
    fn guard_missing_equals_errors() {
        let result = parse_guard_expr("task:foo");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Invalid guard format"), "got: {msg}");
    }

    #[test]
    fn guard_missing_colon_errors() {
        let result = parse_guard_expr("taskfoo=done");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Invalid guard expression"), "got: {msg}");
    }

    #[test]
    fn guard_empty_string_errors() {
        let result = parse_guard_expr("");
        assert!(result.is_err());
    }

    #[test]
    fn guard_whitespace_only_errors() {
        let result = parse_guard_expr("   ");
        assert!(result.is_err());
    }

    // ---- generate_id tests ----

    #[test]
    fn id_slug_from_simple_title() {
        let graph = WorkGraph::new();
        let id = generate_id("Build the widget", &graph);
        assert_eq!(id, "build-the-widget");
    }

    #[test]
    fn id_slug_truncates_to_three_words() {
        let graph = WorkGraph::new();
        let id = generate_id("Build the amazing super widget", &graph);
        assert_eq!(id, "build-the-amazing");
    }

    #[test]
    fn id_slug_strips_special_chars() {
        let graph = WorkGraph::new();
        let id = generate_id("Fix (bug) #123!", &graph);
        assert_eq!(id, "fix-bug-123");
    }

    #[test]
    fn id_slug_collapses_multiple_separators() {
        let graph = WorkGraph::new();
        let id = generate_id("a---b   c", &graph);
        assert_eq!(id, "a-b-c");
    }

    #[test]
    fn id_slug_empty_title_gives_task() {
        let graph = WorkGraph::new();
        let id = generate_id("", &graph);
        assert_eq!(id, "task");
    }

    #[test]
    fn id_slug_whitespace_title_gives_task() {
        let graph = WorkGraph::new();
        let id = generate_id("   ", &graph);
        assert_eq!(id, "task");
    }

    #[test]
    fn id_uniqueness_appends_suffix() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(stub_task("build-it")));
        let id = generate_id("Build it", &graph);
        assert_eq!(id, "build-it-2");
    }

    #[test]
    fn id_uniqueness_increments_until_free() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(stub_task("build-it")));
        graph.add_node(Node::Task(stub_task("build-it-2")));
        graph.add_node(Node::Task(stub_task("build-it-3")));
        let id = generate_id("Build it", &graph);
        assert_eq!(id, "build-it-4");
    }

    #[test]
    fn id_explicit_no_collision() {
        // When an explicit id is provided, generate_id is not called;
        // but the run() function checks uniqueness. Verify generate_id
        // returns the base slug when no collision exists.
        let graph = WorkGraph::new();
        let id = generate_id("Deploy service", &graph);
        assert_eq!(id, "deploy-service");
    }
}

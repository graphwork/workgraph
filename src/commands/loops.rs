use anyhow::{Context, Result};
use std::path::Path;
use workgraph::check::check_cycles;
use workgraph::graph::WorkGraph;
use workgraph::parser::load_graph;

use super::graph_path;

/// Classification of a cycle
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleClassification {
    /// Intentional cycle (has recurring or cycle:intentional tag)
    Intentional,
    /// Warning: likely a bug (short cycle without recurring tag)
    Warning,
    /// Info: needs review (medium cycle)
    Info,
}

impl CycleClassification {
    pub fn as_str(&self) -> &'static str {
        match self {
            CycleClassification::Intentional => "INTENTIONAL RECURRENCE",
            CycleClassification::Warning => "WARNING: Potential bug",
            CycleClassification::Info => "INFO: Complex dependency",
        }
    }
}

/// A classified cycle with metadata
#[derive(Debug, Clone)]
pub struct ClassifiedCycle {
    pub nodes: Vec<String>,
    pub classification: CycleClassification,
    pub reason: String,
}

/// Classify a cycle based on its length and tags
fn classify_cycle(cycle: &[String], graph: &WorkGraph) -> ClassifiedCycle {
    let len = cycle.len();

    // Check if any task in the cycle has recurring or cycle:intentional tag
    let has_intentional_tag = cycle.iter().any(|node_id| {
        if let Some(task) = graph.get_task(node_id) {
            task.tags
                .iter()
                .any(|tag| tag == "recurring" || tag == "cycle:intentional")
        } else {
            false
        }
    });

    let (classification, reason) = if has_intentional_tag {
        (
            CycleClassification::Intentional,
            "has 'recurring' or 'cycle:intentional' tag".to_string(),
        )
    } else if len <= 2 {
        (
            CycleClassification::Warning,
            "Short cycle without recurrence tag".to_string(),
        )
    } else if len >= 5 {
        (
            CycleClassification::Warning,
            format!("Long cycle ({} nodes) likely unintentional", len),
        )
    } else {
        (
            CycleClassification::Info,
            format!("Medium cycle ({} nodes) needs review", len),
        )
    };

    ClassifiedCycle {
        nodes: cycle.to_vec(),
        classification,
        reason,
    }
}

/// Format cycle as a path string (a -> b -> c -> a)
fn format_cycle_path(cycle: &[String]) -> String {
    let mut path = cycle.join(" -> ");
    if let Some(first) = cycle.first() {
        path.push_str(" -> ");
        path.push_str(first);
    }
    path
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let cycles = check_cycles(&graph);

    if cycles.is_empty() {
        if json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "cycles_detected": 0,
                "cycles": []
            }))?);
        } else {
            println!("No cycles detected.");
        }
        return Ok(());
    }

    // Classify all cycles
    let classified: Vec<ClassifiedCycle> = cycles
        .iter()
        .map(|cycle| classify_cycle(cycle, &graph))
        .collect();

    if json {
        let output: Vec<_> = classified
            .iter()
            .map(|c| {
                serde_json::json!({
                    "nodes": c.nodes,
                    "node_count": c.nodes.len(),
                    "classification": match c.classification {
                        CycleClassification::Intentional => "intentional",
                        CycleClassification::Warning => "warning",
                        CycleClassification::Info => "info",
                    },
                    "reason": c.reason,
                    "path": format_cycle_path(&c.nodes),
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "cycles_detected": classified.len(),
                "cycles": output
            }))?
        );
    } else {
        println!("Cycles detected: {}\n", classified.len());

        for (i, cycle) in classified.iter().enumerate() {
            println!(
                "{}. {} ({} nodes)",
                i + 1,
                cycle.classification.as_str(),
                cycle.nodes.len()
            );
            println!("   {}", format_cycle_path(&cycle.nodes));

            // Only show reason for non-intentional cycles or if intentional (to show which tag)
            match cycle.classification {
                CycleClassification::Intentional => {
                    println!("   ({})", cycle.reason);
                }
                _ => {
                    println!("   Reason: {}", cycle.reason);
                }
            }
            println!();
        }

        // Summary
        let warnings = classified
            .iter()
            .filter(|c| c.classification == CycleClassification::Warning)
            .count();
        let infos = classified
            .iter()
            .filter(|c| c.classification == CycleClassification::Info)
            .count();
        let intentional = classified
            .iter()
            .filter(|c| c.classification == CycleClassification::Intentional)
            .count();

        if warnings > 0 || infos > 0 {
            println!("Summary: {} warning(s), {} info(s), {} intentional", warnings, infos, intentional);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Status, Task};

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

    fn make_task_with_tags(id: &str, title: &str, tags: Vec<&str>) -> Task {
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
            tags: tags.into_iter().map(String::from).collect(),
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

    #[test]
    fn test_classify_short_cycle_without_tag_is_warning() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Task A")));
        graph.add_node(Node::Task(make_task("b", "Task B")));

        let cycle = vec!["a".to_string(), "b".to_string()];
        let classified = classify_cycle(&cycle, &graph);

        assert_eq!(classified.classification, CycleClassification::Warning);
        assert!(classified.reason.contains("Short cycle"));
    }

    #[test]
    fn test_classify_cycle_with_recurring_tag_is_intentional() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task_with_tags("a", "Task A", vec!["recurring"])));
        graph.add_node(Node::Task(make_task("b", "Task B")));

        let cycle = vec!["a".to_string(), "b".to_string()];
        let classified = classify_cycle(&cycle, &graph);

        assert_eq!(classified.classification, CycleClassification::Intentional);
    }

    #[test]
    fn test_classify_cycle_with_cycle_intentional_tag_is_intentional() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task_with_tags("a", "Task A", vec!["cycle:intentional"])));
        graph.add_node(Node::Task(make_task("b", "Task B")));

        let cycle = vec!["a".to_string(), "b".to_string()];
        let classified = classify_cycle(&cycle, &graph);

        assert_eq!(classified.classification, CycleClassification::Intentional);
    }

    #[test]
    fn test_classify_medium_cycle_without_tag_is_info() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Task A")));
        graph.add_node(Node::Task(make_task("b", "Task B")));
        graph.add_node(Node::Task(make_task("c", "Task C")));

        let cycle = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let classified = classify_cycle(&cycle, &graph);

        assert_eq!(classified.classification, CycleClassification::Info);
        assert!(classified.reason.contains("Medium cycle"));
    }

    #[test]
    fn test_classify_long_cycle_without_tag_is_warning() {
        let mut graph = WorkGraph::new();
        for id in ["a", "b", "c", "d", "e"] {
            graph.add_node(Node::Task(make_task(id, &format!("Task {}", id))));
        }

        let cycle = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "e".to_string(),
        ];
        let classified = classify_cycle(&cycle, &graph);

        assert_eq!(classified.classification, CycleClassification::Warning);
        assert!(classified.reason.contains("Long cycle"));
    }

    #[test]
    fn test_format_cycle_path() {
        let cycle = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let path = format_cycle_path(&cycle);
        assert_eq!(path, "a -> b -> c -> a");
    }

    #[test]
    fn test_format_cycle_path_single_node() {
        let cycle = vec!["a".to_string()];
        let path = format_cycle_path(&cycle);
        assert_eq!(path, "a -> a");
    }
}

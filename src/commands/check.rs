use anyhow::{Context, Result};
use std::path::Path;
use workgraph::check::{LoopEdgeIssueKind, check_all};
use workgraph::parser::load_graph;

use super::graph_path;

pub fn run(dir: &Path) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let result = check_all(&graph);

    let mut warnings = 0;
    let mut errors = 0;

    // Cycles are warnings (allowed for recurring tasks)
    if !result.cycles.is_empty() {
        println!("Warning: Cycles detected (this is OK for recurring tasks):");
        for cycle in &result.cycles {
            println!("  {}", cycle.join(" -> "));
            warnings += 1;
        }
    }

    // Orphan references are errors
    if !result.orphan_refs.is_empty() {
        println!("Error: Orphan references:");
        for orphan in &result.orphan_refs {
            println!(
                "  {} --[{}]--> {} (not found)",
                orphan.from, orphan.relation, orphan.to
            );
            errors += 1;
        }
    }

    // Loop edge issues are errors
    if !result.loop_edge_issues.is_empty() {
        println!("Error: Loop edge issues:");
        for issue in &result.loop_edge_issues {
            let desc = match &issue.kind {
                LoopEdgeIssueKind::TargetNotFound => {
                    format!(
                        "{} --[loops_to]--> {} (target not found)",
                        issue.from, issue.target
                    )
                }
                LoopEdgeIssueKind::ZeroMaxIterations => {
                    format!(
                        "{} --[loops_to]--> {} (max_iterations is 0, loop will never fire)",
                        issue.from, issue.target
                    )
                }
                LoopEdgeIssueKind::GuardTaskNotFound(guard_task) => {
                    format!(
                        "{} --[loops_to]--> {} (guard references non-existent task '{}')",
                        issue.from, issue.target, guard_task
                    )
                }
                LoopEdgeIssueKind::SelfLoop => {
                    format!(
                        "{} --[loops_to]--> {} (self-loop: task would immediately re-open on completion)",
                        issue.from, issue.target
                    )
                }
            };
            println!("  {}", desc);
            errors += 1;
        }
    }

    // Count loop edges for info
    let loop_edge_count: usize = graph.tasks().map(|t| t.loops_to.len()).sum();
    if loop_edge_count > 0 && result.loop_edge_issues.is_empty() {
        println!("Loop edges: {} edge(s), all valid", loop_edge_count);
    }

    if errors > 0 {
        anyhow::bail!("Found {} error(s) and {} warning(s)", errors, warnings);
    } else if warnings > 0 {
        println!("Graph OK: {} nodes, {} warning(s)", graph.len(), warnings);
    } else {
        println!("Graph OK: {} nodes, no issues found", graph.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{LoopEdge, Node, Status, Task};
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
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
        }
    }

    fn setup_graph(dir: &Path, graph: &workgraph::graph::WorkGraph) {
        std::fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        save_graph(graph, &path).unwrap();
    }

    #[test]
    fn test_check_ok_clean_graph() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        graph.add_node(Node::Task(make_task("t2", "Task 2")));
        setup_graph(&dir, &graph);

        let result = run(&dir);
        assert!(result.is_ok(), "clean graph should pass check");
    }

    #[test]
    fn test_check_fails_on_orphan_refs() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = workgraph::graph::WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["nonexistent".to_string()];
        graph.add_node(Node::Task(t1));
        setup_graph(&dir, &graph);

        let result = run(&dir);
        assert!(result.is_err(), "orphan refs should fail check");
    }

    #[test]
    fn test_check_warns_on_cycles_but_no_error_alone() {
        // Cycles are treated as warnings, not errors, in the command layer.
        // However, cycles in blocked_by also create orphan-like issues only
        // if the target doesn't exist. With valid nodes that have cycles,
        // the check should still succeed (cycles are just warnings).
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = workgraph::graph::WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["t2".to_string()];
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        setup_graph(&dir, &graph);

        // Cycles are warnings, not errors — run should succeed
        // (the command only bails on errors > 0, not warnings)
        let result = run(&dir);
        assert!(
            result.is_ok(),
            "cycles alone should not cause check failure (they are warnings)"
        );
    }

    #[test]
    fn test_check_fails_on_loop_edge_issues() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = workgraph::graph::WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.loops_to = vec![LoopEdge {
            target: "nonexistent".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];
        graph.add_node(Node::Task(t1));
        setup_graph(&dir, &graph);

        let result = run(&dir);
        assert!(result.is_err(), "loop edge issues should fail check");
    }

    #[test]
    fn test_check_fails_when_not_initialized() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        // Don't create anything — dir doesn't even exist

        let result = run(&dir);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not initialized"));
    }

    #[test]
    fn test_check_ok_with_valid_loop_edges() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = workgraph::graph::WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: None,
            max_iterations: 5,
            delay: None,
        }];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        setup_graph(&dir, &graph);

        let result = run(&dir);
        assert!(result.is_ok(), "valid loop edges should pass check");
    }
}

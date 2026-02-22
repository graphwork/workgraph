use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use workgraph::check::check_all;

#[derive(Serialize)]
struct CycleInfo {
    header: String,
    members: Vec<String>,
    reducible: bool,
}

#[derive(Serialize)]
struct CheckJsonOutput {
    ok: bool,
    cycles: Vec<Vec<String>>,
    orphan_refs: Vec<workgraph::check::OrphanRef>,
    stale_assignments: Vec<workgraph::check::StaleAssignment>,
    stuck_blocked: Vec<workgraph::check::StuckBlocked>,
    node_count: usize,
    structural_cycles: Vec<CycleInfo>,
    warnings: usize,
    errors: usize,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;
    let result = check_all(&graph);
    let cycle_analysis = graph.compute_cycle_analysis();
    let irreducible_count = cycle_analysis.cycles.iter().filter(|c| !c.reducible).count();

    let warnings =
        result.cycles.len() + result.stale_assignments.len() + result.stuck_blocked.len() + irreducible_count;
    let errors = result.orphan_refs.len();

    let structural_cycles: Vec<CycleInfo> = cycle_analysis.cycles.iter().map(|c| CycleInfo {
        header: c.header.clone(),
        members: c.members.clone(),
        reducible: c.reducible,
    }).collect();

    if json {
        let output = CheckJsonOutput {
            ok: result.ok,
            cycles: result.cycles,
            orphan_refs: result.orphan_refs,
            stale_assignments: result.stale_assignments,
            stuck_blocked: result.stuck_blocked,
            node_count: graph.len(),
            structural_cycles,
            warnings,
            errors,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // Cycles are warnings (allowed for recurring tasks)
    if !result.cycles.is_empty() {
        eprintln!("Warning: Cycles detected (this is OK for recurring tasks):");
        for cycle in &result.cycles {
            eprintln!("  {}", cycle.join(" -> "));
        }
    }

    // Stale assignments are warnings
    if !result.stale_assignments.is_empty() {
        eprintln!(
            "Warning: Stale assignments (task is open but has an agent assigned — agent may have died):"
        );
        for stale in &result.stale_assignments {
            eprintln!("  {} (assigned to '{}')", stale.task_id, stale.assigned);
        }
    }

    // Stuck blocked tasks are warnings
    if !result.stuck_blocked.is_empty() {
        eprintln!(
            "Warning: Stuck blocked tasks (all dependencies are terminal but task is still blocked):"
        );
        for stuck in &result.stuck_blocked {
            eprintln!(
                "  {} (blocked by: {})",
                stuck.task_id,
                stuck.after_ids.join(", ")
            );
        }
    }

    // Orphan references are errors
    if !result.orphan_refs.is_empty() {
        eprintln!("Error: Orphan references:");
        for orphan in &result.orphan_refs {
            eprintln!(
                "  {} --[{}]--> {} (not found)",
                orphan.from, orphan.relation, orphan.to
            );
        }
    }

    // Structural cycle analysis (Tarjan's SCC on after edges)
    if !cycle_analysis.cycles.is_empty() {
        eprintln!(
            "Structural cycles: {} detected (via Tarjan's SCC on after edges)",
            cycle_analysis.cycles.len()
        );
        for cycle in &cycle_analysis.cycles {
            let reducibility = if cycle.reducible { "reducible" } else { "IRREDUCIBLE" };
            eprintln!(
                "  {} ({} members, {})",
                cycle.header, cycle.members.len(), reducibility
            );
        }
        if irreducible_count > 0 {
            eprintln!(
                "Warning: {} irreducible cycle(s) detected — these have multiple entry points",
                irreducible_count
            );
        }
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
    use super::super::graph_path;
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
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

        let result = run(&dir, false);
        assert!(result.is_ok(), "clean graph should pass check");
    }

    #[test]
    fn test_check_fails_on_orphan_refs() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = workgraph::graph::WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.after = vec!["nonexistent".to_string()];
        graph.add_node(Node::Task(t1));
        setup_graph(&dir, &graph);

        let result = run(&dir, false);
        assert!(result.is_err(), "orphan refs should fail check");
    }

    #[test]
    fn test_check_warns_on_cycles_but_no_error_alone() {
        // Cycles are treated as warnings, not errors, in the command layer.
        // However, cycles in after also create orphan-like issues only
        // if the target doesn't exist. With valid nodes that have cycles,
        // the check should still succeed (cycles are just warnings).
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = workgraph::graph::WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.after = vec!["t2".to_string()];
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        setup_graph(&dir, &graph);

        // Cycles are warnings, not errors — run should succeed
        // (the command only bails on errors > 0, not warnings)
        let result = run(&dir, false);
        assert!(
            result.is_ok(),
            "cycles alone should not cause check failure (they are warnings)"
        );
    }

    #[test]
    fn test_check_fails_when_not_initialized() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        // Don't create anything — dir doesn't even exist

        let result = run(&dir, false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not initialized"));
    }

    #[test]
    fn test_check_warns_on_stale_assignments_but_no_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = workgraph::graph::WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.assigned = Some("agent-dead".to_string());
        graph.add_node(Node::Task(t1));
        setup_graph(&dir, &graph);

        // Stale assignments are warnings, not errors — run should succeed
        let result = run(&dir, false);
        assert!(
            result.is_ok(),
            "stale assignments alone should not cause check failure (they are warnings)"
        );
    }
}

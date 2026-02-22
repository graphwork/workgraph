use anyhow::Result;
use std::path::Path;
use workgraph::graph::CycleAnalysis;

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;
    let analysis = graph.compute_cycle_analysis();

    if json {
        print_json(&analysis, &graph)?;
    } else {
        print_human(&analysis, &graph);
    }

    Ok(())
}

fn print_json(analysis: &CycleAnalysis, graph: &workgraph::graph::WorkGraph) -> Result<()> {
    let cycles_output: Vec<_> = analysis
        .cycles
        .iter()
        .map(|c| {
            let statuses: Vec<_> = c
                .members
                .iter()
                .map(|id| {
                    let status = graph
                        .get_task(id)
                        .map(|t| t.status.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    serde_json::json!({ "id": id, "status": status })
                })
                .collect();

            serde_json::json!({
                "header": c.header,
                "members": c.members,
                "member_count": c.members.len(),
                "reducible": c.reducible,
                "member_statuses": statuses,
            })
        })
        .collect();

    let back_edges: Vec<_> = analysis
        .back_edges
        .iter()
        .map(|(src, tgt)| serde_json::json!({ "from": src, "to": tgt }))
        .collect();

    let output = serde_json::json!({
        "cycle_count": analysis.cycles.len(),
        "cycles": cycles_output,
        "back_edges": back_edges,
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn print_human(analysis: &CycleAnalysis, graph: &workgraph::graph::WorkGraph) {
    if analysis.cycles.is_empty() {
        println!("No cycles detected in after edges.");
        return;
    }

    println!("Cycles detected: {}\n", analysis.cycles.len());

    for (i, cycle) in analysis.cycles.iter().enumerate() {
        let reducibility = if cycle.reducible {
            "REDUCIBLE"
        } else {
            "IRREDUCIBLE"
        };

        // Build cycle path display
        let mut path = Vec::new();
        path.push(cycle.header.clone());
        for member in &cycle.members {
            if member != &cycle.header {
                path.push(member.clone());
            }
        }
        path.push(cycle.header.clone());

        println!(
            "  {}. {} [{}]",
            i + 1,
            path.join(" -> "),
            reducibility
        );
        println!("     Header: {}", cycle.header);
        println!("     Members: {}", cycle.members.join(", "));

        // Show back-edges for this cycle
        let cycle_back_edges: Vec<_> = analysis
            .back_edges
            .iter()
            .filter(|(_, tgt)| tgt == &cycle.header)
            .collect();
        if !cycle_back_edges.is_empty() {
            let be_strs: Vec<_> = cycle_back_edges
                .iter()
                .map(|(src, tgt)| format!("{} -> {}", src, tgt))
                .collect();
            println!("     Back-edges: {}", be_strs.join(", "));
        }

        // Show member details
        for member in &cycle.members {
            if let Some(task) = graph.get_task(member) {
                let hdr = if member == &cycle.header {
                    " (header)"
                } else {
                    ""
                };
                println!(
                    "       {} [{}]{} - {}",
                    member, task.status, hdr, task.title
                );
            }
        }

        if !cycle.reducible {
            println!("     WARNING: Irreducible cycle has multiple entry points.");
        }

        println!();
    }

    let irreducible_count = analysis.cycles.iter().filter(|c| !c.reducible).count();
    if irreducible_count > 0 {
        println!(
            "  Irreducible cycles: {}",
            irreducible_count
        );
    } else {
        println!("  Irreducible cycles: 0");
    }
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
            ..Task::default()
        }
    }

    fn setup_graph(dir: &std::path::Path, graph: &WorkGraph) {
        std::fs::create_dir_all(dir).unwrap();
        let path = super::super::graph_path(dir);
        save_graph(graph, &path).unwrap();
    }

    #[test]
    fn test_cycles_no_cycles() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        setup_graph(&dir, &graph);
        let result = run(&dir, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_cycles_detects_simple_cycle() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.after = vec!["t2".to_string()];
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        setup_graph(&dir, &graph);
        let result = run(&dir, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_cycle_analysis_from_graph() {
        let mut graph = WorkGraph::new();
        let mut write = make_task("write", "Write");
        write.after = vec!["review".to_string()];
        let mut review = make_task("review", "Review");
        review.after = vec!["write".to_string()];
        graph.add_node(Node::Task(write));
        graph.add_node(Node::Task(review));
        let analysis = graph.compute_cycle_analysis();
        assert_eq!(analysis.cycles.len(), 1);
        assert_eq!(analysis.cycles[0].members.len(), 2);
    }

    #[test]
    fn test_cycle_analysis_no_cycles() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        let analysis = graph.compute_cycle_analysis();
        assert!(analysis.cycles.is_empty());
    }

    #[test]
    fn test_cycle_analysis_task_to_cycle_mapping() {
        let mut graph = WorkGraph::new();
        let mut a = make_task("a", "A");
        a.after = vec!["b".to_string()];
        let mut b = make_task("b", "B");
        b.after = vec!["a".to_string()];
        let c = make_task("c", "C");
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        let analysis = graph.compute_cycle_analysis();
        assert!(analysis.task_to_cycle.contains_key("a"));
        assert!(analysis.task_to_cycle.contains_key("b"));
        assert!(!analysis.task_to_cycle.contains_key("c"));
        assert_eq!(analysis.task_to_cycle["a"], analysis.task_to_cycle["b"]);
    }
}

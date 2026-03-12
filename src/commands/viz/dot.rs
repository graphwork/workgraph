use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use workgraph::format_hours;
use workgraph::graph::{Status, Task, WorkGraph};

pub(crate) fn generate_dot(
    graph: &WorkGraph,
    tasks: &[&Task],
    task_ids: &HashSet<&str>,
    critical_path: &HashSet<String>,
    annotations: &HashMap<String, String>,
) -> String {
    let mut lines = vec![
        "digraph workgraph {".to_string(),
        "  rankdir=LR;".to_string(),
        "  node [shape=box];".to_string(),
        String::new(),
    ];

    // Print task nodes
    for task in tasks {
        let style = match task.status {
            Status::Done => "style=filled, fillcolor=lightgreen",
            Status::InProgress => "style=filled, fillcolor=lightyellow",
            Status::Blocked => "style=filled, fillcolor=lightcoral",
            Status::Open => "style=filled, fillcolor=white",
            Status::Failed => "style=filled, fillcolor=salmon",
            Status::Abandoned => "style=filled, fillcolor=lightgray",
            Status::Waiting | Status::PendingValidation => "style=filled, fillcolor=lightyellow",
        };

        // Build label with hours estimate if available
        let hours_str = task
            .estimate
            .as_ref()
            .and_then(|e| e.hours)
            .map(|h| format!("\\n{}h", format_hours(h)))
            .unwrap_or_default();

        // Add phase annotation if present
        let phase_str = annotations
            .get(&task.id)
            .map(|a| format!(" {}", a))
            .unwrap_or_default();

        let label = format!("{}\\n{}{}{}", task.id, task.title, hours_str, phase_str);

        // Check if on critical path
        let node_style = if critical_path.contains(&task.id) {
            format!("{}, penwidth=3, color=red", style)
        } else {
            style.to_string()
        };

        lines.push(format!(
            "  \"{}\" [label=\"{}\", {}];",
            task.id, label, node_style
        ));
    }

    // Print assigned actors as ellipse nodes
    let assigned_actors: HashSet<&str> =
        tasks.iter().filter_map(|t| t.assigned.as_deref()).collect();

    for actor_id in &assigned_actors {
        lines.push(format!(
            "  \"{}\" [label=\"{}\", shape=ellipse, style=filled, fillcolor=lightblue];",
            actor_id, actor_id
        ));
    }

    // Print resources that are required by shown tasks
    let required_resources: HashSet<&str> = tasks
        .iter()
        .flat_map(|t| t.requires.iter().map(String::as_str))
        .collect();

    for resource in graph.resources() {
        if required_resources.contains(resource.id.as_str()) {
            let name = resource.name.as_deref().unwrap_or(&resource.id);
            lines.push(format!(
                "  \"{}\" [label=\"{}\", shape=diamond, style=filled, fillcolor=lightyellow];",
                resource.id, name
            ));
        }
    }

    // Collect truly dangling dependency targets (don't exist in the graph at all)
    let mut dangling_targets: HashSet<String> = HashSet::new();
    for task in tasks {
        for after in &task.after {
            if !task_ids.contains(after.as_str()) && graph.get_node(after).is_none() {
                dangling_targets.insert(after.clone());
            }
        }
    }

    // Add phantom nodes for dangling dependencies
    for target in &dangling_targets {
        lines.push(format!(
            "  \"{}\" [label=\"⚠ {} (missing)\", shape=none, fontcolor=red];",
            target, target
        ));
    }

    lines.push(String::new());

    // Print edges
    for task in tasks {
        for after in &task.after {
            if task_ids.contains(after.as_str()) {
                // Normal edge — check if on critical path
                let edge_style =
                    if critical_path.contains(&task.id) && critical_path.contains(after) {
                        "color=red, penwidth=2"
                    } else {
                        ""
                    };

                if edge_style.is_empty() {
                    lines.push(format!("  \"{}\" -> \"{}\";", after, task.id));
                } else {
                    lines.push(format!(
                        "  \"{}\" -> \"{}\" [{}];",
                        after, task.id, edge_style
                    ));
                }
            } else if dangling_targets.contains(after) {
                // Dangling edge — dashed red
                lines.push(format!(
                    "  \"{}\" -> \"{}\" [style=dashed, color=red];",
                    after, task.id
                ));
            }
        }

        if let Some(ref assigned) = task.assigned {
            lines.push(format!(
                "  \"{}\" -> \"{}\" [style=dashed];",
                task.id, assigned
            ));
        }

        for req in &task.requires {
            if required_resources.contains(req.as_str()) {
                lines.push(format!(
                    "  \"{}\" -> \"{}\" [style=dotted, label=\"requires\"];",
                    task.id, req
                ));
            }
        }
    }

    lines.push("}".to_string());

    lines.join("\n")
}

pub(crate) fn generate_mermaid(
    _graph: &WorkGraph,
    tasks: &[&Task],
    task_ids: &HashSet<&str>,
    critical_path: &HashSet<String>,
    annotations: &HashMap<String, String>,
) -> String {
    let mut lines = Vec::new();

    lines.push("flowchart LR".to_string());

    // Print task nodes
    for task in tasks {
        let hours_str = task
            .estimate
            .as_ref()
            .and_then(|e| e.hours)
            .map(|h| format!(" ({}h)", format_hours(h)))
            .unwrap_or_default();

        // Sanitize title for mermaid (escape quotes)
        let title = task.title.replace('"', "'");

        // Add phase annotation if present
        let phase_str = annotations
            .get(&task.id)
            .map(|a| format!(" {}", a))
            .unwrap_or_default();

        let label = format!("{}: {}{}{}", task.id, title, hours_str, phase_str);

        // Mermaid node shape based on status
        let node = match task.status {
            Status::Done => format!("  {}[/\"{}\"/]", task.id, label),
            Status::InProgress => format!("  {}((\"{}\"))", task.id, label),
            Status::Blocked => format!("  {}{{\"{}\"}}!", task.id, label),
            Status::Open => format!("  {}[\"{}\"]", task.id, label),
            Status::Failed => format!("  {}{{{{\"{}\"}}}}!", task.id, label),
            Status::Abandoned => format!("  {}[\"{}\"]:::abandoned", task.id, label),
            Status::Waiting | Status::PendingValidation => {
                format!("  {}[\"{}\"]:::waiting", task.id, label)
            }
        };
        lines.push(node);
    }

    // Collect truly dangling dependency targets (don't exist in the graph at all)
    let mut dangling_targets: HashSet<String> = HashSet::new();
    for task in tasks {
        for after in &task.after {
            if !task_ids.contains(after.as_str()) && _graph.get_node(after).is_none() {
                dangling_targets.insert(after.clone());
            }
        }
    }

    // Add phantom nodes for dangling dependencies
    for target in &dangling_targets {
        lines.push(format!(
            "  {}[\"⚠ {} (missing)\"]:::dangling",
            target, target
        ));
    }

    lines.push(String::new());

    // Print edges
    for task in tasks {
        for after in &task.after {
            if task_ids.contains(after.as_str()) {
                // Check if this edge is on critical path
                let arrow = if critical_path.contains(&task.id) && critical_path.contains(after) {
                    "==>" // thick arrow for critical path
                } else {
                    "-->"
                };

                lines.push(format!("  {} {} {}", after, arrow, task.id));
            } else if dangling_targets.contains(after) {
                // Dangling edge — dotted red
                lines.push(format!("  {} -.-> {}", after, task.id));
            }
        }
    }

    // Print actor assignments
    let assigned_actors: HashSet<&str> =
        tasks.iter().filter_map(|t| t.assigned.as_deref()).collect();

    if !assigned_actors.is_empty() {
        lines.push(String::new());
        for actor_id in &assigned_actors {
            lines.push(format!("  {}(({}))", actor_id, actor_id));
        }

        for task in tasks {
            if let Some(ref assigned) = task.assigned {
                lines.push(format!("  {} -.-> {}", task.id, assigned));
            }
        }
    }

    // Add styling for critical path nodes
    if !critical_path.is_empty() {
        lines.push(String::new());
        lines.push("  %% Critical path styling".to_string());
        let critical_nodes: Vec<&str> = critical_path.iter().map(String::as_str).collect();
        lines.push(format!(
            "  style {} stroke:#f00,stroke-width:3px",
            critical_nodes.join(",")
        ));
    }

    // Add styling for dangling nodes
    if !dangling_targets.is_empty() {
        lines.push(String::new());
        lines.push(
            "  classDef dangling fill:#fff,stroke:#f00,stroke-dasharray: 5 5,color:#f00"
                .to_string(),
        );
    }

    lines.join("\n")
}

pub(crate) fn render_dot(dot_content: &str, output_path: &str) -> Result<()> {
    // Determine output format from file extension
    let format = if output_path.ends_with(".png") {
        "png"
    } else if output_path.ends_with(".svg") {
        "svg"
    } else if output_path.ends_with(".pdf") {
        "pdf"
    } else {
        "png" // default
    };

    let mut child = Command::new("dot")
        .arg(format!("-T{}", format))
        .arg("-o")
        .arg(output_path)
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to run 'dot' command. Is Graphviz installed?")?;

    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin
            .write_all(dot_content.as_bytes())
            .context("Failed to write to dot stdin")?;
    }

    let status = child.wait().context("Failed to wait for dot process")?;

    if !status.success() {
        anyhow::bail!("dot command failed with status: {}", status);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Estimate, Node, Task};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn make_task_with_hours(id: &str, title: &str, hours: f64) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            estimate: Some(Estimate {
                hours: Some(hours),
                cost: None,
            }),
            ..Task::default()
        }
    }

    fn make_internal_task(id: &str, title: &str, tag: &str, after: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            tags: vec![tag.to_string(), "agency".to_string()],
            after: after.into_iter().map(String::from).collect(),
            ..Task::default()
        }
    }

    #[test]
    fn test_generate_dot_basic() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();

        let no_annots = HashMap::new();
        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(dot.contains("digraph workgraph"));
        assert!(dot.contains("\"t1\""));
        assert!(dot.contains("Task 1"));
    }

    #[test]
    fn test_generate_dot_with_hours() {
        let mut graph = WorkGraph::new();
        let t1 = make_task_with_hours("t1", "Task 1", 8.0);
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(dot.contains("8h"));
    }

    #[test]
    fn test_generate_dot_with_critical_path() {
        let mut graph = WorkGraph::new();
        let t1 = make_task_with_hours("t1", "Task 1", 8.0);
        let mut t2 = make_task_with_hours("t2", "Task 2", 16.0);
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let mut critical_path = HashSet::new();
        critical_path.insert("t1".to_string());
        critical_path.insert("t2".to_string());
        let no_annots = HashMap::new();

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(dot.contains("color=red"));
        assert!(dot.contains("penwidth"));
    }

    #[test]
    fn test_generate_mermaid_basic() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let mermaid = generate_mermaid(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(mermaid.contains("flowchart LR"));
        assert!(mermaid.contains("t1"));
    }

    #[test]
    fn test_generate_mermaid_with_dependency() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let mermaid = generate_mermaid(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(mermaid.contains("t1 --> t2"));
    }

    #[test]
    fn test_dot_hides_internal_tasks_by_default() {
        let mut graph = WorkGraph::new();
        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        let mut assign = make_internal_task(
            "assign-my-task",
            "Assign agent to my-task",
            "assignment",
            vec![],
        );
        assign.status = Status::InProgress;
        parent.after = vec!["assign-my-task".to_string()];
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations = HashMap::new();
        let (filtered, annots) = crate::commands::viz::filter_internal_tasks(
            &graph,
            graph.tasks().collect(),
            &annotations,
        );
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();

        let result = generate_dot(&graph, &filtered, &task_ids, &critical_path, &annots);

        assert!(!result.contains("assign-my-task"));
        assert!(result.contains("my-task"));
        assert!(result.contains("[assigning]"));
    }

    #[test]
    fn test_mermaid_hides_internal_tasks_by_default() {
        let mut graph = WorkGraph::new();
        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        let mut assign = make_internal_task(
            "assign-my-task",
            "Assign agent to my-task",
            "assignment",
            vec![],
        );
        assign.status = Status::InProgress;
        parent.after = vec!["assign-my-task".to_string()];
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations = HashMap::new();
        let (filtered, annots) = crate::commands::viz::filter_internal_tasks(
            &graph,
            graph.tasks().collect(),
            &annotations,
        );
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();

        let result = generate_mermaid(&graph, &filtered, &task_ids, &critical_path, &annots);

        assert!(!result.contains("assign-my-task"));
        assert!(result.contains("my-task"));
        assert!(result.contains("[assigning]"));
    }

    #[test]
    fn test_dot_dangling_dependency_rendering() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.after = vec!["nonexistent-dep".to_string()];
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path, &no_annots);

        // Should have phantom node for the missing dep
        assert!(dot.contains("nonexistent-dep"));
        assert!(dot.contains("(missing)"));
        assert!(dot.contains("fontcolor=red"));
        // Should have dashed red edge
        assert!(dot.contains("style=dashed, color=red"));
    }

    #[test]
    fn test_mermaid_dangling_dependency_rendering() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.after = vec!["nonexistent-dep".to_string()];
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let mermaid = generate_mermaid(&graph, &tasks, &task_ids, &critical_path, &no_annots);

        // Should have phantom node
        assert!(mermaid.contains("nonexistent-dep"));
        assert!(mermaid.contains("(missing)"));
        assert!(mermaid.contains(":::dangling"));
        // Should have dotted edge
        assert!(mermaid.contains("-.->"));
        // Should have dangling class definition
        assert!(mermaid.contains("classDef dangling"));
    }

    #[test]
    fn test_dot_no_dangling_when_dep_exists() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path, &no_annots);

        // Should NOT have phantom node or dangling styling
        assert!(!dot.contains("(missing)"));
        assert!(!dot.contains("style=dashed, color=red"));
    }
}

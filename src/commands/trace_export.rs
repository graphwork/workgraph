use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

use workgraph::agency::{Evaluation, load_all_evaluations_or_warn};
use workgraph::function::{
    self, FunctionVisibility, TraceFunction, export_function, function_visible_at,
};
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::load_graph;
use workgraph::provenance;

/// Exported trace data with metadata, tasks, evaluations, operations, and functions.
#[derive(Debug, Serialize, Deserialize)]
pub struct TraceExport {
    pub metadata: ExportMetadata,
    pub tasks: Vec<ExportedTask>,
    pub evaluations: Vec<Evaluation>,
    pub operations: Vec<provenance::OperationEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<TraceFunction>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportMetadata {
    pub version: String,
    pub exported_at: String,
    pub visibility: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportedTask {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: Status,
    pub visibility: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocked_by")]
    pub after: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocks")]
    pub before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<LogEntry>,
}

pub fn run(
    dir: &Path,
    root: Option<&str>,
    visibility: &str,
    output: Option<&str>,
    json: bool,
) -> Result<()> {
    // Validate visibility
    match visibility {
        "internal" | "public" | "peer" => {}
        other => bail!(
            "Invalid visibility '{}'. Valid values: internal, public, peer",
            other
        ),
    }

    let path = super::graph_path(dir);
    if !path.exists() {
        bail!("Workgraph not initialized. Run `wg init` first.");
    }

    let graph = load_graph(&path)?;

    // Scope selection: if root is provided, collect root + all descendants
    let task_ids: HashSet<String> = if let Some(root_id) = root {
        let mut ids = HashSet::new();
        ids.insert(root_id.to_string());
        collect_descendants(&graph, root_id, &mut ids);
        ids
    } else {
        graph.tasks().map(|t| t.id.clone()).collect()
    };

    // Visibility filtering
    let included_tasks: Vec<&workgraph::graph::Task> = graph
        .tasks()
        .filter(|t| task_ids.contains(&t.id))
        .filter(|t| match visibility {
            "internal" => true,
            "public" => t.visibility == "public",
            "peer" => t.visibility == "public" || t.visibility == "peer",
            _ => false,
        })
        .collect();

    let included_ids: HashSet<&str> = included_tasks.iter().map(|t| t.id.as_str()).collect();

    // Convert tasks to exported format
    let exported_tasks: Vec<ExportedTask> = included_tasks
        .iter()
        .map(|t| {
            let (agent, log) = match visibility {
                "public" => (None, Vec::new()),
                "peer" => (t.agent.clone(), Vec::new()), // peer gets agent but not logs
                _ => (t.agent.clone(), t.log.clone()),   // internal gets everything
            };
            ExportedTask {
                id: t.id.clone(),
                title: t.title.clone(),
                description: t.description.clone(),
                status: t.status,
                visibility: t.visibility.clone(),
                skills: t.skills.clone(),
                after: t.after.clone(),
                before: t.before.clone(),
                tags: t.tags.clone(),
                artifacts: t.artifacts.clone(),
                created_at: t.created_at.clone(),
                completed_at: t.completed_at.clone(),
                agent,
                log,
            }
        })
        .collect();

    // Load evaluations
    let evaluations: Vec<Evaluation> = match visibility {
        "public" => Vec::new(), // public exports exclude evaluations
        _ => {
            let evals_dir = dir.join("agency").join("evaluations");
            let mut evals = load_all_evaluations_or_warn(&evals_dir);
            evals.retain(|e| included_ids.contains(e.task_id.as_str()));
            if visibility == "peer" {
                // Strip notes from peer exports
                for e in &mut evals {
                    e.notes = String::new();
                }
            }
            evals
        }
    };

    // Load operations
    let operations: Vec<provenance::OperationEntry> = {
        let all_ops = provenance::read_all_operations(dir).unwrap_or_default();
        let structural_ops: HashSet<&str> = ["add_task", "done", "fail", "abandon", "retry"]
            .iter()
            .copied()
            .collect();

        all_ops
            .into_iter()
            .filter(|op| {
                op.task_id
                    .as_ref()
                    .map(|tid| included_ids.contains(tid.as_str()))
                    .unwrap_or(false)
            })
            .filter(|op| match visibility {
                "public" => structural_ops.contains(op.op.as_str()),
                _ => true,
            })
            .map(|mut op| {
                if visibility == "public" {
                    // Strip detail from public exports
                    op.detail = serde_json::Value::Null;
                }
                op
            })
            .collect()
    };

    // Load and filter functions by visibility
    let target_vis =
        FunctionVisibility::from_str_opt(visibility).unwrap_or(FunctionVisibility::Internal);
    let functions: Vec<TraceFunction> = {
        let funcs_dir = function::functions_dir(dir);
        let all_funcs = function::load_all_functions(&funcs_dir).unwrap_or_default();
        all_funcs
            .into_iter()
            .filter(|f| function_visible_at(f, &target_vis))
            .filter_map(|f| export_function(&f, &target_vis).ok())
            .collect()
    };

    // Build export
    let export = TraceExport {
        metadata: ExportMetadata {
            version: env!("CARGO_PKG_VERSION").to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
            visibility: visibility.to_string(),
            root_task: root.map(String::from),
            source: None,
        },
        tasks: exported_tasks,
        evaluations,
        operations,
        functions,
    };

    let json_output = serde_json::to_string_pretty(&export)?;

    // Write output
    if let Some(output_path) = output {
        std::fs::write(output_path, &json_output)?;
        if !json {
            let func_msg = if export.functions.is_empty() {
                String::new()
            } else {
                format!(", {} functions", export.functions.len())
            };
            eprintln!(
                "Exported {} tasks{} to {} (visibility: {})",
                export.tasks.len(),
                func_msg,
                output_path,
                visibility
            );
        }
    } else {
        println!("{}", json_output);
    }

    // Record provenance
    let _ = provenance::record(
        dir,
        "trace_export",
        root,
        Some("user"),
        serde_json::json!({
            "visibility": visibility,
            "task_count": export.tasks.len(),
            "evaluation_count": export.evaluations.len(),
            "operation_count": export.operations.len(),
            "function_count": export.functions.len(),
        }),
        provenance::DEFAULT_ROTATION_THRESHOLD,
    );

    let _ = json; // json flag is handled via output format above

    Ok(())
}

/// Collect all descendants of a task (tasks that are after it, transitively).
fn collect_descendants(
    graph: &workgraph::graph::WorkGraph,
    root_id: &str,
    collected: &mut HashSet<String>,
) {
    for task in graph.tasks() {
        if task.after.iter().any(|dep| dep == root_id) && collected.insert(task.id.clone()) {
            collect_descendants(graph, &task.id, collected);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{LogEntry, Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    // ── collect_descendants ──

    #[test]
    fn test_collect_descendants_single_root() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("root", "Root")));

        let mut ids = HashSet::new();
        ids.insert("root".to_string());
        collect_descendants(&graph, "root", &mut ids);

        assert_eq!(ids.len(), 1);
        assert!(ids.contains("root"));
    }

    #[test]
    fn test_collect_descendants_chain() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("root", "Root")));
        let mut child = make_task("child", "Child");
        child.after = vec!["root".to_string()];
        graph.add_node(Node::Task(child));
        let mut grandchild = make_task("grandchild", "Grandchild");
        grandchild.after = vec!["child".to_string()];
        graph.add_node(Node::Task(grandchild));

        let mut ids = HashSet::new();
        ids.insert("root".to_string());
        collect_descendants(&graph, "root", &mut ids);

        assert_eq!(ids.len(), 3);
        assert!(ids.contains("child"));
        assert!(ids.contains("grandchild"));
    }

    #[test]
    fn test_collect_descendants_does_not_include_unrelated() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("root", "Root")));
        graph.add_node(Node::Task(make_task("unrelated", "Unrelated")));

        let mut ids = HashSet::new();
        ids.insert("root".to_string());
        collect_descendants(&graph, "root", &mut ids);

        assert_eq!(ids.len(), 1);
        assert!(!ids.contains("unrelated"));
    }

    // ── TraceExport / ExportedTask serialization ──

    #[test]
    fn test_exported_task_serialization_roundtrip() {
        let task = ExportedTask {
            id: "t1".to_string(),
            title: "Test task".to_string(),
            description: Some("A description".to_string()),
            status: Status::Done,
            visibility: "public".to_string(),
            skills: vec!["rust".to_string()],
            after: vec!["t0".to_string()],
            before: vec![],
            tags: vec!["tag1".to_string()],
            artifacts: vec!["output.txt".to_string()],
            created_at: Some("2026-02-28T12:00:00Z".to_string()),
            completed_at: Some("2026-02-28T13:00:00Z".to_string()),
            agent: Some("agent-1".to_string()),
            log: vec![LogEntry {
                timestamp: "2026-02-28T12:30:00Z".to_string(),
                actor: None,
                message: "Progress update".to_string(),
                ..Default::default()
            }],
        };

        let json = serde_json::to_string(&task).unwrap();
        let parsed: ExportedTask = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "t1");
        assert_eq!(parsed.title, "Test task");
        assert_eq!(parsed.status, Status::Done);
        assert_eq!(parsed.agent, Some("agent-1".to_string()));
        assert_eq!(parsed.log.len(), 1);
    }

    #[test]
    fn test_exported_task_skips_empty_vecs() {
        let task = ExportedTask {
            id: "t1".to_string(),
            title: "Test".to_string(),
            description: None,
            status: Status::Open,
            visibility: "public".to_string(),
            skills: vec![],
            after: vec![],
            before: vec![],
            tags: vec![],
            artifacts: vec![],
            created_at: None,
            completed_at: None,
            agent: None,
            log: vec![],
        };

        let json = serde_json::to_string(&task).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Empty vecs should be skipped in serialization
        assert!(!val.as_object().unwrap().contains_key("skills"));
        assert!(!val.as_object().unwrap().contains_key("after"));
        assert!(!val.as_object().unwrap().contains_key("tags"));
        assert!(!val.as_object().unwrap().contains_key("log"));
    }

    #[test]
    fn test_trace_export_serialization() {
        let export = TraceExport {
            metadata: ExportMetadata {
                version: "0.1.0".to_string(),
                exported_at: "2026-02-28T12:00:00Z".to_string(),
                visibility: "public".to_string(),
                root_task: Some("root".to_string()),
                source: None,
            },
            tasks: vec![],
            evaluations: vec![],
            operations: vec![],
            functions: vec![],
        };

        let json = serde_json::to_string_pretty(&export).unwrap();
        let parsed: TraceExport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.metadata.version, "0.1.0");
        assert_eq!(parsed.metadata.visibility, "public");
        assert_eq!(parsed.metadata.root_task, Some("root".to_string()));
        assert!(parsed.tasks.is_empty());
    }
}

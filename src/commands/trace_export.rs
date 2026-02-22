use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

use workgraph::agency::{Evaluation, load_all_evaluations_or_warn};
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::load_graph;
use workgraph::provenance;
use workgraph::trace_function::{
    self, FunctionVisibility, TraceFunction, export_function, function_visible_at,
};

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
        let structural_ops: HashSet<&str> =
            ["add_task", "done", "fail", "abandon", "retry"].iter().copied().collect();

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
    let target_vis = FunctionVisibility::from_str_opt(visibility)
        .unwrap_or(FunctionVisibility::Internal);
    let functions: Vec<TraceFunction> = {
        let funcs_dir = trace_function::functions_dir(dir);
        let all_funcs = trace_function::load_all_functions(&funcs_dir).unwrap_or_default();
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

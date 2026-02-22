use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;

use workgraph::provenance;
use workgraph::trace_function::{
    self, MemoryInclusions, RunSummary, TaskOutcome, TraceMemoryConfig,
};
use workgraph::trace_memory;

/// Run the `wg func make-adaptive <function-id>` command.
///
/// Upgrades a generative function (version >= 2) to an adaptive function
/// (version 3) by adding TraceMemoryConfig and scanning provenance for
/// past application runs.
pub fn run(dir: &Path, function_id: &str, max_runs: u32) -> Result<()> {
    let func_dir = trace_function::functions_dir(dir);
    let mut func = trace_function::find_function_by_prefix(&func_dir, function_id)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Require version >= 2 (generative)
    if func.version < 2 {
        bail!(
            "Function '{}' is version {} (static). Only generative functions (version >= 2) \
             can be made adaptive. First upgrade to generative with: \
             wg func extract --generative",
            func.id,
            func.version
        );
    }

    if func.version >= 3 && func.memory.is_some() {
        println!(
            "Function '{}' is already adaptive (version {}). Updating trace memory...",
            func.id, func.version
        );
    }

    // Scan provenance for past applications of this function
    let all_ops = provenance::read_all_operations(dir).unwrap_or_default();
    let applications: Vec<_> = all_ops
        .iter()
        .filter(|op| {
            (op.op == "apply" || op.op == "instantiate")
                && op
                    .detail
                    .get("function_id")
                    .and_then(|v| v.as_str())
                    .map(|id| id == func.id)
                    .unwrap_or(false)
        })
        .collect();

    // Build RunSummary for each past application
    let mut summaries = Vec::new();
    let graph_result = super::load_workgraph(dir);

    if let Ok((graph, _path)) = graph_result {
        for inst_op in &applications {
            let created_ids: Vec<String> = inst_op
                .detail
                .get("created_task_ids")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            let prefix = inst_op
                .detail
                .get("prefix")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let inputs: HashMap<String, serde_yaml::Value> = inst_op
                .detail
                .get("inputs")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                serde_yaml::Value::String(
                                    v.as_str().unwrap_or_default().to_string(),
                                ),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();

            // Build task outcomes from current graph state
            let mut task_outcomes = Vec::new();
            let mut all_succeeded = true;

            for task_id in &created_ids {
                if let Some(task) = graph.get_task(task_id) {
                    let status_str = format!("{}", task.status);
                    if status_str != "Done" {
                        all_succeeded = false;
                    }

                    // Compute duration from timestamps
                    let duration_secs = match (&task.started_at, &task.completed_at) {
                        (Some(start), Some(end)) => {
                            if let (Ok(s), Ok(e)) = (
                                chrono::DateTime::parse_from_rfc3339(start),
                                chrono::DateTime::parse_from_rfc3339(end),
                            ) {
                                Some((e - s).num_seconds())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };

                    // Try to find the template_id from the task_id
                    let template_id = task_id
                        .strip_prefix(&format!("{}-", prefix))
                        .unwrap_or(task_id)
                        .to_string();

                    task_outcomes.push(TaskOutcome {
                        template_id,
                        task_id: task_id.clone(),
                        status: status_str,
                        score: None,
                        duration_secs,
                        retry_count: task.retry_count,
                    });
                }
            }

            let summary = RunSummary {
                applied_at: inst_op.timestamp.clone(),
                inputs,
                prefix,
                task_outcomes,
                interventions: vec![],
                wall_clock_secs: None,
                all_succeeded,
                avg_score: None,
            };

            summaries.push(summary);
        }
    }

    // Save summaries to runs.jsonl
    for summary in &summaries {
        trace_memory::append_run_summary(dir, &func.id, summary)
            .context("Failed to save run summary")?;
    }

    // Add TraceMemoryConfig
    func.memory = Some(TraceMemoryConfig {
        max_runs,
        include: MemoryInclusions {
            outcomes: true,
            scores: true,
            interventions: true,
            duration: true,
            retries: false,
            artifacts: false,
        },
        storage_path: None,
    });

    // Append {{memory.run_summaries}} to planner template description if not already present
    if let Some(ref mut planning) = func.planning {
        let marker = "{{memory.run_summaries}}";
        if !planning.planner_template.description.contains(marker) {
            planning.planner_template.description.push_str(&format!(
                "\n\nPast run history:\n{}",
                marker
            ));
        }
    }

    // Bump version to 3
    func.version = 3;

    // Save updated function
    trace_function::save_function(&func, &func_dir)?;

    // Print summary
    println!(
        "Upgraded function '{}' to adaptive (version 3)",
        func.id
    );
    println!("  Memory config: max_runs={}", max_runs);
    if !summaries.is_empty() {
        println!(
            "  Recorded {} past run summaries",
            summaries.len()
        );
    } else {
        println!("  No past applications found in provenance");
    }
    let runs_path = trace_memory::runs_path(dir, &func.id);
    println!("  Runs file: {}", runs_path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::WorkGraph;
    use workgraph::parser::save_graph;
    use workgraph::trace_function::*;

    fn setup_workgraph(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let graph = WorkGraph::new();
        save_graph(&graph, dir.join("graph.jsonl")).unwrap();
    }

    fn sample_generative_function() -> TraceFunction {
        TraceFunction {
            kind: "trace-function".to_string(),
            version: 2,
            id: "gen-func".to_string(),
            name: "Generative Function".to_string(),
            description: "A generative function".to_string(),
            extracted_from: vec![],
            extracted_by: None,
            extracted_at: None,
            tags: vec![],
            inputs: vec![FunctionInput {
                name: "feature_name".to_string(),
                input_type: InputType::String,
                description: "Feature name".to_string(),
                required: true,
                default: None,
                example: None,
                min: None,
                max: None,
                values: None,
            }],
            tasks: vec![TaskTemplate {
                template_id: "implement".to_string(),
                title: "Implement".to_string(),
                description: "Do the work".to_string(),
                skills: vec![],
                after: vec![],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec![],
            }],
            outputs: vec![],
            planning: Some(PlanningConfig {
                planner_template: TaskTemplate {
                    template_id: "planner".to_string(),
                    title: "Plan".to_string(),
                    description: "Plan the work.".to_string(),
                    skills: vec!["analysis".to_string()],
                    after: vec![],
                    loops_to: vec![],
                    role_hint: Some("architect".to_string()),
                    deliverables: vec![],
                    verify: None,
                    tags: vec![],
                },
                output_format: "workgraph-yaml".to_string(),
                static_fallback: true,
                validate_plan: true,
            }),
            constraints: Some(StructuralConstraints {
                min_tasks: Some(1),
                max_tasks: Some(10),
                required_skills: vec![],
                max_depth: None,
                allow_cycles: false,
                max_total_iterations: None,
                required_phases: vec![],
                forbidden_patterns: vec![],
            }),
            memory: None,
            visibility: FunctionVisibility::Internal,
            redacted_fields: vec![],
        }
    }

    #[test]
    fn rejects_static_function() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        let mut func = sample_generative_function();
        func.version = 1;
        func.id = "static-func".to_string();
        let func_dir = trace_function::functions_dir(dir);
        trace_function::save_function(&func, &func_dir).unwrap();

        let result = run(dir, "static-func", 10);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version 1"));
    }

    #[test]
    fn upgrades_generative_to_adaptive() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        let func = sample_generative_function();
        let func_dir = trace_function::functions_dir(dir);
        trace_function::save_function(&func, &func_dir).unwrap();

        run(dir, "gen-func", 5).unwrap();

        let loaded = trace_function::find_function_by_prefix(&func_dir, "gen-func").unwrap();
        assert_eq!(loaded.version, 3);
        assert!(loaded.memory.is_some());
        let memory = loaded.memory.unwrap();
        assert_eq!(memory.max_runs, 5);
        assert!(memory.include.outcomes);
        assert!(memory.include.scores);

        // Check that planner template got the memory marker
        let planning = loaded.planning.unwrap();
        assert!(planning
            .planner_template
            .description
            .contains("{{memory.run_summaries}}"));
    }

    #[test]
    fn already_adaptive_updates() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        let mut func = sample_generative_function();
        func.version = 3;
        func.memory = Some(TraceMemoryConfig {
            max_runs: 5,
            include: MemoryInclusions {
                outcomes: true,
                scores: true,
                interventions: true,
                duration: true,
                retries: false,
                artifacts: false,
            },
            storage_path: None,
        });
        let func_dir = trace_function::functions_dir(dir);
        trace_function::save_function(&func, &func_dir).unwrap();

        // Should not error, just update
        run(dir, "gen-func", 20).unwrap();

        let loaded = trace_function::find_function_by_prefix(&func_dir, "gen-func").unwrap();
        assert_eq!(loaded.memory.unwrap().max_runs, 20);
    }
}

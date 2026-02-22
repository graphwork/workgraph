use anyhow::Result;
use chrono::Utc;
use std::path::Path;

use workgraph::trace_function::{
    self, ExtractionSource, FunctionInput, FunctionOutput, InputType, PlanningConfig,
    StructuralConstraints, TaskTemplate, TraceFunction, FunctionVisibility,
};

/// Run the `wg trace bootstrap` command.
///
/// Creates or re-extracts the `extract-function` meta-function — a built-in
/// generative function that describes the extraction process itself as a
/// workgraph workflow.
pub fn run(dir: &Path, force: bool) -> Result<()> {
    let func_dir = trace_function::functions_dir(dir);

    // Check if it already exists
    if !force
        && let Ok(_existing) = trace_function::find_function_by_prefix(&func_dir, "extract-function") {
            anyhow::bail!(
                "Meta-function 'extract-function' already exists. Use --force to overwrite."
            );
        }

    let now = Utc::now().to_rfc3339();

    let func = TraceFunction {
        kind: "trace-function".to_string(),
        version: 2,
        id: "extract-function".to_string(),
        name: "Extract Function".to_string(),
        description: "Meta-function: extract a trace function from a completed workflow. \
                       The planner analyzes the source task's trace and decides how to \
                       structure the extraction."
            .to_string(),
        extracted_from: vec![ExtractionSource {
            task_id: "bootstrap".to_string(),
            run_id: None,
            timestamp: now.clone(),
        }],
        extracted_by: Some("wg trace bootstrap".to_string()),
        extracted_at: Some(now),
        tags: vec!["meta".to_string(), "extraction".to_string()],
        inputs: vec![
            FunctionInput {
                name: "source_task_id".to_string(),
                input_type: InputType::String,
                description: "Task ID to extract the function from".to_string(),
                required: true,
                default: None,
                example: Some(serde_yaml::Value::String("impl-feature".to_string())),
                min: None,
                max: None,
                values: None,
            },
            FunctionInput {
                name: "function_name".to_string(),
                input_type: InputType::String,
                description: "Name for the extracted function".to_string(),
                required: true,
                default: None,
                example: Some(serde_yaml::Value::String("impl-feature-v2".to_string())),
                min: None,
                max: None,
                values: None,
            },
            FunctionInput {
                name: "subgraph".to_string(),
                input_type: InputType::String,
                description: "Whether to extract the full subgraph (true/false)".to_string(),
                required: false,
                default: Some(serde_yaml::Value::String("true".to_string())),
                example: None,
                min: None,
                max: None,
                values: None,
            },
        ],
        tasks: vec![
            TaskTemplate {
                template_id: "analyze".to_string(),
                title: "Analyze trace of {{input.source_task_id}}".to_string(),
                description: "Examine the execution trace of the source task. \
                              Identify the invariant structure, variable parts, \
                              and parameter points."
                    .to_string(),
                skills: vec!["analysis".to_string()],
                after: vec![],
                loops_to: vec![],
                role_hint: Some("analyst".to_string()),
                deliverables: vec![],
                verify: None,
                tags: vec!["phase:analyze".to_string()],
            },
            TaskTemplate {
                template_id: "draft".to_string(),
                title: "Draft function template for {{input.function_name}}".to_string(),
                description: "Create the trace function YAML from the analysis. \
                              Define inputs, task templates, outputs, and dependency structure."
                    .to_string(),
                skills: vec!["implementation".to_string()],
                after: vec!["analyze".to_string()],
                loops_to: vec![],
                role_hint: Some("implementer".to_string()),
                deliverables: vec![],
                verify: None,
                tags: vec!["phase:draft".to_string()],
            },
            TaskTemplate {
                template_id: "validate".to_string(),
                title: "Validate {{input.function_name}} against trace".to_string(),
                description: "Verify the extracted function matches the original trace. \
                              Check that all task templates are valid, dependencies are correct, \
                              and the function can be instantiated."
                    .to_string(),
                skills: vec!["testing".to_string()],
                after: vec!["draft".to_string()],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec!["phase:validate".to_string()],
            },
            TaskTemplate {
                template_id: "export".to_string(),
                title: "Export {{input.function_name}}".to_string(),
                description: "Save the validated function to the functions directory."
                    .to_string(),
                skills: vec![],
                after: vec!["validate".to_string()],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec!["phase:export".to_string()],
            },
        ],
        outputs: vec![FunctionOutput {
            name: "function_file".to_string(),
            description: "Path to the extracted function YAML".to_string(),
            from_task: "export".to_string(),
            field: "artifacts".to_string(),
        }],
        planning: Some(PlanningConfig {
            planner_template: TaskTemplate {
                template_id: "plan-extraction".to_string(),
                title: "Plan extraction of {{input.source_task_id}}".to_string(),
                description: "Analyze the source task and decide how to structure \
                              the extraction. Consider whether to use subgraph mode, \
                              how many parameter points to detect, and what \
                              validation steps are needed."
                    .to_string(),
                skills: vec!["analysis".to_string(), "planning".to_string()],
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
            min_tasks: Some(2),
            max_tasks: Some(8),
            required_skills: vec!["analysis".to_string()],
            max_depth: Some(5),
            allow_cycles: false,
            max_total_iterations: None,
            required_phases: vec!["analyze".to_string(), "validate".to_string()],
            forbidden_patterns: vec![],
        }),
        memory: None,
        visibility: FunctionVisibility::Internal,
        redacted_fields: vec![],
    };

    // Save
    let saved_path = trace_function::save_function(&func, &func_dir)?;

    println!("Bootstrapped meta-function 'extract-function' (version 2, generative)");
    println!();
    println!("  Inputs:");
    println!("    source_task_id (string, required) — Task ID to extract from");
    println!("    function_name (string, required) — Name for the new function");
    println!("    subgraph (string, optional) — Extract full subgraph (default: true)");
    println!();
    println!("  Static fallback tasks: analyze → draft → validate → export");
    println!(
        "  Constraints: 2-8 tasks, requires analysis skill, max depth 5"
    );
    println!();
    println!("  Saved to: {}", saved_path.display());
    println!();
    println!("  Usage:");
    println!("    wg trace instantiate extract-function \\");
    println!("      --input source_task_id=my-task \\");
    println!("      --input function_name=my-function");
    println!();
    println!("  To make it adaptive (learns from past extractions):");
    println!("    wg trace make-adaptive extract-function");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::trace_function;

    fn setup_workgraph(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let graph = workgraph::graph::WorkGraph::new();
        workgraph::parser::save_graph(&graph, &dir.join("graph.jsonl")).unwrap();
    }

    #[test]
    fn bootstrap_creates_meta_function() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        run(dir, false).unwrap();

        let func_dir = trace_function::functions_dir(dir);
        let func = trace_function::find_function_by_prefix(&func_dir, "extract-function").unwrap();
        assert_eq!(func.id, "extract-function");
        assert_eq!(func.version, 2);
        assert!(func.planning.is_some());
        assert!(func.constraints.is_some());
        assert_eq!(func.inputs.len(), 3);
        assert_eq!(func.tasks.len(), 4);
    }

    #[test]
    fn bootstrap_rejects_without_force() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        run(dir, false).unwrap();

        let result = run(dir, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn bootstrap_force_overwrites() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        run(dir, false).unwrap();
        run(dir, true).unwrap();

        let func_dir = trace_function::functions_dir(dir);
        let func = trace_function::find_function_by_prefix(&func_dir, "extract-function").unwrap();
        assert_eq!(func.id, "extract-function");
    }

    #[test]
    fn bootstrap_function_is_valid() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        run(dir, false).unwrap();

        let func_dir = trace_function::functions_dir(dir);
        let func = trace_function::find_function_by_prefix(&func_dir, "extract-function").unwrap();
        trace_function::validate_function(&func).unwrap();
    }

    #[test]
    fn bootstrap_has_planning_config() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_workgraph(dir);

        run(dir, false).unwrap();

        let func_dir = trace_function::functions_dir(dir);
        let func = trace_function::find_function_by_prefix(&func_dir, "extract-function").unwrap();

        let planning = func.planning.unwrap();
        assert_eq!(planning.planner_template.template_id, "plan-extraction");
        assert!(planning.static_fallback);
        assert!(planning.validate_plan);

        let constraints = func.constraints.unwrap();
        assert_eq!(constraints.min_tasks, Some(2));
        assert_eq!(constraints.max_tasks, Some(8));
        assert!(constraints.required_skills.contains(&"analysis".to_string()));
    }
}

//! Integration tests for func commands: extraction, application, storage,
//! input validation, template substitution, and full round-trips.
//!
//! Storage, validation, and substitution tests use library APIs directly.
//! Extraction and application tests invoke the `wg` binary (CLI).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::trace_function::{
    self, ExtractionSource, FunctionInput, FunctionOutput, FunctionVisibility, InputType,
    LoopEdgeTemplate, TaskTemplate, TraceFunction, TraceFunctionError,
};

// ===========================================================================
// Helpers
// ===========================================================================

fn make_task(id: &str, title: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status: Status::Done,
        ..Task::default()
    }
}

fn setup_workgraph(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let graph = WorkGraph::new();
    save_graph(&graph, dir.join("graph.jsonl")).unwrap();
}

fn setup_graph(dir: &Path, graph: &WorkGraph) {
    std::fs::create_dir_all(dir).unwrap();
    save_graph(graph, dir.join("graph.jsonl")).unwrap();
}

fn setup_function(dir: &Path, func: &TraceFunction) {
    let func_dir = trace_function::functions_dir(dir);
    trace_function::save_function(func, &func_dir).unwrap();
}

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    stdout
}

fn sample_function() -> TraceFunction {
    TraceFunction {
        kind: "trace-function".to_string(),
        version: 1,
        id: "impl-feature".to_string(),
        name: "Implement Feature".to_string(),
        description: "Plan, implement, test a new feature".to_string(),
        extracted_from: vec![ExtractionSource {
            task_id: "impl-global-config".to_string(),
            run_id: Some("run-003".to_string()),
            timestamp: "2026-02-18T14:30:00Z".to_string(),
        }],
        extracted_by: Some("scout".to_string()),
        extracted_at: Some("2026-02-19T12:00:00Z".to_string()),
        tags: vec!["implementation".to_string()],
        inputs: vec![
            FunctionInput {
                name: "feature_name".to_string(),
                input_type: InputType::String,
                description: "Short name for the feature".to_string(),
                required: true,
                default: None,
                example: Some(serde_yaml::Value::String("global-config".to_string())),
                min: None,
                max: None,
                values: None,
            },
            FunctionInput {
                name: "test_command".to_string(),
                input_type: InputType::String,
                description: "Command to verify".to_string(),
                required: false,
                default: Some(serde_yaml::Value::String("cargo test".to_string())),
                example: None,
                min: None,
                max: None,
                values: None,
            },
        ],
        tasks: vec![
            TaskTemplate {
                template_id: "plan".to_string(),
                title: "Plan {{input.feature_name}}".to_string(),
                description: "Plan the implementation of {{input.feature_name}}".to_string(),
                skills: vec!["analysis".to_string()],
                after: vec![],
                loops_to: vec![],
                role_hint: Some("analyst".to_string()),
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
            TaskTemplate {
                template_id: "implement".to_string(),
                title: "Implement {{input.feature_name}}".to_string(),
                description: "Implement the feature. Run: {{input.test_command}}".to_string(),
                skills: vec!["implementation".to_string()],
                after: vec!["plan".to_string()],
                loops_to: vec![],
                role_hint: Some("programmer".to_string()),
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
            TaskTemplate {
                template_id: "validate".to_string(),
                title: "Validate {{input.feature_name}}".to_string(),
                description: "Validate the implementation".to_string(),
                skills: vec!["review".to_string()],
                after: vec!["implement".to_string()],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
            TaskTemplate {
                template_id: "refine".to_string(),
                title: "Refine {{input.feature_name}}".to_string(),
                description: "Address issues found during validation".to_string(),
                skills: vec![],
                after: vec!["validate".to_string()],
                loops_to: vec![LoopEdgeTemplate {
                    target: "validate".to_string(),
                    max_iterations: 3,
                    guard: None,
                    delay: None,
                }],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec![],
            },
        ],
        outputs: vec![FunctionOutput {
            name: "modified_files".to_string(),
            description: "Files changed".to_string(),
            from_task: "implement".to_string(),
            field: "artifacts".to_string(),
        }],
        planning: None,
        constraints: None,
        memory: None,
        visibility: FunctionVisibility::Internal,
        redacted_fields: vec![],
    }
}

// ===========================================================================
// 1. Core storage tests
// ===========================================================================

#[test]
fn storage_round_trip_yaml() {
    let tmp = TempDir::new().unwrap();
    let func = sample_function();

    let path = trace_function::save_function(&func, tmp.path()).unwrap();
    assert!(path.exists());
    assert_eq!(path.file_name().unwrap(), "impl-feature.yaml");

    let loaded = trace_function::load_function(&path).unwrap();
    assert_eq!(loaded.id, func.id);
    assert_eq!(loaded.name, func.name);
    assert_eq!(loaded.description, func.description);
    assert_eq!(loaded.kind, "trace-function");
    assert_eq!(loaded.version, 1);
    assert_eq!(loaded.tasks.len(), func.tasks.len());
    assert_eq!(loaded.inputs.len(), func.inputs.len());
    assert_eq!(loaded.outputs.len(), func.outputs.len());
    assert_eq!(loaded.tags, func.tags);

    // Verify task template details survived
    assert_eq!(loaded.tasks[0].template_id, "plan");
    assert_eq!(loaded.tasks[1].after, vec!["plan"]);
    assert_eq!(loaded.tasks[3].loops_to.len(), 1);
    assert_eq!(loaded.tasks[3].loops_to[0].target, "validate");
    assert_eq!(loaded.tasks[3].loops_to[0].max_iterations, 3);

    // Verify input definitions survived
    assert_eq!(loaded.inputs[0].input_type, InputType::String);
    assert!(loaded.inputs[0].required);
    assert!(!loaded.inputs[1].required);
    assert_eq!(
        loaded.inputs[1].default,
        Some(serde_yaml::Value::String("cargo test".to_string()))
    );
}

#[test]
fn storage_load_from_nonexistent_dir_returns_empty() {
    let result = trace_function::load_all_functions(Path::new("/nonexistent/dir/xyz")).unwrap();
    assert!(result.is_empty());
}

#[test]
fn storage_load_all_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let all = trace_function::load_all_functions(tmp.path()).unwrap();
    assert!(all.is_empty());
}

#[test]
fn storage_load_all_sorts_by_id() {
    let tmp = TempDir::new().unwrap();
    let mut f1 = sample_function();
    f1.id = "zebra-func".to_string();
    let mut f2 = sample_function();
    f2.id = "alpha-func".to_string();
    let mut f3 = sample_function();
    f3.id = "middle-func".to_string();

    trace_function::save_function(&f1, tmp.path()).unwrap();
    trace_function::save_function(&f2, tmp.path()).unwrap();
    trace_function::save_function(&f3, tmp.path()).unwrap();

    let all = trace_function::load_all_functions(tmp.path()).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, "alpha-func");
    assert_eq!(all[1].id, "middle-func");
    assert_eq!(all[2].id, "zebra-func");
}

#[test]
fn storage_find_by_prefix_exact() {
    let tmp = TempDir::new().unwrap();
    trace_function::save_function(&sample_function(), tmp.path()).unwrap();

    let found = trace_function::find_function_by_prefix(tmp.path(), "impl-feature").unwrap();
    assert_eq!(found.id, "impl-feature");
}

#[test]
fn storage_find_by_prefix_partial() {
    let tmp = TempDir::new().unwrap();
    trace_function::save_function(&sample_function(), tmp.path()).unwrap();

    let found = trace_function::find_function_by_prefix(tmp.path(), "impl").unwrap();
    assert_eq!(found.id, "impl-feature");
}

#[test]
fn storage_find_by_prefix_not_found() {
    let tmp = TempDir::new().unwrap();
    trace_function::save_function(&sample_function(), tmp.path()).unwrap();

    let err = trace_function::find_function_by_prefix(tmp.path(), "nonexistent").unwrap_err();
    assert!(matches!(err, TraceFunctionError::NotFound(_)));
}

#[test]
fn storage_find_by_prefix_ambiguous() {
    let tmp = TempDir::new().unwrap();
    let mut f1 = sample_function();
    f1.id = "impl-feature".to_string();
    let mut f2 = sample_function();
    f2.id = "impl-bugfix".to_string();

    trace_function::save_function(&f1, tmp.path()).unwrap();
    trace_function::save_function(&f2, tmp.path()).unwrap();

    let err = trace_function::find_function_by_prefix(tmp.path(), "impl").unwrap_err();
    assert!(matches!(err, TraceFunctionError::Ambiguous(_)));
}

// ===========================================================================
// 2. Input validation tests
// ===========================================================================

#[test]
fn validation_missing_required_input_errors() {
    let defs = vec![FunctionInput {
        name: "feature_name".to_string(),
        input_type: InputType::String,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: None,
        max: None,
        values: None,
    }];

    let provided = HashMap::new();
    let err = trace_function::validate_inputs(&defs, &provided).unwrap_err();
    match err {
        TraceFunctionError::Validation(msg) => {
            assert!(msg.contains("feature_name"));
        }
        _ => panic!("Expected Validation error, got {:?}", err),
    }
}

#[test]
fn validation_wrong_type_string_where_number_expected() {
    let defs = vec![FunctionInput {
        name: "threshold".to_string(),
        input_type: InputType::Number,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: None,
        max: None,
        values: None,
    }];

    let mut provided = HashMap::new();
    provided.insert(
        "threshold".to_string(),
        serde_yaml::Value::String("not-a-number".to_string()),
    );

    let err = trace_function::validate_inputs(&defs, &provided).unwrap_err();
    assert!(matches!(err, TraceFunctionError::Validation(_)));
}

#[test]
fn validation_number_where_string_expected() {
    let defs = vec![FunctionInput {
        name: "name".to_string(),
        input_type: InputType::String,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: None,
        max: None,
        values: None,
    }];

    let mut provided = HashMap::new();
    provided.insert(
        "name".to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(42)),
    );

    let err = trace_function::validate_inputs(&defs, &provided).unwrap_err();
    assert!(matches!(err, TraceFunctionError::Validation(_)));
}

#[test]
fn validation_enum_value_not_in_allowed_list() {
    let defs = vec![FunctionInput {
        name: "language".to_string(),
        input_type: InputType::Enum,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: None,
        max: None,
        values: Some(vec![
            "rust".to_string(),
            "python".to_string(),
            "go".to_string(),
        ]),
    }];

    let mut provided = HashMap::new();
    provided.insert(
        "language".to_string(),
        serde_yaml::Value::String("java".to_string()),
    );

    let err = trace_function::validate_inputs(&defs, &provided).unwrap_err();
    match err {
        TraceFunctionError::Validation(msg) => {
            assert!(msg.contains("java"));
            assert!(msg.contains("rust"));
        }
        _ => panic!("Expected Validation error"),
    }
}

#[test]
fn validation_enum_valid_value_accepted() {
    let defs = vec![FunctionInput {
        name: "language".to_string(),
        input_type: InputType::Enum,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: None,
        max: None,
        values: Some(vec![
            "rust".to_string(),
            "python".to_string(),
            "go".to_string(),
        ]),
    }];

    let mut provided = HashMap::new();
    provided.insert(
        "language".to_string(),
        serde_yaml::Value::String("rust".to_string()),
    );

    let resolved = trace_function::validate_inputs(&defs, &provided).unwrap();
    assert_eq!(resolved.get("language").unwrap().as_str().unwrap(), "rust");
}

#[test]
fn validation_number_below_min_errors() {
    let defs = vec![FunctionInput {
        name: "threshold".to_string(),
        input_type: InputType::Number,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: Some(0.0),
        max: Some(1.0),
        values: None,
    }];

    let mut provided = HashMap::new();
    provided.insert(
        "threshold".to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(-0.5)),
    );

    let err = trace_function::validate_inputs(&defs, &provided).unwrap_err();
    match err {
        TraceFunctionError::Validation(msg) => {
            assert!(msg.contains("below minimum"));
        }
        _ => panic!("Expected Validation error"),
    }
}

#[test]
fn validation_number_above_max_errors() {
    let defs = vec![FunctionInput {
        name: "threshold".to_string(),
        input_type: InputType::Number,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: Some(0.0),
        max: Some(1.0),
        values: None,
    }];

    let mut provided = HashMap::new();
    provided.insert(
        "threshold".to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(1.5)),
    );

    let err = trace_function::validate_inputs(&defs, &provided).unwrap_err();
    match err {
        TraceFunctionError::Validation(msg) => {
            assert!(msg.contains("exceeds maximum"));
        }
        _ => panic!("Expected Validation error"),
    }
}

#[test]
fn validation_number_within_range_accepted() {
    let defs = vec![FunctionInput {
        name: "threshold".to_string(),
        input_type: InputType::Number,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: Some(0.0),
        max: Some(1.0),
        values: None,
    }];

    let mut provided = HashMap::new();
    provided.insert(
        "threshold".to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(0.5)),
    );

    let resolved = trace_function::validate_inputs(&defs, &provided).unwrap();
    assert!(resolved.contains_key("threshold"));
}

#[test]
fn validation_optional_input_with_default_applied() {
    let defs = vec![
        FunctionInput {
            name: "feature_name".to_string(),
            input_type: InputType::String,
            description: "".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        },
        FunctionInput {
            name: "test_command".to_string(),
            input_type: InputType::String,
            description: "".to_string(),
            required: false,
            default: Some(serde_yaml::Value::String("cargo test".to_string())),
            example: None,
            min: None,
            max: None,
            values: None,
        },
    ];

    let mut provided = HashMap::new();
    provided.insert(
        "feature_name".to_string(),
        serde_yaml::Value::String("auth".to_string()),
    );

    let resolved = trace_function::validate_inputs(&defs, &provided).unwrap();
    assert_eq!(
        resolved.get("feature_name").unwrap().as_str().unwrap(),
        "auth"
    );
    assert_eq!(
        resolved.get("test_command").unwrap().as_str().unwrap(),
        "cargo test"
    );
}

#[test]
fn validation_optional_input_without_default_omitted() {
    let defs = vec![FunctionInput {
        name: "notes".to_string(),
        input_type: InputType::String,
        description: "".to_string(),
        required: false,
        default: None,
        example: None,
        min: None,
        max: None,
        values: None,
    }];

    let provided = HashMap::new();
    let resolved = trace_function::validate_inputs(&defs, &provided).unwrap();
    assert!(!resolved.contains_key("notes"));
}

#[test]
fn validation_file_list_requires_sequence() {
    let defs = vec![FunctionInput {
        name: "files".to_string(),
        input_type: InputType::FileList,
        description: "".to_string(),
        required: true,
        default: None,
        example: None,
        min: None,
        max: None,
        values: None,
    }];

    // String instead of list → error
    let mut provided = HashMap::new();
    provided.insert(
        "files".to_string(),
        serde_yaml::Value::String("src/main.rs".to_string()),
    );
    assert!(trace_function::validate_inputs(&defs, &provided).is_err());

    // Correct: sequence
    provided.insert(
        "files".to_string(),
        serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(
            "src/main.rs".to_string(),
        )]),
    );
    assert!(trace_function::validate_inputs(&defs, &provided).is_ok());
}

// ===========================================================================
// 3. Template substitution tests
// ===========================================================================

#[test]
fn substitution_simple_string_replacement() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "feature_name".to_string(),
        serde_yaml::Value::String("auth".to_string()),
    );

    let result = trace_function::substitute("Plan {{input.feature_name}}", &inputs);
    assert_eq!(result, "Plan auth");
}

#[test]
fn substitution_multiple_inputs_in_same_template() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "feature_name".to_string(),
        serde_yaml::Value::String("auth".to_string()),
    );
    inputs.insert(
        "test_command".to_string(),
        serde_yaml::Value::String("cargo test auth".to_string()),
    );

    let result = trace_function::substitute(
        "Implement {{input.feature_name}}. Run: {{input.test_command}}",
        &inputs,
    );
    assert_eq!(result, "Implement auth. Run: cargo test auth");
}

#[test]
fn substitution_file_list_rendered_as_newline_separated() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "files".to_string(),
        serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("src/main.rs".to_string()),
            serde_yaml::Value::String("src/lib.rs".to_string()),
            serde_yaml::Value::String("src/config.rs".to_string()),
        ]),
    );

    let result = trace_function::substitute("Files:\n{{input.files}}", &inputs);
    assert_eq!(result, "Files:\nsrc/main.rs\nsrc/lib.rs\nsrc/config.rs");
}

#[test]
fn substitution_missing_optional_uses_default_in_resolved_map() {
    let func = sample_function();
    let mut provided = HashMap::new();
    provided.insert(
        "feature_name".to_string(),
        serde_yaml::Value::String("auth".to_string()),
    );

    let resolved = trace_function::validate_inputs(&func.inputs, &provided).unwrap();
    let result = trace_function::substitute("Run: {{input.test_command}}", &resolved);
    assert_eq!(result, "Run: cargo test");
}

#[test]
fn substitution_unrecognized_placeholder_left_as_is() {
    let inputs = HashMap::new();
    let result = trace_function::substitute(
        "Hello {{input.unknown}} world {{input.other}}",
        &inputs,
    );
    assert_eq!(result, "Hello {{input.unknown}} world {{input.other}}");
}

#[test]
fn substitution_number_input() {
    let mut inputs = HashMap::new();
    inputs.insert(
        "threshold".to_string(),
        serde_yaml::Value::Number(serde_yaml::Number::from(42)),
    );

    let result = trace_function::substitute("Minimum score: {{input.threshold}}", &inputs);
    assert_eq!(result, "Minimum score: 42");
}

#[test]
fn substitution_task_template_all_fields() {
    let template = TaskTemplate {
        template_id: "plan".to_string(),
        title: "Plan {{input.feature_name}}".to_string(),
        description: "Plan {{input.feature_name}} using {{input.test_command}}".to_string(),
        skills: vec!["analysis".to_string(), "{{input.language}}".to_string()],
        after: vec!["prereq".to_string()],
        loops_to: vec![],
        role_hint: Some("analyst".to_string()),
        deliverables: vec!["docs/{{input.feature_name}}.md".to_string()],
        verify: Some("{{input.test_command}}".to_string()),
        tags: vec!["impl".to_string()],
    };

    let mut inputs = HashMap::new();
    inputs.insert(
        "feature_name".to_string(),
        serde_yaml::Value::String("auth".to_string()),
    );
    inputs.insert(
        "test_command".to_string(),
        serde_yaml::Value::String("cargo test".to_string()),
    );
    inputs.insert(
        "language".to_string(),
        serde_yaml::Value::String("rust".to_string()),
    );

    let result = trace_function::substitute_task_template(&template, &inputs);

    // Substituted fields
    assert_eq!(result.title, "Plan auth");
    assert_eq!(result.description, "Plan auth using cargo test");
    assert_eq!(result.skills, vec!["analysis", "rust"]);
    assert_eq!(result.deliverables, vec!["docs/auth.md"]);
    assert_eq!(result.verify.as_deref(), Some("cargo test"));

    // Preserved fields (not substituted)
    assert_eq!(result.template_id, "plan");
    assert_eq!(result.after, vec!["prereq"]);
    assert_eq!(result.role_hint, Some("analyst".to_string()));
    assert_eq!(result.tags, vec!["impl"]);
}

// ===========================================================================
// 4. Extraction tests (via CLI)
// ===========================================================================

#[test]
fn extract_single_done_task_produces_valid_function() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    let mut task = make_task("impl-config", "Implement config");
    task.description = Some("Add global config at src/config.rs".to_string());
    task.artifacts = vec!["src/config.rs".to_string()];
    graph.add_node(Node::Task(task));
    setup_graph(&dir, &graph);

    wg_ok(&dir, &["func", "extract", "impl-config", "--name", "config-func"]);

    let func_path = dir.join("functions").join("config-func.yaml");
    assert!(func_path.exists());

    let func = trace_function::load_function(&func_path).unwrap();
    assert_eq!(func.id, "config-func");
    assert_eq!(func.kind, "trace-function");
    assert_eq!(func.version, 1);
    assert_eq!(func.tasks.len(), 1);
    assert_eq!(func.tasks[0].template_id, "impl-config");
    assert!(!func.outputs.is_empty(), "Should have outputs from artifacts");
    trace_function::validate_function(&func).unwrap();
}

#[test]
fn extract_from_subgraph_captures_all_tasks_and_dependencies() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "feature".to_string(),
        title: "Feature root".to_string(),
        status: Status::Done,
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: "feature-plan".to_string(),
        title: "Plan feature".to_string(),
        status: Status::Done,
        after: vec!["feature".to_string()],
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: "feature-build".to_string(),
        title: "Build feature".to_string(),
        status: Status::Done,
        after: vec!["feature-plan".to_string()],
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    wg_ok(
        &dir,
        &["func", "extract", "feature", "--name", "my-workflow", "--subgraph"],
    );

    let func_path = dir.join("functions").join("my-workflow.yaml");
    let func = trace_function::load_function(&func_path).unwrap();
    assert_eq!(func.tasks.len(), 3);

    // Check after edges remapped to template IDs
    let plan_tmpl = func.tasks.iter().find(|t| t.template_id == "plan").unwrap();
    assert_eq!(plan_tmpl.after, vec!["feature"]);

    let build_tmpl = func.tasks.iter().find(|t| t.template_id == "build").unwrap();
    assert_eq!(build_tmpl.after, vec!["plan"]);

    trace_function::validate_function(&func).unwrap();
}


#[test]
fn extract_from_non_done_task_errors() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "open-task".to_string(),
        title: "Not done yet".to_string(),
        status: Status::Open,
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    let output = wg_cmd(&dir, &["func", "extract", "open-task"]);
    assert!(
        !output.status.success(),
        "Should fail for non-done task"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("done") || String::from_utf8_lossy(&output.stdout).to_lowercase().contains("done"),
        "Error should mention done status"
    );
}

#[test]
fn extract_detects_file_paths_and_commands() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "impl-auth".to_string(),
        title: "Implement auth".to_string(),
        description: Some(
            "Add authentication to src/auth.rs and src/main.rs. Run cargo test to verify."
                .to_string(),
        ),
        status: Status::Done,
        artifacts: vec!["src/auth.rs".to_string(), "src/main.rs".to_string()],
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    wg_ok(&dir, &["func", "extract", "impl-auth", "--name", "auth-func"]);

    let func_path = dir.join("functions").join("auth-func.yaml");
    let func = trace_function::load_function(&func_path).unwrap();

    assert!(func.inputs.iter().any(|i| i.name == "feature_name"));
    assert!(func.inputs.iter().any(|i| i.name == "source_files"));
    assert!(func.inputs.iter().any(|i| i.name == "test_command"));
}

// ===========================================================================
// 5. Application tests (via CLI — `wg func apply`)
// ===========================================================================

#[test]
fn apply_single_task_function_creates_task() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);

    let func = TraceFunction {
        kind: "trace-function".to_string(),
        version: 1,
        id: "simple-func".to_string(),
        name: "Simple".to_string(),
        description: "A single task".to_string(),
        extracted_from: vec![],
        extracted_by: None,
        extracted_at: None,
        tags: vec![],
        inputs: vec![FunctionInput {
            name: "feature_name".to_string(),
            input_type: InputType::String,
            description: "".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        }],
        tasks: vec![TaskTemplate {
            template_id: "do-thing".to_string(),
            title: "Do {{input.feature_name}}".to_string(),
            description: "Do the thing for {{input.feature_name}}".to_string(),
            skills: vec![],
            after: vec![],
            loops_to: vec![],
            role_hint: None,
            deliverables: vec![],
            verify: None,
            tags: vec![],
        }],
        outputs: vec![],
        planning: None,
        constraints: None,
        memory: None,
        visibility: FunctionVisibility::Internal,
        redacted_fields: vec![],
    };
    setup_function(dir, &func);

    wg_ok(
        dir,
        &["func", "apply", "simple-func", "--input", "feature_name=auth"],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("auth-do-thing").unwrap();
    assert_eq!(task.title, "Do auth");
    assert_eq!(task.status, Status::Open);
    assert!(task.description.as_ref().unwrap().contains("Do the thing for auth"));
}

#[test]
fn apply_multi_task_function_correct_after() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    wg_ok(
        dir,
        &["func", "apply", "impl-feature", "--input", "feature_name=auth"],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();

    assert!(graph.get_task("auth-plan").is_some());
    assert!(graph.get_task("auth-implement").is_some());
    assert!(graph.get_task("auth-validate").is_some());
    assert!(graph.get_task("auth-refine").is_some());

    let plan = graph.get_task("auth-plan").unwrap();
    assert!(plan.after.is_empty());

    let implement = graph.get_task("auth-implement").unwrap();
    assert_eq!(implement.after, vec!["auth-plan"]);

    let validate = graph.get_task("auth-validate").unwrap();
    assert_eq!(validate.after, vec!["auth-implement"]);

    let refine = graph.get_task("auth-refine").unwrap();
    assert_eq!(refine.after, vec!["auth-validate"]);
}


#[test]
fn apply_dry_run_does_not_modify_graph() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    wg_ok(
        dir,
        &[
            "func", "apply", "impl-feature",
            "--input", "feature_name=auth",
            "--dry-run",
        ],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("auth-plan").is_none());
    assert!(graph.get_task("auth-implement").is_none());
}

#[test]
fn apply_after_wires_root_tasks_to_external() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    // Add an external prerequisite task
    {
        let mut graph = load_graph(dir.join("graph.jsonl")).unwrap();
        graph.add_node(Node::Task(Task {
            id: "prerequisite".to_string(),
            title: "Prerequisite".to_string(),
            ..Task::default()
        }));
        save_graph(&graph, dir.join("graph.jsonl")).unwrap();
    }

    wg_ok(
        dir,
        &[
            "func", "apply", "impl-feature",
            "--input", "feature_name=auth",
            "--after", "prerequisite",
        ],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();

    let plan = graph.get_task("auth-plan").unwrap();
    assert!(plan.after.contains(&"prerequisite".to_string()));

    let implement = graph.get_task("auth-implement").unwrap();
    assert!(!implement.after.contains(&"prerequisite".to_string()));
    assert!(implement.after.contains(&"auth-plan".to_string()));
}

#[test]
fn apply_duplicate_task_id_errors() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    // First instantiation
    wg_ok(
        dir,
        &["func", "apply", "impl-feature", "--input", "feature_name=auth"],
    );

    // Second with same prefix should fail
    let output = wg_cmd(
        dir,
        &["func", "apply", "impl-feature", "--input", "feature_name=auth"],
    );
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("already exists"));
}

#[test]
fn apply_with_prefix_override() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    wg_ok(
        dir,
        &[
            "func", "apply", "impl-feature",
            "--input", "feature_name=auth",
            "--prefix", "custom-prefix",
        ],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("custom-prefix-plan").is_some());
    assert!(graph.get_task("custom-prefix-implement").is_some());
    assert!(graph.get_task("custom-prefix-validate").is_some());
    assert!(graph.get_task("custom-prefix-refine").is_some());
}

#[test]
fn apply_substitutes_template_values() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    wg_ok(
        dir,
        &[
            "func", "apply", "impl-feature",
            "--input", "feature_name=auth",
            "--input", "test_command=cargo test auth",
        ],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();

    let plan = graph.get_task("auth-plan").unwrap();
    assert_eq!(plan.title, "Plan auth");
    assert!(plan.description.as_ref().unwrap().contains("auth"));

    let implement = graph.get_task("auth-implement").unwrap();
    assert_eq!(implement.title, "Implement auth");
    assert!(implement.description.as_ref().unwrap().contains("cargo test auth"));
}

#[test]
fn apply_missing_required_input_errors() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    let output = wg_cmd(dir, &["func", "apply", "impl-feature"]);
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("feature_name"));
}

#[test]
fn apply_function_not_found_errors() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);

    let output = wg_cmd(
        dir,
        &["func", "apply", "nonexistent", "--input", "feature_name=auth"],
    );
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("nonexistent"));
}

#[test]
fn apply_with_input_file() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    let input_file = dir.join("inputs.yaml");
    std::fs::write(
        &input_file,
        "feature_name: login\ntest_command: cargo test login\n",
    )
    .unwrap();

    wg_ok(
        dir,
        &[
            "func", "apply", "impl-feature",
            "--input-file", input_file.to_str().unwrap(),
        ],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();
    let plan = graph.get_task("login-plan").unwrap();
    assert_eq!(plan.title, "Plan login");
}

#[test]
fn apply_maintains_blocks_symmetry() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    wg_ok(
        dir,
        &["func", "apply", "impl-feature", "--input", "feature_name=auth"],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();

    let plan = graph.get_task("auth-plan").unwrap();
    assert!(plan.before.contains(&"auth-implement".to_string()));

    let implement = graph.get_task("auth-implement").unwrap();
    assert!(implement.before.contains(&"auth-validate".to_string()));
}

#[test]
fn apply_applies_model() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    wg_ok(
        dir,
        &[
            "func", "apply", "impl-feature",
            "--input", "feature_name=auth",
            "--model", "sonnet",
        ],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();
    for task_id in &["auth-plan", "auth-implement", "auth-validate", "auth-refine"] {
        let task = graph.get_task(task_id).unwrap();
        assert_eq!(task.model, Some("sonnet".to_string()));
    }
}

#[test]
fn apply_adds_skill_and_role_tags() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    wg_ok(
        dir,
        &["func", "apply", "impl-feature", "--input", "feature_name=auth"],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();

    let plan = graph.get_task("auth-plan").unwrap();
    assert!(plan.tags.contains(&"skill:analysis".to_string()));
    assert!(plan.tags.contains(&"role:analyst".to_string()));

    let implement = graph.get_task("auth-implement").unwrap();
    assert!(implement.tags.contains(&"skill:implementation".to_string()));
    assert!(implement.tags.contains(&"role:programmer".to_string()));
}

// ===========================================================================
// 6. Round-trip tests: create → extract → instantiate → verify
// ===========================================================================

#[test]
fn round_trip_extract_then_instantiate_preserves_structure() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    // Step 1: Create a workflow graph
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "proj-design".to_string(),
        title: "Design the project".to_string(),
        description: Some("Create architecture docs".to_string()),
        status: Status::Done,
        skills: vec!["architecture".to_string()],
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: "proj-implement".to_string(),
        title: "Implement the project".to_string(),
        description: Some("Write the code".to_string()),
        status: Status::Done,
        after: vec!["proj-design".to_string()],
        skills: vec!["coding".to_string()],
        artifacts: vec!["src/main.rs".to_string()],
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: "proj-test".to_string(),
        title: "Test the project".to_string(),
        description: Some("Run cargo test".to_string()),
        status: Status::Done,
        after: vec!["proj-implement".to_string()],
        skills: vec!["testing".to_string()],
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    // Step 2: Extract
    wg_ok(
        &dir,
        &["func", "extract", "proj-design", "--name", "project-workflow", "--subgraph"],
    );

    // Step 3: Verify extraction
    let func_path = dir.join("functions").join("project-workflow.yaml");
    let func = trace_function::load_function(&func_path).unwrap();
    assert_eq!(func.tasks.len(), 3);
    trace_function::validate_function(&func).unwrap();

    // Step 4: Instantiate with new prefix
    wg_ok(
        &dir,
        &[
            "func", "apply", "project-workflow",
            "--input", "feature_name=new-proj",
            "--prefix", "new-proj",
        ],
    );

    // Step 5: Verify structure preserved
    // Note: template IDs from extraction keep the root's own ID as-is
    // (strip_prefix doesn't strip when task_id == root_id), so the
    // instantiated IDs are prefix + "-" + template_id.
    let graph = load_graph(dir.join("graph.jsonl")).unwrap();

    // Find the created task IDs based on the function's template IDs
    let func_path = dir.join("functions").join("project-workflow.yaml");
    let func = trace_function::load_function(&func_path).unwrap();
    let template_ids: Vec<&str> = func.tasks.iter().map(|t| t.template_id.as_str()).collect();

    let created_ids: Vec<String> = template_ids
        .iter()
        .map(|tid| format!("new-proj-{}", tid))
        .collect();

    for task_id in &created_ids {
        assert!(
            graph.get_task(task_id).is_some(),
            "Task '{}' should exist",
            task_id
        );
        let task = graph.get_task(task_id).unwrap();
        assert_eq!(task.status, Status::Open);
    }

    // Verify dependency chain is preserved (implement blocked by design, test blocked by implement)
    let implement_tid = func.tasks.iter().find(|t| t.title.contains("Implement")).unwrap();
    let design_tid = func.tasks.iter().find(|t| t.title.contains("Design")).unwrap();
    let test_tid = func.tasks.iter().find(|t| t.title.contains("Test")).unwrap();

    let new_implement = graph.get_task(&format!("new-proj-{}", implement_tid.template_id)).unwrap();
    assert!(
        new_implement.after.contains(&format!("new-proj-{}", design_tid.template_id)),
        "implement should be blocked by design: {:?}",
        new_implement.after
    );

    let new_test = graph.get_task(&format!("new-proj-{}", test_tid.template_id)).unwrap();
    assert!(
        new_test.after.contains(&format!("new-proj-{}", implement_tid.template_id)),
        "test should be blocked by implement: {:?}",
        new_test.after
    );
}


#[test]
fn round_trip_multiple_instantiations_different_prefixes() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("tmpl", "Template task")));
    setup_graph(&dir, &graph);

    // Extract
    wg_ok(&dir, &["func", "extract", "tmpl", "--name", "reusable"]);

    // Instantiate twice
    wg_ok(
        &dir,
        &[
            "func", "apply", "reusable",
            "--input", "feature_name=first",
            "--prefix", "first",
        ],
    );
    wg_ok(
        &dir,
        &[
            "func", "apply", "reusable",
            "--input", "feature_name=second",
            "--prefix", "second",
        ],
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("first-tmpl").is_some());
    assert!(graph.get_task("second-tmpl").is_some());
}

// ===========================================================================
// 7. Function validation tests
// ===========================================================================

#[test]
fn validate_function_with_bad_after_reference() {
    let mut func = sample_function();
    func.tasks[1].after = vec!["nonexistent-task".to_string()];

    let err = trace_function::validate_function(&func).unwrap_err();
    match err {
        TraceFunctionError::Validation(msg) => {
            assert!(msg.contains("nonexistent-task"));
        }
        _ => panic!("Expected Validation error"),
    }
}

#[test]
fn validate_function_with_bad_loops_to_reference() {
    let mut func = sample_function();
    func.tasks[3].loops_to[0].target = "nonexistent-task".to_string();

    let err = trace_function::validate_function(&func).unwrap_err();
    match err {
        TraceFunctionError::Validation(msg) => {
            assert!(msg.contains("nonexistent-task"));
        }
        _ => panic!("Expected Validation error"),
    }
}

#[test]
fn validate_function_with_duplicate_template_ids() {
    let mut func = sample_function();
    func.tasks[1].template_id = "plan".to_string(); // duplicate

    let err = trace_function::validate_function(&func).unwrap_err();
    match err {
        TraceFunctionError::Validation(msg) => {
            assert!(msg.contains("Duplicate"));
        }
        _ => panic!("Expected Validation error"),
    }
}

#[test]
fn validate_function_valid_passes() {
    let func = sample_function();
    trace_function::validate_function(&func).unwrap();
}

// ===========================================================================
// 8. Extraction filtering tests (via CLI)
// ===========================================================================

#[test]
fn extract_filters_evaluate_tasks_from_subgraph() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "root".to_string(),
        title: "Root task".to_string(),
        status: Status::Done,
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: "root-impl".to_string(),
        title: "Implement feature".to_string(),
        status: Status::Done,
        after: vec!["root".to_string()],
        ..Task::default()
    }));
    // Coordinator-generated evaluate task (should be filtered)
    graph.add_node(Node::Task(Task {
        id: "evaluate-root-impl".to_string(),
        title: "Evaluate root-impl".to_string(),
        status: Status::Done,
        after: vec!["root-impl".to_string()],
        ..Task::default()
    }));
    // Coordinator-generated assign task (should be filtered)
    graph.add_node(Node::Task(Task {
        id: "assign-root-impl".to_string(),
        title: "Assign root-impl".to_string(),
        status: Status::Done,
        after: vec!["root".to_string()],
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    // Default extraction: evaluate-*/assign-* tasks should be filtered out
    wg_ok(
        &dir,
        &["func", "extract", "root", "--name", "filtered-func", "--subgraph"],
    );

    let func_path = dir.join("functions").join("filtered-func.yaml");
    let func = trace_function::load_function(&func_path).unwrap();

    // Only root + root-impl; evaluate-root-impl and assign-root-impl filtered
    assert_eq!(
        func.tasks.len(),
        2,
        "Should filter out evaluate-* and assign-* tasks, got {} tasks: {:?}",
        func.tasks.len(),
        func.tasks.iter().map(|t| &t.template_id).collect::<Vec<_>>()
    );
    assert!(func.tasks.iter().all(|t| !t.template_id.starts_with("evaluate")));
    assert!(func.tasks.iter().all(|t| !t.template_id.starts_with("assign")));
}

#[test]
fn extract_include_evaluations_keeps_all_tasks() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "root".to_string(),
        title: "Root task".to_string(),
        status: Status::Done,
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: "root-impl".to_string(),
        title: "Implement".to_string(),
        status: Status::Done,
        after: vec!["root".to_string()],
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: "evaluate-root-impl".to_string(),
        title: "Evaluate root-impl".to_string(),
        status: Status::Done,
        after: vec!["root-impl".to_string()],
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    // With --include-evaluations: keeps all tasks
    wg_ok(
        &dir,
        &[
            "func", "extract", "root",
            "--name", "unfiltered-func",
            "--subgraph",
            "--include-evaluations",
        ],
    );

    let func_path = dir.join("functions").join("unfiltered-func.yaml");
    let func = trace_function::load_function(&func_path).unwrap();
    assert_eq!(func.tasks.len(), 3, "Should include all tasks with --include-evaluations");
}

// ===========================================================================
// 9. Parameter detection tests
// ===========================================================================

#[test]
fn extract_does_not_detect_random_numbers_as_params() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "impl-feature".to_string(),
        title: "Implement feature with 5 components".to_string(),
        description: Some(
            "This task has 3 subtasks. The implementation took 42 lines of code across 7 files."
                .to_string(),
        ),
        status: Status::Done,
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    wg_ok(
        &dir,
        &["func", "extract", "impl-feature", "--name", "no-random-nums"],
    );

    let func_path = dir.join("functions").join("no-random-nums.yaml");
    let func = trace_function::load_function(&func_path).unwrap();

    // Should NOT extract random numbers (5, 3, 42, 7) as numeric parameters
    let numeric_inputs: Vec<_> = func
        .inputs
        .iter()
        .filter(|i| i.input_type == InputType::Number)
        .collect();
    assert!(
        numeric_inputs.is_empty(),
        "Should not extract random numbers without keyword context, got: {:?}",
        numeric_inputs.iter().map(|i| &i.name).collect::<Vec<_>>()
    );
}

#[test]
fn extract_detects_contextual_numbers_as_params() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "impl-config".to_string(),
        title: "Implement config".to_string(),
        description: Some(
            "Set max retries to 3 and timeout threshold to 0.8 seconds.".to_string(),
        ),
        status: Status::Done,
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    wg_ok(
        &dir,
        &["func", "extract", "impl-config", "--name", "contextual-nums"],
    );

    let func_path = dir.join("functions").join("contextual-nums.yaml");
    let func = trace_function::load_function(&func_path).unwrap();

    // Should detect numbers that appear near parameterizable keywords
    let numeric_inputs: Vec<_> = func
        .inputs
        .iter()
        .filter(|i| i.input_type == InputType::Number)
        .collect();
    assert!(
        !numeric_inputs.is_empty(),
        "Should detect contextual numbers (retries, threshold)"
    );
}

// ===========================================================================
// 10. FuncCommands CLI parsing tests
// ===========================================================================

#[test]
fn func_list_succeeds_on_empty() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");
    setup_workgraph(&dir);

    // wg func list should succeed even with no functions
    let output = wg_cmd(&dir, &["func", "list"]);
    assert!(
        output.status.success(),
        "wg func list should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn func_show_on_existing_function() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");
    setup_workgraph(&dir);
    setup_function(&dir, &sample_function());

    let stdout = wg_ok(&dir, &["func", "show", "impl-feature"]);
    assert!(stdout.contains("impl-feature"));
    assert!(stdout.contains("Implement Feature"));
}

#[test]
fn func_show_not_found_errors() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");
    setup_workgraph(&dir);

    let output = wg_cmd(&dir, &["func", "show", "nonexistent"]);
    assert!(!output.status.success(), "Should fail for nonexistent function");
}

#[test]
fn func_extract_creates_function() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "done-task".to_string(),
        title: "A done task".to_string(),
        status: Status::Done,
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    wg_ok(&dir, &["func", "extract", "done-task", "--name", "cli-extract-test"]);

    let func_path = dir.join("functions").join("cli-extract-test.yaml");
    assert!(func_path.exists(), "Function file should be created by wg func extract");
}

#[test]
fn func_apply_creates_tasks() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    wg_ok(dir, &["func", "apply", "impl-feature", "--input", "feature_name=test"]);

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("test-plan").is_some());
    assert!(graph.get_task("test-implement").is_some());
    assert!(graph.get_task("test-validate").is_some());
    assert!(graph.get_task("test-refine").is_some());
}

#[test]
fn func_bootstrap_creates_meta_function() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);

    wg_ok(dir, &["func", "bootstrap"]);

    let func_path = dir.join("functions").join("extract-function.yaml");
    assert!(func_path.exists(), "Bootstrap should create extract-function");
}

// ===========================================================================
// 11. Backward-compatibility alias tests (wg trace → wg func)
// ===========================================================================

#[test]
fn trace_extract_alias_works_with_deprecation_warning() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".workgraph");

    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "alias-task".to_string(),
        title: "Alias test".to_string(),
        status: Status::Done,
        ..Task::default()
    }));
    setup_graph(&dir, &graph);

    let output = wg_cmd(
        &dir,
        &["trace", "extract", "alias-task", "--name", "alias-extract"],
    );
    assert!(
        output.status.success(),
        "wg trace extract should still work as alias; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deprecated") || stderr.contains("Use 'wg func extract'"),
        "Should show deprecation warning; stderr: {}",
        stderr
    );

    let func_path = dir.join("functions").join("alias-extract.yaml");
    assert!(func_path.exists(), "Alias should create function file");
}

#[test]
fn trace_instantiate_alias_works_with_deprecation_warning() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    let output = wg_cmd(
        dir,
        &["trace", "instantiate", "impl-feature", "--input", "feature_name=alias-test"],
    );
    assert!(
        output.status.success(),
        "wg trace instantiate should still work as alias; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deprecated") || stderr.contains("Use 'wg func apply'"),
        "Should show deprecation warning; stderr: {}",
        stderr
    );

    let graph = load_graph(dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("alias-test-plan").is_some());
}

#[test]
fn trace_list_functions_alias_works() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    let output = wg_cmd(dir, &["trace", "list-functions"]);
    assert!(
        output.status.success(),
        "wg trace list-functions should still work as alias; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deprecated") || stderr.contains("Use 'wg func list'"),
        "Should show deprecation warning"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("impl-feature"), "Should list the function");
}

#[test]
fn trace_show_function_alias_works() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);
    setup_function(dir, &sample_function());

    let output = wg_cmd(dir, &["trace", "show-function", "impl-feature"]);
    assert!(
        output.status.success(),
        "wg trace show-function should still work as alias; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deprecated") || stderr.contains("Use 'wg func show'"),
        "Should show deprecation warning"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Implement Feature"), "Should show function details");
}

#[test]
fn trace_bootstrap_alias_works() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);

    let output = wg_cmd(dir, &["trace", "bootstrap"]);
    assert!(
        output.status.success(),
        "wg trace bootstrap should still work as alias; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deprecated") || stderr.contains("Use 'wg func bootstrap'"),
        "Should show deprecation warning"
    );
}

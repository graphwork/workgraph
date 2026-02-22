//! Integration tests for the trace-function protocol layers.
//!
//! Layer 1 — Static functions: extraction with visibility, YAML round-trip
//!   including new fields (visibility, redacted_fields), instantiation with
//!   task creation.
//!
//! Layer 2 — Generative functions: PlanningConfig, StructuralConstraints,
//!   plan validation against constraints, constraint violations.
//!
//! Layer 3 — Adaptive functions: RunSummary save/load via trace_memory,
//!   render_summaries_text output verification, memory injection into
//!   template substitution.
//!
//! Visibility — export_function at each level, redaction behavior.

use std::collections::HashMap;
use std::path::Path;
use tempfile::TempDir;

use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::plan_validator::{self, ValidationError};
use workgraph::trace_function::{
    self, ExtractionSource, ForbiddenPattern, FunctionInput, FunctionOutput, FunctionVisibility,
    InputType, InterventionSummary, LoopEdgeTemplate, MemoryInclusions, PlanningConfig,
    RunSummary, StructuralConstraints, TaskOutcome, TaskTemplate, TraceFunction,
    TraceMemoryConfig,
};
use workgraph::trace_memory;

// ===========================================================================
// Helpers
// ===========================================================================

fn setup_workgraph(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let graph = WorkGraph::new();
    save_graph(&graph, &dir.join("graph.jsonl")).unwrap();
}

fn setup_function(dir: &Path, func: &TraceFunction) {
    let func_dir = trace_function::functions_dir(dir);
    trace_function::save_function(func, &func_dir).unwrap();
}

fn make_template(id: &str) -> TaskTemplate {
    TaskTemplate {
        template_id: id.to_string(),
        title: id.to_string(),
        description: String::new(),
        skills: vec![],
        after: vec![],
        loops_to: vec![],
        role_hint: None,
        deliverables: vec![],
        verify: None,
        tags: vec![],
    }
}

fn empty_constraints() -> StructuralConstraints {
    StructuralConstraints {
        min_tasks: None,
        max_tasks: None,
        required_skills: vec![],
        max_depth: None,
        allow_cycles: false,
        max_total_iterations: None,
        required_phases: vec![],
        forbidden_patterns: vec![],
    }
}

fn default_inclusions() -> MemoryInclusions {
    MemoryInclusions {
        outcomes: true,
        scores: true,
        interventions: true,
        duration: true,
        retries: false,
        artifacts: false,
    }
}

fn sample_run_summary() -> RunSummary {
    RunSummary {
        instantiated_at: "2026-02-20T12:00:00Z".to_string(),
        inputs: {
            let mut m = HashMap::new();
            m.insert(
                "feature_name".to_string(),
                serde_yaml::Value::String("auth".to_string()),
            );
            m
        },
        prefix: "auth".to_string(),
        task_outcomes: vec![
            TaskOutcome {
                template_id: "plan".to_string(),
                task_id: "auth-plan".to_string(),
                status: "Done".to_string(),
                score: Some(0.9),
                duration_secs: Some(60),
                retry_count: 0,
            },
            TaskOutcome {
                template_id: "implement".to_string(),
                task_id: "auth-implement".to_string(),
                status: "Done".to_string(),
                score: Some(0.85),
                duration_secs: Some(300),
                retry_count: 1,
            },
        ],
        interventions: vec![InterventionSummary {
            task_id: "auth-implement".to_string(),
            kind: "retry".to_string(),
            description: Some("Flaky test, retried".to_string()),
            timestamp: "2026-02-20T12:05:00Z".to_string(),
        }],
        wall_clock_secs: Some(360),
        all_succeeded: true,
        avg_score: Some(0.875),
    }
}

/// Build a v1 static function with visibility and redacted_fields set.
fn sample_v1_with_visibility(vis: FunctionVisibility) -> TraceFunction {
    TraceFunction {
        kind: "trace-function".to_string(),
        version: 1,
        id: "vis-func".to_string(),
        name: "Visibility Test Function".to_string(),
        description: "Testing visibility on a v1 function".to_string(),
        extracted_from: vec![ExtractionSource {
            task_id: "impl-config".to_string(),
            run_id: Some("run-007".to_string()),
            timestamp: "2026-02-18T14:30:00Z".to_string(),
        }],
        extracted_by: Some("scout-abc123".to_string()),
        extracted_at: Some("2026-02-19T12:00:00Z".to_string()),
        tags: vec!["implementation".to_string(), "feature".to_string()],
        inputs: vec![
            FunctionInput {
                name: "feature_name".to_string(),
                input_type: InputType::String,
                description: "Feature name".to_string(),
                required: true,
                default: None,
                example: Some(serde_yaml::Value::String("global-config".to_string())),
                min: None,
                max: None,
                values: None,
            },
            FunctionInput {
                name: "source_dir".to_string(),
                input_type: InputType::String,
                description: "Source directory".to_string(),
                required: false,
                default: Some(serde_yaml::Value::String(
                    "/home/user/project/src".to_string(),
                )),
                example: Some(serde_yaml::Value::String("src/lib.rs".to_string())),
                min: None,
                max: None,
                values: None,
            },
        ],
        tasks: vec![
            TaskTemplate {
                template_id: "plan".to_string(),
                title: "Plan {{input.feature_name}}".to_string(),
                description: "Design the implementation".to_string(),
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
                description: "Build it".to_string(),
                skills: vec!["implementation".to_string()],
                after: vec!["plan".to_string()],
                loops_to: vec![],
                role_hint: Some("programmer".to_string()),
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
        visibility: vis,
        redacted_fields: vec!["extracted_by".to_string()],
    }
}

/// Build a v2 generative function.
fn sample_v2_generative() -> TraceFunction {
    TraceFunction {
        kind: "trace-function".to_string(),
        version: 2,
        id: "gen-api".to_string(),
        name: "Implement API".to_string(),
        description: "Read API spec, plan, implement, validate".to_string(),
        extracted_from: vec![],
        extracted_by: Some("scout".to_string()),
        extracted_at: Some("2026-02-19T12:00:00Z".to_string()),
        tags: vec!["api".to_string()],
        inputs: vec![FunctionInput {
            name: "api_name".to_string(),
            input_type: InputType::String,
            description: "API name".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        }],
        tasks: vec![
            TaskTemplate {
                template_id: "implement".to_string(),
                title: "Implement API".to_string(),
                description: "Fallback implementation".to_string(),
                skills: vec!["implementation".to_string()],
                after: vec![],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec!["implement".to_string()],
            },
            TaskTemplate {
                template_id: "test".to_string(),
                title: "Test API".to_string(),
                description: "Fallback testing".to_string(),
                skills: vec!["testing".to_string()],
                after: vec!["implement".to_string()],
                loops_to: vec![],
                role_hint: None,
                deliverables: vec![],
                verify: None,
                tags: vec!["test".to_string()],
            },
        ],
        outputs: vec![],
        planning: Some(PlanningConfig {
            planner_template: TaskTemplate {
                template_id: "plan-api".to_string(),
                title: "Plan API implementation".to_string(),
                description: "Read the spec and produce a task plan.".to_string(),
                skills: vec!["analysis".to_string(), "api-design".to_string()],
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
            max_tasks: Some(20),
            required_skills: vec!["implementation".to_string(), "testing".to_string()],
            max_depth: Some(4),
            allow_cycles: false,
            max_total_iterations: None,
            required_phases: vec!["implement".to_string(), "test".to_string()],
            forbidden_patterns: vec![ForbiddenPattern {
                tags: vec!["untested".to_string(), "production".to_string()],
                reason: "Cannot deploy untested code".to_string(),
            }],
        }),
        memory: None,
        visibility: FunctionVisibility::Peer,
        redacted_fields: vec!["extracted_by".to_string()],
    }
}

/// Build a v3 adaptive function.
fn sample_v3_adaptive() -> TraceFunction {
    let mut func = sample_v2_generative();
    func.version = 3;
    func.id = "adaptive-deploy".to_string();
    func.name = "Adaptive Deploy".to_string();
    func.description = "Deploy with memory of past runs".to_string();
    func.visibility = FunctionVisibility::Internal;
    func.redacted_fields = vec![];
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
    // Add memory placeholder to planner description
    if let Some(ref mut planning) = func.planning {
        planning.planner_template.description =
            "Plan the deployment.\n\nPast runs:\n{{memory.run_summaries}}".to_string();
    }
    func
}

// ===========================================================================
// Layer 1: Static function tests
// ===========================================================================

#[test]
fn layer1_yaml_round_trip_with_visibility_and_redacted_fields() {
    let func = sample_v1_with_visibility(FunctionVisibility::Peer);

    let yaml = serde_yaml::to_string(&func).unwrap();
    let loaded: TraceFunction = serde_yaml::from_str(&yaml).unwrap();

    assert_eq!(loaded.id, "vis-func");
    assert_eq!(loaded.visibility, FunctionVisibility::Peer);
    assert_eq!(loaded.redacted_fields, vec!["extracted_by"]);
    assert_eq!(loaded.tasks.len(), 2);
    assert_eq!(loaded.inputs.len(), 2);
    assert_eq!(loaded.extracted_by, Some("scout-abc123".to_string()));
    assert_eq!(loaded.extracted_from[0].run_id, Some("run-007".to_string()));
}

#[test]
fn layer1_save_load_preserves_visibility_fields() {
    let tmp = TempDir::new().unwrap();
    let func = sample_v1_with_visibility(FunctionVisibility::Public);

    let path = trace_function::save_function(&func, tmp.path()).unwrap();
    let loaded = trace_function::load_function(&path).unwrap();

    assert_eq!(loaded.visibility, FunctionVisibility::Public);
    assert_eq!(loaded.redacted_fields, vec!["extracted_by"]);
    assert_eq!(loaded.tags, vec!["implementation", "feature"]);
}

#[test]
fn layer1_instantiate_v1_with_visibility_creates_tasks() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);

    let func = sample_v1_with_visibility(FunctionVisibility::Internal);
    setup_function(dir, &func);

    // Use CLI-style instantiation via the library
    let graph_file = dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_file).unwrap();

    // Simulate instantiation: substitute templates & create tasks
    let mut inputs = HashMap::new();
    inputs.insert(
        "feature_name".to_string(),
        serde_yaml::Value::String("auth".to_string()),
    );
    let resolved = trace_function::validate_inputs(&func.inputs, &inputs).unwrap();

    for template in &func.tasks {
        let rendered = trace_function::substitute_task_template(template, &resolved);
        let task_id = format!("auth-{}", template.template_id);

        let mut real_after = vec![];
        for dep in &template.after {
            real_after.push(format!("auth-{}", dep));
        }

        let task = Task {
            id: task_id.clone(),
            title: rendered.title.clone(),
            description: Some(rendered.description.clone()),
            status: Status::Open,
            after: real_after,
            skills: rendered.skills.clone(),
            ..Task::default()
        };
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &graph_file).unwrap();

    // Verify tasks created
    let graph = load_graph(&graph_file).unwrap();
    let plan = graph.get_task("auth-plan").unwrap();
    assert_eq!(plan.title, "Plan auth");
    assert_eq!(plan.status, Status::Open);
    assert!(plan.after.is_empty());

    let implement = graph.get_task("auth-implement").unwrap();
    assert_eq!(implement.title, "Implement auth");
    assert_eq!(implement.after, vec!["auth-plan"]);
}

#[test]
fn layer1_v1_yaml_omits_layer2_layer3_fields() {
    let func = sample_v1_with_visibility(FunctionVisibility::Internal);
    let yaml = serde_yaml::to_string(&func).unwrap();

    assert!(!yaml.contains("planning:"), "v1 should not have planning");
    assert!(
        !yaml.contains("constraints:"),
        "v1 should not have constraints"
    );
    assert!(!yaml.contains("memory:"), "v1 should not have memory");
}

#[test]
fn layer1_blocked_by_alias_deserialized_as_after() {
    let yaml = r#"
kind: trace-function
version: 1
id: alias-test
name: "Alias Test"
description: "Test blocked_by → after alias"
tasks:
  - template_id: a
    title: A
    description: First
  - template_id: b
    title: B
    description: Second
    blocked_by: [a]
"#;
    let func: TraceFunction = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(func.tasks[1].after, vec!["a"]);
}

// ===========================================================================
// Layer 2: Generative function tests
// ===========================================================================

#[test]
fn layer2_v2_yaml_round_trip() {
    let func = sample_v2_generative();

    let yaml = serde_yaml::to_string(&func).unwrap();
    let loaded: TraceFunction = serde_yaml::from_str(&yaml).unwrap();

    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.visibility, FunctionVisibility::Peer);

    let planning = loaded.planning.unwrap();
    assert_eq!(planning.planner_template.template_id, "plan-api");
    assert_eq!(planning.output_format, "workgraph-yaml");
    assert!(planning.static_fallback);
    assert!(planning.validate_plan);

    let constraints = loaded.constraints.unwrap();
    assert_eq!(constraints.min_tasks, Some(2));
    assert_eq!(constraints.max_tasks, Some(20));
    assert_eq!(
        constraints.required_skills,
        vec!["implementation", "testing"]
    );
    assert_eq!(constraints.max_depth, Some(4));
    assert!(!constraints.allow_cycles);
    assert_eq!(
        constraints.required_phases,
        vec!["implement", "test"]
    );
    assert_eq!(constraints.forbidden_patterns.len(), 1);
    assert!(loaded.memory.is_none());
}

#[test]
fn layer2_validate_plan_passes_valid_plan() {
    let mut impl_task = make_template("impl");
    impl_task.skills = vec!["implementation".to_string()];
    impl_task.tags = vec!["implement".to_string()];

    let mut test_task = make_template("test");
    test_task.skills = vec!["testing".to_string()];
    test_task.tags = vec!["test".to_string()];
    test_task.after = vec!["impl".to_string()];

    let tasks = vec![impl_task, test_task];
    let constraints = StructuralConstraints {
        min_tasks: Some(2),
        max_tasks: Some(10),
        required_skills: vec!["implementation".to_string(), "testing".to_string()],
        required_phases: vec!["implement".to_string(), "test".to_string()],
        max_depth: Some(3),
        ..empty_constraints()
    };

    assert!(plan_validator::validate_plan(&tasks, &constraints).is_ok());
}

#[test]
fn layer2_validate_plan_too_few_tasks() {
    let tasks = vec![make_template("a")];
    let constraints = StructuralConstraints {
        min_tasks: Some(3),
        ..empty_constraints()
    };

    let errs = plan_validator::validate_plan(&tasks, &constraints).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
        e,
        ValidationError::TooFewTasks { count: 1, min: 3 }
    )));
}

#[test]
fn layer2_validate_plan_too_many_tasks() {
    let tasks = vec![
        make_template("a"),
        make_template("b"),
        make_template("c"),
        make_template("d"),
    ];
    let constraints = StructuralConstraints {
        max_tasks: Some(2),
        ..empty_constraints()
    };

    let errs = plan_validator::validate_plan(&tasks, &constraints).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
        e,
        ValidationError::TooManyTasks { count: 4, max: 2 }
    )));
}

#[test]
fn layer2_validate_plan_missing_skills() {
    let mut t = make_template("a");
    t.skills = vec!["rust".to_string()];
    let tasks = vec![t];

    let constraints = StructuralConstraints {
        required_skills: vec!["rust".to_string(), "python".to_string()],
        ..empty_constraints()
    };

    let errs = plan_validator::validate_plan(&tasks, &constraints).unwrap_err();
    assert_eq!(errs.len(), 1);
    assert!(matches!(&errs[0], ValidationError::MissingSkill(s) if s == "python"));
}

#[test]
fn layer2_validate_plan_forbidden_pattern_detected() {
    let mut t = make_template("deploy-prod");
    t.tags = vec![
        "untested".to_string(),
        "production".to_string(),
    ];
    let tasks = vec![t, make_template("other")];

    let constraints = StructuralConstraints {
        forbidden_patterns: vec![ForbiddenPattern {
            tags: vec!["untested".to_string(), "production".to_string()],
            reason: "Cannot deploy untested code".to_string(),
        }],
        ..empty_constraints()
    };

    let errs = plan_validator::validate_plan(&tasks, &constraints).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
        e,
        ValidationError::ForbiddenPatternFound { reason, .. }
        if reason == "Cannot deploy untested code"
    )));
}

#[test]
fn layer2_validate_plan_forbidden_pattern_partial_match_ok() {
    let mut t = make_template("deploy");
    t.tags = vec!["production".to_string()]; // Missing "untested" → not a match

    let constraints = StructuralConstraints {
        forbidden_patterns: vec![ForbiddenPattern {
            tags: vec!["untested".to_string(), "production".to_string()],
            reason: "Cannot deploy untested code".to_string(),
        }],
        ..empty_constraints()
    };

    assert!(plan_validator::validate_plan(&[t], &constraints).is_ok());
}

#[test]
fn layer2_validate_plan_depth_exceeded() {
    // a → b → c → d  (depth 3)
    let a = make_template("a");
    let mut b = make_template("b");
    b.after = vec!["a".to_string()];
    let mut c = make_template("c");
    c.after = vec!["b".to_string()];
    let mut d = make_template("d");
    d.after = vec!["c".to_string()];

    let constraints = StructuralConstraints {
        max_depth: Some(2),
        ..empty_constraints()
    };

    let errs = plan_validator::validate_plan(&[a, b, c, d], &constraints).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
        e,
        ValidationError::DepthExceeded { depth: 3, max: 2 }
    )));
}

#[test]
fn layer2_validate_plan_cycles_not_allowed() {
    let mut a = make_template("a");
    a.after = vec!["b".to_string()];
    let mut b = make_template("b");
    b.after = vec!["a".to_string()];

    let constraints = StructuralConstraints {
        allow_cycles: false,
        ..empty_constraints()
    };

    let errs = plan_validator::validate_plan(&[a, b], &constraints).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| matches!(e, ValidationError::CyclesNotAllowed { .. })));
}

#[test]
fn layer2_validate_plan_multiple_errors_collected() {
    let tasks = vec![make_template("a")];
    let constraints = StructuralConstraints {
        min_tasks: Some(3),
        required_skills: vec!["rust".to_string()],
        required_phases: vec!["test".to_string()],
        ..empty_constraints()
    };

    let errs = plan_validator::validate_plan(&tasks, &constraints).unwrap_err();
    assert_eq!(errs.len(), 3, "Should collect all 3 violations: {:?}", errs);
}

#[test]
fn layer2_validate_plan_max_total_iterations_exceeded() {
    let mut a = make_template("a");
    a.loops_to = vec![LoopEdgeTemplate {
        target: "b".to_string(),
        max_iterations: 5,
        guard: None,
        delay: None,
    }];
    let mut b = make_template("b");
    b.loops_to = vec![LoopEdgeTemplate {
        target: "a".to_string(),
        max_iterations: 4,
        guard: None,
        delay: None,
    }];

    let constraints = StructuralConstraints {
        allow_cycles: true,
        max_total_iterations: Some(8),
        ..empty_constraints()
    };

    let errs = plan_validator::validate_plan(&[a, b], &constraints).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
        e,
        ValidationError::TooManyCycleIterations { total: 9, max: 8 }
    )));
}

#[test]
fn layer2_v2_function_validation_passes() {
    let func = sample_v2_generative();
    trace_function::validate_function(&func).unwrap();
}

#[test]
fn layer2_planning_config_defaults() {
    let yaml = r#"
planner_template:
  template_id: plan
  title: "Plan"
  description: "Plan it"
"#;
    let config: PlanningConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.output_format, "workgraph-yaml");
    assert!(!config.static_fallback);
    assert!(config.validate_plan);
}

#[test]
fn layer2_constraints_defaults() {
    let yaml = "{}";
    let c: StructuralConstraints = serde_yaml::from_str(yaml).unwrap();
    assert!(c.min_tasks.is_none());
    assert!(c.max_tasks.is_none());
    assert!(c.required_skills.is_empty());
    assert!(c.max_depth.is_none());
    assert!(!c.allow_cycles);
    assert!(c.max_total_iterations.is_none());
    assert!(c.required_phases.is_empty());
    assert!(c.forbidden_patterns.is_empty());
}

// ===========================================================================
// Layer 3: Adaptive function tests
// ===========================================================================

#[test]
fn layer3_save_and_load_run_summary_via_per_run_json() {
    let tmp = TempDir::new().unwrap();
    let summary = sample_run_summary();

    let path = trace_memory::save_run_summary("my-func", &summary, tmp.path()).unwrap();
    assert!(path.exists());
    assert!(path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .ends_with(".json"));

    let loaded = trace_memory::load_recent_summaries("my-func", 10, tmp.path()).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].prefix, "auth");
    assert!(loaded[0].all_succeeded);
    assert_eq!(loaded[0].task_outcomes.len(), 2);
    assert_eq!(loaded[0].interventions.len(), 1);
    assert_eq!(loaded[0].avg_score, Some(0.875));
}

#[test]
fn layer3_save_load_jsonl_round_trip() {
    let tmp = TempDir::new().unwrap();
    let summary = sample_run_summary();

    trace_memory::append_run_summary(tmp.path(), "test-func", &summary).unwrap();

    let config = TraceMemoryConfig {
        max_runs: 10,
        include: default_inclusions(),
        storage_path: None,
    };
    let loaded = trace_memory::load_run_summaries(tmp.path(), "test-func", &config);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].prefix, "auth");
}

#[test]
fn layer3_load_recent_summaries_sorted_newest_first() {
    let tmp = TempDir::new().unwrap();

    let mut s1 = sample_run_summary();
    s1.instantiated_at = "2026-02-18T10:00:00Z".to_string();
    trace_memory::save_run_summary("func-a", &s1, tmp.path()).unwrap();

    let mut s2 = sample_run_summary();
    s2.instantiated_at = "2026-02-20T12:00:00Z".to_string();
    trace_memory::save_run_summary("func-a", &s2, tmp.path()).unwrap();

    let mut s3 = sample_run_summary();
    s3.instantiated_at = "2026-02-19T08:00:00Z".to_string();
    trace_memory::save_run_summary("func-a", &s3, tmp.path()).unwrap();

    let loaded = trace_memory::load_recent_summaries("func-a", 10, tmp.path()).unwrap();
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[0].instantiated_at, "2026-02-20T12:00:00Z");
    assert_eq!(loaded[1].instantiated_at, "2026-02-19T08:00:00Z");
    assert_eq!(loaded[2].instantiated_at, "2026-02-18T10:00:00Z");
}

#[test]
fn layer3_load_recent_respects_max_runs() {
    let tmp = TempDir::new().unwrap();
    for i in 0..5u32 {
        let mut s = sample_run_summary();
        s.instantiated_at = format!("2026-02-{:02}T12:00:00Z", 15 + i);
        trace_memory::save_run_summary("func-b", &s, tmp.path()).unwrap();
    }

    let loaded = trace_memory::load_recent_summaries("func-b", 2, tmp.path()).unwrap();
    assert_eq!(loaded.len(), 2);
    // Should be the two newest
    assert_eq!(loaded[0].instantiated_at, "2026-02-19T12:00:00Z");
    assert_eq!(loaded[1].instantiated_at, "2026-02-18T12:00:00Z");
}

#[test]
fn layer3_jsonl_respects_max_runs() {
    let tmp = TempDir::new().unwrap();
    let summary = sample_run_summary();
    for _ in 0..5 {
        trace_memory::append_run_summary(tmp.path(), "test-func", &summary).unwrap();
    }

    let config = TraceMemoryConfig {
        max_runs: 2,
        include: default_inclusions(),
        storage_path: None,
    };
    let loaded = trace_memory::load_run_summaries(tmp.path(), "test-func", &config);
    assert_eq!(loaded.len(), 2);
}

#[test]
fn layer3_render_summaries_text_empty() {
    let text = trace_memory::render_summaries_text(&[]);
    assert_eq!(text, "No previous runs recorded.");
}

#[test]
fn layer3_render_summaries_text_single_run() {
    let summary = sample_run_summary();
    let text = trace_memory::render_summaries_text(&[summary]);

    assert!(text.contains("Past Runs (1 total)"));
    assert!(text.contains("Run 1"));
    assert!(text.contains("2026-02-20T12:00:00Z"));
    assert!(text.contains("[SUCCESS]"));
    assert!(text.contains("Avg score: 0.88"));
    assert!(text.contains("plan (Done)"));
    assert!(text.contains("implement (Done)"));
    assert!(text.contains("score=0.85"));
    assert!(text.contains("retries=1"));
    assert!(text.contains("retry on auth-implement: Flaky test"));
}

#[test]
fn layer3_render_summaries_text_multiple_runs() {
    let s1 = sample_run_summary();
    let mut s2 = sample_run_summary();
    s2.instantiated_at = "2026-02-19T08:00:00Z".to_string();
    s2.all_succeeded = false;
    s2.task_outcomes[1].status = "Failed".to_string();

    let text = trace_memory::render_summaries_text(&[s1, s2]);
    assert!(text.contains("Past Runs (2 total)"));
    assert!(text.contains("Run 1"));
    assert!(text.contains("Run 2"));
    assert!(text.contains("[SUCCESS]"));
    assert!(text.contains("[ISSUES]"));
}

#[test]
fn layer3_render_run_summaries_config_aware() {
    let summary = sample_run_summary();

    // With all inclusions
    let text = trace_memory::render_run_summaries(&[summary.clone()], &default_inclusions());
    assert!(text.contains("SUCCESS"));
    assert!(text.contains("2/2 succeeded"));
    assert!(text.contains("0.88"));
    assert!(text.contains("retry"));

    // With no inclusions
    let no_include = MemoryInclusions {
        outcomes: false,
        scores: false,
        interventions: false,
        duration: false,
        retries: false,
        artifacts: false,
    };
    let text = trace_memory::render_run_summaries(&[summary], &no_include);
    assert!(!text.contains("SUCCESS"));
    assert!(!text.contains("Avg Score"));
    assert!(!text.contains("Duration"));
    assert!(!text.contains("Interventions"));
}

#[test]
fn layer3_render_run_summaries_with_retries() {
    let summary = sample_run_summary();
    let inclusions = MemoryInclusions {
        retries: true,
        ..default_inclusions()
    };
    let text = trace_memory::render_run_summaries(&[summary], &inclusions);
    assert!(text.contains("Total Retries: 1"));
}

#[test]
fn layer3_memory_injection_into_template_substitution() {
    // Simulate what trace_instantiate does for v3 functions:
    // 1. Load run summaries → render to text
    // 2. Substitute inputs in template
    // 3. Replace {{memory.run_summaries}} with rendered text

    let summaries = vec![sample_run_summary()];
    let memory_text = trace_memory::render_summaries_text(&summaries);

    let mut inputs = HashMap::new();
    inputs.insert(
        "feature_name".to_string(),
        serde_yaml::Value::String("auth".to_string()),
    );

    let template = "Plan {{input.feature_name}}\n\nPast runs:\n{{memory.run_summaries}}";
    let after_input_sub = trace_function::substitute(template, &inputs);
    let final_text = after_input_sub.replace("{{memory.run_summaries}}", &memory_text);

    assert!(final_text.contains("Plan auth"));
    assert!(final_text.contains("Past Runs (1 total)"));
    assert!(final_text.contains("[SUCCESS]"));
    assert!(!final_text.contains("{{memory.run_summaries}}"));
    assert!(!final_text.contains("{{input.feature_name}}"));
}

#[test]
fn layer3_v3_yaml_round_trip() {
    let func = sample_v3_adaptive();

    let yaml = serde_yaml::to_string(&func).unwrap();
    let loaded: TraceFunction = serde_yaml::from_str(&yaml).unwrap();

    assert_eq!(loaded.version, 3);
    assert!(loaded.memory.is_some());

    let mem = loaded.memory.unwrap();
    assert_eq!(mem.max_runs, 5);
    assert!(mem.include.outcomes);
    assert!(mem.include.scores);
    assert!(mem.include.interventions);
    assert!(mem.include.duration);
    assert!(!mem.include.retries);
    assert!(!mem.include.artifacts);
    assert!(mem.storage_path.is_none());

    assert!(loaded.planning.is_some());
    assert!(loaded.constraints.is_some());
}

#[test]
fn layer3_memory_dir_path() {
    let wg = Path::new("/tmp/.workgraph");
    let dir = trace_memory::memory_dir(wg, "deploy-prod");
    assert_eq!(
        dir.to_str().unwrap(),
        "/tmp/.workgraph/functions/deploy-prod.memory"
    );
}

#[test]
fn layer3_load_empty_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let loaded =
        trace_memory::load_recent_summaries("nonexistent", 10, tmp.path()).unwrap();
    assert!(loaded.is_empty());

    let config = TraceMemoryConfig {
        max_runs: 10,
        include: default_inclusions(),
        storage_path: None,
    };
    let jsonl_loaded = trace_memory::load_run_summaries(tmp.path(), "nonexistent", &config);
    assert!(jsonl_loaded.is_empty());
}

#[test]
fn layer3_build_run_summary_from_graph() {
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(Task {
        id: "pfx/build".to_string(),
        title: "Build".to_string(),
        status: Status::Done,
        started_at: Some("2026-02-20T12:00:00Z".to_string()),
        completed_at: Some("2026-02-20T12:02:00Z".to_string()),
        retry_count: 0,
        ..Task::default()
    }));
    graph.add_node(Node::Task(Task {
        id: "pfx/test".to_string(),
        title: "Test".to_string(),
        status: Status::Failed,
        started_at: Some("2026-02-20T12:02:00Z".to_string()),
        completed_at: Some("2026-02-20T12:03:00Z".to_string()),
        retry_count: 1,
        ..Task::default()
    }));

    let tmp = TempDir::new().unwrap();
    let eval_dir = tmp.path().join("evaluations");
    std::fs::create_dir_all(&eval_dir).unwrap();

    let task_ids = vec!["pfx/build".to_string(), "pfx/test".to_string()];
    let summary = trace_memory::build_run_summary(
        &task_ids,
        &graph,
        &eval_dir,
        tmp.path(),
        "2026-02-20T12:00:00Z",
        "pfx/",
    )
    .unwrap();

    assert_eq!(summary.prefix, "pfx/");
    assert!(!summary.all_succeeded);
    assert_eq!(summary.task_outcomes.len(), 2);
    assert_eq!(summary.task_outcomes[0].template_id, "build");
    assert_eq!(summary.task_outcomes[0].status, "Done");
    assert_eq!(summary.task_outcomes[0].duration_secs, Some(120));
    assert_eq!(summary.task_outcomes[1].template_id, "test");
    assert_eq!(summary.task_outcomes[1].status, "Failed");
    assert_eq!(summary.task_outcomes[1].retry_count, 1);
    assert_eq!(summary.wall_clock_secs, Some(180));
}

// ===========================================================================
// Visibility tests
// ===========================================================================

#[test]
fn visibility_ordering() {
    assert!(FunctionVisibility::Internal < FunctionVisibility::Peer);
    assert!(FunctionVisibility::Peer < FunctionVisibility::Public);
    assert!(FunctionVisibility::Internal < FunctionVisibility::Public);
}

#[test]
fn visibility_default_is_internal() {
    assert_eq!(FunctionVisibility::default(), FunctionVisibility::Internal);
}

#[test]
fn visibility_serde_kebab_case() {
    let yaml_internal = "\"internal\"";
    assert_eq!(
        serde_yaml::from_str::<FunctionVisibility>(yaml_internal).unwrap(),
        FunctionVisibility::Internal,
    );
    let yaml_peer = "\"peer\"";
    assert_eq!(
        serde_yaml::from_str::<FunctionVisibility>(yaml_peer).unwrap(),
        FunctionVisibility::Peer,
    );
    let yaml_public = "\"public\"";
    assert_eq!(
        serde_yaml::from_str::<FunctionVisibility>(yaml_public).unwrap(),
        FunctionVisibility::Public,
    );

    let serialized = serde_yaml::to_string(&FunctionVisibility::Peer).unwrap();
    assert_eq!(serialized.trim(), "peer");
}

#[test]
fn export_internal_at_internal_no_redaction() {
    let func = sample_v1_with_visibility(FunctionVisibility::Internal);
    let exported =
        trace_function::export_function(&func, &FunctionVisibility::Internal).unwrap();

    assert_eq!(exported.extracted_by, Some("scout-abc123".to_string()));
    assert_eq!(
        exported.extracted_at,
        Some("2026-02-19T12:00:00Z".to_string())
    );
    assert_eq!(
        exported.extracted_from[0].run_id,
        Some("run-007".to_string())
    );
}

#[test]
fn export_internal_at_peer_fails() {
    let func = sample_v1_with_visibility(FunctionVisibility::Internal);
    let err = trace_function::export_function(&func, &FunctionVisibility::Peer).unwrap_err();
    match err {
        trace_function::TraceFunctionError::Validation(msg) => {
            assert!(msg.contains("internal"));
            assert!(msg.contains("peer"));
        }
        _ => panic!("Expected Validation error"),
    }
}

#[test]
fn export_internal_at_public_fails() {
    let func = sample_v1_with_visibility(FunctionVisibility::Internal);
    assert!(trace_function::export_function(&func, &FunctionVisibility::Public).is_err());
}

#[test]
fn export_peer_at_peer_redacts_correctly() {
    let func = sample_v1_with_visibility(FunctionVisibility::Peer);
    let exported = trace_function::export_function(&func, &FunctionVisibility::Peer).unwrap();

    // extracted_by generalized to "agent", but then redacted_fields contains "extracted_by"
    // so it gets stripped to None
    assert!(
        exported.extracted_by.is_none(),
        "extracted_by in redacted_fields should be stripped"
    );

    // extracted_from: run_id stripped, timestamp kept
    assert!(exported.extracted_from[0].run_id.is_none());
    assert_eq!(
        exported.extracted_from[0].timestamp,
        "2026-02-18T14:30:00Z"
    );
    assert_eq!(exported.extracted_from[0].task_id, "impl-config");
}

#[test]
fn export_peer_at_internal_no_redaction() {
    let func = sample_v1_with_visibility(FunctionVisibility::Peer);
    let exported =
        trace_function::export_function(&func, &FunctionVisibility::Internal).unwrap();

    assert_eq!(exported.extracted_by, Some("scout-abc123".to_string()));
    assert!(exported.extracted_from[0].run_id.is_some());
}

#[test]
fn export_peer_at_public_fails() {
    let func = sample_v1_with_visibility(FunctionVisibility::Peer);
    assert!(trace_function::export_function(&func, &FunctionVisibility::Public).is_err());
}

#[test]
fn export_public_at_public_strips_provenance() {
    let mut func = sample_v1_with_visibility(FunctionVisibility::Public);
    func.memory = Some(TraceMemoryConfig {
        max_runs: 10,
        include: default_inclusions(),
        storage_path: Some("/secret/path".to_string()),
    });

    let exported =
        trace_function::export_function(&func, &FunctionVisibility::Public).unwrap();

    // extracted_by and extracted_at stripped
    assert!(exported.extracted_by.is_none());
    assert!(exported.extracted_at.is_none());

    // extracted_from: timestamp cleared, run_id stripped
    assert!(exported.extracted_from[0].run_id.is_none());
    assert!(exported.extracted_from[0].timestamp.is_empty());
    assert_eq!(exported.extracted_from[0].task_id, "impl-config");

    // Memory stripped entirely
    assert!(exported.memory.is_none());

    // Path-like defaults stripped from inputs
    // source_dir default is "/home/user/project/src" → path-like → stripped
    assert!(exported.inputs[1].default.is_none());
    // source_dir example is "src/lib.rs" → contains '/' → path-like → stripped
    assert!(exported.inputs[1].example.is_none());

    // redacted_fields cleared for public
    assert!(exported.redacted_fields.is_empty());
}

#[test]
fn export_public_at_peer_applies_peer_redaction() {
    let func = sample_v1_with_visibility(FunctionVisibility::Public);
    let exported = trace_function::export_function(&func, &FunctionVisibility::Peer).unwrap();

    // Peer redaction: extracted_by generalized then stripped by redacted_fields
    assert!(exported.extracted_by.is_none());
    // run_id stripped
    assert!(exported.extracted_from[0].run_id.is_none());
    // timestamp preserved at peer level
    assert_eq!(
        exported.extracted_from[0].timestamp,
        "2026-02-18T14:30:00Z"
    );
}

#[test]
fn export_public_preserves_non_path_defaults() {
    let mut func = sample_v1_with_visibility(FunctionVisibility::Public);
    func.inputs[1].default = Some(serde_yaml::Value::String("cargo test".to_string()));
    func.inputs[1].example = Some(serde_yaml::Value::String("my-feature".to_string()));

    let exported =
        trace_function::export_function(&func, &FunctionVisibility::Public).unwrap();
    assert_eq!(
        exported.inputs[1].default,
        Some(serde_yaml::Value::String("cargo test".to_string()))
    );
    assert_eq!(
        exported.inputs[1].example,
        Some(serde_yaml::Value::String("my-feature".to_string()))
    );
}

#[test]
fn function_visible_at_levels() {
    let internal_fn = sample_v1_with_visibility(FunctionVisibility::Internal);
    assert!(trace_function::function_visible_at(
        &internal_fn,
        &FunctionVisibility::Internal
    ));
    assert!(!trace_function::function_visible_at(
        &internal_fn,
        &FunctionVisibility::Peer
    ));
    assert!(!trace_function::function_visible_at(
        &internal_fn,
        &FunctionVisibility::Public
    ));

    let peer_fn = sample_v1_with_visibility(FunctionVisibility::Peer);
    assert!(trace_function::function_visible_at(
        &peer_fn,
        &FunctionVisibility::Internal
    ));
    assert!(trace_function::function_visible_at(
        &peer_fn,
        &FunctionVisibility::Peer
    ));
    assert!(!trace_function::function_visible_at(
        &peer_fn,
        &FunctionVisibility::Public
    ));

    let public_fn = sample_v1_with_visibility(FunctionVisibility::Public);
    assert!(trace_function::function_visible_at(
        &public_fn,
        &FunctionVisibility::Internal
    ));
    assert!(trace_function::function_visible_at(
        &public_fn,
        &FunctionVisibility::Peer
    ));
    assert!(trace_function::function_visible_at(
        &public_fn,
        &FunctionVisibility::Public
    ));
}

#[test]
fn export_v2_function_peer_with_memory() {
    let mut func = sample_v2_generative();
    func.memory = Some(TraceMemoryConfig {
        max_runs: 5,
        include: default_inclusions(),
        storage_path: Some("/secret/path/runs.jsonl".to_string()),
    });

    let exported = trace_function::export_function(&func, &FunctionVisibility::Peer).unwrap();

    // Memory config retained at peer level but storage_path stripped
    let mem = exported.memory.unwrap();
    assert!(mem.storage_path.is_none());
    assert_eq!(mem.max_runs, 5);
    assert!(mem.include.outcomes);
}

#[test]
fn export_peer_redacted_fields_strips_tags() {
    let mut func = sample_v1_with_visibility(FunctionVisibility::Peer);
    func.redacted_fields = vec![
        "extracted_by".to_string(),
        "tags".to_string(),
    ];

    let exported = trace_function::export_function(&func, &FunctionVisibility::Peer).unwrap();
    assert!(exported.extracted_by.is_none());
    assert!(exported.tags.is_empty());
}

// ===========================================================================
// Cross-layer integration tests
// ===========================================================================

#[test]
fn cross_layer_v1_to_v2_to_v3_serialization() {
    // Verify version progression serializes correctly
    let v1 = sample_v1_with_visibility(FunctionVisibility::Internal);
    let yaml1 = serde_yaml::to_string(&v1).unwrap();
    assert!(!yaml1.contains("planning:"));
    assert!(!yaml1.contains("memory:"));

    let v2 = sample_v2_generative();
    let yaml2 = serde_yaml::to_string(&v2).unwrap();
    assert!(yaml2.contains("planning:"));
    assert!(yaml2.contains("constraints:"));
    assert!(!yaml2.contains("memory:"));

    let v3 = sample_v3_adaptive();
    let yaml3 = serde_yaml::to_string(&v3).unwrap();
    assert!(yaml3.contains("planning:"));
    assert!(yaml3.contains("constraints:"));
    assert!(yaml3.contains("memory:"));
}

#[test]
fn cross_layer_v3_function_with_runs_full_cycle() {
    // Full cycle: save function → append run summaries → load → render → inject
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    setup_workgraph(dir);

    let func = sample_v3_adaptive();
    setup_function(dir, &func);

    // Write some past run summaries
    let mut s1 = sample_run_summary();
    s1.instantiated_at = "2026-02-18T10:00:00Z".to_string();
    s1.all_succeeded = false;
    s1.task_outcomes[1].status = "Failed".to_string();
    trace_memory::append_run_summary(dir, &func.id, &s1).unwrap();

    let mut s2 = sample_run_summary();
    s2.instantiated_at = "2026-02-19T10:00:00Z".to_string();
    trace_memory::append_run_summary(dir, &func.id, &s2).unwrap();

    // Load run summaries through the config
    let config = func.memory.as_ref().unwrap();
    let summaries = trace_memory::load_run_summaries(dir, &func.id, config);
    assert_eq!(summaries.len(), 2);

    // Render and inject
    let memory_text = trace_memory::render_run_summaries(&summaries, &config.include);
    assert!(memory_text.contains("2 Previous Run"));
    assert!(memory_text.contains("PARTIAL")); // s1 failed
    assert!(memory_text.contains("SUCCESS")); // s2 succeeded

    // Simulate template substitution with memory injection
    let mut inputs = HashMap::new();
    inputs.insert(
        "api_name".to_string(),
        serde_yaml::Value::String("users".to_string()),
    );

    if let Some(ref planning) = func.planning {
        let rendered = trace_function::substitute(&planning.planner_template.description, &inputs);
        let final_desc = rendered.replace("{{memory.run_summaries}}", &memory_text);

        assert!(final_desc.contains("Plan the deployment"));
        assert!(final_desc.contains("2 Previous Run"));
        assert!(!final_desc.contains("{{memory.run_summaries}}"));
    } else {
        panic!("Expected planning config");
    }
}

#[test]
fn cross_layer_validate_generated_plan_against_v2_constraints() {
    let func = sample_v2_generative();
    let constraints = func.constraints.as_ref().unwrap();

    // A plan that satisfies all constraints
    let mut impl_task = make_template("impl");
    impl_task.skills = vec!["implementation".to_string()];
    impl_task.tags = vec!["implement".to_string()];

    let mut test_task = make_template("test");
    test_task.skills = vec!["testing".to_string()];
    test_task.tags = vec!["test".to_string()];
    test_task.after = vec!["impl".to_string()];

    let good_plan = vec![impl_task, test_task];
    assert!(plan_validator::validate_plan(&good_plan, constraints).is_ok());

    // A plan that violates constraints
    let bad_plan = vec![make_template("lonely-task")]; // 1 task < min 2, missing skills/phases
    let errs = plan_validator::validate_plan(&bad_plan, constraints).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, ValidationError::TooFewTasks { .. })));
    assert!(errs.iter().any(|e| matches!(e, ValidationError::MissingSkill(_))));
    assert!(errs.iter().any(|e| matches!(e, ValidationError::MissingPhase(_))));
}

#[test]
fn cross_layer_export_v2_function_with_constraints_peer() {
    let func = sample_v2_generative();
    let exported = trace_function::export_function(&func, &FunctionVisibility::Peer).unwrap();

    // Verify planning and constraints survive export
    assert!(exported.planning.is_some());
    assert!(exported.constraints.is_some());

    let constraints = exported.constraints.unwrap();
    assert_eq!(constraints.min_tasks, Some(2));
    assert_eq!(constraints.max_tasks, Some(20));

    // Redacted fields applied
    assert!(exported.extracted_by.is_none()); // in redacted_fields
}

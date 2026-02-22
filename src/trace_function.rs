use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TraceFunctionError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Ambiguous(String),
    #[error("Validation error: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Core data structures
// ---------------------------------------------------------------------------

/// A parameterized workflow template extracted from completed traces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceFunction {
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extracted_from: Vec<ExtractionSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<FunctionInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskTemplate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<FunctionOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionSource {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionInput {
    pub name: String,
    #[serde(rename = "type")]
    pub input_type: InputType,
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_yaml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<serde_yaml::Value>,
    // Type-specific validation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    String,
    Text,
    FileList,
    FileContent,
    Number,
    Url,
    Enum,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTemplate {
    pub template_id: String,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocked_by")]
    pub after: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops_to: Vec<LoopEdgeTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliverables: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopEdgeTemplate {
    pub target: String,
    pub max_iterations: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionOutput {
    pub name: String,
    pub description: String,
    pub from_task: String,
    pub field: String,
}

// ---------------------------------------------------------------------------
// Storage: load / save / list / find
// ---------------------------------------------------------------------------

/// Directory name under .workgraph/ for trace functions.
pub const FUNCTIONS_DIR: &str = "functions";

/// Load a single trace function from a YAML file.
pub fn load_function(path: &Path) -> Result<TraceFunction, TraceFunctionError> {
    let contents = fs::read_to_string(path)?;
    let func: TraceFunction = serde_yaml::from_str(&contents)?;
    Ok(func)
}

/// Save a trace function as `<id>.yaml` inside the given directory.
pub fn save_function(func: &TraceFunction, dir: &Path) -> Result<PathBuf, TraceFunctionError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.yaml", func.id));
    let yaml = serde_yaml::to_string(func)?;
    fs::write(&path, yaml)?;
    Ok(path)
}

/// Load all trace functions from `*.yaml` files in a directory.
pub fn load_all_functions(dir: &Path) -> Result<Vec<TraceFunction>, TraceFunctionError> {
    let mut functions = Vec::new();
    if !dir.exists() {
        return Ok(functions);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            functions.push(load_function(&path)?);
        }
    }
    functions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(functions)
}

/// Find a trace function by prefix match (like agency entities).
pub fn find_function_by_prefix(
    dir: &Path,
    prefix: &str,
) -> Result<TraceFunction, TraceFunctionError> {
    let all = load_all_functions(dir)?;
    let matches: Vec<&TraceFunction> = all.iter().filter(|f| f.id.starts_with(prefix)).collect();
    match matches.len() {
        0 => Err(TraceFunctionError::NotFound(format!(
            "No function matching '{}'",
            prefix
        ))),
        1 => Ok(matches[0].clone()),
        n => {
            let ids: Vec<&str> = matches.iter().map(|f| f.id.as_str()).collect();
            Err(TraceFunctionError::Ambiguous(format!(
                "Prefix '{}' matches {} functions: {}",
                prefix,
                n,
                ids.join(", ")
            )))
        }
    }
}

/// Return the functions directory for a workgraph directory.
pub fn functions_dir(workgraph_dir: &Path) -> PathBuf {
    workgraph_dir.join(FUNCTIONS_DIR)
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

/// Validate a set of input values against a function's input definitions.
///
/// Returns the resolved input map with defaults applied.
/// Errors on missing required fields, type mismatches, invalid enum values,
/// and out-of-range numbers.
pub fn validate_inputs(
    input_defs: &[FunctionInput],
    provided: &HashMap<String, serde_yaml::Value>,
) -> Result<HashMap<String, serde_yaml::Value>, TraceFunctionError> {
    let mut resolved = HashMap::new();

    for def in input_defs {
        let value = provided.get(&def.name);

        match (value, def.required, &def.default) {
            // Value provided
            (Some(v), _, _) => {
                validate_value(&def.name, v, def)?;
                resolved.insert(def.name.clone(), v.clone());
            }
            // Not provided but has default
            (None, _, Some(default)) => {
                resolved.insert(def.name.clone(), default.clone());
            }
            // Not provided, required, no default
            (None, true, None) => {
                return Err(TraceFunctionError::Validation(format!(
                    "Missing required input '{}'",
                    def.name
                )));
            }
            // Not provided, optional, no default — skip
            (None, false, None) => {}
        }
    }

    Ok(resolved)
}

/// Validate a single value against its input definition.
fn validate_value(
    name: &str,
    value: &serde_yaml::Value,
    def: &FunctionInput,
) -> Result<(), TraceFunctionError> {
    match def.input_type {
        InputType::String | InputType::Text | InputType::Url => {
            if !value.is_string() {
                return Err(TraceFunctionError::Validation(format!(
                    "Input '{}' must be a string, got {:?}",
                    name,
                    value_type_name(value)
                )));
            }
        }
        InputType::Number => {
            let num = match value {
                serde_yaml::Value::Number(n) => n.as_f64(),
                _ => None,
            };
            let num = num.ok_or_else(|| {
                TraceFunctionError::Validation(format!(
                    "Input '{}' must be a number, got {:?}",
                    name,
                    value_type_name(value)
                ))
            })?;
            if let Some(min) = def.min
                && num < min {
                    return Err(TraceFunctionError::Validation(format!(
                        "Input '{}' value {} is below minimum {}",
                        name, num, min
                    )));
                }
            if let Some(max) = def.max
                && num > max {
                    return Err(TraceFunctionError::Validation(format!(
                        "Input '{}' value {} exceeds maximum {}",
                        name, num, max
                    )));
                }
        }
        InputType::FileList => {
            if !value.is_sequence() {
                return Err(TraceFunctionError::Validation(format!(
                    "Input '{}' must be a list, got {:?}",
                    name,
                    value_type_name(value)
                )));
            }
        }
        InputType::FileContent => {
            if !value.is_string() {
                return Err(TraceFunctionError::Validation(format!(
                    "Input '{}' must be a file path (string), got {:?}",
                    name,
                    value_type_name(value)
                )));
            }
        }
        InputType::Enum => {
            let s = value.as_str().ok_or_else(|| {
                TraceFunctionError::Validation(format!(
                    "Input '{}' must be a string for enum type, got {:?}",
                    name,
                    value_type_name(value)
                ))
            })?;
            if let Some(ref allowed) = def.values
                && !allowed.iter().any(|v| v == s) {
                    return Err(TraceFunctionError::Validation(format!(
                        "Input '{}' value '{}' is not one of: {}",
                        name,
                        s,
                        allowed.join(", ")
                    )));
                }
        }
        InputType::Json => {
            // Any YAML value is valid as JSON
        }
    }

    Ok(())
}

fn value_type_name(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "bool",
        serde_yaml::Value::Number(_) => "number",
        serde_yaml::Value::String(_) => "string",
        serde_yaml::Value::Sequence(_) => "list",
        serde_yaml::Value::Mapping(_) => "mapping",
        serde_yaml::Value::Tagged(_) => "tagged",
    }
}

// ---------------------------------------------------------------------------
// Template substitution
// ---------------------------------------------------------------------------

/// Render a value as a string suitable for template substitution.
pub fn render_value(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::Null => String::new(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                f.to_string()
            } else {
                n.to_string()
            }
        }
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Sequence(seq) => {
            // Render list items separated by newlines (file_list style)
            seq.iter().map(render_value).collect::<Vec<_>>().join("\n")
        }
        serde_yaml::Value::Mapping(_) | serde_yaml::Value::Tagged(_) => {
            // Serialize complex values as JSON for readability
            serde_json::to_string(value).unwrap_or_default()
        }
    }
}

/// Apply input values to a template string using `{{input.<name>}}` substitution.
///
/// Matches the existing `TemplateVars::apply()` pattern: simple `str::replace()`.
pub fn substitute(template: &str, inputs: &HashMap<String, serde_yaml::Value>) -> String {
    let mut result = template.to_string();
    for (name, value) in inputs {
        let placeholder = format!("{{{{input.{}}}}}", name);
        result = result.replace(&placeholder, &render_value(value));
    }
    result
}

/// Apply template substitution to an entire TaskTemplate, producing rendered strings.
pub fn substitute_task_template(
    template: &TaskTemplate,
    inputs: &HashMap<String, serde_yaml::Value>,
) -> TaskTemplate {
    TaskTemplate {
        template_id: template.template_id.clone(),
        title: substitute(&template.title, inputs),
        description: substitute(&template.description, inputs),
        skills: template.skills.iter().map(|s| substitute(s, inputs)).collect(),
        after: template.after.clone(),
        loops_to: template.loops_to.clone(),
        role_hint: template.role_hint.clone(),
        deliverables: template
            .deliverables
            .iter()
            .map(|d| substitute(d, inputs))
            .collect(),
        verify: template.verify.as_ref().map(|v| substitute(v, inputs)),
        tags: template.tags.clone(),
    }
}

// ---------------------------------------------------------------------------
// Struct validation (internal consistency of a TraceFunction)
// ---------------------------------------------------------------------------

/// Validate the internal consistency of a trace function definition.
///
/// Checks:
/// - All `after` references resolve to template IDs within the function
/// - All `loops_to` targets resolve to template IDs within the function
/// - No circular `after` dependencies (loops are only via `loops_to`)
/// - Required inputs without defaults, optional inputs noted
pub fn validate_function(func: &TraceFunction) -> Result<(), TraceFunctionError> {
    let template_ids: Vec<&str> = func.tasks.iter().map(|t| t.template_id.as_str()).collect();

    // Check for duplicate template IDs
    let mut seen = std::collections::HashSet::new();
    for id in &template_ids {
        if !seen.insert(id) {
            return Err(TraceFunctionError::Validation(format!(
                "Duplicate template_id '{}'",
                id
            )));
        }
    }

    for task in &func.tasks {
        // Check after references
        for dep in &task.after {
            if !template_ids.contains(&dep.as_str()) {
                return Err(TraceFunctionError::Validation(format!(
                    "Task '{}' has after '{}' which is not a template_id in this function",
                    task.template_id, dep
                )));
            }
        }

        // Check loops_to references
        for loop_edge in &task.loops_to {
            if !template_ids.contains(&loop_edge.target.as_str()) {
                return Err(TraceFunctionError::Validation(format!(
                    "Task '{}' has loops_to target '{}' which is not a template_id in this function",
                    task.template_id, loop_edge.target
                )));
            }
        }
    }

    // Check for circular after (simple cycle detection via DFS)
    for task in &func.tasks {
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![task.template_id.as_str()];
        while let Some(current) = stack.pop() {
            if !visited.insert(current) {
                if current == task.template_id.as_str() {
                    return Err(TraceFunctionError::Validation(format!(
                        "Circular after dependency detected involving '{}'",
                        task.template_id
                    )));
                }
                continue;
            }
            // Find tasks that `current` blocks (i.e., tasks whose after contains `current`)
            for t in &func.tasks {
                if t.after.iter().any(|b| b == current) && t.template_id != task.template_id {
                    stack.push(t.template_id.as_str());
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    // -- Serialization round-trip --

    #[test]
    fn yaml_round_trip() {
        let func = sample_function();
        let yaml = serde_yaml::to_string(&func).unwrap();
        let loaded: TraceFunction = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(loaded.id, func.id);
        assert_eq!(loaded.tasks.len(), func.tasks.len());
        assert_eq!(loaded.inputs.len(), func.inputs.len());
        assert_eq!(loaded.inputs[0].input_type, InputType::String);
    }

    // -- Storage: save/load/list --

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let func = sample_function();
        let path = save_function(&func, dir.path()).unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), "impl-feature.yaml");

        let loaded = load_function(&path).unwrap();
        assert_eq!(loaded.id, "impl-feature");
        assert_eq!(loaded.name, "Implement Feature");
    }

    #[test]
    fn load_all_sorts_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut f1 = sample_function();
        f1.id = "zebra".to_string();
        let mut f2 = sample_function();
        f2.id = "alpha".to_string();

        save_function(&f1, dir.path()).unwrap();
        save_function(&f2, dir.path()).unwrap();

        let all = load_all_functions(dir.path()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "alpha");
        assert_eq!(all[1].id, "zebra");
    }

    #[test]
    fn load_all_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let all = load_all_functions(dir.path()).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn load_all_nonexistent_dir() {
        let all = load_all_functions(Path::new("/nonexistent/path")).unwrap();
        assert!(all.is_empty());
    }

    // -- Find by prefix --

    #[test]
    fn find_by_exact_id() {
        let dir = tempfile::tempdir().unwrap();
        let func = sample_function();
        save_function(&func, dir.path()).unwrap();

        let found = find_function_by_prefix(dir.path(), "impl-feature").unwrap();
        assert_eq!(found.id, "impl-feature");
    }

    #[test]
    fn find_by_prefix_match() {
        let dir = tempfile::tempdir().unwrap();
        let func = sample_function();
        save_function(&func, dir.path()).unwrap();

        let found = find_function_by_prefix(dir.path(), "impl").unwrap();
        assert_eq!(found.id, "impl-feature");
    }

    #[test]
    fn find_by_prefix_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let mut f1 = sample_function();
        f1.id = "impl-feature".to_string();
        let mut f2 = sample_function();
        f2.id = "impl-bug".to_string();

        save_function(&f1, dir.path()).unwrap();
        save_function(&f2, dir.path()).unwrap();

        let err = find_function_by_prefix(dir.path(), "impl").unwrap_err();
        assert!(matches!(err, TraceFunctionError::Ambiguous(_)));
    }

    #[test]
    fn find_by_prefix_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let func = sample_function();
        save_function(&func, dir.path()).unwrap();

        let err = find_function_by_prefix(dir.path(), "nonexistent").unwrap_err();
        assert!(matches!(err, TraceFunctionError::NotFound(_)));
    }

    // -- Input validation --

    #[test]
    fn validate_inputs_required_present() {
        let func = sample_function();
        let mut provided = HashMap::new();
        provided.insert(
            "feature_name".to_string(),
            serde_yaml::Value::String("my-feature".to_string()),
        );

        let resolved = validate_inputs(&func.inputs, &provided).unwrap();
        assert_eq!(
            resolved.get("feature_name").unwrap().as_str().unwrap(),
            "my-feature"
        );
        // test_command should get its default
        assert_eq!(
            resolved.get("test_command").unwrap().as_str().unwrap(),
            "cargo test"
        );
    }

    #[test]
    fn validate_inputs_missing_required() {
        let func = sample_function();
        let provided = HashMap::new();

        let err = validate_inputs(&func.inputs, &provided).unwrap_err();
        match err {
            TraceFunctionError::Validation(msg) => {
                assert!(msg.contains("feature_name"));
            }
            _ => panic!("Expected Validation error"),
        }
    }

    #[test]
    fn validate_inputs_wrong_type() {
        let func = sample_function();
        let mut provided = HashMap::new();
        provided.insert(
            "feature_name".to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(42)),
        );

        let err = validate_inputs(&func.inputs, &provided).unwrap_err();
        assert!(matches!(err, TraceFunctionError::Validation(_)));
    }

    #[test]
    fn validate_number_range() {
        let defs = vec![FunctionInput {
            name: "threshold".to_string(),
            input_type: InputType::Number,
            description: "Score threshold".to_string(),
            required: true,
            default: None,
            example: None,
            min: Some(0.0),
            max: Some(1.0),
            values: None,
        }];

        // Valid
        let mut provided = HashMap::new();
        provided.insert(
            "threshold".to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(0.5)),
        );
        assert!(validate_inputs(&defs, &provided).is_ok());

        // Too low
        provided.insert(
            "threshold".to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(-0.1)),
        );
        assert!(validate_inputs(&defs, &provided).is_err());

        // Too high
        provided.insert(
            "threshold".to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(1.5)),
        );
        assert!(validate_inputs(&defs, &provided).is_err());
    }

    #[test]
    fn validate_enum_values() {
        let defs = vec![FunctionInput {
            name: "language".to_string(),
            input_type: InputType::Enum,
            description: "Language".to_string(),
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

        // Valid
        let mut provided = HashMap::new();
        provided.insert(
            "language".to_string(),
            serde_yaml::Value::String("rust".to_string()),
        );
        assert!(validate_inputs(&defs, &provided).is_ok());

        // Invalid
        provided.insert(
            "language".to_string(),
            serde_yaml::Value::String("java".to_string()),
        );
        let err = validate_inputs(&defs, &provided).unwrap_err();
        match err {
            TraceFunctionError::Validation(msg) => {
                assert!(msg.contains("java"));
                assert!(msg.contains("rust"));
            }
            _ => panic!("Expected Validation error"),
        }
    }

    #[test]
    fn validate_file_list() {
        let defs = vec![FunctionInput {
            name: "files".to_string(),
            input_type: InputType::FileList,
            description: "Source files".to_string(),
            required: true,
            default: None,
            example: None,
            min: None,
            max: None,
            values: None,
        }];

        // Valid
        let mut provided = HashMap::new();
        provided.insert(
            "files".to_string(),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("src/main.rs".to_string()),
                serde_yaml::Value::String("src/lib.rs".to_string()),
            ]),
        );
        assert!(validate_inputs(&defs, &provided).is_ok());

        // Invalid (string instead of list)
        provided.insert(
            "files".to_string(),
            serde_yaml::Value::String("src/main.rs".to_string()),
        );
        assert!(validate_inputs(&defs, &provided).is_err());
    }

    // -- Template substitution --

    #[test]
    fn substitute_simple() {
        let mut inputs = HashMap::new();
        inputs.insert(
            "feature_name".to_string(),
            serde_yaml::Value::String("my-feature".to_string()),
        );

        let result = substitute("Plan {{input.feature_name}}", &inputs);
        assert_eq!(result, "Plan my-feature");
    }

    #[test]
    fn substitute_multiple() {
        let mut inputs = HashMap::new();
        inputs.insert(
            "feature_name".to_string(),
            serde_yaml::Value::String("auth".to_string()),
        );
        inputs.insert(
            "test_command".to_string(),
            serde_yaml::Value::String("cargo test auth".to_string()),
        );

        let result = substitute(
            "Implement {{input.feature_name}}. Run: {{input.test_command}}",
            &inputs,
        );
        assert_eq!(result, "Implement auth. Run: cargo test auth");
    }

    #[test]
    fn substitute_number() {
        let mut inputs = HashMap::new();
        inputs.insert(
            "threshold".to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(42)),
        );

        let result = substitute("Score must be at least {{input.threshold}}", &inputs);
        assert_eq!(result, "Score must be at least 42");
    }

    #[test]
    fn substitute_file_list() {
        let mut inputs = HashMap::new();
        inputs.insert(
            "files".to_string(),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("src/main.rs".to_string()),
                serde_yaml::Value::String("src/lib.rs".to_string()),
            ]),
        );

        let result = substitute("Files:\n{{input.files}}", &inputs);
        assert_eq!(result, "Files:\nsrc/main.rs\nsrc/lib.rs");
    }

    #[test]
    fn substitute_missing_placeholder_unchanged() {
        let inputs = HashMap::new();
        let result = substitute("Hello {{input.unknown}}", &inputs);
        assert_eq!(result, "Hello {{input.unknown}}");
    }

    #[test]
    fn substitute_task_template_all_fields() {
        let template = TaskTemplate {
            template_id: "plan".to_string(),
            title: "Plan {{input.feature_name}}".to_string(),
            description: "Plan {{input.feature_name}} using {{input.test_command}}".to_string(),
            skills: vec!["analysis".to_string(), "{{input.language}}".to_string()],
            after: vec![],
            loops_to: vec![],
            role_hint: Some("analyst".to_string()),
            deliverables: vec!["docs/{{input.feature_name}}.md".to_string()],
            verify: Some("{{input.test_command}}".to_string()),
            tags: vec![],
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

        let result = substitute_task_template(&template, &inputs);
        assert_eq!(result.title, "Plan auth");
        assert_eq!(result.description, "Plan auth using cargo test");
        assert_eq!(result.skills, vec!["analysis", "rust"]);
        assert_eq!(result.deliverables, vec!["docs/auth.md"]);
        assert_eq!(result.verify.unwrap(), "cargo test");
    }

    // -- Function validation --

    #[test]
    fn validate_function_valid() {
        let func = sample_function();
        assert!(validate_function(&func).is_ok());
    }

    #[test]
    fn validate_function_bad_after() {
        let mut func = sample_function();
        func.tasks[1].after = vec!["nonexistent".to_string()];

        let err = validate_function(&func).unwrap_err();
        match err {
            TraceFunctionError::Validation(msg) => {
                assert!(msg.contains("nonexistent"));
            }
            _ => panic!("Expected Validation error"),
        }
    }

    #[test]
    fn validate_function_bad_loops_to() {
        let mut func = sample_function();
        func.tasks[3].loops_to[0].target = "nonexistent".to_string();

        let err = validate_function(&func).unwrap_err();
        match err {
            TraceFunctionError::Validation(msg) => {
                assert!(msg.contains("nonexistent"));
            }
            _ => panic!("Expected Validation error"),
        }
    }

    #[test]
    fn validate_function_duplicate_template_ids() {
        let mut func = sample_function();
        func.tasks[1].template_id = "plan".to_string(); // duplicate

        let err = validate_function(&func).unwrap_err();
        match err {
            TraceFunctionError::Validation(msg) => {
                assert!(msg.contains("Duplicate"));
            }
            _ => panic!("Expected Validation error"),
        }
    }

    // -- YAML format compatibility --

    #[test]
    fn deserialize_yaml_from_design_doc() {
        // Verify we can parse the YAML format shown in the design doc
        let yaml = r#"
kind: trace-function
version: 1
id: impl-feature
name: "Implement Feature"
description: "Plan, implement, test, and commit a new feature"
extracted_from:
  - task_id: impl-global-config
    run_id: run-003
    timestamp: "2026-02-18T14:30:00Z"
extracted_by: scout
extracted_at: "2026-02-19T12:00:00Z"
tags: [implementation, feature]
inputs:
  - name: feature_name
    type: string
    description: "Short name for the feature"
    required: true
    example: "global-config"
  - name: threshold
    type: number
    description: "Minimum score"
    required: false
    default: 0.8
    min: 0.0
    max: 1.0
  - name: language
    type: enum
    description: "Primary language"
    values: [rust, python, go]
    default: rust
tasks:
  - template_id: plan
    title: "Plan {{input.feature_name}}"
    description: "Design the implementation"
    skills: [analysis]
    role_hint: analyst
  - template_id: implement
    title: "Implement {{input.feature_name}}"
    description: "Build it"
    after: [plan]
    skills: [implementation]
  - template_id: refine
    title: "Refine {{input.feature_name}}"
    description: "Fix issues"
    after: [implement]
    loops_to:
      - target: implement
        max_iterations: 3
outputs:
  - name: modified_files
    description: "Files changed"
    from_task: implement
    field: artifacts
"#;
        let func: TraceFunction = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(func.id, "impl-feature");
        assert_eq!(func.version, 1);
        assert_eq!(func.inputs.len(), 3);
        assert_eq!(func.inputs[0].input_type, InputType::String);
        assert_eq!(func.inputs[1].input_type, InputType::Number);
        assert_eq!(func.inputs[1].min, Some(0.0));
        assert_eq!(func.inputs[1].max, Some(1.0));
        assert_eq!(func.inputs[2].input_type, InputType::Enum);
        assert_eq!(
            func.inputs[2].values,
            Some(vec![
                "rust".to_string(),
                "python".to_string(),
                "go".to_string()
            ])
        );
        assert_eq!(func.tasks.len(), 3);
        assert_eq!(func.tasks[1].after, vec!["plan"]);
        assert_eq!(func.tasks[2].loops_to.len(), 1);
        assert_eq!(func.tasks[2].loops_to[0].target, "implement");
        assert_eq!(func.tasks[2].loops_to[0].max_iterations, 3);
        assert_eq!(func.outputs.len(), 1);
    }
}

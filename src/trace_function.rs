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

    // === Layer 2: Generative topology ===
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning: Option<PlanningConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<StructuralConstraints>,

    // === Layer 3: Adaptive memory ===
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<TraceMemoryConfig>,

    // === Boundary and visibility ===
    #[serde(default)]
    pub visibility: FunctionVisibility,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted_fields: Vec<String>,
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
// Layer 2/3 types: Generative + Adaptive function support
// ---------------------------------------------------------------------------

/// Controls who can discover and use a function.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum FunctionVisibility {
    #[default]
    Internal, // only within this workgraph
    Peer,     // discoverable by federated peers, redaction applies
    Public,   // fully portable, provenance stripped
}

impl FunctionVisibility {
    /// Numeric openness level: Internal=0, Peer=1, Public=2.
    fn openness(&self) -> u8 {
        match self {
            FunctionVisibility::Internal => 0,
            FunctionVisibility::Peer => 1,
            FunctionVisibility::Public => 2,
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "internal" => Some(FunctionVisibility::Internal),
            "peer" => Some(FunctionVisibility::Peer),
            "public" => Some(FunctionVisibility::Public),
            _ => None,
        }
    }
}

impl PartialOrd for FunctionVisibility {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FunctionVisibility {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.openness().cmp(&other.openness())
    }
}


impl std::fmt::Display for FunctionVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FunctionVisibility::Internal => write!(f, "internal"),
            FunctionVisibility::Peer => write!(f, "peer"),
            FunctionVisibility::Public => write!(f, "public"),
        }
    }
}

/// Configuration for a planning node (Layer 2: Generative functions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanningConfig {
    /// The task template for the planning node itself.
    pub planner_template: TaskTemplate,

    /// Format the planner should output its task graph in.
    #[serde(default = "default_output_format")]
    pub output_format: String,

    /// Use static tasks as fallback if planner fails.
    #[serde(default)]
    pub static_fallback: bool,

    /// Validate planner output against constraints before applying.
    #[serde(default = "default_true")]
    pub validate_plan: bool,
}

fn default_output_format() -> String {
    "workgraph-yaml".to_string()
}

fn default_true() -> bool {
    true
}

/// Constraints on the shape of a generated task graph (Layer 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralConstraints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tasks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tasks: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub allow_cycles: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_iterations: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_phases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_patterns: Vec<ForbiddenPattern>,
}

/// A tag combination that is forbidden in generated task graphs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForbiddenPattern {
    pub tags: Vec<String>,
    pub reason: String,
}

/// Configuration for trace memory (Layer 3: Adaptive functions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMemoryConfig {
    /// Maximum past run summaries to include in planning prompt.
    #[serde(default = "default_max_runs")]
    pub max_runs: u32,
    pub include: MemoryInclusions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_path: Option<String>,
}

fn default_max_runs() -> u32 {
    10
}

/// Which aspects of past runs to include in trace memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInclusions {
    #[serde(default = "default_true")]
    pub outcomes: bool,
    #[serde(default = "default_true")]
    pub scores: bool,
    #[serde(default = "default_true")]
    pub interventions: bool,
    #[serde(default = "default_true")]
    pub duration: bool,
    #[serde(default)]
    pub retries: bool,
    #[serde(default)]
    pub artifacts: bool,
}

/// Summary of a single past application run (used in trace memory).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    #[serde(alias = "instantiated_at")]
    pub applied_at: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub inputs: HashMap<String, serde_yaml::Value>,
    pub prefix: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_outcomes: Vec<TaskOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interventions: Vec<InterventionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_secs: Option<i64>,
    pub all_succeeded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_score: Option<f64>,
}

/// Outcome of a single task within a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub template_id: String,
    pub task_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<i64>,
    #[serde(default)]
    pub retry_count: u32,
}

/// Summary of a human or system intervention during a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionSummary {
    pub task_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub timestamp: String,
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

/// Load run summaries from the `.runs.jsonl` file for a function.
/// Returns an empty vec if no runs file exists.
pub fn load_runs(func_dir: &Path, function_id: &str) -> Vec<RunSummary> {
    let path = func_dir.join(format!("{}.runs.jsonl", function_id));
    if !path.exists() {
        return Vec::new();
    }
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
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
// Export boundary: visibility-based redaction
// ---------------------------------------------------------------------------

/// Returns true if a string looks like a file path.
fn looks_like_path(s: &str) -> bool {
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('~')
        || s.contains('/')
            && !s.starts_with("http://")
            && !s.starts_with("https://")
}

/// Apply visibility-based redaction to a trace function for export.
///
/// Implements the boundary crossing protocol from the design doc (section 5):
/// - Internal: full copy, no redaction
/// - Peer: generalize extracted_by, strip redacted_fields, memory config-only,
///   extracted_from keeps task_id + timestamp
/// - Public: strip extracted_by + provenance details + memory + path defaults
///
/// Returns an error if the function's declared visibility is less open than the
/// requested target (e.g., an Internal function cannot be exported as Peer).
pub fn export_function(
    func: &TraceFunction,
    target_visibility: &FunctionVisibility,
) -> Result<TraceFunction, TraceFunctionError> {
    // 1. Check: func.visibility must be >= target_visibility
    if func.visibility < *target_visibility {
        return Err(TraceFunctionError::Validation(format!(
            "Function '{}' has visibility '{}' which cannot be exported at '{}' level",
            func.id, func.visibility, target_visibility
        )));
    }

    // 2. Clone
    let mut exported = func.clone();

    // 3. Apply redaction rules per target visibility
    match target_visibility {
        FunctionVisibility::Internal => {
            // Full copy, no redaction
        }
        FunctionVisibility::Peer => {
            apply_peer_redaction(&mut exported);
        }
        FunctionVisibility::Public => {
            apply_public_redaction(&mut exported);
        }
    }

    Ok(exported)
}

/// Apply peer-level redaction rules.
fn apply_peer_redaction(func: &mut TraceFunction) {
    // Generalize extracted_by (e.g., "scout-abc123" → "agent")
    if func.extracted_by.is_some() {
        func.extracted_by = Some("agent".to_string());
    }

    // extracted_from: keep task_id + timestamp, strip run_id
    for source in &mut func.extracted_from {
        source.run_id = None;
    }

    // Strip fields listed in redacted_fields
    for field in &func.redacted_fields.clone() {
        match field.as_str() {
            "extracted_by" => func.extracted_by = None,
            "extracted_at" => func.extracted_at = None,
            "tags" => func.tags.clear(),
            _ => {} // unknown redacted field, ignore
        }
    }

    // Memory: config schema only (strip storage_path, keep structure)
    if let Some(ref mut mem) = func.memory {
        mem.storage_path = None;
    }
}

/// Apply public-level redaction rules.
fn apply_public_redaction(func: &mut TraceFunction) {
    // Strip extracted_by entirely
    func.extracted_by = None;
    func.extracted_at = None;

    // extracted_from: keep task_id only, strip timestamp and run_id
    for source in &mut func.extracted_from {
        source.run_id = None;
        source.timestamp = String::new();
    }

    // Strip path-specific defaults from inputs
    for input in &mut func.inputs {
        if let Some(ref default_val) = input.default
            && let Some(s) = default_val.as_str()
                && looks_like_path(s) {
                    input.default = None;
                }
        if let Some(ref example_val) = input.example
            && let Some(s) = example_val.as_str()
                && looks_like_path(s) {
                    input.example = None;
                }
    }

    // Strip memory entirely
    func.memory = None;

    // Redacted fields are not meaningful in public exports
    func.redacted_fields.clear();
}

/// Check whether a function should be included in an export at the given visibility level.
pub fn function_visible_at(func: &TraceFunction, target: &FunctionVisibility) -> bool {
    func.visibility >= *target
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
            planning: None,
            constraints: None,
            memory: None,
            visibility: FunctionVisibility::Internal,
            redacted_fields: vec![],
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
        // New fields default correctly when absent from v1 YAML
        assert_eq!(func.visibility, FunctionVisibility::Internal);
        assert!(func.planning.is_none());
        assert!(func.constraints.is_none());
        assert!(func.memory.is_none());
        assert!(func.redacted_fields.is_empty());
    }

    // -- Layer 2/3 types --

    #[test]
    fn visibility_serde_kebab_case() {
        let yaml = "\"internal\"";
        let v: FunctionVisibility = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(v, FunctionVisibility::Internal);

        let yaml = "\"peer\"";
        let v: FunctionVisibility = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(v, FunctionVisibility::Peer);

        let yaml = "\"public\"";
        let v: FunctionVisibility = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(v, FunctionVisibility::Public);

        // Round-trip
        let serialized = serde_yaml::to_string(&FunctionVisibility::Peer).unwrap();
        assert!(serialized.trim() == "peer");
    }

    #[test]
    fn visibility_default_is_internal() {
        assert_eq!(FunctionVisibility::default(), FunctionVisibility::Internal);
    }

    #[test]
    fn deserialize_v2_generative_function() {
        let yaml = r#"
kind: trace-function
version: 2
id: impl-api
name: "Implement API from Spec"
description: "Read an API spec, plan tasks per endpoint, implement, validate."
visibility: peer
inputs:
  - name: api_spec
    type: file_content
    description: "The API specification"
    required: true
planning:
  planner_template:
    template_id: plan-api
    title: "Plan API implementation"
    description: "Read the API spec and produce a task plan."
    skills: [analysis, api-design]
    role_hint: architect
  output_format: workgraph-yaml
  static_fallback: true
  validate_plan: true
constraints:
  min_tasks: 2
  max_tasks: 20
  required_skills: [implementation, testing]
  required_phases: [implement, test]
  max_depth: 4
  forbidden_patterns:
    - tags: [untested, production]
      reason: "Cannot deploy untested code"
tasks:
  - template_id: implement
    title: "Implement API"
    description: "Fallback implementation"
    skills: [implementation]
  - template_id: test
    title: "Test API"
    description: "Fallback testing"
    after: [implement]
    skills: [testing]
redacted_fields:
  - extracted_by
"#;
        let func: TraceFunction = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(func.version, 2);
        assert_eq!(func.visibility, FunctionVisibility::Peer);
        assert_eq!(func.redacted_fields, vec!["extracted_by"]);

        let planning = func.planning.unwrap();
        assert_eq!(planning.planner_template.template_id, "plan-api");
        assert_eq!(planning.output_format, "workgraph-yaml");
        assert!(planning.static_fallback);
        assert!(planning.validate_plan);

        let constraints = func.constraints.unwrap();
        assert_eq!(constraints.min_tasks, Some(2));
        assert_eq!(constraints.max_tasks, Some(20));
        assert_eq!(constraints.required_skills, vec!["implementation", "testing"]);
        assert_eq!(constraints.required_phases, vec!["implement", "test"]);
        assert_eq!(constraints.max_depth, Some(4));
        assert!(!constraints.allow_cycles);
        assert_eq!(constraints.forbidden_patterns.len(), 1);
        assert_eq!(constraints.forbidden_patterns[0].tags, vec!["untested", "production"]);

        assert!(func.memory.is_none());
    }

    #[test]
    fn deserialize_v3_adaptive_function() {
        let yaml = r#"
kind: trace-function
version: 3
id: deploy-production
name: "Deploy to Production"
description: "Build, test, stage, deploy with memory of past deploys."
visibility: internal
inputs:
  - name: version
    type: string
    description: "Version to deploy"
    required: true
planning:
  planner_template:
    template_id: plan-deploy
    title: "Plan deployment"
    description: "Plan the deployment."
    skills: [devops, planning]
    role_hint: architect
  validate_plan: true
constraints:
  required_phases: [build, test, deploy]
  required_skills: [devops]
memory:
  max_runs: 10
  include:
    outcomes: true
    scores: true
    interventions: true
    duration: true
    retries: false
    artifacts: false
tasks:
  - template_id: build
    title: "Build"
    description: "Build step"
  - template_id: test
    title: "Test"
    description: "Test step"
    after: [build]
  - template_id: deploy
    title: "Deploy"
    description: "Deploy step"
    after: [test]
"#;
        let func: TraceFunction = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(func.version, 3);
        assert_eq!(func.visibility, FunctionVisibility::Internal);

        let memory = func.memory.unwrap();
        assert_eq!(memory.max_runs, 10);
        assert!(memory.include.outcomes);
        assert!(memory.include.scores);
        assert!(memory.include.interventions);
        assert!(memory.include.duration);
        assert!(!memory.include.retries);
        assert!(!memory.include.artifacts);
        assert!(memory.storage_path.is_none());
    }

    #[test]
    fn v2_function_round_trip() {
        let mut func = sample_function();
        func.version = 2;
        func.visibility = FunctionVisibility::Peer;
        func.redacted_fields = vec!["extracted_by".to_string()];
        func.planning = Some(PlanningConfig {
            planner_template: TaskTemplate {
                template_id: "planner".to_string(),
                title: "Plan".to_string(),
                description: "Plan the work".to_string(),
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
        });
        func.constraints = Some(StructuralConstraints {
            min_tasks: Some(2),
            max_tasks: Some(10),
            required_skills: vec!["implementation".to_string()],
            max_depth: None,
            allow_cycles: false,
            max_total_iterations: None,
            required_phases: vec![],
            forbidden_patterns: vec![],
        });

        let yaml = serde_yaml::to_string(&func).unwrap();
        let loaded: TraceFunction = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.visibility, FunctionVisibility::Peer);
        assert_eq!(loaded.redacted_fields, vec!["extracted_by"]);
        assert!(loaded.planning.is_some());
        assert!(loaded.constraints.is_some());
        assert!(loaded.memory.is_none());

        let planning = loaded.planning.unwrap();
        assert_eq!(planning.planner_template.template_id, "planner");
        assert!(planning.validate_plan);

        let constraints = loaded.constraints.unwrap();
        assert_eq!(constraints.min_tasks, Some(2));
        assert_eq!(constraints.max_tasks, Some(10));
    }

    #[test]
    fn v1_yaml_omits_new_fields() {
        let func = sample_function();
        let yaml = serde_yaml::to_string(&func).unwrap();

        // New optional fields should not appear in v1 serialization
        assert!(!yaml.contains("planning:"));
        assert!(!yaml.contains("constraints:"));
        assert!(!yaml.contains("memory:"));
        assert!(!yaml.contains("redacted_fields:"));
        // visibility defaults to internal but is serialized since it has no skip_serializing_if
        // (it always has a value via Default)
    }

    #[test]
    fn planning_config_defaults() {
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
    fn structural_constraints_defaults() {
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

    #[test]
    fn memory_inclusions_defaults() {
        let yaml = "{}";
        let m: MemoryInclusions = serde_yaml::from_str(yaml).unwrap();
        assert!(m.outcomes);
        assert!(m.scores);
        assert!(m.interventions);
        assert!(m.duration);
        assert!(!m.retries);
        assert!(!m.artifacts);
    }

    #[test]
    fn run_summary_round_trip() {
        let summary = RunSummary {
            applied_at: "2026-02-20T12:00:00Z".to_string(),
            inputs: {
                let mut m = HashMap::new();
                m.insert(
                    "version".to_string(),
                    serde_yaml::Value::String("1.0".to_string()),
                );
                m
            },
            prefix: "deploy-1.0/".to_string(),
            task_outcomes: vec![TaskOutcome {
                template_id: "build".to_string(),
                task_id: "deploy-1.0/build".to_string(),
                status: "Done".to_string(),
                score: Some(0.95),
                duration_secs: Some(120),
                retry_count: 0,
            }],
            interventions: vec![InterventionSummary {
                task_id: "deploy-1.0/test".to_string(),
                kind: "manual-retry".to_string(),
                description: Some("Flaky test, retried".to_string()),
                timestamp: "2026-02-20T12:05:00Z".to_string(),
            }],
            wall_clock_secs: Some(300),
            all_succeeded: true,
            avg_score: Some(0.95),
        };

        let yaml = serde_yaml::to_string(&summary).unwrap();
        let loaded: RunSummary = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(loaded.prefix, "deploy-1.0/");
        assert!(loaded.all_succeeded);
        assert_eq!(loaded.task_outcomes.len(), 1);
        assert_eq!(loaded.task_outcomes[0].score, Some(0.95));
        assert_eq!(loaded.interventions.len(), 1);
        assert_eq!(loaded.interventions[0].kind, "manual-retry");
    }

    // -- Visibility ordering --

    #[test]
    fn visibility_ordering() {
        assert!(FunctionVisibility::Internal < FunctionVisibility::Peer);
        assert!(FunctionVisibility::Peer < FunctionVisibility::Public);
        assert!(FunctionVisibility::Internal < FunctionVisibility::Public);
    }

    // -- Export function boundary tests --

    fn sample_peer_function() -> TraceFunction {
        let mut func = sample_function();
        func.visibility = FunctionVisibility::Peer;
        func.extracted_by = Some("scout-abc123".to_string());
        func.extracted_at = Some("2026-02-19T12:00:00Z".to_string());
        func.extracted_from = vec![ExtractionSource {
            task_id: "impl-auth".to_string(),
            run_id: Some("run-007".to_string()),
            timestamp: "2026-02-18T14:30:00Z".to_string(),
        }];
        func.memory = Some(TraceMemoryConfig {
            max_runs: 5,
            include: MemoryInclusions {
                outcomes: true,
                scores: true,
                interventions: false,
                duration: true,
                retries: false,
                artifacts: false,
            },
            storage_path: Some("/home/user/.workgraph/memory".to_string()),
        });
        func.inputs[0].default = Some(serde_yaml::Value::String("/home/user/project".to_string()));
        func.inputs[0].example = Some(serde_yaml::Value::String("src/lib.rs".to_string()));
        func
    }

    fn sample_public_function() -> TraceFunction {
        let mut func = sample_peer_function();
        func.visibility = FunctionVisibility::Public;
        func
    }

    #[test]
    fn export_internal_function_at_internal() {
        let func = sample_function(); // Internal visibility
        let exported = export_function(&func, &FunctionVisibility::Internal).unwrap();
        // Internal → Internal: no redaction
        assert_eq!(exported.extracted_by, func.extracted_by);
        assert_eq!(exported.extracted_at, func.extracted_at);
    }

    #[test]
    fn export_internal_function_at_peer_fails() {
        let func = sample_function(); // Internal visibility
        let err = export_function(&func, &FunctionVisibility::Peer).unwrap_err();
        match err {
            TraceFunctionError::Validation(msg) => {
                assert!(msg.contains("internal"));
                assert!(msg.contains("peer"));
            }
            _ => panic!("Expected Validation error"),
        }
    }

    #[test]
    fn export_internal_function_at_public_fails() {
        let func = sample_function(); // Internal visibility
        assert!(export_function(&func, &FunctionVisibility::Public).is_err());
    }

    #[test]
    fn export_peer_function_at_peer() {
        let func = sample_peer_function();
        let exported = export_function(&func, &FunctionVisibility::Peer).unwrap();

        // extracted_by generalized
        assert_eq!(exported.extracted_by, Some("agent".to_string()));

        // extracted_from: run_id stripped, timestamp kept
        assert!(exported.extracted_from[0].run_id.is_none());
        assert_eq!(
            exported.extracted_from[0].timestamp,
            "2026-02-18T14:30:00Z"
        );
        assert_eq!(exported.extracted_from[0].task_id, "impl-auth");

        // Memory: storage_path stripped, config retained
        let mem = exported.memory.unwrap();
        assert!(mem.storage_path.is_none());
        assert_eq!(mem.max_runs, 5);
        assert!(mem.include.outcomes);
    }

    #[test]
    fn export_peer_function_at_internal() {
        let func = sample_peer_function();
        // Peer function exported at internal level: no redaction
        let exported = export_function(&func, &FunctionVisibility::Internal).unwrap();
        assert_eq!(exported.extracted_by, Some("scout-abc123".to_string()));
        assert!(exported.extracted_from[0].run_id.is_some());
    }

    #[test]
    fn export_peer_function_at_public_fails() {
        let func = sample_peer_function();
        assert!(export_function(&func, &FunctionVisibility::Public).is_err());
    }

    #[test]
    fn export_public_function_at_public() {
        let func = sample_public_function();
        let exported = export_function(&func, &FunctionVisibility::Public).unwrap();

        // extracted_by and extracted_at stripped
        assert!(exported.extracted_by.is_none());
        assert!(exported.extracted_at.is_none());

        // extracted_from: timestamp cleared, run_id stripped
        assert!(exported.extracted_from[0].run_id.is_none());
        assert!(exported.extracted_from[0].timestamp.is_empty());
        assert_eq!(exported.extracted_from[0].task_id, "impl-auth");

        // Memory stripped entirely
        assert!(exported.memory.is_none());

        // Path-like defaults stripped from inputs
        assert!(exported.inputs[0].default.is_none()); // was /home/user/project
        // example "src/lib.rs" contains '/' so it's path-like → stripped
        assert!(exported.inputs[0].example.is_none());

        // redacted_fields cleared for public
        assert!(exported.redacted_fields.is_empty());
    }

    #[test]
    fn export_public_function_at_peer() {
        let func = sample_public_function();
        // Public function can be exported at peer level (less open than declared)
        let exported = export_function(&func, &FunctionVisibility::Peer).unwrap();
        // Peer redaction applied
        assert_eq!(exported.extracted_by, Some("agent".to_string()));
        assert!(exported.extracted_from[0].run_id.is_none());
        // Memory config retained (peer gets schema)
        let mem = exported.memory.unwrap();
        assert!(mem.storage_path.is_none());
    }

    #[test]
    fn export_peer_redacted_fields() {
        let mut func = sample_peer_function();
        func.redacted_fields = vec!["extracted_by".to_string(), "tags".to_string()];
        let exported = export_function(&func, &FunctionVisibility::Peer).unwrap();
        // redacted_fields causes these to be stripped
        assert!(exported.extracted_by.is_none());
        assert!(exported.tags.is_empty());
    }

    #[test]
    fn function_visible_at_levels() {
        let internal_fn = sample_function();
        assert!(function_visible_at(&internal_fn, &FunctionVisibility::Internal));
        assert!(!function_visible_at(&internal_fn, &FunctionVisibility::Peer));
        assert!(!function_visible_at(&internal_fn, &FunctionVisibility::Public));

        let peer_fn = sample_peer_function();
        assert!(function_visible_at(&peer_fn, &FunctionVisibility::Internal));
        assert!(function_visible_at(&peer_fn, &FunctionVisibility::Peer));
        assert!(!function_visible_at(&peer_fn, &FunctionVisibility::Public));

        let public_fn = sample_public_function();
        assert!(function_visible_at(&public_fn, &FunctionVisibility::Internal));
        assert!(function_visible_at(&public_fn, &FunctionVisibility::Peer));
        assert!(function_visible_at(&public_fn, &FunctionVisibility::Public));
    }

    #[test]
    fn looks_like_path_detection() {
        assert!(looks_like_path("/usr/bin/foo"));
        assert!(looks_like_path("./relative/path"));
        assert!(looks_like_path("../parent/path"));
        assert!(looks_like_path("~/home/path"));
        assert!(looks_like_path("src/main.rs"));
        assert!(!looks_like_path("https://example.com/path"));
        assert!(!looks_like_path("http://example.com"));
        assert!(!looks_like_path("cargo test"));
        assert!(!looks_like_path("just a string"));
    }

    #[test]
    fn export_non_path_defaults_preserved_in_public() {
        let mut func = sample_public_function();
        func.inputs[0].default = Some(serde_yaml::Value::String("cargo test".to_string()));
        func.inputs[0].example = Some(serde_yaml::Value::String("my-feature".to_string()));
        let exported = export_function(&func, &FunctionVisibility::Public).unwrap();
        // Non-path defaults should be preserved
        assert_eq!(
            exported.inputs[0].default,
            Some(serde_yaml::Value::String("cargo test".to_string()))
        );
        assert_eq!(
            exported.inputs[0].example,
            Some(serde_yaml::Value::String("my-feature".to_string()))
        );
    }
}

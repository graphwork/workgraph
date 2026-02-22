use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use workgraph::agency;
use workgraph::graph::{LoopGuard, Status, Task, WorkGraph};
use workgraph::provenance;
use workgraph::trace_function::{
    self, ExtractionSource, FunctionInput, FunctionOutput, FunctionVisibility, InputType,
    LoopEdgeTemplate, PlanningConfig, StructuralConstraints, TaskTemplate, TraceFunction,
};

/// Check whether a task is coordinator-generated infrastructure noise
/// (evaluation tasks, assignment tasks) that should be excluded from extraction.
fn is_coordinator_noise(task: &Task) -> bool {
    // Filter evaluate-* tasks (coordinator-generated evaluations)
    if task.id.starts_with("evaluate-") {
        return true;
    }
    // Filter assign-* tasks (coordinator-generated agent assignment tasks)
    if task.id.starts_with("assign-") {
        return true;
    }
    // Filter by evaluation tag
    if task.tags.iter().any(|t| t == "evaluation") {
        return true;
    }
    false
}

/// Run the `wg trace extract <task-id>` command.
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    task_id: &str,
    name: Option<&str>,
    subgraph: bool,
    generalize: bool,
    output: Option<&str>,
    force: bool,
    include_evaluations: bool,
) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;
    let task = graph.get_task_or_err(task_id)?;

    // Task must be Done
    if task.status != Status::Done {
        bail!(
            "Task '{}' is in '{}' status. Only completed (done) tasks can be extracted into trace functions.",
            task_id,
            task.status
        );
    }

    // Determine function ID
    let func_id = name.map(|s| s.to_string()).unwrap_or_else(|| sanitize_id(task_id));

    // Check for existing function
    let functions_dir = if let Some(out) = output {
        PathBuf::from(out)
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    } else {
        trace_function::functions_dir(dir)
    };

    if output.is_none() {
        let target_path = functions_dir.join(format!("{}.yaml", func_id));
        if target_path.exists() && !force {
            bail!(
                "Function '{}' already exists at {}. Use --force to overwrite.",
                func_id,
                target_path.display()
            );
        }
    } else if let Some(out) = output {
        let target_path = PathBuf::from(out);
        if target_path.exists() && !force {
            bail!(
                "Output file {} already exists. Use --force to overwrite.",
                target_path.display()
            );
        }
    }

    // Collect tasks to include
    let mut tasks_to_extract: Vec<&Task> = if subgraph {
        collect_subgraph(task_id, &graph)
    } else {
        vec![task]
    };

    // Filter out coordinator-generated noise (evaluate-*, assign-*) unless opted in
    if !include_evaluations {
        tasks_to_extract.retain(|t| !is_coordinator_noise(t));
    }

    // Build task templates
    let subgraph_ids: HashSet<&str> = tasks_to_extract.iter().map(|t| t.id.as_str()).collect();
    let templates: Vec<TaskTemplate> = tasks_to_extract
        .iter()
        .map(|t| build_template(t, task_id, &subgraph_ids, dir, &graph))
        .collect();

    // Collect artifacts for outputs
    let outputs = build_outputs(&tasks_to_extract);

    // Detect parameters
    let suggested_inputs = detect_parameters(&tasks_to_extract);

    // Read provenance operations
    let all_ops = provenance::read_all_operations(dir).unwrap_or_default();
    let task_ops: Vec<_> = all_ops
        .iter()
        .filter(|e| {
            e.task_id
                .as_ref()
                .map(|id| subgraph_ids.contains(id.as_str()))
                .unwrap_or(false)
        })
        .collect();

    // Build the trace function
    let now = Utc::now().to_rfc3339();
    let func = TraceFunction {
        kind: "trace-function".to_string(),
        version: 1,
        id: func_id.clone(),
        name: name
            .map(title_case)
            .unwrap_or_else(|| title_case(&sanitize_id(task_id))),
        description: task
            .description
            .clone()
            .unwrap_or_else(|| task.title.clone()),
        extracted_from: vec![ExtractionSource {
            task_id: task_id.to_string(),
            run_id: None,
            timestamp: now.clone(),
        }],
        extracted_by: task.assigned.clone(),
        extracted_at: Some(now),
        tags: task.tags.clone(),
        inputs: suggested_inputs.clone(),
        tasks: templates,
        outputs,
        planning: None,
        constraints: None,
        memory: None,
        visibility: FunctionVisibility::Internal,
        redacted_fields: vec![],
    };

    // Handle --generalize: invoke executor for generalization pass
    let func = if generalize {
        match generalize_with_executor(dir, &func) {
            Ok(generalized) => {
                eprintln!("Function generalized via executor.");
                generalized
            }
            Err(e) => {
                eprintln!(
                    "Warning: Generalization failed: {}. Saving raw extraction.",
                    e
                );
                func
            }
        }
    } else {
        func
    };

    // Validate
    trace_function::validate_function(&func)
        .context("Extracted function failed validation")?;

    // Save
    let saved_path = if let Some(out) = output {
        let out_path = PathBuf::from(out);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(&func)?;
        std::fs::write(&out_path, yaml)?;
        out_path
    } else {
        trace_function::save_function(&func, &functions_dir)?
    };

    // Print summary
    println!(
        "Extracted trace function '{}' from task '{}'",
        func_id, task_id
    );
    println!();
    println!(
        "Tasks: {} ({})",
        func.tasks.len(),
        if func.tasks.len() == 1 {
            "standalone task".to_string()
        } else {
            format!("{} task subgraph", func.tasks.len())
        }
    );

    if !suggested_inputs.is_empty() {
        println!();
        println!("Suggested parameters:");
        for input in &suggested_inputs {
            let req = if input.required {
                ", required"
            } else {
                ""
            };
            let example_str = input
                .example
                .as_ref()
                .map(|e| format!(" — e.g. {}", trace_function::render_value(e)))
                .unwrap_or_default();
            println!(
                "  {} ({:?}{}){}",
                input.name, input.input_type, req, example_str
            );
        }
    }

    if !task_ops.is_empty() {
        println!();
        println!("Provenance: {} operations recorded", task_ops.len());
    }

    println!();
    println!("Saved to: {}", saved_path.display());
    println!();
    println!("Review and edit the function file to adjust parameters and descriptions.");

    Ok(())
}

/// Collect the subgraph rooted at a task: the task itself plus all tasks
/// that are (transitively) blocked by it.
fn collect_subgraph<'a>(root_id: &str, graph: &'a workgraph::graph::WorkGraph) -> Vec<&'a Task> {
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut queue = vec![root_id.to_string()];

    while let Some(id) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some(task) = graph.get_task(&id) {
            result.push(task);
            // Find tasks that list this task in their after
            for t in graph.tasks() {
                if t.after.iter().any(|b| b == &id) {
                    queue.push(t.id.clone());
                }
            }
        }
    }

    // Sort by dependency order (tasks with fewer deps first)
    result.sort_by_key(|t| t.after.len());
    result
}

/// Build a TaskTemplate from a Task.
fn build_template(
    task: &Task,
    root_id: &str,
    subgraph_ids: &HashSet<&str>,
    dir: &Path,
    graph: &WorkGraph,
) -> TaskTemplate {
    let template_id = strip_prefix(&task.id, root_id);

    // Map after to template IDs (only those in our subgraph)
    let after: Vec<String> = task
        .after
        .iter()
        .filter(|b| subgraph_ids.contains(b.as_str()))
        .map(|b| strip_prefix(b, root_id))
        .collect();

    // Look up role hint from agency
    let role_hint = lookup_role_hint(task, dir);

    // Capture cycle_config into LoopEdgeTemplate entries
    let loops_to = if let Some(ref config) = task.cycle_config {
        let targets = find_cycle_loop_targets(task, subgraph_ids, graph);
        targets
            .iter()
            .map(|target_id| LoopEdgeTemplate {
                target: strip_prefix(target_id, root_id),
                max_iterations: config.max_iterations,
                guard: config.guard.as_ref().and_then(|g| match g {
                    LoopGuard::Always => None,
                    _ => Some(format_loop_guard(g)),
                }),
                delay: config.delay.clone(),
            })
            .collect()
    } else {
        vec![]
    };

    TaskTemplate {
        template_id,
        title: task.title.clone(),
        description: task
            .description
            .clone()
            .unwrap_or_else(|| task.title.clone()),
        skills: task.skills.clone(),
        after,
        loops_to,
        role_hint,
        deliverables: task.deliverables.clone(),
        verify: task.verify.clone(),
        tags: task.tags.clone(),
    }
}

/// Look up the role name for a task's agent from the agency storage.
fn lookup_role_hint(task: &Task, dir: &Path) -> Option<String> {
    let agent_hash = task.agent.as_ref()?;
    let agents_dir = dir.join("agency").join("agents");
    let roles_dir = dir.join("agency").join("roles");

    let agent = agency::find_agent_by_prefix(&agents_dir, agent_hash).ok()?;
    let role = agency::find_role_by_prefix(&roles_dir, &agent.role_id).ok()?;
    Some(role.name.to_lowercase().replace(' ', "-"))
}

/// Find the loop-back targets for a cycle header task.
///
/// A task with `cycle_config` is the cycle header. When it completes, the cycle
/// may iterate. The loop targets are the tasks that depend on the header AND are
/// reachable from the header via `after` edges (forming a cycle).
fn find_cycle_loop_targets(
    header: &Task,
    subgraph_ids: &HashSet<&str>,
    graph: &WorkGraph,
) -> Vec<String> {
    let mut reachable = HashSet::new();
    let mut stack: Vec<String> = header
        .after
        .iter()
        .filter(|a| subgraph_ids.contains(a.as_str()))
        .cloned()
        .collect();

    while let Some(id) = stack.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        if let Some(task) = graph.get_task(&id) {
            for dep in &task.after {
                if subgraph_ids.contains(dep.as_str()) && !reachable.contains(dep) {
                    stack.push(dep.clone());
                }
            }
        }
    }

    graph
        .tasks()
        .filter(|t| {
            subgraph_ids.contains(t.id.as_str())
                && t.id != header.id
                && t.after.iter().any(|dep| dep == &header.id)
                && reachable.contains(&t.id)
        })
        .map(|t| t.id.clone())
        .collect()
}

/// Format a LoopGuard as a human-readable string for LoopEdgeTemplate.
fn format_loop_guard(guard: &LoopGuard) -> String {
    match guard {
        LoopGuard::Always => "always".to_string(),
        LoopGuard::IterationLessThan(n) => format!("iteration < {}", n),
        LoopGuard::TaskStatus { task, status } => format!("task_status:{}:{}", task, status),
    }
}

/// Send a prompt to `claude --print` and return the response text.
fn invoke_claude(prompt: &str) -> Result<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("claude")
        .args(["--print"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start claude CLI. Is it installed?")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes())?;
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Claude CLI failed: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Extract JSON content from an LLM response, stripping markdown fences if present.
fn extract_json_from_response(response: &str) -> String {
    // Try ```json ... ```
    if let Some(start) = response.find("```json") {
        let content_start = start + "```json".len();
        if let Some(end) = response[content_start..].find("```") {
            return response[content_start..content_start + end]
                .trim()
                .to_string();
        }
    }
    // Try generic ``` ... ```
    if let Some(start) = response.find("```") {
        let content_start = start + "```".len();
        let content_start = response[content_start..]
            .find('\n')
            .map(|i| content_start + i + 1)
            .unwrap_or(content_start);
        if let Some(end) = response[content_start..].find("```") {
            return response[content_start..content_start + end]
                .trim()
                .to_string();
        }
    }
    // Try to find raw JSON — prefer whichever delimiter appears first
    let obj_start = response.find('{');
    let arr_start = response.find('[');
    match (arr_start, obj_start) {
        (Some(a), Some(o)) if a < o => {
            // Array starts first
            if let Some(end) = response.rfind(']')
                && end > a {
                    return response[a..=end].to_string();
                }
        }
        (_, Some(o)) => {
            if let Some(end) = response.rfind('}')
                && end > o {
                    return response[o..=end].to_string();
                }
        }
        (Some(a), None) => {
            if let Some(end) = response.rfind(']')
                && end > a {
                    return response[a..=end].to_string();
                }
        }
        _ => {}
    }
    response.trim().to_string()
}

/// Invoke the coordinator's executor to generalize an extracted function using
/// a three-pass LLM approach:
///   1. Identify the ROLE of each task in the workflow
///   2. Rewrite descriptions to be role-based rather than instance-specific
///   3. Identify which {{input.*}} placeholders to create
fn generalize_with_executor(dir: &Path, func: &TraceFunction) -> Result<TraceFunction> {
    let config = workgraph::config::Config::load_or_default(dir);
    let executor = &config.coordinator.executor;

    if executor != "claude" {
        bail!(
            "--generalize requires the 'claude' executor (current: '{}'). \
             Configure with: wg config coordinator.executor claude",
            executor
        );
    }

    // === Pass 1: Role identification ===
    eprintln!("Generalize pass 1/3: identifying task roles...");
    let roles = pass1_identify_roles(func)?;

    // === Pass 2: Description rewriting ===
    eprintln!("Generalize pass 2/3: rewriting descriptions...");
    let rewrites = pass2_rewrite_descriptions(func, &roles)?;

    // === Pass 3: Placeholder identification ===
    eprintln!("Generalize pass 3/3: extracting placeholders...");
    let new_inputs = pass3_identify_placeholders(func, &rewrites)?;

    // === Merge results into the function ===
    let mut result = func.clone();

    // Apply role hints from pass 1
    for template in &mut result.tasks {
        if let Some(role) = roles.get(&template.template_id) {
            template.role_hint = Some(role.clone());
        }
    }

    // Apply rewritten titles and descriptions from pass 2
    for template in &mut result.tasks {
        if let Some(rewrite) = rewrites.get(&template.template_id) {
            template.title = rewrite.title.clone();
            template.description = rewrite.description.clone();
            if !rewrite.deliverables.is_empty() {
                template.deliverables = rewrite.deliverables.clone();
            }
        }
    }

    // Apply the top-level function description if provided
    if let Some(func_rewrite) = rewrites.get("__function__") {
        result.description = func_rewrite.description.clone();
        if !func_rewrite.title.is_empty() {
            result.name = func_rewrite.title.clone();
        }
    }

    // Merge new inputs: keep existing detected params, add LLM-suggested ones
    let existing_names: HashSet<String> = result.inputs.iter().map(|i| i.name.clone()).collect();
    for input in new_inputs {
        if !existing_names.contains(&input.name) {
            result.inputs.push(input);
        }
    }

    // Mark feature_name as required if present and referenced in templates
    let all_text: String = result
        .tasks
        .iter()
        .map(|t| format!("{} {}", t.title, t.description))
        .collect::<Vec<_>>()
        .join(" ");
    for input in &mut result.inputs {
        if all_text.contains(&format!("{{{{input.{}}}}}", input.name)) {
            input.required = true;
        }
    }

    Ok(result)
}

/// Task role classification from pass 1.
#[derive(Debug, Clone, serde::Deserialize)]
struct RoleEntry {
    template_id: String,
    role: String,
}

/// Pass 1: Identify the role of each task based on description, skills, and
/// position in the graph (dependency depth).
fn pass1_identify_roles(func: &TraceFunction) -> Result<HashMap<String, String>> {
    let mut task_summaries = Vec::new();
    for (i, t) in func.tasks.iter().enumerate() {
        let deps = if t.after.is_empty() {
            "root (no dependencies)".to_string()
        } else {
            format!("depends on: {}", t.after.join(", "))
        };
        task_summaries.push(format!(
            "  {}: template_id={}, title={:?}, description={:?}, skills=[{}], {}",
            i + 1,
            t.template_id,
            t.title,
            t.description,
            t.skills.join(", "),
            deps
        ));
    }

    let prompt = format!(
        "You are analyzing a workflow extracted from a completed software project.\n\n\
         Below are the tasks in the workflow. For each task, identify its ROLE in the \
         workflow pattern. Roles describe what function the task serves, not what specific \
         code it touches.\n\n\
         Common roles include (but are not limited to):\n\
         - data-model: defines core types, enums, structs\n\
         - implementation: writes the main business logic\n\
         - integration: connects components, wires things together\n\
         - validator: adds validation, error handling, constraints\n\
         - tests: writes unit or integration tests\n\
         - documentation: writes docs, updates README\n\
         - cli: adds command-line interface or commands\n\
         - refactor: restructures existing code\n\
         - configuration: sets up config, settings, options\n\
         - review: reviews or verifies work from other tasks\n\n\
         Tasks:\n{}\n\n\
         Output ONLY a JSON array of objects, one per task, each with \"template_id\" and \
         \"role\" fields. No explanation, no markdown.\n\n\
         Example: [{{\"template_id\": \"define-types\", \"role\": \"data-model\"}}]",
        task_summaries.join("\n")
    );

    let response = invoke_claude(&prompt)?;
    let json_str = extract_json_from_response(&response);
    let entries: Vec<RoleEntry> = serde_json::from_str(&json_str)
        .context("Pass 1: Failed to parse role identification JSON")?;

    let mut roles = HashMap::new();
    for entry in entries {
        roles.insert(entry.template_id, entry.role);
    }
    Ok(roles)
}

/// Rewritten task description from pass 2.
#[derive(Debug, Clone, serde::Deserialize)]
struct RewriteEntry {
    template_id: String,
    title: String,
    description: String,
    #[serde(default)]
    deliverables: Vec<String>,
}

/// Pass 2: Rewrite task descriptions to be role-based and generic.
///
/// Uses the roles from pass 1 to guide the rewriting. Instance-specific details
/// (file names, type names, specific features) are replaced with role-based
/// descriptions that describe WHAT the task does in the workflow pattern.
fn pass2_rewrite_descriptions(
    func: &TraceFunction,
    roles: &HashMap<String, String>,
) -> Result<HashMap<String, RewriteEntry>> {
    let mut task_summaries = Vec::new();
    for t in &func.tasks {
        let role = roles
            .get(&t.template_id)
            .map(|r| r.as_str())
            .unwrap_or("unknown");
        let deliverables_str = if t.deliverables.is_empty() {
            String::new()
        } else {
            format!(", deliverables={:?}", t.deliverables)
        };
        task_summaries.push(format!(
            "  template_id={}, role={}, original_title={:?}, original_description={:?}{}",
            t.template_id, role, t.title, t.description, deliverables_str
        ));
    }

    let existing_inputs: Vec<String> = func.inputs.iter().map(|i| i.name.clone()).collect();
    let input_note = if existing_inputs.is_empty() {
        "No existing input parameters detected yet.".to_string()
    } else {
        format!(
            "Already-detected input parameters: {}. You may reference these as {{{{input.<name>}}}}.",
            existing_inputs.join(", ")
        )
    };

    let prompt = format!(
        "You are generalizing a workflow template extracted from a completed software project.\n\n\
         The original function: name={:?}, description={:?}\n\n\
         Each task has been classified with a role. Now rewrite each task's title and \
         description to describe the ROLE it plays in the workflow PATTERN, not the specific \
         instance.\n\n\
         Key principles:\n\
         - Extract the PATTERN, not the FOSSIL\n\
         - \"Add FunctionVisibility enum to src/trace_function.rs\" becomes \
           \"Define the core data types needed for {{{{input.feature_name}}}}\"\n\
         - \"Write tests for trace extraction\" becomes \
           \"Write unit and integration tests for {{{{input.feature_name}}}}\"\n\
         - Descriptions should explain what role the task plays, not what specific code to write\n\
         - Use {{{{input.feature_name}}}} for the feature/project being worked on\n\
         - Use {{{{input.source_files}}}} if referencing specific files\n\
         - Use {{{{input.test_command}}}} if referencing test commands\n\
         - Deliverables should also be generalized if present\n\n\
         {}\n\n\
         Tasks to rewrite:\n{}\n\n\
         Output a JSON array of objects with: \"template_id\", \"title\", \"description\", \
         and optionally \"deliverables\" (array of strings).\n\
         Also include one entry with template_id=\"__function__\" for the top-level function \
         name and description.\n\n\
         Output ONLY valid JSON. No explanation, no markdown.",
        func.name, func.description, input_note, task_summaries.join("\n")
    );

    let response = invoke_claude(&prompt)?;
    let json_str = extract_json_from_response(&response);
    let entries: Vec<RewriteEntry> = serde_json::from_str(&json_str)
        .context("Pass 2: Failed to parse description rewrite JSON")?;

    let mut rewrites = HashMap::new();
    for entry in entries {
        rewrites.insert(entry.template_id.clone(), entry);
    }
    Ok(rewrites)
}

/// New input parameter suggested by pass 3.
#[derive(Debug, Clone, serde::Deserialize)]
struct PlaceholderEntry {
    name: String,
    #[serde(rename = "type", default = "default_input_type")]
    input_type: String,
    description: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    example: Option<String>,
}

fn default_input_type() -> String {
    "string".to_string()
}

/// Pass 3: Identify which {{input.*}} placeholders to create from the
/// generalized descriptions.
fn pass3_identify_placeholders(
    func: &TraceFunction,
    rewrites: &HashMap<String, RewriteEntry>,
) -> Result<Vec<FunctionInput>> {
    // Collect all generalized text to scan for placeholders
    let mut all_text = String::new();
    for t in &func.tasks {
        if let Some(rw) = rewrites.get(&t.template_id) {
            all_text.push_str(&rw.title);
            all_text.push('\n');
            all_text.push_str(&rw.description);
            all_text.push('\n');
            for d in &rw.deliverables {
                all_text.push_str(d);
                all_text.push('\n');
            }
        }
    }
    if let Some(rw) = rewrites.get("__function__") {
        all_text.push_str(&rw.description);
        all_text.push('\n');
    }

    let existing_inputs: Vec<String> = func.inputs.iter().map(|i| i.name.clone()).collect();

    let prompt = format!(
        "You are finalizing a generalized workflow template.\n\n\
         The rewritten task descriptions below contain {{{{input.<name>}}}} placeholders. \
         Identify ALL unique placeholders used and define their input parameters.\n\n\
         Already-defined inputs (do NOT re-define these): {}\n\n\
         Generalized text:\n{}\n\n\
         For each NEW placeholder (not already defined), output a JSON array of objects with:\n\
         - \"name\": the placeholder name (without input. prefix)\n\
         - \"type\": one of \"string\", \"text\", \"file_list\", \"number\", \"url\", \"enum\"\n\
         - \"description\": what this input represents\n\
         - \"required\": boolean\n\
         - \"example\": an example value (string)\n\n\
         If no new placeholders are needed, output an empty array: []\n\n\
         Output ONLY valid JSON. No explanation, no markdown.",
        if existing_inputs.is_empty() {
            "none".to_string()
        } else {
            existing_inputs.join(", ")
        },
        all_text
    );

    let response = invoke_claude(&prompt)?;
    let json_str = extract_json_from_response(&response);
    let entries: Vec<PlaceholderEntry> = serde_json::from_str(&json_str)
        .context("Pass 3: Failed to parse placeholder identification JSON")?;

    let mut inputs = Vec::new();
    for entry in entries {
        let input_type = match entry.input_type.as_str() {
            "string" => InputType::String,
            "text" => InputType::Text,
            "file_list" => InputType::FileList,
            "number" => InputType::Number,
            "url" => InputType::Url,
            "enum" => InputType::Enum,
            "json" => InputType::Json,
            _ => InputType::String,
        };
        inputs.push(FunctionInput {
            name: entry.name,
            input_type,
            description: entry.description,
            required: entry.required,
            default: None,
            example: entry.example.map(serde_yaml::Value::String),
            min: None,
            max: None,
            values: None,
        });
    }
    Ok(inputs)
}

/// Extract YAML content from an executor response, stripping markdown fences if present.
#[cfg(test)]
fn extract_yaml_from_response(response: &str) -> String {
    if let Some(start) = response.find("```yaml") {
        let content_start = start + "```yaml".len();
        if let Some(end) = response[content_start..].find("```") {
            return response[content_start..content_start + end]
                .trim()
                .to_string();
        }
    }
    if let Some(start) = response.find("```") {
        let content_start = start + "```".len();
        let content_start = response[content_start..]
            .find('\n')
            .map(|i| content_start + i + 1)
            .unwrap_or(content_start);
        if let Some(end) = response[content_start..].find("```") {
            return response[content_start..content_start + end]
                .trim()
                .to_string();
        }
    }
    response.trim().to_string()
}

/// Run multi-trace generative extraction (`wg trace extract --generative`).
///
/// Analyzes multiple completed traces, aligns their topologies to find common
/// vs variable structure, synthesizes a planning prompt and structural constraints,
/// and produces a version 2 (generative) function.
pub fn run_generative(
    dir: &Path,
    task_ids: &[String],
    name: Option<&str>,
    output: Option<&str>,
    force: bool,
    include_evaluations: bool,
) -> Result<()> {
    if task_ids.len() < 2 {
        bail!("--generative requires at least 2 task IDs for multi-trace extraction");
    }

    let (graph, _path) = super::load_workgraph(dir)?;

    for tid in task_ids {
        let task = graph.get_task_or_err(tid)?;
        if task.status != Status::Done {
            bail!(
                "Task '{}' is '{}'. --generative requires all tasks to be Done.",
                tid,
                task.status
            );
        }
    }

    let mut traces: Vec<Vec<&Task>> = task_ids
        .iter()
        .map(|tid| collect_subgraph(tid, &graph))
        .collect();

    // Filter out coordinator-generated noise (evaluate-*, assign-*) unless opted in
    if !include_evaluations {
        for trace in &mut traces {
            trace.retain(|t| !is_coordinator_noise(t));
        }
    }

    let task_counts: Vec<usize> = traces.iter().map(|t| t.len()).collect();
    let min_tasks = *task_counts.iter().min().unwrap() as u32;
    let max_tasks = *task_counts.iter().max().unwrap() as u32;

    // Check if all topologies are identical
    if task_counts.iter().all(|&c| c == task_counts[0]) {
        let skill_patterns: Vec<Vec<Vec<String>>> = traces
            .iter()
            .map(|trace| {
                trace
                    .iter()
                    .map(|t| {
                        let mut s = t.skills.clone();
                        s.sort();
                        s
                    })
                    .collect()
            })
            .collect();
        if skill_patterns.iter().all(|p| p == &skill_patterns[0]) {
            eprintln!(
                "All {} traces have identical topology. Falling back to static extraction.",
                task_ids.len()
            );
            return run(dir, &task_ids[0], name, true, false, output, force, include_evaluations);
        }
    }

    // Collect skills across all traces
    let skill_sets: Vec<HashSet<String>> = traces
        .iter()
        .map(|trace| {
            trace
                .iter()
                .flat_map(|t| t.skills.iter().cloned())
                .collect()
        })
        .collect();

    let all_skills: HashSet<String> = skill_sets.iter().flat_map(|s| s.iter().cloned()).collect();

    let common_skills: Vec<String> = if let Some(first) = skill_sets.first() {
        let mut skills: Vec<String> = first
            .iter()
            .filter(|s| skill_sets.iter().all(|set| set.contains(*s)))
            .cloned()
            .collect();
        skills.sort();
        skills
    } else {
        vec![]
    };

    let tag_sets: Vec<HashSet<String>> = traces
        .iter()
        .map(|trace| {
            trace
                .iter()
                .flat_map(|t| t.tags.iter().cloned())
                .collect()
        })
        .collect();

    let common_tags: Vec<String> = if let Some(first) = tag_sets.first() {
        let mut tags: Vec<String> = first
            .iter()
            .filter(|t| tag_sets.iter().all(|set| set.contains(*t)))
            .cloned()
            .collect();
        tags.sort();
        tags
    } else {
        vec![]
    };

    // Find median trace for static fallback
    let mut sorted_indices: Vec<usize> = (0..traces.len()).collect();
    sorted_indices.sort_by_key(|&i| traces[i].len());
    let median_idx = sorted_indices[sorted_indices.len() / 2];

    let max_depth = traces
        .iter()
        .map(|trace| compute_max_depth(trace))
        .max()
        .unwrap_or(0) as u32;

    let has_cycles = traces
        .iter()
        .any(|trace| trace.iter().any(|t| t.cycle_config.is_some()));

    // Build static fallback from median trace
    let median_tasks = &traces[median_idx];
    let median_root = &task_ids[median_idx];
    let median_ids: HashSet<&str> = median_tasks.iter().map(|t| t.id.as_str()).collect();
    let fallback_templates: Vec<TaskTemplate> = median_tasks
        .iter()
        .map(|t| build_template(t, median_root, &median_ids, dir, &graph))
        .collect();

    // Synthesize planning prompt
    let all_skills_sorted = {
        let mut s: Vec<String> = all_skills.iter().cloned().collect();
        s.sort();
        s
    };
    let planning_description = format!(
        "Analyze the inputs and produce a task graph in workgraph-yaml format.\n\n\
         This function was extracted from {} similar traces with {} to {} tasks each.\n\n\
         Common skills observed: {}\n\
         All skills observed: {}\n\n\
         The generated plan should follow similar patterns but adapt the number and \
         structure of tasks to the specific inputs provided.\n\n\
         Each task in the output should have: template_id, title, description, skills, \
         and after (dependencies).",
        task_ids.len(),
        min_tasks,
        max_tasks,
        if common_skills.is_empty() { "none".to_string() } else { common_skills.join(", ") },
        if all_skills_sorted.is_empty() { "none".to_string() } else { all_skills_sorted.join(", ") }
    );

    let planner_template = TaskTemplate {
        template_id: "planner".to_string(),
        title: "Plan task graph".to_string(),
        description: planning_description,
        skills: vec!["analysis".to_string(), "planning".to_string()],
        after: vec![],
        loops_to: vec![],
        role_hint: Some("architect".to_string()),
        deliverables: vec![],
        verify: None,
        tags: vec![],
    };

    let constraints = StructuralConstraints {
        min_tasks: Some(min_tasks),
        max_tasks: Some(max_tasks),
        required_skills: common_skills.clone(),
        max_depth: if max_depth > 0 { Some(max_depth) } else { None },
        allow_cycles: has_cycles,
        max_total_iterations: if has_cycles {
            traces.iter().flat_map(|trace| {
                trace.iter().filter_map(|t| t.cycle_config.as_ref().map(|c| c.max_iterations))
            }).max()
        } else {
            None
        },
        required_phases: common_skills.clone(),
        forbidden_patterns: vec![],
    };

    let all_tasks_flat: Vec<&Task> = traces.iter().flat_map(|t| t.iter().copied()).collect();
    let suggested_inputs = detect_parameters(&all_tasks_flat);

    let func_id = name
        .map(|s| s.to_string())
        .unwrap_or_else(|| sanitize_id(&format!("{}-generative", task_ids[0])));

    let now = Utc::now().to_rfc3339();
    let extracted_from: Vec<ExtractionSource> = task_ids
        .iter()
        .map(|tid| ExtractionSource {
            task_id: tid.clone(),
            run_id: None,
            timestamp: now.clone(),
        })
        .collect();

    let outputs = build_outputs(median_tasks);

    let functions_dir = if let Some(out) = output {
        PathBuf::from(out).parent().unwrap_or(Path::new(".")).to_path_buf()
    } else {
        trace_function::functions_dir(dir)
    };

    if output.is_none() {
        let target_path = functions_dir.join(format!("{}.yaml", func_id));
        if target_path.exists() && !force {
            bail!(
                "Function '{}' already exists at {}. Use --force to overwrite.",
                func_id,
                target_path.display()
            );
        }
    }

    let func = TraceFunction {
        kind: "trace-function".to_string(),
        version: 2,
        id: func_id.clone(),
        name: name.map(title_case).unwrap_or_else(|| title_case(&func_id)),
        description: format!(
            "Generative function synthesized from {} traces ({} to {} tasks)",
            task_ids.len(), min_tasks, max_tasks
        ),
        extracted_from,
        extracted_by: None,
        extracted_at: Some(now),
        tags: common_tags,
        inputs: suggested_inputs,
        tasks: fallback_templates,
        outputs,
        planning: Some(PlanningConfig {
            planner_template,
            output_format: "workgraph-yaml".to_string(),
            static_fallback: true,
            validate_plan: true,
        }),
        constraints: Some(constraints),
        memory: None,
        visibility: FunctionVisibility::Internal,
        redacted_fields: vec![],
    };

    trace_function::validate_function(&func)
        .context("Generated function failed validation")?;

    let saved_path = if let Some(out) = output {
        let out_path = PathBuf::from(out);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(&func)?;
        std::fs::write(&out_path, yaml)?;
        out_path
    } else {
        trace_function::save_function(&func, &functions_dir)?
    };

    println!(
        "Extracted generative function '{}' from {} traces",
        func_id,
        task_ids.len()
    );
    println!("Version: 2 (generative)");
    println!("Fallback tasks: {}", func.tasks.len());
    println!("Constraints: {} to {} tasks", min_tasks, max_tasks);
    if !common_skills.is_empty() {
        println!("Required skills: {}", common_skills.join(", "));
    }
    println!();
    println!("Saved to: {}", saved_path.display());

    Ok(())
}

/// Compute the maximum dependency depth in a set of tasks.
fn compute_max_depth(tasks: &[&Task]) -> usize {
    let ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let mut depths: HashMap<&str, usize> = HashMap::new();

    for task in tasks {
        depths.insert(task.id.as_str(), 0);
    }

    let mut changed = true;
    while changed {
        changed = false;
        for task in tasks {
            let max_dep = task
                .after
                .iter()
                .filter(|d| ids.contains(d.as_str()))
                .map(|d| depths.get(d.as_str()).copied().unwrap_or(0) + 1)
                .max()
                .unwrap_or(0);

            if max_dep > *depths.get(task.id.as_str()).unwrap_or(&0) {
                depths.insert(task.id.as_str(), max_dep);
                changed = true;
            }
        }
    }

    depths.values().copied().max().unwrap_or(0)
}

/// Build FunctionOutput entries from task artifacts.
fn build_outputs(tasks: &[&Task]) -> Vec<FunctionOutput> {
    let mut outputs = Vec::new();
    for task in tasks {
        if !task.artifacts.is_empty() {
            let template_id = sanitize_id(&task.id);
            outputs.push(FunctionOutput {
                name: format!("{}_artifacts", template_id.replace('-', "_")),
                description: format!("Artifacts produced by {}", task.title),
                from_task: template_id,
                field: "artifacts".to_string(),
            });
        }
    }
    outputs
}

/// Detect parameters from task descriptions using heuristics.
///
/// Scans task titles and descriptions for instance-specific values:
/// - Task IDs → suggest as feature_name
/// - File paths → suggest as source_files
/// - URLs → suggest as url parameters
/// - Numbers → suggest as numeric parameters
/// - Commands (cargo test, npm test, etc.) → suggest as test_command
fn detect_parameters(tasks: &[&Task]) -> Vec<FunctionInput> {
    let mut inputs = Vec::new();
    let mut seen_names = HashSet::new();

    // Collect all text to scan
    let mut all_text = String::new();
    for task in tasks {
        all_text.push_str(&task.title);
        all_text.push('\n');
        if let Some(ref desc) = task.description {
            all_text.push_str(desc);
            all_text.push('\n');
        }
    }

    // Detect feature name from root task ID pattern.
    // Strip common verb prefixes (impl-, add-, fix-, etc.) to get the feature name.
    if let Some(first) = tasks.first() {
        let feature = derive_feature_name(&first.id);
        if !feature.is_empty() && !seen_names.contains("feature_name") {
            inputs.push(FunctionInput {
                name: "feature_name".to_string(),
                input_type: InputType::String,
                description: "Short name for the feature (used as task ID prefix)".to_string(),
                required: true,
                default: None,
                example: Some(serde_yaml::Value::String(feature)),
                min: None,
                max: None,
                values: None,
            });
            seen_names.insert("feature_name".to_string());
        }
    }

    // Detect source_files only from task artifacts (explicitly recorded output files),
    // not from every file path mentioned in descriptions.
    if !seen_names.contains("source_files") {
        let mut artifact_paths: Vec<String> = tasks
            .iter()
            .flat_map(|t| t.artifacts.iter().cloned())
            .collect();
        artifact_paths.sort();
        artifact_paths.dedup();
        if !artifact_paths.is_empty() {
            inputs.push(FunctionInput {
                name: "source_files".to_string(),
                input_type: InputType::FileList,
                description: "Key source files to modify".to_string(),
                required: false,
                default: Some(serde_yaml::Value::Sequence(Vec::new())),
                example: Some(serde_yaml::Value::Sequence(
                    artifact_paths
                        .iter()
                        .map(|p| serde_yaml::Value::String(p.clone()))
                        .collect(),
                )),
                min: None,
                max: None,
                values: None,
            });
            seen_names.insert("source_files".to_string());
        }
    }

    // Detect URLs
    let urls: Vec<String> = extract_urls(&all_text);
    for (i, url) in urls.iter().enumerate() {
        let param_name = if i == 0 {
            "url".to_string()
        } else {
            format!("url_{}", i + 1)
        };
        if !seen_names.contains(&param_name) {
            inputs.push(FunctionInput {
                name: param_name.clone(),
                input_type: InputType::Url,
                description: "URL reference".to_string(),
                required: false,
                default: None,
                example: Some(serde_yaml::Value::String(url.clone())),
                min: None,
                max: None,
                values: None,
            });
            seen_names.insert(param_name);
        }
    }

    // Detect test/build commands — only actual CLI commands (known prefixes or backtick-quoted)
    let commands = extract_cli_commands(&all_text);
    if !commands.is_empty() && !seen_names.contains("test_command") {
        inputs.push(FunctionInput {
            name: "test_command".to_string(),
            input_type: InputType::String,
            description: "Command to verify the implementation".to_string(),
            required: false,
            default: Some(serde_yaml::Value::String(
                commands.first().unwrap().clone(),
            )),
            example: None,
            min: None,
            max: None,
            values: None,
        });
        seen_names.insert("test_command".to_string());
    }

    // Detect numbers only when they appear near parameterizable keywords
    // (e.g., "--max-iterations 3", "threshold 0.8", "limit 10").
    // Skip standalone numbers with no semantic context.
    let contextual_numbers = extract_contextual_numbers(&all_text);
    for (param_name, num) in contextual_numbers.iter() {
        if !seen_names.contains(param_name) {
            inputs.push(FunctionInput {
                name: param_name.clone(),
                input_type: InputType::Number,
                description: format!("Numeric parameter ({})", param_name.replace('_', " ")),
                required: false,
                default: Some(serde_yaml::Value::Number(
                    serde_yaml::Number::from(*num as i64),
                )),
                example: None,
                min: None,
                max: None,
                values: None,
            });
            seen_names.insert(param_name.clone());
        }
    }

    inputs
}


/// Extract URLs from text.
fn extract_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();

    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| c == ',' || c == ';' || c == '"' || c == '\'' || c == '(' || c == ')');
        if (word.starts_with("http://") || word.starts_with("https://")) && word.len() > 10
            && seen.insert(word.to_string()) {
                urls.push(word.to_string());
            }
    }

    urls
}

/// Extract actual CLI commands from text.
///
/// Only extracts commands that start with known CLI tool prefixes or appear
/// inside backticks. Does NOT extract arbitrary sentence fragments.
fn extract_cli_commands(text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut seen = HashSet::new();

    // Known CLI command prefixes to look for in plain text
    let command_prefixes = [
        "cargo test",
        "cargo build",
        "cargo clippy",
        "cargo check",
        "cargo install",
        "cargo run",
        "npm test",
        "npm run",
        "npm install",
        "yarn test",
        "yarn build",
        "pytest",
        "python -m pytest",
        "go test",
        "go build",
        "make test",
        "make check",
        "make build",
        "make install",
        "git ",
        "wg ",
    ];

    let text_lower = text.to_lowercase();
    for prefix in &command_prefixes {
        let mut search_from = 0;
        while let Some(pos) = text_lower[search_from..].find(prefix) {
            let abs_pos = search_from + pos;
            // Only match at word boundary (start of line or preceded by whitespace/punctuation)
            if abs_pos > 0 {
                let prev = text.as_bytes()[abs_pos - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' {
                    search_from = abs_pos + 1;
                    continue;
                }
            }
            // Extract to end of line
            let rest = &text[abs_pos..];
            let end = rest.find('\n').unwrap_or(rest.len());
            let cmd = rest[..end].trim().to_string();
            // Trim trailing punctuation that's not part of the command
            let cmd = cmd.trim_end_matches(['.', ',', ';']);
            if !cmd.is_empty() && seen.insert(cmd.to_string()) {
                commands.push(cmd.to_string());
            }
            search_from = abs_pos + end;
        }
    }

    // Also extract backtick-quoted commands
    for cap in text.split('`').collect::<Vec<_>>().chunks(2) {
        if cap.len() == 2 {
            let inside = cap[1].trim();
            // Only treat as command if it starts with a known CLI tool
            let known_tools = [
                "cargo", "npm", "yarn", "pytest", "python", "go", "make",
                "git", "wg", "docker", "kubectl", "rustc", "gcc", "g++",
                "javac", "mvn", "gradle", "pip", "ruby", "node", "deno", "bun",
            ];
            let first_word = inside.split_whitespace().next().unwrap_or("");
            if known_tools.contains(&first_word) && seen.insert(inside.to_string())
            {
                commands.push(inside.to_string());
            }
        }
    }

    commands
}

/// Extract numbers only when they appear near parameterizable keywords.
///
/// Returns (parameter_name, value) pairs. Only extracts numbers that appear
/// within 3 words of a keyword like "threshold", "max", "limit", etc.
/// Skips standalone numbers that have no semantic context.
/// Caps at 3 results.
fn extract_contextual_numbers(text: &str) -> Vec<(String, f64)> {
    let mut results = Vec::new();
    let mut seen_names = HashSet::new();

    // Specific keywords sorted longest-first to prefer more specific matches
    // (e.g., "iterations" matches before "max" in "max-iterations")
    let keywords = [
        "threshold",
        "iterations",
        "concurrency",
        "interval",
        "attempts",
        "retries",
        "timeout",
        "workers",
        "agents",
        "limit",
        "count",
        "batch",
        "depth",
        "size",
        "max",
        "min",
    ];

    let words: Vec<&str> = text.split_whitespace().collect();

    for (i, word) in words.iter().enumerate() {
        let cleaned =
            word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != '-');
        if cleaned.is_empty() {
            continue;
        }
        // Skip version-like patterns (1.2.3)
        if cleaned.matches('.').count() > 1 {
            continue;
        }
        if let Ok(n) = cleaned.parse::<f64>() {
            if n == 0.0 || n == 1.0 || !n.is_finite() {
                continue;
            }
            // Match --flag-name patterns like --max-iterations 3
            // Prefer the most specific keyword found in the flag
            let flag_keyword = if i > 0 {
                let prev = words[i - 1];
                if prev.starts_with("--") {
                    let flag = prev.trim_start_matches('-').to_lowercase();
                    keywords.iter().find(|&&kw| flag.contains(kw)).copied()
                } else {
                    None
                }
            } else {
                None
            };

            // Also check the word immediately before the number (e.g., "retries 3")
            let adjacent_keyword = if flag_keyword.is_none() && i > 0 {
                let prev = words[i - 1].to_lowercase();
                let prev_clean = prev.trim_matches(|c: char| !c.is_alphanumeric());
                keywords
                    .iter()
                    .find(|&&kw| prev_clean == kw || prev_clean.contains(kw))
                    .copied()
            } else {
                None
            };

            // Fall back to a wider context window (3 words)
            let context_keyword = if flag_keyword.is_none() && adjacent_keyword.is_none() {
                let start = i.saturating_sub(3);
                let end = (i + 4).min(words.len());
                let context = words[start..end].join(" ").to_lowercase();
                keywords.iter().find(|&&kw| context.contains(kw)).copied()
            } else {
                None
            };

            let matched = flag_keyword.or(adjacent_keyword).or(context_keyword);

            if let Some(keyword) = matched {
                let param_name = keyword.replace('-', "_");
                if seen_names.insert(param_name.clone()) {
                    results.push((param_name, n));
                }
            }
        }
    }

    // Cap at 3 numeric parameters
    results.truncate(3);
    results
}

/// Derive a feature name from a root task ID by stripping common verb prefixes.
///
/// Examples:
///   "impl-auth-system" → "auth-system"
///   "add-dark-mode"    → "dark-mode"
///   "fix-login-bug"    → "login-bug"
///   "auth-system"      → "auth-system" (no verb prefix)
fn derive_feature_name(task_id: &str) -> String {
    let verb_prefixes = [
        "impl-", "implement-", "add-", "fix-", "create-", "build-", "setup-", "update-",
        "refactor-", "design-", "write-", "define-", "configure-", "enable-", "remove-",
    ];

    for prefix in &verb_prefixes {
        if let Some(rest) = task_id.strip_prefix(prefix)
            && !rest.is_empty() {
                return rest.to_string();
            }
    }

    // No verb prefix found — return the full ID
    task_id.to_string()
}

/// Sanitize a string into a valid kebab-case ID.
fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Strip a common prefix from a task ID to produce a shorter template ID.
/// If the task ID starts with the root ID + "-", strip that prefix.
/// Otherwise return the sanitized task ID.
fn strip_prefix(task_id: &str, root_id: &str) -> String {
    let prefix = format!("{}-", root_id);
    if task_id.starts_with(&prefix) && task_id.len() > prefix.len() {
        sanitize_id(&task_id[prefix.len()..])
    } else {
        sanitize_id(task_id)
    }
}

/// Convert a kebab-case string to Title Case.
fn title_case(s: &str) -> String {
    s.split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    upper + &chars.collect::<String>()
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
            status: Status::Done,
            ..Task::default()
        }
    }

    fn setup_graph(dir: &Path, graph: &WorkGraph) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("graph.jsonl");
        save_graph(graph, &path).unwrap();
    }

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("impl-feature"), "impl-feature");
        assert_eq!(sanitize_id("My Feature!"), "my-feature");
        assert_eq!(sanitize_id("---test---"), "test");
    }

    #[test]
    fn test_strip_prefix() {
        assert_eq!(strip_prefix("root-sub1", "root"), "sub1");
        assert_eq!(strip_prefix("root-sub1-detail", "root"), "sub1-detail");
        assert_eq!(strip_prefix("other-task", "root"), "other-task");
        assert_eq!(strip_prefix("root", "root"), "root");
    }

    #[test]
    fn test_title_case() {
        assert_eq!(title_case("impl-feature"), "Impl Feature");
        assert_eq!(title_case("hello"), "Hello");
        assert_eq!(title_case("a-b-c"), "A B C");
    }

    #[test]
    fn test_extract_urls() {
        let text = "Check https://api.example.com/v1 and http://localhost:3000/test";
        let urls = extract_urls(text);
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"https://api.example.com/v1".to_string()));
        assert!(urls.contains(&"http://localhost:3000/test".to_string()));
    }

    #[test]
    fn test_extract_cli_commands() {
        let text = "After changes, run:\ncargo test --lib\nand verify output";
        let commands = extract_cli_commands(text);
        assert!(!commands.is_empty(), "Should find at least one command");
        assert!(
            commands.iter().any(|c| c.starts_with("cargo test")),
            "Should find 'cargo test' command, got: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_cli_commands_rejects_sentence_fragments() {
        let text = "Implement the configuration system for the project.\nEnsure cargo test passes.";
        let commands = extract_cli_commands(text);
        // Should find "cargo test passes" or similar, but NOT "Implement the configuration..."
        assert!(
            commands.iter().all(|c| {
                let first = c.split_whitespace().next().unwrap_or("");
                ["cargo", "npm", "yarn", "pytest", "python", "go", "make", "git", "wg"]
                    .contains(&first)
            }),
            "All commands should start with known CLI tools, got: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_cli_commands_backtick_quoted() {
        let text = "Run `cargo test --lib` to verify and `npm run build` for frontend";
        let commands = extract_cli_commands(text);
        assert!(
            commands.iter().any(|c| c == "cargo test --lib"),
            "Should find backtick-quoted cargo command, got: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_contextual_numbers() {
        let text = "Set threshold to 0.8 and max retries to 3";
        let numbers = extract_contextual_numbers(text);
        assert!(
            numbers.iter().any(|(name, _)| name == "threshold"),
            "Should find 'threshold' parameter, got: {:?}",
            numbers
        );
        assert!(
            numbers.iter().any(|(name, _)| name == "retries"),
            "Should find 'retries' parameter, got: {:?}",
            numbers
        );
    }

    #[test]
    fn test_extract_contextual_numbers_skips_random() {
        let text = "Task has 5 subtasks across 3 files and took 42 seconds to run";
        let numbers = extract_contextual_numbers(text);
        assert!(
            numbers.is_empty(),
            "Should not extract random numbers without keyword context, got: {:?}",
            numbers
        );
    }

    #[test]
    fn test_extract_contextual_numbers_flag_syntax() {
        let text = "Run with --max-iterations 5 and --batch-size 32";
        let numbers = extract_contextual_numbers(text);
        assert!(
            numbers.iter().any(|(name, val)| name == "iterations" && *val == 5.0),
            "Should extract iterations from --max-iterations flag, got: {:?}",
            numbers
        );
        // --batch-size matches "batch" keyword
        assert!(
            numbers.iter().any(|(name, val)| name == "batch" && *val == 32.0),
            "Should extract batch from --batch-size flag, got: {:?}",
            numbers
        );
    }

    #[test]
    fn test_extract_contextual_numbers_ignores_versions() {
        let text = "Version 1.2.3 released";
        let numbers = extract_contextual_numbers(text);
        assert!(numbers.is_empty());
    }

    #[test]
    fn test_derive_feature_name() {
        assert_eq!(derive_feature_name("impl-auth-system"), "auth-system");
        assert_eq!(derive_feature_name("add-dark-mode"), "dark-mode");
        assert_eq!(derive_feature_name("fix-login-bug"), "login-bug");
        assert_eq!(derive_feature_name("auth-system"), "auth-system");
        assert_eq!(derive_feature_name("create-api"), "api");
    }

    #[test]
    fn test_extract_single_done_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let mut task = make_task("impl-config", "Implement config");
        task.description = Some("Add global config at src/config.rs".to_string());
        task.artifacts = vec!["src/config.rs".to_string()];
        graph.add_node(Node::Task(task));
        setup_graph(&dir, &graph);

        let result = run(&dir, "impl-config", Some("my-func"), false, false, None, false, false);
        assert!(result.is_ok());

        // Verify function was saved
        let func_path = dir.join("functions").join("my-func.yaml");
        assert!(func_path.exists());

        let func = trace_function::load_function(&func_path).unwrap();
        assert_eq!(func.id, "my-func");
        assert_eq!(func.tasks.len(), 1);
        assert_eq!(func.tasks[0].template_id, "impl-config");
    }

    #[test]
    fn test_extract_rejects_non_done_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "t1".to_string(),
            title: "Open task".to_string(),
            status: Status::Open,
            ..Task::default()
        }));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", None, false, false, None, false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("done"));
    }

    #[test]
    fn test_extract_with_subgraph() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let root = Task {
            id: "root".to_string(),
            title: "Root task".to_string(),
            status: Status::Done,
            ..Task::default()
        };
        let child1 = Task {
            id: "root-child1".to_string(),
            title: "Child 1".to_string(),
            status: Status::Done,
            after: vec!["root".to_string()],
            ..Task::default()
        };
        let child2 = Task {
            id: "root-child2".to_string(),
            title: "Child 2".to_string(),
            status: Status::Done,
            after: vec!["root-child1".to_string()],
            ..Task::default()
        };
        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(child1));
        graph.add_node(Node::Task(child2));
        setup_graph(&dir, &graph);

        let result = run(&dir, "root", Some("subgraph-func"), true, false, None, false, false);
        assert!(result.is_ok());

        let func_path = dir.join("functions").join("subgraph-func.yaml");
        let func = trace_function::load_function(&func_path).unwrap();
        assert_eq!(func.tasks.len(), 3);

        // Check that after references are remapped to template IDs
        let child1_tmpl = func.tasks.iter().find(|t| t.template_id == "child1").unwrap();
        assert_eq!(child1_tmpl.after, vec!["root"]);

        let child2_tmpl = func.tasks.iter().find(|t| t.template_id == "child2").unwrap();
        assert_eq!(child2_tmpl.after, vec!["child1"]);
    }

    #[test]
    fn test_extract_force_overwrite() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        // First extraction
        run(&dir, "t1", Some("overwrite-test"), false, false, None, false, false).unwrap();

        // Second without force should fail
        let result = run(&dir, "t1", Some("overwrite-test"), false, false, None, false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        // With force should succeed
        let result = run(&dir, "t1", Some("overwrite-test"), false, false, None, true, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_extract_custom_output_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        let out_path = tmp.path().join("custom").join("output.yaml");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        let result = run(
            &dir,
            "t1",
            Some("custom-out"),
            false,
            false,
            Some(out_path.to_str().unwrap()),
            false,
            false,
        );
        assert!(result.is_ok());
        assert!(out_path.exists());
    }

    #[test]
    fn test_extract_generalize_warns() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        // Should succeed but print warning (we just test it doesn't error)
        let result = run(&dir, "t1", Some("gen-test"), false, true, None, false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_detect_parameters_with_description() {
        let task = Task {
            id: "impl-auth".to_string(),
            title: "Implement auth".to_string(),
            description: Some(
                "Add authentication to src/auth.rs and src/main.rs. Run cargo test auth to verify."
                    .to_string(),
            ),
            status: Status::Done,
            artifacts: vec!["src/auth.rs".to_string(), "src/main.rs".to_string()],
            ..Task::default()
        };

        let params = detect_parameters(&[&task]);

        // Should detect feature_name (derived: "auth" from "impl-auth")
        let feature = params.iter().find(|p| p.name == "feature_name");
        assert!(feature.is_some(), "Should detect feature_name");
        let example = feature.unwrap().example.as_ref().unwrap();
        assert_eq!(
            example,
            &serde_yaml::Value::String("auth".to_string()),
            "feature_name should be 'auth' (stripped 'impl-' prefix)"
        );
        // Should detect source_files from artifacts (not from description text)
        assert!(params.iter().any(|p| p.name == "source_files"));
        // Should detect test_command
        assert!(params.iter().any(|p| p.name == "test_command"));
    }

    #[test]
    fn test_detect_parameters_no_source_files_without_artifacts() {
        let task = Task {
            id: "impl-auth".to_string(),
            title: "Implement auth".to_string(),
            description: Some(
                "Add authentication to src/auth.rs and src/main.rs."
                    .to_string(),
            ),
            status: Status::Done,
            // No artifacts — file paths in description should NOT become source_files
            ..Task::default()
        };

        let params = detect_parameters(&[&task]);
        assert!(
            !params.iter().any(|p| p.name == "source_files"),
            "source_files should NOT be detected from description text alone"
        );
    }

    #[test]
    fn test_detect_parameters_no_random_numbers() {
        let task = Task {
            id: "impl-feature".to_string(),
            title: "Implement feature with 5 components".to_string(),
            description: Some(
                "This task has 3 subtasks. The implementation took 42 lines of code across 7 files."
                    .to_string(),
            ),
            status: Status::Done,
            ..Task::default()
        };

        let params = detect_parameters(&[&task]);
        // Should NOT extract random numbers like 5, 3, 42, 7 as parameters
        assert!(
            !params.iter().any(|p| p.input_type == InputType::Number),
            "Should not extract random numbers without keyword context, got: {:?}",
            params.iter().filter(|p| p.input_type == InputType::Number).map(|p| &p.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_detect_parameters_with_url() {
        let task = Task {
            id: "fetch-data".to_string(),
            title: "Fetch data".to_string(),
            description: Some(
                "Download data from https://api.example.com/v2/data endpoint".to_string(),
            ),
            status: Status::Done,
            ..Task::default()
        };

        let params = detect_parameters(&[&task]);
        assert!(params.iter().any(|p| p.name == "url" && p.input_type == InputType::Url));
    }

    #[test]
    fn test_collect_subgraph_standalone() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("alone", "Alone")));
        let sub = collect_subgraph("alone", &graph);
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].id, "alone");
    }

    #[test]
    fn test_collect_subgraph_chain() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "A")));
        graph.add_node(Node::Task(Task {
            id: "b".to_string(),
            title: "B".to_string(),
            status: Status::Done,
            after: vec!["a".to_string()],
            ..Task::default()
        }));
        graph.add_node(Node::Task(Task {
            id: "c".to_string(),
            title: "C".to_string(),
            status: Status::Done,
            after: vec!["b".to_string()],
            ..Task::default()
        }));

        let sub = collect_subgraph("a", &graph);
        assert_eq!(sub.len(), 3);
    }

    #[test]
    fn test_collect_subgraph_no_external_deps() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("root", "Root")));
        graph.add_node(Node::Task(Task {
            id: "child".to_string(),
            title: "Child".to_string(),
            status: Status::Done,
            after: vec!["root".to_string()],
            ..Task::default()
        }));
        // External task not in subgraph
        graph.add_node(Node::Task(Task {
            id: "external".to_string(),
            title: "External".to_string(),
            status: Status::Done,
            after: vec!["unrelated".to_string()],
            ..Task::default()
        }));

        let sub = collect_subgraph("root", &graph);
        assert_eq!(sub.len(), 2); // root + child, not external
    }

    #[test]
    fn test_extract_yaml_from_response_fenced() {
        let response = "Here is the result:\n```yaml\nkind: trace-function\nid: test\n```\nDone.";
        let yaml = extract_yaml_from_response(response);
        assert_eq!(yaml, "kind: trace-function\nid: test");
    }

    #[test]
    fn test_extract_yaml_from_response_bare() {
        let response = "kind: trace-function\nid: test\n";
        let yaml = extract_yaml_from_response(response);
        assert_eq!(yaml, "kind: trace-function\nid: test");
    }

    #[test]
    fn test_extract_yaml_from_response_generic_fence() {
        let response = "```\nkind: trace-function\nid: test\n```";
        let yaml = extract_yaml_from_response(response);
        assert_eq!(yaml, "kind: trace-function\nid: test");
    }

    #[test]
    fn test_extract_json_from_response_fenced() {
        let response = "Here is the result:\n```json\n[{\"template_id\": \"t1\", \"role\": \"data-model\"}]\n```\nDone.";
        let json = extract_json_from_response(response);
        assert_eq!(json, r#"[{"template_id": "t1", "role": "data-model"}]"#);
    }

    #[test]
    fn test_extract_json_from_response_bare_object() {
        let response = "Sure, here is the result: {\"name\": \"test\"} end";
        let json = extract_json_from_response(response);
        assert_eq!(json, r#"{"name": "test"}"#);
    }

    #[test]
    fn test_extract_json_from_response_bare_array() {
        let response = "Result:\n[{\"a\": 1}, {\"b\": 2}]\nDone.";
        let json = extract_json_from_response(response);
        assert_eq!(json, r#"[{"a": 1}, {"b": 2}]"#);
    }

    #[test]
    fn test_extract_json_from_response_generic_fence() {
        let response = "```\n{\"key\": \"value\"}\n```";
        let json = extract_json_from_response(response);
        assert_eq!(json, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_format_loop_guard_always() {
        assert_eq!(format_loop_guard(&LoopGuard::Always), "always");
    }

    #[test]
    fn test_format_loop_guard_iteration() {
        assert_eq!(
            format_loop_guard(&LoopGuard::IterationLessThan(5)),
            "iteration < 5"
        );
    }

    #[test]
    fn test_format_loop_guard_task_status() {
        let guard = LoopGuard::TaskStatus {
            task: "check-task".to_string(),
            status: Status::Done,
        };
        let result = format_loop_guard(&guard);
        assert!(result.starts_with("task_status:check-task:"));
    }

    #[test]
    fn test_build_template_captures_cycle_config() {
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();

        // header task with cycle_config
        let header = Task {
            id: "root-header".to_string(),
            title: "Header".to_string(),
            status: Status::Done,
            cycle_config: Some(CycleConfig {
                max_iterations: 3,
                guard: Some(LoopGuard::IterationLessThan(3)),
                delay: Some("5s".to_string()),
            }),
            after: vec!["root-step".to_string()],
            ..Task::default()
        };
        // step task that depends on header (forming cycle back-edge)
        let step = Task {
            id: "root-step".to_string(),
            title: "Step".to_string(),
            status: Status::Done,
            after: vec!["root-header".to_string()],
            ..Task::default()
        };

        graph.add_node(Node::Task(header.clone()));
        graph.add_node(Node::Task(step));

        let subgraph_ids: HashSet<&str> =
            ["root-header", "root-step"].iter().cloned().collect();
        let tmp = TempDir::new().unwrap();

        let template = build_template(&header, "root", &subgraph_ids, tmp.path(), &graph);
        assert!(!template.loops_to.is_empty(), "Should have loop edges");

        let loop_edge = &template.loops_to[0];
        assert_eq!(loop_edge.max_iterations, 3);
        assert!(loop_edge.guard.is_some());
        assert_eq!(loop_edge.delay.as_deref(), Some("5s"));
    }

    #[test]
    fn test_build_template_no_cycle_config() {
        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Simple task");
        graph.add_node(Node::Task(task.clone()));

        let subgraph_ids: HashSet<&str> = ["t1"].iter().cloned().collect();
        let tmp = TempDir::new().unwrap();

        let template = build_template(&task, "t1", &subgraph_ids, tmp.path(), &graph);
        assert!(template.loops_to.is_empty());
    }

    #[test]
    fn test_compute_max_depth_linear() {
        let a = Task {
            id: "a".to_string(),
            title: "A".to_string(),
            status: Status::Done,
            ..Task::default()
        };
        let b = Task {
            id: "b".to_string(),
            title: "B".to_string(),
            status: Status::Done,
            after: vec!["a".to_string()],
            ..Task::default()
        };
        let c = Task {
            id: "c".to_string(),
            title: "C".to_string(),
            status: Status::Done,
            after: vec!["b".to_string()],
            ..Task::default()
        };

        let tasks: Vec<&Task> = vec![&a, &b, &c];
        assert_eq!(compute_max_depth(&tasks), 2);
    }

    #[test]
    fn test_compute_max_depth_single() {
        let a = make_task("a", "A");
        assert_eq!(compute_max_depth(&[&a]), 0);
    }

    #[test]
    fn test_generative_requires_two_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        setup_graph(&dir, &graph);

        let result = run_generative(&dir, &["t1".to_string()], None, None, false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least 2"));
    }

    #[test]
    fn test_generative_identical_traces_fallback() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        // Two root tasks with identical child structure
        let root1 = Task {
            id: "r1".to_string(),
            title: "Root 1".to_string(),
            status: Status::Done,
            skills: vec!["rust".to_string()],
            ..Task::default()
        };
        let child1 = Task {
            id: "r1-child".to_string(),
            title: "Child".to_string(),
            status: Status::Done,
            skills: vec!["testing".to_string()],
            after: vec!["r1".to_string()],
            ..Task::default()
        };
        let root2 = Task {
            id: "r2".to_string(),
            title: "Root 2".to_string(),
            status: Status::Done,
            skills: vec!["rust".to_string()],
            ..Task::default()
        };
        let child2 = Task {
            id: "r2-child".to_string(),
            title: "Child".to_string(),
            status: Status::Done,
            skills: vec!["testing".to_string()],
            after: vec!["r2".to_string()],
            ..Task::default()
        };

        graph.add_node(Node::Task(root1));
        graph.add_node(Node::Task(child1));
        graph.add_node(Node::Task(root2));
        graph.add_node(Node::Task(child2));
        setup_graph(&dir, &graph);

        // Identical topology should fall back to static extraction
        let result = run_generative(
            &dir,
            &["r1".to_string(), "r2".to_string()],
            Some("ident-test"),
            None,
            false,
            false,
        );
        assert!(result.is_ok());

        // Should produce version 1 (static) since topologies are identical
        let func_path = dir.join("functions").join("ident-test.yaml");
        let func = trace_function::load_function(&func_path).unwrap();
        assert_eq!(func.version, 1);
    }

    #[test]
    fn test_generative_different_traces() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        // Trace 1: 2 tasks
        let r1 = Task {
            id: "r1".to_string(),
            title: "Root 1".to_string(),
            status: Status::Done,
            skills: vec!["rust".to_string()],
            ..Task::default()
        };
        let r1c = Task {
            id: "r1-child".to_string(),
            title: "Child".to_string(),
            status: Status::Done,
            skills: vec!["testing".to_string()],
            after: vec!["r1".to_string()],
            ..Task::default()
        };
        // Trace 2: 3 tasks (different topology)
        let r2 = Task {
            id: "r2".to_string(),
            title: "Root 2".to_string(),
            status: Status::Done,
            skills: vec!["rust".to_string()],
            ..Task::default()
        };
        let r2a = Task {
            id: "r2-a".to_string(),
            title: "Step A".to_string(),
            status: Status::Done,
            skills: vec!["design".to_string()],
            after: vec!["r2".to_string()],
            ..Task::default()
        };
        let r2b = Task {
            id: "r2-b".to_string(),
            title: "Step B".to_string(),
            status: Status::Done,
            skills: vec!["testing".to_string()],
            after: vec!["r2-a".to_string()],
            ..Task::default()
        };

        graph.add_node(Node::Task(r1));
        graph.add_node(Node::Task(r1c));
        graph.add_node(Node::Task(r2));
        graph.add_node(Node::Task(r2a));
        graph.add_node(Node::Task(r2b));
        setup_graph(&dir, &graph);

        let result = run_generative(
            &dir,
            &["r1".to_string(), "r2".to_string()],
            Some("gen-diff"),
            None,
            false,
            false,
        );
        assert!(result.is_ok());

        let func_path = dir.join("functions").join("gen-diff.yaml");
        let func = trace_function::load_function(&func_path).unwrap();
        assert_eq!(func.version, 2);
        assert!(func.planning.is_some());
        assert!(func.constraints.is_some());

        let constraints = func.constraints.unwrap();
        assert_eq!(constraints.min_tasks, Some(2));
        assert_eq!(constraints.max_tasks, Some(3));
        assert!(constraints.required_skills.contains(&"rust".to_string()));
    }

    #[test]
    fn test_is_coordinator_noise() {
        let eval_task = Task {
            id: "evaluate-func-impl".to_string(),
            title: "Evaluate func-impl".to_string(),
            status: Status::Done,
            ..Task::default()
        };
        assert!(is_coordinator_noise(&eval_task));

        let assign_task = Task {
            id: "assign-func-impl".to_string(),
            title: "Assign func-impl".to_string(),
            status: Status::Done,
            ..Task::default()
        };
        assert!(is_coordinator_noise(&assign_task));

        let tagged_task = Task {
            id: "some-task".to_string(),
            title: "Some task".to_string(),
            status: Status::Done,
            tags: vec!["evaluation".to_string()],
            ..Task::default()
        };
        assert!(is_coordinator_noise(&tagged_task));

        let normal_task = Task {
            id: "impl-feature".to_string(),
            title: "Implement feature".to_string(),
            status: Status::Done,
            ..Task::default()
        };
        assert!(!is_coordinator_noise(&normal_task));
    }

    #[test]
    fn test_extract_subgraph_filters_coordinator_noise() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let root = Task {
            id: "root".to_string(),
            title: "Root task".to_string(),
            status: Status::Done,
            ..Task::default()
        };
        let child = Task {
            id: "root-impl".to_string(),
            title: "Implementation".to_string(),
            status: Status::Done,
            after: vec!["root".to_string()],
            ..Task::default()
        };
        let eval_task = Task {
            id: "evaluate-root-impl".to_string(),
            title: "Evaluate root-impl".to_string(),
            status: Status::Done,
            after: vec!["root-impl".to_string()],
            ..Task::default()
        };
        let assign_task = Task {
            id: "assign-root-impl".to_string(),
            title: "Assign root-impl".to_string(),
            status: Status::Done,
            after: vec!["root".to_string()],
            ..Task::default()
        };
        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(child));
        graph.add_node(Node::Task(eval_task));
        graph.add_node(Node::Task(assign_task));
        setup_graph(&dir, &graph);

        // Default: filters out evaluate-* and assign-* tasks
        let result = run(&dir, "root", Some("filtered"), true, false, None, false, false);
        assert!(result.is_ok());
        let func_path = dir.join("functions").join("filtered.yaml");
        let func = trace_function::load_function(&func_path).unwrap();
        assert_eq!(func.tasks.len(), 2); // root + root-impl only

        // With --include-evaluations: keeps all tasks
        let result = run(&dir, "root", Some("unfiltered"), true, false, None, false, true);
        assert!(result.is_ok());
        let func_path = dir.join("functions").join("unfiltered.yaml");
        let func = trace_function::load_function(&func_path).unwrap();
        assert_eq!(func.tasks.len(), 4); // all tasks included
    }
}

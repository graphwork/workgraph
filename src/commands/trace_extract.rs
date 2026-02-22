use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use workgraph::agency;
use workgraph::graph::{Status, Task};
use workgraph::provenance;
use workgraph::trace_function::{
    self, ExtractionSource, FunctionInput, FunctionOutput, InputType, TaskTemplate, TraceFunction,
};

/// Run the `wg trace extract <task-id>` command.
pub fn run(
    dir: &Path,
    task_id: &str,
    name: Option<&str>,
    subgraph: bool,
    generalize: bool,
    output: Option<&str>,
    force: bool,
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
    let tasks_to_extract: Vec<&Task> = if subgraph {
        collect_subgraph(task_id, &graph)
    } else {
        vec![task]
    };

    // Build task templates
    let subgraph_ids: HashSet<&str> = tasks_to_extract.iter().map(|t| t.id.as_str()).collect();
    let templates: Vec<TaskTemplate> = tasks_to_extract
        .iter()
        .map(|t| build_template(t, task_id, &subgraph_ids, dir))
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
    };

    // Handle --generalize
    if generalize {
        eprintln!("Warning: --generalize requires LLM integration which is not yet wired. Saving raw extraction without generalization.");
    }

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

    TaskTemplate {
        template_id,
        title: task.title.clone(),
        description: task
            .description
            .clone()
            .unwrap_or_else(|| task.title.clone()),
        skills: task.skills.clone(),
        after,
        loops_to: vec![],
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

    // Detect task ID patterns (kebab-case identifiers that look like task IDs)
    if let Some(first) = tasks.first() {
        let id_parts: Vec<&str> = first.id.splitn(2, '-').collect();
        if !id_parts.is_empty() {
            let base_name = &first.id;
            if !seen_names.contains("feature_name") {
                inputs.push(FunctionInput {
                    name: "feature_name".to_string(),
                    input_type: InputType::String,
                    description: "Short name for the feature (used as task ID prefix)".to_string(),
                    required: true,
                    default: None,
                    example: Some(serde_yaml::Value::String(base_name.clone())),
                    min: None,
                    max: None,
                    values: None,
                });
                seen_names.insert("feature_name".to_string());
            }
        }
    }

    // Detect file paths (patterns like src/foo.rs, path/to/file.ext)
    let file_paths: Vec<String> = extract_file_paths(&all_text);
    if !file_paths.is_empty() && !seen_names.contains("source_files") {
        inputs.push(FunctionInput {
            name: "source_files".to_string(),
            input_type: InputType::FileList,
            description: "Key source files to modify".to_string(),
            required: false,
            default: Some(serde_yaml::Value::Sequence(Vec::new())),
            example: Some(serde_yaml::Value::Sequence(
                file_paths
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

    // Also add source_files from artifacts if not already detected from text
    if !seen_names.contains("source_files") {
        let artifact_paths: Vec<String> = tasks
            .iter()
            .flat_map(|t| t.artifacts.iter().cloned())
            .collect();
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

    // Detect test/build commands
    let commands = extract_commands(&all_text);
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

    // Detect standalone numbers (thresholds, counts, etc.)
    let numbers = extract_numbers(&all_text);
    for (i, num) in numbers.iter().enumerate() {
        let param_name = if i == 0 {
            "threshold".to_string()
        } else {
            format!("value_{}", i + 1)
        };
        if !seen_names.contains(&param_name) {
            inputs.push(FunctionInput {
                name: param_name.clone(),
                input_type: InputType::Number,
                description: "Numeric parameter".to_string(),
                required: false,
                default: Some(serde_yaml::Value::Number(
                    serde_yaml::Number::from(*num),
                )),
                example: None,
                min: None,
                max: None,
                values: None,
            });
            seen_names.insert(param_name);
        }
    }

    inputs
}

/// Extract file paths from text using simple heuristics.
/// Matches patterns like: src/foo.rs, path/to/file.ext, ./relative/path.py
fn extract_file_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| c == ',' || c == ';' || c == '"' || c == '\'' || c == '(' || c == ')' || c == '[' || c == ']');
        // Must contain a '/' and end with a file extension
        if word.contains('/') && !word.starts_with("http") && !word.starts_with("//") {
            // Check for common file extension pattern
            if let Some(ext_pos) = word.rfind('.') {
                let ext = &word[ext_pos + 1..];
                if matches!(
                    ext,
                    "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java"
                        | "c" | "cpp" | "h" | "hpp" | "rb" | "yml" | "yaml"
                        | "toml" | "json" | "md" | "txt" | "sh" | "css"
                        | "html" | "sql" | "proto" | "zig" | "ex" | "exs"
                )
                    && seen.insert(word.to_string()) {
                        paths.push(word.to_string());
                    }
            }
        }
    }

    paths
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

/// Extract test/build commands from text.
fn extract_commands(text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut seen = HashSet::new();

    let command_prefixes = [
        "cargo test",
        "cargo build",
        "cargo clippy",
        "cargo check",
        "npm test",
        "npm run",
        "yarn test",
        "pytest",
        "python -m pytest",
        "go test",
        "make test",
        "make check",
        "make build",
    ];

    let text_lower = text.to_lowercase();
    for prefix in &command_prefixes {
        if let Some(pos) = text_lower.find(prefix) {
            // Extract to end of line
            let rest = &text[pos..];
            let end = rest.find('\n').unwrap_or(rest.len());
            let cmd = rest[..end].trim().to_string();
            if seen.insert(cmd.clone()) {
                commands.push(cmd);
            }
        }
    }

    commands
}

/// Extract standalone numbers that look like thresholds or counts.
/// Ignores numbers that are part of file paths, dates, or version strings.
fn extract_numbers(text: &str) -> Vec<f64> {
    let mut numbers = Vec::new();
    let mut seen = HashSet::new();

    for word in text.split_whitespace() {
        let word = word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != '-');
        if word.is_empty() {
            continue;
        }
        // Skip version-like patterns (1.2.3), dates, hex hashes
        if word.matches('.').count() > 1 {
            continue;
        }
        if word.len() > 8 {
            continue; // likely a hash or timestamp
        }
        if let Ok(n) = word.parse::<f64>() {
            // Skip 0 and 1 as they're too common
            if n != 0.0 && n != 1.0 && n.is_finite() {
                let key = format!("{}", n);
                if seen.insert(key) {
                    numbers.push(n);
                }
            }
        }
    }

    numbers
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
    fn test_extract_file_paths() {
        let text = "Modify src/main.rs and src/lib.rs for the feature";
        let paths = extract_file_paths(text);
        assert_eq!(paths, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn test_extract_file_paths_with_punctuation() {
        let text = "Files: [src/config.rs, src/graph.rs]";
        let paths = extract_file_paths(text);
        assert!(paths.contains(&"src/config.rs".to_string()));
        assert!(paths.contains(&"src/graph.rs".to_string()));
    }

    #[test]
    fn test_extract_file_paths_ignores_urls() {
        let text = "Visit https://example.com/test.html for docs";
        let paths = extract_file_paths(text);
        assert!(paths.is_empty());
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
    fn test_extract_commands() {
        let text = "After changes, run:\ncargo test --lib\nand verify output";
        let commands = extract_commands(text);
        assert!(!commands.is_empty(), "Should find at least one command");
        assert!(
            commands.iter().any(|c| c.starts_with("cargo test")),
            "Should find 'cargo test' command, got: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_numbers() {
        let text = "Set threshold to 0.8 and max retries to 3";
        let numbers = extract_numbers(text);
        assert!(numbers.contains(&0.8));
        assert!(numbers.contains(&3.0));
    }

    #[test]
    fn test_extract_numbers_ignores_versions() {
        let text = "Version 1.2.3 released";
        let numbers = extract_numbers(text);
        assert!(numbers.is_empty());
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

        let result = run(&dir, "impl-config", Some("my-func"), false, false, None, false);
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

        let result = run(&dir, "t1", None, false, false, None, false);
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

        let result = run(&dir, "root", Some("subgraph-func"), true, false, None, false);
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
        run(&dir, "t1", Some("overwrite-test"), false, false, None, false).unwrap();

        // Second without force should fail
        let result = run(&dir, "t1", Some("overwrite-test"), false, false, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        // With force should succeed
        let result = run(&dir, "t1", Some("overwrite-test"), false, false, None, true);
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
        let result = run(&dir, "t1", Some("gen-test"), false, true, None, false);
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
            ..Task::default()
        };

        let params = detect_parameters(&[&task]);

        // Should detect feature_name
        assert!(params.iter().any(|p| p.name == "feature_name"));
        // Should detect source_files
        assert!(params.iter().any(|p| p.name == "source_files"));
        // Should detect test_command
        assert!(params.iter().any(|p| p.name == "test_command"));
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
}

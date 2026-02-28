use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use workgraph::agency::{
    self, Evaluation, EvaluatorInput,
    load_all_evaluations_or_warn,
    load_tradeoff, load_role,
    record_evaluation, record_evaluation_with_inference,
    render_evaluator_prompt,
    render_identity_prompt_rich, resolve_all_components, resolve_outcome, eval_source,
};
use workgraph::config::Config;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::load_graph;
use workgraph::provenance;

/// Extract the model from a task's spawn log entry.
///
/// Spawn log entries have the format:
///   "Spawned by coordinator --executor claude --model anthropic/claude-opus-4-6"
/// Returns the model string if found.
fn extract_spawn_model(log: &[LogEntry]) -> Option<String> {
    for entry in log {
        if let Some(rest) = entry.message.strip_prefix("Spawned by ")
            && let Some(idx) = rest.find("--model ")
        {
            let model_start = idx + "--model ".len();
            let model = rest[model_start..].trim();
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
    }
    None
}

/// Maximum length (in bytes) for the artifact diff included in the evaluator prompt.
/// Diffs exceeding this are truncated with a notice.
const MAX_DIFF_BYTES: usize = 30_000;

/// Compute a git diff of artifact files, diffing from the commit closest to
/// `started_at` up to HEAD. Returns `None` if git is unavailable, there are no
/// artifacts, or no diff could be computed.
fn compute_artifact_diff(artifacts: &[String], started_at: Option<&str>) -> Option<String> {
    if artifacts.is_empty() {
        return None;
    }

    // Find the commit closest to when the task started.
    // If started_at is unavailable, we can't produce a meaningful diff.
    let base_commit = if let Some(started) = started_at {
        let output = Command::new("git")
            .args(["log", "--before", started, "--format=%H", "-1"])
            .output()
            .ok()?;
        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if hash.is_empty() {
            // No commit before started_at — use the initial empty tree
            "4b825dc642cb6eb9a060e54bf899d15f3f9382e1".to_string()
        } else {
            hash
        }
    } else {
        return None;
    };

    let mut args = vec![
        "diff".to_string(),
        format!("{}..HEAD", base_commit),
        "--".to_string(),
    ];
    args.extend(artifacts.iter().cloned());

    let output = Command::new("git").args(&args).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let diff = String::from_utf8_lossy(&output.stdout).to_string();
    if diff.trim().is_empty() {
        return None;
    }

    // Truncate overly large diffs
    if diff.len() > MAX_DIFF_BYTES {
        let truncated = &diff[..MAX_DIFF_BYTES];
        // Find the last newline to avoid cutting mid-line
        let cut_point = truncated.rfind('\n').unwrap_or(MAX_DIFF_BYTES);
        Some(format!(
            "{}\n\n... (diff truncated at {} bytes, total {} bytes)",
            &diff[..cut_point],
            cut_point,
            diff.len()
        ))
    } else {
        Some(diff)
    }
}

/// Run `wg evaluate <task-id>` — trigger evaluation of a completed task.
pub fn run(
    dir: &Path,
    task_id: &str,
    evaluator_model: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        bail!("Workgraph not initialized. Run `wg init` first.");
    }

    let graph = load_graph(&path)?;
    let task = graph.get_task_or_err(task_id)?;

    // Step 1: Verify task is done or failed
    // Failed tasks are also evaluated — there is useful signal in what kinds
    // of tasks cause which agents to fail (see §4.3 of agency design).
    match task.status {
        Status::Done | Status::Failed => {}
        ref other => {
            bail!(
                "Task '{}' has status {:?} — must be done or failed to evaluate",
                task_id,
                other
            );
        }
    }

    // Step 2: Load the task's agent and resolve its role + tradeoff
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let agents_dir = agency_dir.join("cache/agents");

    let (resolved_agent, role, resolved_tradeoff, agent_role_id, agent_tradeoff_id) = if let Some(
        ref agent_hash,
    ) = task.agent
    {
        match agency::find_agent_by_prefix(&agents_dir, agent_hash) {
            Ok(agent) => {
                let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
                let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));

                let role = if role_path.exists() {
                    Some(load_role(&role_path).context("Failed to load role")?)
                } else {
                    eprintln!(
                        "Warning: role '{}' not found, evaluating without role context",
                        agent.role_id
                    );
                    None
                };

                let resolved_tradeoff = if tradeoff_path.exists() {
                    Some(load_tradeoff(&tradeoff_path).context("Failed to load tradeoff")?)
                } else {
                    eprintln!(
                        "Warning: tradeoff '{}' not found, evaluating without tradeoff context",
                        agent.tradeoff_id
                    );
                    None
                };

                let role_id = agent.role_id.clone();
                let tradeoff_id = agent.tradeoff_id.clone();
                (Some(agent), role, resolved_tradeoff, role_id, tradeoff_id)
            }
            Err(e) => {
                eprintln!(
                    "Warning: agent '{}' not found ({}), evaluating without agent context",
                    agent_hash, e
                );
                (
                    None,
                    None,
                    None,
                    "unknown".to_string(),
                    "unknown".to_string(),
                )
            }
        }
    } else {
        eprintln!("Note: task has no assigned agent — evaluating without role/tradeoff context");
        (
            None,
            None,
            None,
            "unknown".to_string(),
            "unknown".to_string(),
        )
    };

    // Step 3: Collect task artifacts and log entries
    let artifacts = &task.artifacts;
    let log_entries = &task.log;

    // Step 3.5: Compute git diff of artifacts (R5 — ground truth for evaluator)
    let artifact_diff = compute_artifact_diff(artifacts, task.started_at.as_deref());

    // Step 3.6: Resolve evaluator agent identity (if configured)
    let config = Config::load_or_default(dir);
    let evaluator_identity = config.agency.evaluator_agent.as_ref().and_then(|eval_hash| {
        let agent_path = agents_dir.join(format!("{}.yaml", eval_hash));
        let eval_agent = agency::load_agent(&agent_path).ok()?;
        let eval_role_path = roles_dir.join(format!("{}.yaml", eval_agent.role_id));
        let eval_role = load_role(&eval_role_path).ok()?;
        let eval_tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", eval_agent.tradeoff_id));
        let eval_tradeoff = load_tradeoff(&eval_tradeoff_path).ok()?;
        let workgraph_root = dir;
        let resolved_skills = resolve_all_components(&eval_role, workgraph_root, &agency_dir);
        let outcome = resolve_outcome(&eval_role.outcome_id, &agency_dir);
        Some(render_identity_prompt_rich(&eval_role, &eval_tradeoff, &resolved_skills, outcome.as_ref()))
    });

    // Step 3.7: Collect downstream task context for organizational impact scoring.
    // `task.before` lists task IDs that depend on this task's output.
    let downstream_tasks: Vec<(String, String, Option<String>)> = task
        .before
        .iter()
        .filter_map(|dep_id| {
            let dep = graph.get_task(dep_id)?;
            let status_str = format!("{:?}", dep.status);
            let desc = dep.description.clone();
            Some((dep.title.clone(), status_str, desc))
        })
        .collect();

    // Step 4: Build evaluator prompt
    let evaluator_input = EvaluatorInput {
        task_title: &task.title,
        task_description: task.description.as_deref(),
        task_skills: &task.skills,
        verify: task.verify.as_deref(),
        agent: resolved_agent.as_ref(),
        role: role.as_ref(),
        tradeoff: resolved_tradeoff.as_ref(),
        artifacts,
        log_entries,
        started_at: task.started_at.as_deref(),
        completed_at: task.completed_at.as_deref(),
        artifact_diff: artifact_diff.as_deref(),
        evaluator_identity: evaluator_identity.as_deref(),
        downstream_tasks: &downstream_tasks,
    };

    let prompt = render_evaluator_prompt(&evaluator_input);

    // Determine the model to use
    let model = evaluator_model
        .map(std::string::ToString::to_string)
        .or(config.agency.evaluator_model.clone())
        .or(task.model.clone())
        .unwrap_or_else(|| config.agent.model.clone());

    // Resolve the task execution model early so dry-run can show it
    let task_model_preview = extract_spawn_model(&task.log).or_else(|| task.model.clone());

    // Step 5: --dry-run shows what would be evaluated
    if dry_run {
        println!("=== Dry Run: wg evaluate {} ===\n", task_id);
        println!("Task: {} ({})", task.title, task_id);
        println!("Status: {:?}", task.status);
        if let Some(ref agent_hash) = task.agent {
            println!("Agent: {}", agent_hash);
            println!("Role: {}", agent_role_id);
            println!("Tradeoff: {}", agent_tradeoff_id);
        } else {
            println!("Agent: (none)");
        }
        println!(
            "Task model:     {}",
            task_model_preview.as_deref().unwrap_or("(unknown)")
        );
        println!("Artifacts: {}", artifacts.len());
        println!("Log entries: {}", log_entries.len());
        println!("Evaluator model: {}", model);
        println!("\n--- Evaluator Prompt ---\n");
        println!("{}", prompt);
        return Ok(());
    }

    // Step 6: Spawn a Claude agent with the evaluator prompt (--print for non-interactive)
    println!("Evaluating task '{}' with model '{}'...", task_id, model);

    let output = Command::new("claude")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDECODE")
        .arg("--model")
        .arg(&model)
        .arg("--print")
        .arg("--dangerously-skip-permissions")
        .arg(&prompt)
        .output()
        .context("Failed to run claude CLI — is it installed and in PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Claude evaluator failed (exit code {:?}):\n{}",
            output.status.code(),
            stderr
        );
    }

    let raw_output = String::from_utf8_lossy(&output.stdout);

    // Step 7: Parse the JSON output from the evaluator
    let eval_json =
        extract_json(&raw_output).context("Failed to extract valid JSON from evaluator output")?;

    let parsed: EvalOutput = serde_json::from_str(&eval_json)
        .with_context(|| format!("Failed to parse evaluator JSON:\n{}", eval_json))?;

    // Build the Evaluation record using the agent/role/tradeoff resolved above
    let agent_id = resolved_agent
        .as_ref()
        .map(|a| a.id.clone())
        .unwrap_or_default();
    let role_id = agent_role_id;
    let tradeoff_id = agent_tradeoff_id;

    // Resolve the model that was used to execute this task.
    // Best source: the spawn log entry which records the effective model.
    // Fallback: task.model field.
    let task_model = extract_spawn_model(&task.log).or_else(|| task.model.clone());

    let timestamp = chrono::Utc::now().to_rfc3339();
    let eval_id = format!("eval-{}-{}", task_id, timestamp.replace(':', "-"));

    let evaluation = Evaluation {
        id: eval_id,
        task_id: task_id.to_string(),
        agent_id,
        role_id: role_id.clone(),
        tradeoff_id: tradeoff_id.clone(),
        score: parsed.score,
        dimensions: parsed.dimensions,
        notes: parsed.notes,
        evaluator: format!("claude:{}", model),
        timestamp,
        model: task_model.clone(),
        source: "llm".to_string(),
    };

    // Step 8: Save evaluation, update performance records, and trigger retrospective inference
    if role_id != "unknown" && tradeoff_id != "unknown" {
        let eval_path =
            record_evaluation_with_inference(&evaluation, &agency_dir, &config.agency)
                .context("Failed to record evaluation")?;

        if json {
            let out = serde_json::json!({
                "task_id": task_id,
                "evaluation_id": evaluation.id,
                "score": evaluation.score,
                "dimensions": evaluation.dimensions,
                "notes": evaluation.notes,
                "evaluator": evaluation.evaluator,
                "model": evaluation.model,
                "path": eval_path.display().to_string(),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("\n=== Evaluation Complete ===");
            println!("Task:       {} ({})", task.title, task_id);
            if let Some(ref m) = evaluation.model {
                println!("Model:      {}", m);
            }
            println!("Score:      {:.2}", evaluation.score);
            // Individual quality dimensions
            if let Some(c) = evaluation.dimensions.get("correctness") {
                println!("  correctness:            {:.2}", c);
            }
            if let Some(c) = evaluation.dimensions.get("completeness") {
                println!("  completeness:           {:.2}", c);
            }
            if let Some(e) = evaluation.dimensions.get("efficiency") {
                println!("  efficiency:             {:.2}", e);
            }
            if let Some(s) = evaluation.dimensions.get("style_adherence") {
                println!("  style_adherence:        {:.2}", s);
            }
            // Organizational impact dimensions
            if let Some(d) = evaluation.dimensions.get("downstream_usability") {
                println!("  downstream_usability:   {:.2}", d);
            }
            if let Some(c) = evaluation.dimensions.get("coordination_overhead") {
                println!("  coordination_overhead:  {:.2}", c);
            }
            if let Some(b) = evaluation.dimensions.get("blocking_impact") {
                println!("  blocking_impact:        {:.2}", b);
            }
            println!("Notes:      {}", evaluation.notes);
            println!("Evaluator:  {}", evaluation.evaluator);
            println!("Saved to:   {}", eval_path.display());
        }
    } else {
        // No identity — save evaluation directly without updating performance records
        agency::init(&agency_dir)?;
        let eval_path = agency::save_evaluation(&evaluation, &agency_dir.join("evaluations"))
            .context("Failed to save evaluation")?;

        if json {
            let out = serde_json::json!({
                "task_id": task_id,
                "evaluation_id": evaluation.id,
                "score": evaluation.score,
                "dimensions": evaluation.dimensions,
                "notes": evaluation.notes,
                "evaluator": evaluation.evaluator,
                "model": evaluation.model,
                "path": eval_path.display().to_string(),
                "warning": "No identity assigned — performance records not updated",
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("\n=== Evaluation Complete ===");
            println!("Task:       {} ({})", task.title, task_id);
            if let Some(ref m) = evaluation.model {
                println!("Model:      {}", m);
            }
            println!("Score:      {:.2}", evaluation.score);
            println!("Notes:      {}", evaluation.notes);
            println!("Evaluator:  {}", evaluation.evaluator);
            println!("Saved to:   {}", eval_path.display());
            println!(
                "Warning: no identity assigned — role/tradeoff performance records not updated"
            );
        }
    }

    // Step 9: Record evaluator agent performance (if evaluator_agent is configured)
    // This tracks the evaluator's own performance: did it produce valid output,
    // was the score in range, etc. Enables performance history for the evaluator.
    if let Some(ref eval_agent_hash) = config.agency.evaluator_agent {
        let eval_agent_path = agents_dir.join(format!("{}.yaml", eval_agent_hash));
        if let Ok(eval_agent) = agency::load_agent(&eval_agent_path) {
            // Basic quality signal: the evaluator successfully produced a valid evaluation.
            // Score in [0,1] range = 1.0, dimensions present = bonus.
            let mut eval_quality = 1.0f64;
            if evaluation.score < 0.0 || evaluation.score > 1.0 {
                eval_quality -= 0.3;
            }
            if evaluation.dimensions.is_empty() {
                eval_quality -= 0.1;
            }
            if evaluation.notes.is_empty() {
                eval_quality -= 0.1;
            }

            let eval_of_evaluator = Evaluation {
                id: format!("meta-eval-{}-{}", task_id, chrono::Utc::now().to_rfc3339().replace(':', "-")),
                task_id: format!("evaluate-{}", task_id),
                agent_id: eval_agent.id.clone(),
                role_id: eval_agent.role_id.clone(),
                tradeoff_id: eval_agent.tradeoff_id.clone(),
                score: eval_quality.max(0.0),
                dimensions: HashMap::new(),
                notes: format!("Auto-recorded: evaluator produced valid evaluation for task '{}'", task_id),
                evaluator: "system".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                model: None,
                source: eval_source::LLM.to_string(),
            };

            if let Err(e) = record_evaluation(&eval_of_evaluator, &agency_dir) {
                eprintln!("Warning: failed to record evaluator performance: {}", e);
            }
        }
    }

    Ok(())
}

/// Record an evaluation from an external source.
pub fn run_record(
    dir: &Path,
    task_id: &str,
    score: f64,
    source: &str,
    notes: Option<&str>,
    dimensions: &[String],
    json: bool,
) -> Result<()> {
    // Validate score range
    if !(0.0..=1.0).contains(&score) {
        bail!("Score must be in [0.0, 1.0] range, got {}", score);
    }

    let path = super::graph_path(dir);
    if !path.exists() {
        bail!("Workgraph not initialized. Run `wg init` first.");
    }

    let graph = load_graph(&path)?;
    let task = graph.get_task_or_err(task_id)?;

    // Resolve agent info if available
    let agency_dir = dir.join("agency");
    let agents_dir = agency_dir.join("cache/agents");

    let (agent_id, role_id, tradeoff_id) = if let Some(ref agent_hash) = task.agent {
        match agency::find_agent_by_prefix(&agents_dir, agent_hash) {
            Ok(agent) => (
                agent.id.clone(),
                agent.role_id.clone(),
                agent.tradeoff_id.clone(),
            ),
            Err(_) => (String::new(), String::new(), String::new()),
        }
    } else {
        (String::new(), String::new(), String::new())
    };

    // Parse dimensional scores
    let mut dim_map = HashMap::new();
    for dim in dimensions {
        if let Some((key, val)) = dim.split_once('=') {
            let v: f64 = val
                .parse()
                .with_context(|| format!("Invalid dimension score '{}' in '{}'", val, dim))?;
            dim_map.insert(key.to_string(), v);
        } else {
            bail!(
                "Invalid dimension format '{}'. Expected key=value (e.g. correctness=0.8)",
                dim
            );
        }
    }

    let timestamp = chrono::Utc::now().to_rfc3339();
    let eval_id = format!("eval-{}-{}", task_id, timestamp.replace(':', "-"));

    let evaluation = Evaluation {
        id: eval_id,
        task_id: task_id.to_string(),
        agent_id,
        role_id: role_id.clone(),
        tradeoff_id: tradeoff_id.clone(),
        score,
        dimensions: dim_map,
        notes: notes.unwrap_or("").to_string(),
        evaluator: source.to_string(),
        timestamp,
        model: None,
        source: source.to_string(),
    };

    // Save evaluation and trigger retrospective inference for learning assignments
    let config = Config::load_or_default(dir);
    if !role_id.is_empty() && !tradeoff_id.is_empty() {
        let eval_path =
            record_evaluation_with_inference(&evaluation, &agency_dir, &config.agency)
                .context("Failed to record evaluation")?;

        if json {
            let out = serde_json::json!({
                "task_id": task_id,
                "evaluation_id": evaluation.id,
                "score": evaluation.score,
                "source": evaluation.source,
                "dimensions": evaluation.dimensions,
                "path": eval_path.display().to_string(),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("Recorded evaluation for task '{}'", task_id);
            println!("  Score:  {:.2}", evaluation.score);
            println!("  Source: {}", evaluation.source);
            println!("  Saved:  {}", eval_path.display());
        }
    } else {
        agency::init(&agency_dir)?;
        let evals_dir = agency_dir.join("evaluations");
        let eval_path = agency::save_evaluation(&evaluation, &evals_dir)
            .context("Failed to save evaluation")?;

        if json {
            let out = serde_json::json!({
                "task_id": task_id,
                "evaluation_id": evaluation.id,
                "score": evaluation.score,
                "source": evaluation.source,
                "dimensions": evaluation.dimensions,
                "path": eval_path.display().to_string(),
                "warning": "No agent identity — performance records not updated",
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("Recorded evaluation for task '{}'", task_id);
            println!("  Score:  {:.2}", evaluation.score);
            println!("  Source: {}", evaluation.source);
            println!("  Saved:  {}", eval_path.display());
            println!("  Note:   No agent identity — performance records not updated");
        }
    }

    // Record provenance
    let _ = provenance::record(
        dir,
        "evaluate_record",
        Some(task_id),
        Some("external"),
        serde_json::json!({
            "source": source,
            "score": score,
        }),
        provenance::DEFAULT_ROTATION_THRESHOLD,
    );

    Ok(())
}

/// Show evaluation history with optional filters.
///
/// When `task_detail` is provided, shows evaluations for that specific task.
/// Otherwise, shows a filtered history list.
pub fn run_show(
    dir: &Path,
    task_filter: Option<&str>,
    agent_filter: Option<&str>,
    source_filter: Option<&str>,
    limit: Option<usize>,
    json: bool,
    task_detail: Option<&str>,
) -> Result<()> {
    let evals_dir = dir.join("agency").join("evaluations");

    // If a specific task was requested, show both levels side by side
    if let Some(tid) = task_detail {
        let mut task_evals = load_all_evaluations_or_warn(&evals_dir);
        task_evals.retain(|e| e.task_id == tid || e.task_id.starts_with(tid));
        task_evals.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));


        if json {
            let out = serde_json::json!({
                "task_id": tid,
                "evaluations": task_evals,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("=== Evaluations for task '{}' ===\n", tid);

            println!("Evaluations ({}):", task_evals.len());
            if task_evals.is_empty() {
                println!("  (none)");
            } else {
                for e in &task_evals {
                    println!("  Score: {:.3}  Source: {}  Agent: {}  {}",
                        e.score, e.source,
                        if e.agent_id.is_empty() { "-" } else { &e.agent_id[..e.agent_id.len().min(10)] },
                        e.timestamp);
                    for (dim, val) in &e.dimensions {
                        println!("    {}: {:.3}", dim, val);
                    }
                }
            }

        }
        return Ok(());
    }

    let mut evals = load_all_evaluations_or_warn(&evals_dir);

    // Apply filters
    if let Some(task_prefix) = task_filter {
        evals.retain(|e| e.task_id.starts_with(task_prefix));
    }
    if let Some(agent_prefix) = agent_filter {
        evals.retain(|e| e.agent_id.starts_with(agent_prefix));
    }
    if let Some(source_pat) = source_filter {
        if source_pat.contains('*') {
            // Glob match: convert simple glob to prefix/suffix match
            let parts: Vec<&str> = source_pat.split('*').collect();
            evals.retain(|e| {
                if parts.len() == 2 {
                    e.source.starts_with(parts[0]) && e.source.ends_with(parts[1])
                } else {
                    e.source == source_pat
                }
            });
        } else {
            evals.retain(|e| e.source == source_pat);
        }
    }

    // Sort by timestamp descending
    evals.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    // Apply limit
    if let Some(n) = limit {
        evals.truncate(n);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&evals)?);
    } else if evals.is_empty() {
        println!("No evaluations found.");
    } else {
        // Table header
        println!(
            "{:<20} {:>5}  {:<16} {:<12} Timestamp",
            "Task", "Score", "Source", "Agent"
        );
        println!("{}", "─".repeat(75));

        for e in &evals {
            let agent_display = if e.agent_id.is_empty() {
                "-"
            } else if e.agent_id.len() > 10 {
                &e.agent_id[..10]
            } else {
                &e.agent_id
            };
            let task_display = if e.task_id.len() > 18 {
                &e.task_id[..18]
            } else {
                &e.task_id
            };
            let source_display = if e.source.len() > 14 {
                &e.source[..14]
            } else {
                &e.source
            };
            println!(
                "{:<20} {:>5.2}  {:<16} {:<12} {}",
                task_display, e.score, source_display, agent_display, e.timestamp
            );
        }

        println!("\n{} evaluation(s)", evals.len());
    }

    Ok(())
}

/// Output shape we expect from the evaluator LLM.
#[derive(serde::Deserialize)]
struct EvalOutput {
    score: f64,
    #[serde(default)]
    dimensions: std::collections::HashMap<String, f64>,
    #[serde(default)]
    notes: String,
}

/// Extract a JSON object from potentially noisy LLM output.
///
/// The evaluator is instructed to return only JSON, but it may wrap it in
/// markdown fences or include leading/trailing commentary. This function
/// finds the first `{...}` that parses as valid JSON.
fn extract_json(raw: &str) -> Option<String> {
    // Try the whole string first (ideal case)
    let trimmed = raw.trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    // Strip markdown code fences if present
    let stripped = if trimmed.starts_with("```") {
        let inner = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
            return Some(inner.to_string());
        }
        inner
    } else {
        trimmed
    };

    // Find the first { and last } and try to parse
    if let Some(start) = stripped.find('{')
        && let Some(end) = stripped.rfind('}')
    {
        let candidate = &stripped[start..=end];
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_plain() {
        let input = r#"{"score": 0.85, "dimensions": {}, "notes": "Good work"}"#;
        let result = extract_json(input).unwrap();
        assert!(result.contains("0.85"));
    }

    #[test]
    fn extract_json_with_fences() {
        let input = "```json\n{\"score\": 0.7, \"dimensions\": {}, \"notes\": \"ok\"}\n```";
        let result = extract_json(input).unwrap();
        assert!(result.contains("0.7"));
    }

    #[test]
    fn extract_json_with_surrounding_text() {
        let input = "Here is my evaluation:\n{\"score\": 0.9, \"notes\": \"great\"}\nEnd.";
        let result = extract_json(input).unwrap();
        assert!(result.contains("0.9"));
    }

    #[test]
    fn extract_json_returns_none_for_garbage() {
        assert!(extract_json("no json here at all").is_none());
    }

    #[test]
    fn parse_eval_output_minimal() {
        let json = r#"{"score": 0.75}"#;
        let parsed: EvalOutput = serde_json::from_str(json).unwrap();
        assert!((parsed.score - 0.75).abs() < f64::EPSILON);
        assert!(parsed.dimensions.is_empty());
        assert!(parsed.notes.is_empty());
    }

    #[test]
    fn parse_eval_output_full() {
        let json = r#"{
            "score": 0.82,
            "dimensions": {
                "correctness": 0.9,
                "completeness": 0.8,
                "efficiency": 0.75,
                "style_adherence": 0.8
            },
            "notes": "Well implemented but could be more efficient"
        }"#;
        let parsed: EvalOutput = serde_json::from_str(json).unwrap();
        assert!((parsed.score - 0.82).abs() < f64::EPSILON);
        assert_eq!(parsed.dimensions.len(), 4);
        assert_eq!(parsed.notes, "Well implemented but could be more efficient");
    }
}

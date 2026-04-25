use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use workgraph::agency::{
    self, Evaluation, EvaluatorInput, FlipComparisonInput, FlipInferenceInput, eval_source,
    load_all_evaluations_or_warn, load_role, load_tradeoff, record_evaluation,
    record_evaluation_with_inference, render_evaluator_prompt, render_flip_comparison_prompt,
    render_flip_inference_prompt, render_identity_prompt_rich, resolve_all_components,
    resolve_outcome,
};
use workgraph::config::Config;
use workgraph::graph::{LogEntry, Status, TokenUsage};
use workgraph::parser::load_graph;
use workgraph::provenance;

/// Extract the model from a task's spawn log entry.
///
/// Spawn log entries have the format:
///   "Spawned by coordinator --executor claude --model opus"
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
        let safe_end = diff.floor_char_boundary(MAX_DIFF_BYTES);
        let truncated = &diff[..safe_end];
        // Find the last newline to avoid cutting mid-line
        let cut_point = truncated.rfind('\n').unwrap_or(safe_end);
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

    let (resolved_agent, role, resolved_tradeoff, agent_role_id, agent_tradeoff_id) =
        if let Some(ref agent_hash) = task.agent {
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
            eprintln!(
                "Note: task has no assigned agent — evaluating without role/tradeoff context"
            );
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
    let evaluator_identity = config
        .agency
        .evaluator_agent
        .as_ref()
        .and_then(|eval_hash| {
            let agent_path = agents_dir.join(format!("{}.yaml", eval_hash));
            let eval_agent = agency::load_agent(&agent_path).ok()?;
            let eval_role_path = roles_dir.join(format!("{}.yaml", eval_agent.role_id));
            let eval_role = load_role(&eval_role_path).ok()?;
            let eval_tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", eval_agent.tradeoff_id));
            let eval_tradeoff = load_tradeoff(&eval_tradeoff_path).ok()?;
            let workgraph_root = dir;
            let resolved_skills = resolve_all_components(&eval_role, workgraph_root, &agency_dir);
            let outcome = resolve_outcome(&eval_role.outcome_id, &agency_dir);
            Some(render_identity_prompt_rich(
                &eval_role,
                &eval_tradeoff,
                &resolved_skills,
                outcome.as_ref(),
            ))
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

    // Step 3.75: Collect child task context for decomposition detection.
    // Find tasks that have this task as a dependency (tasks where `task.after` contains current task_id).
    let child_tasks: Vec<(String, String, Option<String>)> = graph
        .tasks()
        .filter(|t| t.after.contains(&task_id.to_string()))
        .map(|child| {
            let status_str = format!("{:?}", child.status);
            let desc = child.description.clone();
            (child.title.clone(), status_str, desc)
        })
        .collect();

    // Step 3.8: Load FLIP score and verify findings (if available)
    let flip_score = {
        let evals_dir = agency_dir.join("evaluations");
        let all_evals = load_all_evaluations_or_warn(&evals_dir);
        all_evals
            .iter()
            .find(|e| e.task_id == task_id && e.source == eval_source::FLIP)
            .map(|e| e.score)
    };

    let verify_task_id = format!(".verify-{}", task_id);
    let verify_task_data = graph.get_task(&verify_task_id);
    let verify_status_owned: Option<String> = verify_task_data.and_then(|vt| match vt.status {
        Status::Done => Some("passed".to_string()),
        Status::Failed => Some("failed".to_string()),
        _ => None,
    });
    let verify_findings_owned: Option<String> = verify_task_data.and_then(|vt| {
        if vt.log.is_empty() {
            None
        } else {
            let entries: Vec<String> = vt
                .log
                .iter()
                .map(|entry| {
                    let actor = entry.actor.as_deref().unwrap_or("system");
                    format!("[{}] ({}): {}", entry.timestamp, actor, entry.message)
                })
                .collect();
            Some(entries.join("\n"))
        }
    });

    // Step 4: Build evaluator prompt
    let evaluated_outcome = role
        .as_ref()
        .and_then(|r| resolve_outcome(&r.outcome_id, &agency_dir));
    let evaluated_outcome_name = evaluated_outcome.as_ref().map(|o| o.name.as_str());
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
        flip_score,
        verify_status: verify_status_owned.as_deref(),
        verify_findings: verify_findings_owned.as_deref(),
        resolved_outcome_name: evaluated_outcome_name,
        child_tasks: &child_tasks,
    };

    let prompt = render_evaluator_prompt(&evaluator_input);

    // Determine the model to use via model routing
    let model = evaluator_model
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| {
            config
                .resolve_model_for_role(workgraph::config::DispatchRole::Evaluator)
                .model
        });

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

    // Step 6: Run lightweight LLM call for evaluation (replaces claude --print)
    println!("Evaluating task '{}' with model '{}'...", task_id, model);

    // Eval calls can be slow with large task outputs — use a generous timeout.
    // The triage_timeout is designed for short triage calls; evals need more.
    let timeout_secs = config.agency.triage_timeout.unwrap_or(60).max(300);

    // Retry LLM call up to 3 times if JSON extraction fails (transient format failures)
    let (eval_json, eval_token_usage) = {
        let mut last_text = String::new();
        let mut extracted = None;
        let mut token_usage = None;
        for attempt in 1..=3 {
            let eval_result = workgraph::service::llm::run_lightweight_llm_call(
                &config,
                workgraph::config::DispatchRole::Evaluator,
                &prompt,
                timeout_secs,
            )
            .context("Evaluation LLM call failed")?;
            last_text = eval_result.text;
            token_usage = eval_result.token_usage;
            if let Some(json) = extract_json(&last_text) {
                extracted = Some(json);
                break;
            }
            if attempt < 3 {
                eprintln!(
                    "[evaluate] JSON extraction failed, retrying ({}/3)...",
                    attempt
                );
            }
        }
        let json = extracted.with_context(|| {
            format!(
                "Failed to extract valid JSON from evaluator output after 3 attempts. Last response:\n{}",
                last_text
            )
        })?;
        (json, token_usage)
    };

    let parsed: EvalOutput = serde_json::from_str(&eval_json)
        .with_context(|| format!("Failed to parse evaluator JSON:\n{}", eval_json))?;

    // Capture the verdict before moving fields out of parsed
    let verdict = parsed.verdict.clone();

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

    let mut dimensions = parsed.dimensions;
    if let Some(fs) = flip_score {
        dimensions.insert("intent_fidelity".to_string(), fs);
    }

    let evaluation = Evaluation {
        id: eval_id,
        task_id: task_id.to_string(),
        agent_id,
        role_id: role_id.clone(),
        tradeoff_id: tradeoff_id.clone(),
        score: parsed.score,
        dimensions,
        notes: parsed.notes,
        evaluator: format!("claude:{}", model),
        timestamp,
        model: task_model.clone(),
        source: "llm".to_string(),
    };

    // Step 8: Save evaluation, update performance records, and trigger retrospective inference
    if role_id != "unknown" && tradeoff_id != "unknown" {
        let eval_path = record_evaluation_with_inference(&evaluation, &agency_dir, &config.agency)
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
            if let Some(f) = evaluation.dimensions.get("intent_fidelity") {
                println!("  intent_fidelity:        {:.2}", f);
            }
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

    // Step 8.5: Persist token usage to the .evaluate-* task
    if let Some(ref usage) = eval_token_usage {
        let eval_task_id = format!(".evaluate-{}", task_id);
        let graph_path = super::graph_path(dir);
        let usage_clone = usage.clone();
        let _ = workgraph::parser::modify_graph(&graph_path, |graph| {
            if let Some(eval_task) = graph.get_task_mut(&eval_task_id) {
                eval_task.token_usage = Some(usage_clone.clone());
                true
            } else {
                false
            }
        });
        // Emit machine-readable token summary for inline eval capture.
        // The spawn_eval_inline script greps for this line and calls `wg tokens`.
        if let Ok(json) = serde_json::to_string(usage) {
            eprintln!("__WG_TOKENS__:{}", json);
        }
    }

    // Step 8.6: Eval gate — reject the original task if score is below threshold
    let rejected = check_eval_gate(dir, task_id, &task.tags, &evaluation, &config, json)?;
    if rejected && !json {
        println!("  REJECTED: task '{}' failed by evaluation gate", task_id);
    }

    // Step 8.7: LLM verification gate (docs/design/llm-verification-gate.md).
    // When the source task opted in via validation="llm" and is currently
    // PendingValidation, translate the evaluation score/notes into a gate
    // decision and apply approve/reject/escalate accordingly.
    // `rejected` above means the score gate already failed the task; no need
    // to double-reject.
    let is_llm_gated = task.validation.as_deref() == Some("llm");
    if !rejected && is_llm_gated {
        // Re-load to see current state (check_eval_gate may have mutated).
        let path = super::graph_path(dir);
        let graph2 = workgraph::parser::load_graph(&path).ok();
        let still_pending = graph2
            .as_ref()
            .and_then(|g| g.get_task(task_id))
            .map(|t| t.status == Status::PendingValidation)
            .unwrap_or(false);
        if still_pending {
            let gate = GateDecision::from_evaluation(&evaluation, &config);
            match apply_gate_decision(dir, task_id, &gate, &config) {
                Ok(action) => {
                    if !json {
                        match action {
                            GateAction::Approved => {
                                println!("  LLM gate: approved '{}' (score {:.2})", task_id, evaluation.score)
                            }
                            GateAction::Rejected => {
                                println!("  LLM gate: rejected '{}' (score {:.2})", task_id, evaluation.score)
                            }
                            GateAction::Held => {
                                println!("  LLM gate: '{}' held for human review (score {:.2})", task_id, evaluation.score)
                            }
                            GateAction::Skipped => {}
                        }
                    }
                }
                Err(e) => eprintln!("Warning: LLM gate application failed: {}", e),
            }
        }
    }

    // Step 8.8: Verdict-driven parent task status transition.
    // The evaluator's verdict (pass / incomplete / fail) is the primary
    // determinant of the source task's terminal status, replacing --verify.
    // If no explicit verdict was emitted, derive one from the score:
    //   score >= 0.6 → pass, score >= 0.3 → incomplete, else fail.
    if !rejected {
        let effective_verdict = verdict
            .as_deref()
            .map(|v| v.to_lowercase())
            .unwrap_or_else(|| {
                if evaluation.score >= 0.6 {
                    "pass".to_string()
                } else if evaluation.score >= 0.3 {
                    "incomplete".to_string()
                } else {
                    "fail".to_string()
                }
            });

        let graph_path = super::graph_path(dir);
        let task_id_owned = task_id.to_string();
        let notes_clone = evaluation.notes.clone();
        let score = evaluation.score;

        match effective_verdict.as_str() {
            "pass" => {
                // Task stays Done (already transitioned by wg done)
                if !json {
                    println!("  Verdict: PASS — task '{}' confirmed done", task_id);
                }
            }
            "incomplete" => {
                let _ = workgraph::parser::modify_graph(&graph_path, |graph| {
                    if let Some(t) = graph.get_task_mut(&task_id_owned) {
                        if t.status == Status::Done {
                            t.status = Status::Incomplete;
                            t.log.push(LogEntry {
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                actor: Some("evaluate-verdict".to_string()),
                                user: None,
                                message: format!(
                                    "Verdict: INCOMPLETE (score {:.2}). {}",
                                    score, notes_clone,
                                ),
                            });
                            return true;
                        }
                    }
                    false
                });
                if !json {
                    println!(
                        "  Verdict: INCOMPLETE — task '{}' marked incomplete (retryable, score {:.2})",
                        task_id, score
                    );
                }
                super::notify_graph_changed(dir);
            }
            "fail" => {
                let reason = format!(
                    "Evaluation verdict: fail (score {:.2}). {}",
                    score, notes_clone,
                );
                let _ = workgraph::parser::modify_graph(&graph_path, |graph| {
                    if let Some(t) = graph.get_task_mut(&task_id_owned) {
                        if t.status == Status::Done {
                            t.status = Status::Failed;
                            t.assigned = None;
                            t.failure_reason = Some(reason.clone());
                            t.log.push(LogEntry {
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                actor: Some("evaluate-verdict".to_string()),
                                user: None,
                                message: format!("Verdict: FAIL (score {:.2}). {}", score, notes_clone),
                            });
                            return true;
                        }
                    }
                    false
                });
                if !json {
                    println!(
                        "  Verdict: FAIL — task '{}' marked failed (score {:.2})",
                        task_id, score
                    );
                }
                super::notify_graph_changed(dir);
            }
            other => {
                eprintln!(
                    "Warning: unrecognized verdict '{}' from evaluator, treating as pass",
                    other
                );
            }
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
                id: format!(
                    "meta-eval-{}-{}",
                    task_id,
                    chrono::Utc::now().to_rfc3339().replace(':', "-")
                ),
                task_id: format!(".evaluate-{}", task_id),
                agent_id: eval_agent.id.clone(),
                role_id: eval_agent.role_id.clone(),
                tradeoff_id: eval_agent.tradeoff_id.clone(),
                score: eval_quality.max(0.0),
                dimensions: HashMap::new(),
                notes: format!(
                    "Auto-recorded: evaluator produced valid evaluation for task '{}'",
                    task_id
                ),
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

/// Run FLIP (Fidelity via Latent Intent Probing) evaluation of a completed task.
///
/// Two-phase roundtrip intent fidelity evaluation:
/// 1. Inference: An LLM sees only the task output and reconstructs what the prompt was
/// 2. Comparison: Another LLM compares the inferred prompt to the actual prompt
pub fn run_flip(
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

    // Verify task is done or failed
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

    // Check FLIP is enabled or task is tagged
    let config = Config::load_or_default(dir);
    let flip_enabled = config.agency.flip_enabled || task.tags.iter().any(|t| t == "flip-eval");
    if !flip_enabled {
        bail!(
            "FLIP evaluation is not enabled. Enable with `wg config --flip-enabled true` \
             or tag the task with 'flip-eval'."
        );
    }

    // Load agent identity (same as regular evaluation)
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let agents_dir = agency_dir.join("cache/agents");

    let (resolved_agent, role, resolved_tradeoff, agent_role_id, agent_tradeoff_id) =
        if let Some(ref agent_hash) = task.agent {
            match agency::find_agent_by_prefix(&agents_dir, agent_hash) {
                Ok(agent) => {
                    let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
                    let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));

                    let role = if role_path.exists() {
                        Some(load_role(&role_path).context("Failed to load role")?)
                    } else {
                        None
                    };

                    let resolved_tradeoff = if tradeoff_path.exists() {
                        Some(load_tradeoff(&tradeoff_path).context("Failed to load tradeoff")?)
                    } else {
                        None
                    };

                    let role_id = agent.role_id.clone();
                    let tradeoff_id = agent.tradeoff_id.clone();
                    (Some(agent), role, resolved_tradeoff, role_id, tradeoff_id)
                }
                Err(_) => (
                    None,
                    None,
                    None,
                    "unknown".to_string(),
                    "unknown".to_string(),
                ),
            }
        } else {
            (
                None,
                None,
                None,
                "unknown".to_string(),
                "unknown".to_string(),
            )
        };

    // Collect artifacts and compute diff
    let artifacts = &task.artifacts;
    let log_entries = &task.log;
    let artifact_diff = compute_artifact_diff(artifacts, task.started_at.as_deref());

    // Determine models for each phase.
    // Priority: CLI --evaluator-model > per-task model > config (DispatchRole) > tier default
    let task_model = extract_spawn_model(&task.log).or_else(|| task.model.clone());

    let (inference_model, inference_source) = if let Some(m) = evaluator_model {
        (m.to_string(), "cli-override")
    } else if let Some(ref m) = task_model {
        // Per-task model: FLIP should probe the same 'mind' that did the work
        (m.clone(), "task-model")
    } else {
        (
            config
                .resolve_model_for_role(workgraph::config::DispatchRole::FlipInference)
                .model,
            "config",
        )
    };

    let (comparison_model, comparison_source) = if let Some(m) = evaluator_model {
        (m.to_string(), "cli-override")
    } else if let Some(ref m) = task_model {
        (m.clone(), "task-model")
    } else {
        (
            config
                .resolve_model_for_role(workgraph::config::DispatchRole::FlipComparison)
                .model,
            "config",
        )
    };

    eprintln!(
        "FLIP models: inference='{}' ({}), comparison='{}' ({})",
        inference_model, inference_source, comparison_model, comparison_source
    );

    // --- Phase 1: Inference ---
    let inference_input = FlipInferenceInput {
        agent: resolved_agent.as_ref(),
        role: role.as_ref(),
        tradeoff: resolved_tradeoff.as_ref(),
        artifacts,
        log_entries,
        started_at: task.started_at.as_deref(),
        completed_at: task.completed_at.as_deref(),
        artifact_diff: artifact_diff.as_deref(),
    };

    let inference_prompt = render_flip_inference_prompt(&inference_input);

    if dry_run {
        println!("=== Dry Run: wg evaluate {} --flip ===\n", task_id);
        println!("Task: {} ({})", task.title, task_id);
        println!("Status: {:?}", task.status);
        println!("Inference model: {}", inference_model);
        println!("Comparison model: {}", comparison_model);
        println!("Artifacts: {}", artifacts.len());
        println!("Log entries: {}", log_entries.len());
        println!("\n--- Phase 1: Inference Prompt ---\n");
        println!("{}", inference_prompt);
        println!("\n--- Phase 2: Comparison prompt will be generated from Phase 1 output ---\n");
        return Ok(());
    }

    // Phase 1: Run inference
    println!(
        "FLIP Phase 1: Inferring prompt from output (model: '{}')...",
        inference_model
    );

    let flip_timeout = config.agency.triage_timeout.unwrap_or(60).max(300);

    // Retry LLM call up to 3 times if JSON extraction fails (transient format failures)
    let (inference_json, inference_token_usage) = {
        let mut last_text = String::new();
        let mut extracted = None;
        let mut token_usage = None;
        for attempt in 1..=3 {
            let inference_result = workgraph::service::llm::run_lightweight_llm_call(
                &config,
                workgraph::config::DispatchRole::FlipInference,
                &inference_prompt,
                flip_timeout,
            )
            .context("FLIP inference LLM call failed")?;
            last_text = inference_result.text;
            token_usage = inference_result.token_usage;
            if let Some(json) = extract_json(&last_text) {
                extracted = Some(json);
                break;
            }
            if attempt < 3 {
                eprintln!(
                    "[evaluate] JSON extraction failed, retrying ({}/3)...",
                    attempt
                );
            }
        }
        let json = extracted.with_context(|| {
            format!(
                "Failed to extract JSON from FLIP inference output after 3 attempts. Last response:\n{}",
                last_text
            )
        })?;
        (json, token_usage)
    };

    let parsed_inference: FlipInferenceOutput = serde_json::from_str(&inference_json)
        .with_context(|| format!("Failed to parse FLIP inference JSON:\n{}", inference_json))?;

    println!(
        "  Inferred prompt length: {} chars",
        parsed_inference.inferred_prompt.len()
    );

    // --- Phase 2: Comparison ---
    let comparison_input = FlipComparisonInput {
        actual_title: &task.title,
        actual_description: task.description.as_deref(),
        inferred_prompt: &parsed_inference.inferred_prompt,
    };

    let comparison_prompt = render_flip_comparison_prompt(&comparison_input);

    println!(
        "FLIP Phase 2: Comparing prompts (model: '{}')...",
        comparison_model
    );

    // Retry LLM call up to 3 times if JSON extraction fails (transient format failures)
    let (comparison_json, comparison_token_usage) = {
        let mut last_text = String::new();
        let mut extracted = None;
        let mut token_usage = None;
        for attempt in 1..=3 {
            let comparison_result = workgraph::service::llm::run_lightweight_llm_call(
                &config,
                workgraph::config::DispatchRole::FlipComparison,
                &comparison_prompt,
                flip_timeout,
            )
            .context("FLIP comparison LLM call failed")?;
            last_text = comparison_result.text;
            token_usage = comparison_result.token_usage;
            if let Some(json) = extract_json(&last_text) {
                extracted = Some(json);
                break;
            }
            if attempt < 3 {
                eprintln!(
                    "[evaluate] JSON extraction failed, retrying ({}/3)...",
                    attempt
                );
            }
        }
        let json = extracted.with_context(|| {
            format!(
                "Failed to extract JSON from FLIP comparison output after 3 attempts. Last response:\n{}",
                last_text
            )
        })?;
        (json, token_usage)
    };

    let parsed_comparison: FlipComparisonOutput = serde_json::from_str(&comparison_json)
        .with_context(|| format!("Failed to parse FLIP comparison JSON:\n{}", comparison_json))?;

    // Build the Evaluation record
    let agent_id = resolved_agent
        .as_ref()
        .map(|a| a.id.clone())
        .unwrap_or_default();

    let timestamp = chrono::Utc::now().to_rfc3339();
    let eval_id = format!("flip-{}-{}", task_id, timestamp.replace(':', "-"));

    // Store inferred prompt and comparison details in notes (JSON-encoded metadata)
    let flip_metadata = serde_json::json!({
        "inferred_prompt": parsed_inference.inferred_prompt,
        "inference_model": inference_model,
        "inference_source": inference_source,
        "comparison_model": comparison_model,
        "comparison_source": comparison_source,
    });

    let notes = format!(
        "{}\n\nFLIP metadata: {}",
        parsed_comparison.notes,
        serde_json::to_string(&flip_metadata).unwrap_or_default()
    );

    let evaluation = Evaluation {
        id: eval_id,
        task_id: task_id.to_string(),
        agent_id,
        role_id: agent_role_id.clone(),
        tradeoff_id: agent_tradeoff_id.clone(),
        score: parsed_comparison.flip_score,
        dimensions: parsed_comparison.dimensions,
        notes,
        evaluator: format!("flip:{}+{}", inference_model, comparison_model),
        timestamp,
        model: task_model.clone(),
        source: eval_source::FLIP.to_string(),
    };

    // Save evaluation
    if agent_role_id != "unknown" && agent_tradeoff_id != "unknown" {
        let eval_path = record_evaluation_with_inference(&evaluation, &agency_dir, &config.agency)
            .context("Failed to record FLIP evaluation")?;

        if json {
            let out = serde_json::json!({
                "task_id": task_id,
                "evaluation_id": evaluation.id,
                "flip_score": evaluation.score,
                "dimensions": evaluation.dimensions,
                "inferred_prompt": parsed_inference.inferred_prompt,
                "notes": parsed_comparison.notes,
                "evaluator": evaluation.evaluator,
                "model": evaluation.model,
                "source": "flip",
                "path": eval_path.display().to_string(),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("\n=== FLIP Evaluation Complete ===");
            println!("Task:       {} ({})", task.title, task_id);
            if let Some(ref m) = evaluation.model {
                println!("Model:      {}", m);
            }
            println!("FLIP Score: {:.2}", evaluation.score);
            if let Some(s) = evaluation.dimensions.get("semantic_match") {
                println!("  semantic_match:        {:.2}", s);
            }
            if let Some(c) = evaluation.dimensions.get("requirement_coverage") {
                println!("  requirement_coverage:  {:.2}", c);
            }
            if let Some(s) = evaluation.dimensions.get("specificity_match") {
                println!("  specificity_match:     {:.2}", s);
            }
            if let Some(h) = evaluation.dimensions.get("hallucination_rate") {
                println!("  hallucination_rate:    {:.2}", h);
            }
            println!("Evaluator:  {}", evaluation.evaluator);
            println!("Saved to:   {}", eval_path.display());

            // Show a snippet of the inferred prompt
            let snippet = if parsed_inference.inferred_prompt.len() > 200 {
                let end = parsed_inference.inferred_prompt.floor_char_boundary(200);
                format!("{}...", &parsed_inference.inferred_prompt[..end])
            } else {
                parsed_inference.inferred_prompt.clone()
            };
            println!("\nInferred prompt (preview):\n  {}", snippet);
        }
    } else {
        agency::init(&agency_dir)?;
        let eval_path = agency::save_evaluation(&evaluation, &agency_dir.join("evaluations"))
            .context("Failed to save FLIP evaluation")?;

        if json {
            let out = serde_json::json!({
                "task_id": task_id,
                "evaluation_id": evaluation.id,
                "flip_score": evaluation.score,
                "dimensions": evaluation.dimensions,
                "inferred_prompt": parsed_inference.inferred_prompt,
                "notes": parsed_comparison.notes,
                "evaluator": evaluation.evaluator,
                "model": evaluation.model,
                "source": "flip",
                "path": eval_path.display().to_string(),
                "warning": "No identity assigned — performance records not updated",
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("\n=== FLIP Evaluation Complete ===");
            println!("Task:       {} ({})", task.title, task_id);
            println!("FLIP Score: {:.2}", evaluation.score);
            println!("Evaluator:  {}", evaluation.evaluator);
            println!("Saved to:   {}", eval_path.display());
            println!(
                "Warning: no identity assigned — role/tradeoff performance records not updated"
            );
        }
    }

    // Persist combined token usage from both FLIP phases to the .flip-* task
    let combined_usage = combine_token_usage(&[inference_token_usage, comparison_token_usage]);
    if let Some(ref usage) = combined_usage {
        let eval_task_id = format!(".flip-{}", task_id);
        let graph_path = super::graph_path(dir);
        let usage_clone = usage.clone();
        let _ = workgraph::parser::modify_graph(&graph_path, |graph| {
            if let Some(eval_task) = graph.get_task_mut(&eval_task_id) {
                eval_task.token_usage = Some(usage_clone.clone());
                true
            } else {
                false
            }
        });
        // Emit machine-readable token summary for inline eval capture.
        if let Ok(json) = serde_json::to_string(usage) {
            eprintln!("__WG_TOKENS__:{}", json);
        }
    }

    Ok(())
}

/// Combine multiple optional TokenUsage values into a single sum.
fn combine_token_usage(usages: &[Option<TokenUsage>]) -> Option<TokenUsage> {
    let mut total = TokenUsage {
        cost_usd: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    let mut found_any = false;
    for usage in usages.iter().flatten() {
        found_any = true;
        total.cost_usd += usage.cost_usd;
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        total.cache_read_input_tokens += usage.cache_read_input_tokens;
        total.cache_creation_input_tokens += usage.cache_creation_input_tokens;
    }
    if found_any { Some(total) } else { None }
}

/// Output shape for FLIP inference phase.
#[derive(serde::Deserialize)]
struct FlipInferenceOutput {
    inferred_prompt: String,
}

/// Output shape for FLIP comparison phase.
#[derive(serde::Deserialize)]
struct FlipComparisonOutput {
    flip_score: f64,
    #[serde(default)]
    dimensions: HashMap<String, f64>,
    #[serde(default)]
    notes: String,
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
        let eval_path = record_evaluation_with_inference(&evaluation, &agency_dir, &config.agency)
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
                    println!(
                        "  Score: {:.3}  Source: {}  Agent: {}  {}",
                        e.score,
                        e.source,
                        if e.agent_id.is_empty() {
                            "-"
                        } else {
                            &e.agent_id[..e.agent_id.floor_char_boundary(10)]
                        },
                        e.timestamp
                    );
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
                &e.agent_id[..e.agent_id.floor_char_boundary(10)]
            } else {
                &e.agent_id
            };
            let task_display = if e.task_id.len() > 18 {
                &e.task_id[..e.task_id.floor_char_boundary(18)]
            } else {
                &e.task_id
            };
            let source_display = if e.source.len() > 14 {
                &e.source[..e.source.floor_char_boundary(14)]
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
    #[serde(default)]
    verdict: Option<String>,
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

/// Check the eval gate: if a threshold is configured and the task is gated,
/// reject (fail) the original task when the evaluation score is below the
/// threshold. Returns `true` if the task was rejected.
///
/// Eval gate applies when:
/// 1. `config.agency.eval_gate_threshold` is set, AND
/// 2. Either `config.agency.eval_gate_all` is true, OR the task has the
///    "eval-gate" tag.
///
/// When rejecting, this function:
/// - Fails the original task with a descriptive reason
/// - Warns about any downstream tasks that are already in-progress
fn check_eval_gate(
    dir: &Path,
    task_id: &str,
    task_tags: &[String],
    evaluation: &Evaluation,
    config: &Config,
    json: bool,
) -> Result<bool> {
    let threshold = match config.agency.eval_gate_threshold {
        Some(t) => t,
        None => return Ok(false), // No threshold configured
    };

    // Check if this task is gated
    let is_gated = config.agency.eval_gate_all || task_tags.iter().any(|t| t == "eval-gate");
    if !is_gated {
        return Ok(false);
    }

    // Skip system evaluations (infrastructure failures) - these should not trigger task failure
    if evaluation.source == "system" {
        return Ok(false);
    }

    // Check if score is below threshold
    if evaluation.score >= threshold {
        return Ok(false); // Score is acceptable
    }

    // Score is below threshold — reject the task
    let reason = format!(
        "evaluation rejected: score {:.2} below threshold {:.2} ({})",
        evaluation.score, threshold, evaluation.notes
    );

    // Warn about in-progress dependents before rejecting
    let graph_path = super::graph_path(dir);
    if graph_path.exists() {
        let graph = workgraph::parser::load_graph(&graph_path)?;
        let in_progress_dependents: Vec<_> = graph
            .tasks()
            .filter(|t| {
                t.after.contains(&task_id.to_string())
                    && t.status == workgraph::graph::Status::InProgress
            })
            .map(|t| t.id.clone())
            .collect();

        if !in_progress_dependents.is_empty() {
            let warning = format!(
                "Warning: {} dependent task(s) already in-progress when eval gate rejected '{}': [{}]. \
                 These agents will NOT be killed but new dependents will be blocked.",
                in_progress_dependents.len(),
                task_id,
                in_progress_dependents.join(", ")
            );
            if json {
                eprintln!("{}", warning);
            } else {
                println!("{}", warning);
            }
        }
    }

    // Reject the original task
    super::fail::run_eval_reject(dir, task_id, Some(&reason))?;

    // Auto-rescue: evaluation-drives-remediation. The evaluator's notes
    // become the rescue task's brief, so the rescue worker knows what
    // went wrong and what the fix should be. The rescue task is
    // first-class (no dot-prefix), visible in wg list / wg show, and
    // inherits the failed task's graph slot — successors unblock from
    // it instead of the failed target. See docs/design/
    // nex-as-coordinator.md on the broader "real work in the regular
    // graph" principle.
    if config.agency.auto_rescue_on_eval_fail {
        let eval_task_id = format!(".evaluate-{}", task_id);
        let rescue_desc = format!(
            "Evaluation of the prior attempt scored {:.2}/1.0 (below the {:.2} \
             threshold).\n\n**Evaluator's notes:**\n\n{}\n\n**Your job:** \
             address the issues above and complete the task correctly. The \
             full source task context is available via `wg show {}`. Evaluation \
             artifacts are in `.workgraph/agency/evaluations/`.",
            evaluation.score, threshold, evaluation.notes, task_id
        );

        match super::rescue::run(
            dir,
            task_id,
            &rescue_desc,
            None,
            None,
            Some(&eval_task_id),
            Some("eval-gate"),
        ) {
            Ok(new_id) => {
                let msg = format!(
                    "  [auto-rescue] created '{}' from eval notes — successors now \
                     unblock from the rescue instead of the failed target",
                    new_id
                );
                if json {
                    eprintln!("{}", msg);
                } else {
                    println!("{}", msg);
                }
            }
            Err(e) => {
                // Non-fatal: the fail-reject already landed. Rescue is
                // bonus. Log and move on so the eval still records.
                eprintln!(
                    "\x1b[33mwarning:\x1b[0m auto-rescue failed for '{}': {}",
                    task_id, e
                );
            }
        }
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// LLM verification gate — per docs/design/llm-verification-gate.md
// ---------------------------------------------------------------------------

/// The gate verdict produced by the evaluator for `validation = "llm"` tasks.
///
/// Maps to the `decision` field in the `gate` block of the evaluator's JSON
/// output. Derived from the overall score when no explicit gate block is
/// present (see `GateDecision::from_evaluation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    Pass,
    Fail,
    Uncertain,
}

/// Action taken by `apply_gate_decision`. Returned for observability and
/// used by callers to wire further side effects (notifications, rescue, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    /// Source task was approved (transitioned PendingValidation → Done).
    Approved,
    /// Source task was rejected (wg reject was called — reopen or fail).
    Rejected,
    /// Source task stayed in PendingValidation awaiting human adjudication.
    Held,
    /// Source task was not in PendingValidation — gate took no action.
    Skipped,
}

/// The structured gate decision the evaluator produces for a gated task.
///
/// Parallels the `gate` block of the extended `EvalOutput` in the design.
#[derive(Debug, Clone)]
pub struct GateDecision {
    pub decision: GateVerdict,
    pub confidence: f64,
    pub must_fix: Vec<String>,
    pub rationale: String,
}

impl GateDecision {
    /// Derive a gate decision from a plain evaluation score/notes when the
    /// evaluator did not emit an explicit gate block. Uses the configured
    /// `eval_gate_threshold` and `gate_confidence_threshold` as bands:
    /// - score >= gate_confidence_threshold → Pass, confidence = score
    /// - score < fail_cutoff (0.4 by default)  → Fail, confidence = 1 - score
    /// - otherwise                             → Uncertain, confidence = low
    pub fn from_evaluation(evaluation: &Evaluation, config: &Config) -> Self {
        let pass_threshold = config.agency.gate_confidence_threshold;
        let fail_cutoff = 0.4f64;
        if evaluation.score >= pass_threshold {
            GateDecision {
                decision: GateVerdict::Pass,
                confidence: evaluation.score,
                must_fix: Vec::new(),
                rationale: evaluation.notes.clone(),
            }
        } else if evaluation.score < fail_cutoff {
            GateDecision {
                decision: GateVerdict::Fail,
                confidence: 1.0 - evaluation.score,
                must_fix: if evaluation.notes.is_empty() {
                    Vec::new()
                } else {
                    vec![evaluation.notes.clone()]
                },
                rationale: evaluation.notes.clone(),
            }
        } else {
            GateDecision {
                decision: GateVerdict::Uncertain,
                confidence: (pass_threshold - evaluation.score).abs().max(0.1),
                must_fix: Vec::new(),
                rationale: evaluation.notes.clone(),
            }
        }
    }
}

/// Apply a gate decision to a source task that is in PendingValidation
/// because its `validation = "llm"`.
///
/// - Pass + confidence ≥ threshold → approve::run (→ Done)
/// - Fail + confidence ≥ threshold → reject::run (→ Open or Failed)
/// - Uncertain or low-confidence    → policy-driven (escalate / retry / fail-closed)
///
/// Always increments `gate_attempts` and logs the decision on the task.
/// Returns the action taken so callers can wire notifications.
pub fn apply_gate_decision(
    dir: &Path,
    task_id: &str,
    gate: &GateDecision,
    config: &Config,
) -> Result<GateAction> {
    let path = super::graph_path(dir);
    if !path.exists() {
        bail!("Workgraph not initialized. Run `wg init` first.");
    }

    // Snapshot prerequisites and always bump gate_attempts so the caller
    // (and the escalate / retry policies) see the same counter.
    let mut is_pending = false;
    let mut attempts_now: u32 = 0;
    let mut validation_is_llm = false;
    let _ = workgraph::parser::modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(task_id) {
            Some(t) => t,
            None => return false,
        };
        is_pending = matches!(task.status, Status::PendingValidation);
        validation_is_llm = task.validation.as_deref() == Some("llm");
        task.gate_attempts = task.gate_attempts.saturating_add(1);
        attempts_now = task.gate_attempts;
        let verdict = match gate.decision {
            GateVerdict::Pass => "pass",
            GateVerdict::Fail => "fail",
            GateVerdict::Uncertain => "uncertain",
        };
        task.log.push(LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: None,
            user: Some(workgraph::current_user()),
            message: format!(
                "LLM gate decision: {} (confidence {:.2}, attempts {}/{}). {}",
                verdict,
                gate.confidence,
                attempts_now,
                config.agency.gate_max_attempts,
                gate.rationale
            ),
        });
        true
    })
    .context("Failed to save graph for gate decision")?;

    if !is_pending {
        return Ok(GateAction::Skipped);
    }
    if !validation_is_llm {
        // The source task is PendingValidation but not llm-mode — leave it
        // alone; the existing external-validation path controls it.
        return Ok(GateAction::Held);
    }

    let threshold = config.agency.gate_confidence_threshold;
    let high_conf = gate.confidence >= threshold;
    let over_budget = attempts_now >= config.agency.gate_max_attempts;

    match (gate.decision, high_conf, over_budget) {
        (GateVerdict::Pass, true, _) => {
            super::approve::run(dir, task_id)?;
            Ok(GateAction::Approved)
        }
        (GateVerdict::Fail, true, _) => {
            let reason = if gate.must_fix.is_empty() {
                gate.rationale.clone()
            } else {
                format!(
                    "LLM gate rejected (confidence {:.2}). Must fix:\n- {}",
                    gate.confidence,
                    gate.must_fix.join("\n- ")
                )
            };
            super::reject::run(dir, task_id, &reason)?;
            Ok(GateAction::Rejected)
        }
        _ => {
            // Uncertain, low-confidence, or over-budget: policy-driven.
            match config.agency.gate_uncertain_policy.as_str() {
                "fail-closed" => {
                    let reason = format!(
                        "LLM gate uncertain (confidence {:.2}, attempts {}): {}",
                        gate.confidence, attempts_now, gate.rationale
                    );
                    super::reject::run(dir, task_id, &reason)?;
                    Ok(GateAction::Rejected)
                }
                "retry" if !over_budget => {
                    // Leave in PendingValidation so the coordinator can
                    // re-dispatch another eval. The gate_attempts counter
                    // we just bumped prevents runaway.
                    Ok(GateAction::Held)
                }
                // "escalate" (default) or "retry" over budget
                _ => Ok(GateAction::Held),
            }
        }
    }
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

    #[test]
    fn flip_score_injected_as_intent_fidelity() {
        let parsed = EvalOutput {
            score: 0.80,
            dimensions: {
                let mut d = HashMap::new();
                d.insert("correctness".to_string(), 0.9);
                d
            },
            notes: "good".to_string(),
        };
        let flip_score: Option<f64> = Some(0.75);

        let mut dimensions = parsed.dimensions;
        if let Some(fs) = flip_score {
            dimensions.insert("intent_fidelity".to_string(), fs);
        }

        assert_eq!(dimensions.get("intent_fidelity"), Some(&0.75));
        assert_eq!(dimensions.get("correctness"), Some(&0.9));
        assert_eq!(dimensions.len(), 2);
    }

    #[test]
    fn no_intent_fidelity_when_flip_score_none() {
        let parsed = EvalOutput {
            score: 0.80,
            dimensions: {
                let mut d = HashMap::new();
                d.insert("correctness".to_string(), 0.9);
                d
            },
            notes: "good".to_string(),
        };
        let flip_score: Option<f64> = None;

        let mut dimensions = parsed.dimensions;
        if let Some(fs) = flip_score {
            dimensions.insert("intent_fidelity".to_string(), fs);
        }

        assert!(dimensions.get("intent_fidelity").is_none());
        assert_eq!(dimensions.len(), 1);
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // '─' is a 3-byte UTF-8 char (E2 94 80).
        // Build a string where a naive byte slice at 10 would land inside a multi-byte char.
        let text = "ab─cd─ef─gh─ij"; // bytes: a(1) b(1) ─(3) c(1) d(1) ─(3) ...
        assert!(text.len() > 10);

        // Naive &text[..10] would panic because byte 10 is inside '─'.
        // floor_char_boundary finds the nearest valid boundary at or before 10.
        let end = text.floor_char_boundary(10);
        let truncated = &text[..end];
        // Must not panic, and must be valid UTF-8 (guaranteed by &str).
        assert!(truncated.len() <= 10);
        assert!(text.is_char_boundary(end));

        // Also test with emoji (4-byte char)
        let emoji_text = "hello 🎉 world";
        let end2 = emoji_text.floor_char_boundary(8);
        let truncated2 = &emoji_text[..end2];
        assert!(truncated2.len() <= 8);
        assert!(emoji_text.is_char_boundary(end2));
    }

    // -------------------------------------------------------------------
    // LLM gate decision tests (docs/design/llm-verification-gate.md)
    // -------------------------------------------------------------------

    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

    fn setup_gate_fixture(dir: &Path, validation: Option<&str>) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = super::super::graph_path(dir);
        let mut graph = WorkGraph::new();
        let task = Task {
            id: "t1".to_string(),
            title: "Test llm-gated task".to_string(),
            status: Status::PendingValidation,
            validation: validation.map(String::from),
            ..Task::default()
        };
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();
        path
    }

    fn cfg_with_threshold(threshold: f64) -> Config {
        let mut cfg = Config::default();
        cfg.agency.gate_confidence_threshold = threshold;
        cfg
    }

    #[test]
    fn test_llm_verify_pass() {
        // Pass decision with confidence above threshold → source task
        // transitions PendingValidation → Done via approve.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_gate_fixture(dir_path, Some("llm"));

        let config = cfg_with_threshold(0.7);
        let gate = GateDecision {
            decision: GateVerdict::Pass,
            confidence: 0.9,
            must_fix: vec![],
            rationale: "all criteria met".to_string(),
        };

        let action = apply_gate_decision(dir_path, "t1", &gate, &config).unwrap();
        assert_eq!(action, GateAction::Approved);

        let path = super::super::graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
        assert_eq!(task.gate_attempts, 1);
        assert!(task.log.iter().any(|e| e.message.contains("LLM gate decision: pass")));
    }

    #[test]
    fn test_llm_verify_fail() {
        // Fail decision with confidence above threshold → source task
        // transitions PendingValidation → Open (for re-dispatch) via reject.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_gate_fixture(dir_path, Some("llm"));

        let config = cfg_with_threshold(0.7);
        let gate = GateDecision {
            decision: GateVerdict::Fail,
            confidence: 0.95,
            must_fix: vec!["tests are missing".to_string()],
            rationale: "work does not match task description".to_string(),
        };

        let action = apply_gate_decision(dir_path, "t1", &gate, &config).unwrap();
        assert_eq!(action, GateAction::Rejected);

        let path = super::super::graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        // reject::run reopens the task by default (rejection_count < max)
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.rejection_count, 1);
        assert_eq!(task.gate_attempts, 1);
        assert!(
            task.log.iter().any(|e| e.message.contains("LLM gate decision: fail")),
            "expected fail decision log entry"
        );
    }

    #[test]
    fn test_llm_verify_uncertain() {
        // Uncertain decision → task stays in PendingValidation (escalate policy),
        // gate_attempts is bumped so the retry/escalate logic can bound cost.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_gate_fixture(dir_path, Some("llm"));

        let mut config = cfg_with_threshold(0.7);
        config.agency.gate_uncertain_policy = "escalate".to_string();
        let gate = GateDecision {
            decision: GateVerdict::Uncertain,
            confidence: 0.4,
            must_fix: vec![],
            rationale: "insufficient evidence to decide".to_string(),
        };

        let action = apply_gate_decision(dir_path, "t1", &gate, &config).unwrap();
        assert_eq!(action, GateAction::Held);

        let path = super::super::graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        // Stays in PendingValidation for human adjudication.
        assert_eq!(task.status, Status::PendingValidation);
        assert_eq!(task.gate_attempts, 1);
        assert!(
            task.log.iter().any(|e| e.message.contains("LLM gate decision: uncertain")),
            "expected uncertain decision log entry"
        );
    }

    #[test]
    fn test_llm_verify_fail_closed_policy_rejects_uncertain() {
        // fail-closed: uncertain verdicts are converted into rejections.
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_gate_fixture(dir_path, Some("llm"));

        let mut config = cfg_with_threshold(0.7);
        config.agency.gate_uncertain_policy = "fail-closed".to_string();
        let gate = GateDecision {
            decision: GateVerdict::Uncertain,
            confidence: 0.4,
            must_fix: vec![],
            rationale: "ambiguous".to_string(),
        };

        let action = apply_gate_decision(dir_path, "t1", &gate, &config).unwrap();
        assert_eq!(action, GateAction::Rejected);

        let path = super::super::graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_gate_decision_from_evaluation_score_bands() {
        let config = cfg_with_threshold(0.7);
        let mk_eval = |score: f64| Evaluation {
            id: "e".into(),
            task_id: "t1".into(),
            agent_id: String::new(),
            role_id: String::new(),
            tradeoff_id: String::new(),
            score,
            dimensions: HashMap::new(),
            notes: String::new(),
            evaluator: String::new(),
            timestamp: String::new(),
            model: None,
            source: String::new(),
        };
        assert_eq!(
            GateDecision::from_evaluation(&mk_eval(0.9), &config).decision,
            GateVerdict::Pass
        );
        assert_eq!(
            GateDecision::from_evaluation(&mk_eval(0.3), &config).decision,
            GateVerdict::Fail
        );
        assert_eq!(
            GateDecision::from_evaluation(&mk_eval(0.55), &config).decision,
            GateVerdict::Uncertain
        );
    }

    #[test]
    fn test_gate_skips_non_pending_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Task in Done status — gate is a no-op
        fs::create_dir_all(dir_path).unwrap();
        let path = super::super::graph_path(dir_path);
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: "t1".to_string(),
            title: "already done".to_string(),
            status: Status::Done,
            validation: Some("llm".to_string()),
            ..Task::default()
        }));
        save_graph(&graph, &path).unwrap();

        let config = cfg_with_threshold(0.7);
        let gate = GateDecision {
            decision: GateVerdict::Pass,
            confidence: 0.95,
            must_fix: vec![],
            rationale: String::new(),
        };
        let action = apply_gate_decision(dir_path, "t1", &gate, &config).unwrap();
        assert_eq!(action, GateAction::Skipped);

        let graph = load_graph(&path).unwrap();
        // Status unchanged
        assert_eq!(graph.get_task("t1").unwrap().status, Status::Done);
    }
}

mod deferred;
mod fanout;
mod meta;
mod operations;
pub(crate) mod partition;
mod parser;
mod prompt;
mod strategy;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use workgraph::agency::{self, Evaluation};
use workgraph::config::Config;

pub use strategy::Strategy;

pub use deferred::{run_deferred_approve, run_deferred_list, run_deferred_reject};

use deferred::defer_self_mutation;
use meta::print_operation_result;
use operations::apply_operation;
use parser::parse_evolver_output;
use prompt::{build_evolver_prompt, build_performance_summary, load_evolver_skills};

/// Run `wg evolve` — trigger an evolution cycle on agency roles and tradeoffs.
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    dry_run: bool,
    strategy: Option<&str>,
    budget: Option<u32>,
    model: Option<&str>,
    json: bool,
    autopoietic: bool,
    max_iterations: Option<u32>,
    cycle_delay: Option<u64>,
    force_fanout: bool,
    single_shot: bool,
) -> Result<()> {
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let evals_dir = agency_dir.join("evaluations");
    let skills_dir = agency_dir.join("evolver-skills");

    // Validate agency exists
    if !roles_dir.exists() || !tradeoffs_dir.exists() {
        bail!("Agency not initialized. Run `wg agency init` first.");
    }

    // Pre-flight: check that claude CLI is available
    if Command::new("claude")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDECODE")
        .arg("--version")
        .output()
        .is_err()
    {
        bail!(
            "The 'claude' CLI is required for evolve but was not found in PATH.\n\
             Install it from https://docs.anthropic.com/en/docs/claude-code and ensure it is on your PATH."
        );
    }

    // Parse strategy
    let strategy = match strategy {
        Some(s) => Strategy::from_str(s)?,
        None => Strategy::All,
    };

    // Load all agency data
    let mut roles = agency::load_all_roles(&roles_dir).context("Failed to load roles")?;
    let mut tradeoffs =
        agency::load_all_tradeoffs(&tradeoffs_dir).context("Failed to load tradeoffs")?;
    let all_evaluations =
        agency::load_all_evaluations(&evals_dir).context("Failed to load evaluations")?;

    // Filter out evaluations from human agents — their work quality isn't a
    // reflection of a role+tradeoff prompt, so including them would pollute
    // the evolution signal.
    let agents_dir = agency_dir.join("cache/agents");
    let agents = agency::load_all_agents_or_warn(&agents_dir);
    let human_agent_ids: HashSet<&str> = agents
        .iter()
        .filter(|a| a.is_human())
        .map(|a| a.id.as_str())
        .collect();
    let evaluations: Vec<Evaluation> = all_evaluations
        .into_iter()
        .filter(|e| e.agent_id.is_empty() || !human_agent_ids.contains(e.agent_id.as_str()))
        .collect();

    if roles.is_empty() && tradeoffs.is_empty() {
        bail!("No roles or tradeoffs found. Run `wg agency init` to seed starters.");
    }

    // Load config for evolver identity and model (needed for routing decision)
    let config = Config::load_or_default(dir);

    // Route to fan-out mode or single-shot mode based on eval count and flags
    let use_fanout = if single_shot {
        false
    } else if autopoietic || force_fanout {
        true
    } else {
        evaluations.len() >= fanout::FANOUT_THRESHOLD
    };

    if use_fanout {
        let strategy_str = match strategy {
            Strategy::All => None,
            other => Some(other.label()),
        };
        return fanout::run_fanout(
            dir,
            dry_run,
            strategy_str,
            budget,
            model,
            json,
            autopoietic,
            max_iterations,
            cycle_delay,
            &roles,
            &tradeoffs,
            &evaluations,
            &config,
        );
    }

    // === Single-shot legacy mode ===

    // Load evolver skill documents
    let skill_docs = load_evolver_skills(&skills_dir, strategy)?;

    // Determine model: CLI flag > model routing > legacy config > agent.model
    let model = model
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| {
            config
                .resolve_model_for_role(workgraph::config::DispatchRole::Evolver)
                .model
        });

    // Build performance summary
    let perf_summary = build_performance_summary(&roles, &tradeoffs, &evaluations, &config);

    // Build the evolver prompt
    let prompt = build_evolver_prompt(
        &perf_summary,
        &skill_docs,
        strategy,
        budget,
        &config,
        &roles,
        &tradeoffs,
        &agency_dir,
    );

    // Generate a run ID
    let run_id = format!("run-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));

    if dry_run {
        if json {
            let out = serde_json::json!({
                "mode": "dry_run",
                "strategy": strategy.label(),
                "budget": budget,
                "model": model,
                "run_id": run_id,
                "roles": roles.len(),
                "tradeoffs": tradeoffs.len(),
                "evaluations": evaluations.len(),
                "skill_documents": skill_docs.len(),
                "prompt_length": prompt.len(),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("=== Dry Run: wg evolve ===\n");
            println!("Strategy:        {}", strategy.label());
            println!(
                "Budget:          {}",
                budget
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "unlimited".into())
            );
            println!("Model:           {}", model);
            println!("Run ID:          {}", run_id);
            println!("Roles:           {}", roles.len());
            println!("Tradeoffs:       {}", tradeoffs.len());
            println!("Evaluations:     {}", evaluations.len());
            println!("Skill docs:      {}", skill_docs.len());
            println!("Prompt length:   {} chars", prompt.len());
            if let Some(ref agent) = config.agency.evolver_agent {
                println!("Evolver agent:   {}", agent);
            }
            println!("\n--- Evolver Prompt ---\n");
            println!("{}", prompt);
        }
        return Ok(());
    }

    // Spawn the evolver agent
    println!(
        "Running evolution cycle (strategy: {}, model: {})...",
        strategy.label(),
        model
    );

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
            "Evolver agent failed (exit code {:?}):\n{}",
            output.status.code(),
            stderr
        );
    }

    let raw_output = String::from_utf8_lossy(&output.stdout);

    // Parse the structured output
    let evolver_output =
        parse_evolver_output(&raw_output).context("Failed to parse evolver output")?;

    let actual_run_id = evolver_output.run_id.as_deref().unwrap_or(&run_id);

    // Apply budget limit
    let operations = if let Some(max) = budget {
        if evolver_output.operations.len() > max as usize {
            eprintln!(
                "Budget limit: applying {} of {} proposed operations",
                max,
                evolver_output.operations.len()
            );
            evolver_output.operations[..max as usize].to_vec()
        } else {
            evolver_output.operations
        }
    } else {
        evolver_output.operations
    };

    if operations.is_empty() {
        if json {
            let out = serde_json::json!({
                "run_id": actual_run_id,
                "operations_applied": 0,
                "summary": evolver_output.summary,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("\nNo operations proposed by the evolver.");
            if let Some(ref summary) = evolver_output.summary {
                println!("Summary: {}", summary);
            }
        }
        return Ok(());
    }

    // Determine evolver's own role/tradeoff IDs for self-mutation detection
    let evolver_entity_ids: HashSet<String> = {
        let mut ids = HashSet::new();
        if let Some(ref agent_hash) = config.agency.evolver_agent {
            let agent_path = agency_dir
                .join("agents")
                .join(format!("{}.yaml", agent_hash));
            if let Ok(agent) = agency::load_agent(&agent_path) {
                ids.insert(agent.role_id.clone());
                ids.insert(agent.tradeoff_id);
            }
        }
        ids
    };

    // Apply operations
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut applied = 0;
    let mut deferred = 0;

    for op in &operations {
        // Self-mutation safety: operations targeting the evolver's own
        // role or tradeoff are deferred to a verified workgraph task
        // that requires human approval.
        if !evolver_entity_ids.is_empty()
            && let Some(ref target) = op.target_id
        {
            let target_ids: Vec<&str> = target
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if target_ids
                .iter()
                .any(|tid| evolver_entity_ids.contains(*tid))
            {
                match defer_self_mutation(op, dir, actual_run_id) {
                    Ok(task_id) => {
                        deferred += 1;
                        let result = serde_json::json!({
                            "op": op.op,
                            "target_id": op.target_id,
                            "status": "deferred_for_review",
                            "review_task": task_id,
                            "reason": "Operation targets evolver's own identity — requires human approval",
                        });
                        if !json {
                            eprintln!(
                                "  [deferred] {} on {:?} → review task '{}' (evolver self-mutation)",
                                op.op, op.target_id, task_id,
                            );
                        }
                        results.push(result);
                    }
                    Err(e) => {
                        let err_msg = format!("Failed to defer self-mutation {:?}: {}", op.op, e);
                        eprintln!("{}", err_msg);
                        results.push(serde_json::json!({
                            "op": op.op,
                            "error": err_msg,
                        }));
                    }
                }
                continue;
            }
        }

        // Meta-agent self-mutation safety: any meta_swap/meta_compose targeting
        // the "evolver" slot is deferred for human approval.
        if matches!(
            op.op.as_str(),
            "meta_swap_role" | "meta_swap_tradeoff" | "meta_compose_agent"
        ) && op.meta_role.as_deref() == Some("evolver")
        {
            match defer_self_mutation(op, dir, actual_run_id) {
                Ok(task_id) => {
                    deferred += 1;
                    let result = serde_json::json!({
                        "op": op.op,
                        "meta_role": "evolver",
                        "status": "deferred_for_review",
                        "review_task": task_id,
                        "reason": "Operation targets evolver's own configuration — requires human approval",
                    });
                    if !json {
                        eprintln!(
                            "  [deferred] {} on evolver → review task '{}' (evolver self-mutation)",
                            op.op, task_id,
                        );
                    }
                    results.push(result);
                }
                Err(e) => {
                    let err_msg =
                        format!("Failed to defer evolver self-mutation {:?}: {}", op.op, e);
                    eprintln!("{}", err_msg);
                    results.push(serde_json::json!({
                        "op": op.op,
                        "error": err_msg,
                    }));
                }
            }
            continue;
        }

        match apply_operation(
            op,
            &roles,
            &tradeoffs,
            actual_run_id,
            &roles_dir,
            &tradeoffs_dir,
            &agency_dir,
            dir,
        ) {
            Ok(result) => {
                applied += 1;
                if !json {
                    print_operation_result(op, &result);
                }
                results.push(result);

                // Reload roles/tradeoffs so subsequent operations see newly
                // created entities (e.g. a modify_role targeting a role that
                // was just created in the same batch).
                if matches!(
                    op.op.as_str(),
                    "create_role"
                        | "modify_role"
                        | "retire_role"
                        | "component_substitution"
                        | "config_add_component"
                        | "config_remove_component"
                        | "config_swap_outcome"
                        | "random_compose_role"
                ) && let Ok(updated) = agency::load_all_roles(&roles_dir)
                {
                    roles = updated;
                }
                if matches!(
                    op.op.as_str(),
                    "create_motivation" | "modify_motivation" | "retire_motivation"
                ) && let Ok(updated) = agency::load_all_tradeoffs(&tradeoffs_dir)
                {
                    tradeoffs = updated;
                }
            }
            Err(e) => {
                let err_msg = format!("Failed to apply operation {:?}: {}", op.op, e);
                eprintln!("{}", err_msg);
                results.push(serde_json::json!({
                    "op": op.op,
                    "error": err_msg,
                }));
            }
        }
    }

    // Save evolution run report
    let report = serde_json::json!({
        "run_id": actual_run_id,
        "timestamp": Utc::now().to_rfc3339(),
        "strategy": strategy.label(),
        "model": model,
        "budget": budget,
        "input": {
            "roles": roles.len(),
            "tradeoffs": tradeoffs.len(),
            "evaluations": evaluations.len(),
            "skill_documents": skill_docs.len(),
        },
        "operations_proposed": operations.len(),
        "operations_applied": applied,
        "operations_deferred": deferred,
        "results": results,
        "summary": evolver_output.summary,
        "raw_output": raw_output.as_ref(),
    });

    let runs_dir = agency_dir.join("evolution_runs");
    fs::create_dir_all(&runs_dir)?;
    let report_path = runs_dir.join(format!("{}.json", actual_run_id));
    fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("\n=== Evolution Complete ===");
        println!("Run ID:     {}", actual_run_id);
        println!("Strategy:   {}", strategy.label());
        println!("Model:      {}", model);
        println!("Applied:    {} of {} operations", applied, operations.len());
        if deferred > 0 {
            println!(
                "Deferred:   {} (evolver self-mutations, require human approval)",
                deferred
            );
        }
        if let Some(ref summary) = evolver_output.summary {
            println!("\nSummary:\n  {}", summary);
        }
        println!("\nReport saved: {}", report_path.display());
    }

    // Record evolver agent performance (if evolver_agent is configured)
    // This tracks whether the evolver produced valid, applicable operations.
    if let Some(ref evolver_hash) = config.agency.evolver_agent {
        let evolver_agent_path = agents_dir.join(format!("{}.yaml", evolver_hash));
        if let Ok(evolver_agent) = agency::load_agent(&evolver_agent_path) {
            // Quality signal: proportion of operations that succeeded
            let total = operations.len() as f64;
            let score = if total > 0.0 {
                (applied as f64 / total).min(1.0)
            } else {
                0.5 // no operations proposed: neutral
            };

            let eval_of_evolver = Evaluation {
                id: format!(
                    "meta-eval-evolve-{}-{}",
                    actual_run_id,
                    chrono::Utc::now().to_rfc3339().replace(':', "-")
                ),
                task_id: format!("evolve-{}", actual_run_id),
                agent_id: evolver_agent.id.clone(),
                role_id: evolver_agent.role_id.clone(),
                tradeoff_id: evolver_agent.tradeoff_id.clone(),
                score,
                dimensions: HashMap::new(),
                notes: format!(
                    "Auto-recorded: evolver applied {}/{} operations (strategy: {})",
                    applied,
                    operations.len(),
                    strategy.label()
                ),
                evaluator: "system".to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                model: None,
                source: "llm".to_string(),
            };

            if let Err(e) = agency::record_evaluation(&eval_of_evolver, &agency_dir) {
                eprintln!("Warning: failed to record evolver performance: {}", e);
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
    use super::operations::apply_operation;
    use super::parser::extract_json;
    use super::prompt::{build_evolver_prompt, build_performance_summary};
    use super::strategy::{EvolverOperation, EvolverOutput};
    use super::*;
    use workgraph::agency::{AccessControl, Lineage, PerformanceRecord, Role, TradeoffConfig};

    #[test]
    fn test_strategy_from_str() {
        assert_eq!(Strategy::from_str("mutation").unwrap(), Strategy::Mutation);
        assert_eq!(
            Strategy::from_str("crossover").unwrap(),
            Strategy::Crossover
        );
        assert_eq!(
            Strategy::from_str("gap-analysis").unwrap(),
            Strategy::GapAnalysis
        );
        assert_eq!(
            Strategy::from_str("retirement").unwrap(),
            Strategy::Retirement
        );
        assert_eq!(
            Strategy::from_str("motivation-tuning").unwrap(),
            Strategy::MotivationTuning
        );
        assert_eq!(Strategy::from_str("all").unwrap(), Strategy::All);
        assert!(Strategy::from_str("invalid").is_err());
    }

    #[test]
    fn test_extract_json_plain() {
        let input = r#"{"run_id": "test", "operations": [], "summary": "nothing"}"#;
        let result = extract_json(input).unwrap();
        assert!(result.contains("test"));
    }

    #[test]
    fn test_extract_json_with_fences() {
        let input = "```json\n{\"run_id\": \"test\", \"operations\": []}\n```";
        let result = extract_json(input).unwrap();
        assert!(result.contains("test"));
    }

    #[test]
    fn test_extract_json_with_surrounding_text() {
        let input = "Here is my analysis:\n{\"run_id\": \"r1\", \"operations\": [], \"summary\": \"ok\"}\nDone.";
        let result = extract_json(input).unwrap();
        assert!(result.contains("r1"));
    }

    #[test]
    fn test_extract_json_returns_none_for_garbage() {
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn test_parse_evolver_output() {
        let json = r#"{
            "run_id": "run-20250201",
            "operations": [
                {
                    "op": "create_role",
                    "new_id": "test-role",
                    "name": "Test Role",
                    "description": "A test",
                    "skills": ["testing"],
                    "desired_outcome": "Pass tests",
                    "rationale": "Need more testing"
                }
            ],
            "summary": "Added test role"
        }"#;

        let output: EvolverOutput = serde_json::from_str(json).unwrap();
        assert_eq!(output.run_id, Some("run-20250201".to_string()));
        assert_eq!(output.operations.len(), 1);
        assert_eq!(output.operations[0].op, "create_role");
        assert_eq!(output.operations[0].new_id, Some("test-role".to_string()));
    }

    #[test]
    fn test_parse_retire_operation() {
        let json = r#"{
            "operations": [
                {
                    "op": "retire_role",
                    "target_id": "bad-role",
                    "rationale": "Consistently low scores"
                }
            ]
        }"#;

        let output: EvolverOutput = serde_json::from_str(json).unwrap();
        assert_eq!(output.operations.len(), 1);
        assert_eq!(output.operations[0].op, "retire_role");
        assert_eq!(output.operations[0].target_id, Some("bad-role".to_string()));
    }

    #[test]
    fn test_build_performance_summary_empty() {
        let summary = build_performance_summary(&[], &[], &[], &Config::default());
        assert!(summary.contains("Total roles: 0"));
        assert!(summary.contains("Total evaluations: 0"));
    }

    #[test]
    fn test_build_performance_summary_with_data() {
        let roles = vec![Role {
            id: "r1".into(),
            name: "Role 1".into(),
            description: "Test role".into(),
            component_ids: vec![],
            outcome_id: "Test".into(),
            performance: PerformanceRecord {
                task_count: 2,
                avg_score: Some(0.75),
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        }];
        let motivations = vec![TradeoffConfig {
            id: "m1".into(),
            name: "Mot 1".into(),
            description: "Test motivation".into(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord {
                task_count: 1,
                avg_score: Some(0.60),
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        }];

        let summary = build_performance_summary(&roles, &motivations, &[], &Config::default());
        assert!(summary.contains("Role 1"));
        assert!(summary.contains("Mot 1"));
        assert!(summary.contains("0.750"));
    }

    #[test]
    fn test_apply_create_role() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        let op = EvolverOperation {
            op: "create_role".into(),
            target_id: None,
            new_id: Some("new-role".into()),
            name: Some("New Role".into()),
            description: Some("A new role".into()),
            component_ids: Some(vec!["skill-a".into(), "skill-b".into()]),
            outcome_id: Some("Do things well".into()),
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: Some("Gap analysis".into()),
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "test-run",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");

        // ID should be a content hash, not the LLM-suggested new_id
        let id = result["id"].as_str().unwrap();
        assert!(id.len() == 64, "ID should be a full SHA-256 hex hash");
        assert_ne!(id, "new-role");

        // Verify the file was created with hash-based filename
        let role_path = roles_dir.join(format!("{}.yaml", id));
        assert!(role_path.exists());

        let role = agency::load_role(&role_path).unwrap();
        assert_eq!(role.id, id);
        assert_eq!(role.name, "New Role");
        assert_eq!(role.component_ids.len(), 2);
        assert_eq!(role.lineage.generation, 0);
        assert!(role.lineage.created_by.contains("test-run"));
    }

    #[test]
    fn test_apply_modify_role_mutation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        let parent = Role {
            id: "parent-role".into(),
            name: "Parent".into(),
            description: "Original".into(),
            component_ids: vec!["coding".to_string()],
            outcome_id: "Code well".into(),
            performance: PerformanceRecord {
                task_count: 5,
                avg_score: Some(0.55),
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        };

        let op = EvolverOperation {
            op: "modify_role".into(),
            target_id: Some("parent-role".into()),
            new_id: Some("parent-role-m1".into()),
            name: Some("Parent (Test-Focused)".into()),
            description: Some("Improved".into()),
            component_ids: Some(vec!["coding".into(), "testing".into()]),
            outcome_id: Some("Code and test well".into()),
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: Some("Low completeness scores".into()),
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[parent],
            &[],
            "test-run",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");
        assert_eq!(result["generation"], 1);

        // new_id should be a content hash, not the LLM-suggested slug
        let new_id = result["new_id"].as_str().unwrap();
        assert!(new_id.len() == 64, "ID should be a full SHA-256 hex hash");
        assert_ne!(new_id, "parent-role-m1");

        let role = agency::load_role(&roles_dir.join(format!("{}.yaml", new_id))).unwrap();
        assert_eq!(role.lineage.parent_ids, vec!["parent-role"]);
        assert_eq!(role.lineage.generation, 1);
    }

    #[test]
    fn test_apply_retire_role() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        // Create two roles (can't retire the last one)
        let role_a = Role {
            id: "role-a".into(),
            name: "A".into(),
            description: "".into(),
            component_ids: vec![],
            outcome_id: "".into(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        };
        let role_b = Role {
            id: "role-b".into(),
            name: "B".into(),
            description: "".into(),
            component_ids: vec![],
            outcome_id: "".into(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        };

        agency::save_role(&role_a, &roles_dir).unwrap();
        agency::save_role(&role_b, &roles_dir).unwrap();

        let op = EvolverOperation {
            op: "retire_role".into(),
            target_id: Some("role-a".into()),
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: Some("Poor performance".into()),
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[role_a, role_b],
            &[],
            "test-run",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");

        // .yaml should be gone, .yaml.retired should exist
        assert!(!roles_dir.join("role-a.yaml").exists());
        assert!(roles_dir.join("role-a.yaml.retired").exists());
    }

    #[test]
    fn test_retire_last_role_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        let role = Role {
            id: "only-role".into(),
            name: "Only".into(),
            description: "".into(),
            component_ids: vec![],
            outcome_id: "".into(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        };
        agency::save_role(&role, &roles_dir).unwrap();

        let op = EvolverOperation {
            op: "retire_role".into(),
            target_id: Some("only-role".into()),
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[role],
            &[],
            "test-run",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("only remaining role")
        );
    }

    // =======================================================================
    // parse_evolver_output: complex multi-operation responses
    // =======================================================================

    #[test]
    fn test_parse_evolver_output_multi_operations() {
        let raw = r#"{
            "run_id": "run-20250501-140000",
            "operations": [
                {
                    "op": "create_role",
                    "new_id": "security-expert",
                    "name": "Security Expert",
                    "description": "Specializes in security audits and vulnerability assessment",
                    "skills": ["security-audit", "penetration-testing", "code-review"],
                    "desired_outcome": "Comprehensive security report with remediation steps",
                    "rationale": "Gap analysis revealed no security-focused role"
                },
                {
                    "op": "modify_role",
                    "target_id": "existing-dev",
                    "new_id": "existing-dev-v2",
                    "name": "Enhanced Developer",
                    "description": "Improved developer with testing focus",
                    "skills": ["coding", "testing", "debugging"],
                    "desired_outcome": "Well-tested code",
                    "rationale": "Low test coverage scores"
                },
                {
                    "op": "retire_role",
                    "target_id": "obsolete-role",
                    "rationale": "Consistently underperforming"
                },
                {
                    "op": "create_motivation",
                    "new_id": "security-first",
                    "name": "Security First",
                    "description": "Prioritizes security above all else",
                    "acceptable_tradeoffs": ["Slower delivery", "More verbose code"],
                    "unacceptable_tradeoffs": ["Known vulnerabilities", "Skipping auth checks"],
                    "rationale": "Need security-oriented motivation"
                },
                {
                    "op": "modify_motivation",
                    "target_id": "existing-mot",
                    "new_id": "existing-mot-v2",
                    "name": "Tuned Careful",
                    "description": "Relaxed speed constraints",
                    "acceptable_tradeoffs": ["Moderate slowness"],
                    "unacceptable_tradeoffs": ["Untested code"],
                    "rationale": "Motivation was too conservative"
                },
                {
                    "op": "retire_motivation",
                    "target_id": "bad-mot",
                    "rationale": "Produced poor outcomes"
                }
            ],
            "summary": "Comprehensive evolution: added security role/motivation, improved dev, retired underperformers"
        }"#;

        let output = parse_evolver_output(raw).unwrap();
        assert_eq!(output.run_id, Some("run-20250501-140000".to_string()));
        assert_eq!(output.operations.len(), 6);
        assert_eq!(
            output.summary,
            Some("Comprehensive evolution: added security role/motivation, improved dev, retired underperformers".to_string())
        );

        // Verify operation types in order
        assert_eq!(output.operations[0].op, "create_role");
        assert_eq!(output.operations[1].op, "modify_role");
        assert_eq!(output.operations[2].op, "retire_role");
        assert_eq!(output.operations[3].op, "create_motivation");
        assert_eq!(output.operations[4].op, "modify_motivation");
        assert_eq!(output.operations[5].op, "retire_motivation");

        // Verify fields on the create_role operation
        let create_role = &output.operations[0];
        assert_eq!(create_role.name, Some("Security Expert".to_string()));
        assert_eq!(
            create_role.component_ids,
            Some(vec![
                "security-audit".to_string(),
                "penetration-testing".to_string(),
                "code-review".to_string(),
            ])
        );
        assert_eq!(
            create_role.outcome_id,
            Some("Comprehensive security report with remediation steps".to_string())
        );

        // Verify fields on the create_motivation operation
        let create_mot = &output.operations[3];
        assert_eq!(
            create_mot.acceptable_tradeoffs,
            Some(vec![
                "Slower delivery".to_string(),
                "More verbose code".to_string()
            ])
        );
        assert_eq!(
            create_mot.unacceptable_tradeoffs,
            Some(vec![
                "Known vulnerabilities".to_string(),
                "Skipping auth checks".to_string()
            ])
        );
    }

    #[test]
    fn test_parse_evolver_output_with_markdown_fences_and_commentary() {
        let raw = r#"I've analyzed the performance data. Here's my evolution plan:

```json
{
    "run_id": "run-fenced",
    "operations": [
        {
            "op": "create_role",
            "name": "Optimizer",
            "description": "Performance optimization specialist",
            "skills": ["profiling", "benchmarking"],
            "desired_outcome": "Measurably faster code"
        }
    ],
    "summary": "Added optimizer role"
}
```

Let me know if you'd like me to adjust anything."#;

        let output = parse_evolver_output(raw).unwrap();
        assert_eq!(output.run_id, Some("run-fenced".to_string()));
        assert_eq!(output.operations.len(), 1);
        assert_eq!(output.operations[0].name, Some("Optimizer".to_string()));
    }

    #[test]
    fn test_parse_evolver_output_no_run_id() {
        let raw = r#"{"operations": [{"op": "retire_role", "target_id": "old"}]}"#;
        let output = parse_evolver_output(raw).unwrap();
        assert_eq!(output.run_id, None);
        assert_eq!(output.summary, None);
        assert_eq!(output.operations.len(), 1);
    }

    #[test]
    fn test_parse_evolver_output_empty_operations() {
        let raw = r#"{"run_id": "noop", "operations": [], "summary": "No changes needed"}"#;
        let output = parse_evolver_output(raw).unwrap();
        assert!(output.operations.is_empty());
        assert_eq!(output.summary, Some("No changes needed".to_string()));
    }

    #[test]
    fn test_parse_evolver_output_garbage_fails() {
        let result = parse_evolver_output("This is not JSON at all");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_evolver_output_missing_operations_fails() {
        let raw = r#"{"run_id": "bad", "summary": "missing operations field"}"#;
        let result = parse_evolver_output(raw);
        assert!(result.is_err());
    }

    // =======================================================================
    // apply_operations: create/modify/retire motivations with lineage
    // =======================================================================

    #[test]
    fn test_apply_create_motivation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "create_motivation".into(),
            target_id: None,
            new_id: Some("new-mot".into()),
            name: Some("Security First".into()),
            description: Some("Prioritizes security".into()),
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: Some(vec!["Slower delivery".into(), "More verbose code".into()]),
            unacceptable_tradeoffs: Some(vec!["Known vulnerabilities".into()]),
            rationale: Some("Gap analysis".into()),
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");
        assert_eq!(result["op"], "create_motivation");

        // ID should be a content hash, not the LLM-suggested new_id
        let id = result["id"].as_str().unwrap();
        assert_eq!(id.len(), 64, "ID should be a full SHA-256 hex hash");
        assert_ne!(id, "new-mot");

        // Verify the file was created and can be loaded
        let mot_path = tradeoffs_dir.join(format!("{}.yaml", id));
        assert!(mot_path.exists());

        let motivation = agency::load_tradeoff(&mot_path).unwrap();
        assert_eq!(motivation.id, id);
        assert_eq!(motivation.name, "Security First");
        assert_eq!(motivation.description, "Prioritizes security");
        assert_eq!(
            motivation.acceptable_tradeoffs,
            vec!["Slower delivery", "More verbose code"]
        );
        assert_eq!(
            motivation.unacceptable_tradeoffs,
            vec!["Known vulnerabilities"]
        );
        assert_eq!(motivation.lineage.generation, 0);
        assert!(motivation.lineage.created_by.contains("test-run"));
        assert!(motivation.lineage.parent_ids.is_empty());
    }

    #[test]
    fn test_apply_create_motivation_missing_name_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "create_motivation".into(),
            target_id: None,
            new_id: None,
            name: None, // missing!
            description: Some("desc".into()),
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("requires name"));
    }

    #[test]
    fn test_apply_modify_motivation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let parent = TradeoffConfig {
            id: "parent-mot".into(),
            name: "Careful".into(),
            description: "Prioritizes reliability".into(),
            acceptable_tradeoffs: vec!["Slow".into()],
            unacceptable_tradeoffs: vec!["Untested code".into()],
            performance: PerformanceRecord {
                task_count: 3,
                avg_score: Some(0.65),
                evaluations: vec![],
            },
            lineage: Lineage {
                parent_ids: vec![],
                generation: 0,
                created_by: "human".into(),
                created_at: chrono::Utc::now(),
            },
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        };

        let op = EvolverOperation {
            op: "modify_motivation".into(),
            target_id: Some("parent-mot".into()),
            new_id: Some("parent-mot-v2".into()),
            name: Some("Carefully Fast".into()),
            description: Some("Balance of speed and reliability".into()),
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: Some(vec!["Moderate slowness".into()]),
            unacceptable_tradeoffs: Some(vec!["Untested code".into(), "Known bugs".into()]),
            rationale: Some("Motivation was too conservative".into()),
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[parent],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");
        assert_eq!(result["op"], "modify_motivation");
        assert_eq!(result["target_id"], "parent-mot");
        assert_eq!(result["generation"], 1);

        // Verify lineage
        let parent_ids: Vec<String> = result["parent_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(parent_ids, vec!["parent-mot"]);

        // ID should be content hash
        let new_id = result["new_id"].as_str().unwrap();
        assert_eq!(new_id.len(), 64);

        // Load and verify
        let mot = agency::load_tradeoff(&tradeoffs_dir.join(format!("{}.yaml", new_id))).unwrap();
        assert_eq!(mot.name, "Carefully Fast");
        assert_eq!(mot.lineage.generation, 1);
        assert_eq!(mot.lineage.parent_ids, vec!["parent-mot"]);
        assert!(mot.lineage.created_by.contains("test-run"));
    }

    #[test]
    fn test_apply_modify_motivation_missing_target_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "modify_motivation".into(),
            target_id: None, // missing!
            new_id: None,
            name: Some("X".into()),
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires target_id")
        );
    }

    #[test]
    fn test_apply_modify_motivation_parent_not_found_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "modify_motivation".into(),
            target_id: Some("nonexistent".into()),
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_apply_modify_motivation_crossover() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let parent_a = TradeoffConfig {
            id: "mot-careful".into(),
            name: "Careful".into(),
            description: "Prioritizes reliability".into(),
            acceptable_tradeoffs: vec!["Slow".into()],
            unacceptable_tradeoffs: vec!["Untested".into()],
            performance: PerformanceRecord {
                task_count: 5,
                avg_score: Some(0.7),
                evaluations: vec![],
            },
            lineage: Lineage {
                parent_ids: vec![],
                generation: 2,
                created_by: "run-1".into(),
                created_at: chrono::Utc::now(),
            },
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        };

        let parent_b = TradeoffConfig {
            id: "mot-fast".into(),
            name: "Fast".into(),
            description: "Prioritizes speed".into(),
            acceptable_tradeoffs: vec!["Verbose".into()],
            unacceptable_tradeoffs: vec!["Unreliable".into()],
            performance: PerformanceRecord {
                task_count: 3,
                avg_score: Some(0.8),
                evaluations: vec![],
            },
            lineage: Lineage {
                parent_ids: vec![],
                generation: 1,
                created_by: "run-0".into(),
                created_at: chrono::Utc::now(),
            },
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        };

        let op = EvolverOperation {
            op: "modify_motivation".into(),
            target_id: Some("mot-careful,mot-fast".into()),
            new_id: None,
            name: Some("Balanced".into()),
            description: Some("Balance of speed and reliability".into()),
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: Some(vec!["Moderate slowness".into()]),
            unacceptable_tradeoffs: Some(vec!["Untested".into(), "Unreliable".into()]),
            rationale: Some("Crossover of careful and fast".into()),
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[parent_a, parent_b],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");

        // Generation should be max(2, 1) + 1 = 3
        assert_eq!(result["generation"], 3);

        // Parent IDs should include both parents
        let parent_ids: Vec<String> = result["parent_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(parent_ids, vec!["mot-careful", "mot-fast"]);

        // Verify the content-hash ID
        let new_id = result["new_id"].as_str().unwrap();
        assert_eq!(new_id.len(), 64);

        // Load and verify
        let mot = agency::load_tradeoff(&tradeoffs_dir.join(format!("{}.yaml", new_id))).unwrap();
        assert_eq!(mot.name, "Balanced");
        assert_eq!(mot.lineage.generation, 3);
        assert_eq!(mot.lineage.parent_ids, vec!["mot-careful", "mot-fast"]);
    }

    #[test]
    fn test_apply_modify_motivation_crossover_missing_parent_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let parent_a = TradeoffConfig {
            id: "mot-a".into(),
            name: "A".into(),
            description: "".into(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        };

        let op = EvolverOperation {
            op: "modify_motivation".into(),
            target_id: Some("mot-a,nonexistent".into()),
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[parent_a],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found for crossover")
        );
    }

    #[test]
    fn test_apply_retire_motivation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let mot_a = TradeoffConfig {
            id: "mot-a".into(),
            name: "A".into(),
            description: "".into(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        };
        let mot_b = TradeoffConfig {
            id: "mot-b".into(),
            name: "B".into(),
            description: "".into(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        };

        agency::save_tradeoff(&mot_a, &tradeoffs_dir).unwrap();
        agency::save_tradeoff(&mot_b, &tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "retire_motivation".into(),
            target_id: Some("mot-a".into()),
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: Some("Poor outcomes".into()),
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[mot_a, mot_b],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");
        assert_eq!(result["op"], "retire_motivation");

        // .yaml should be gone, .yaml.retired should exist
        assert!(!tradeoffs_dir.join("mot-a.yaml").exists());
        assert!(tradeoffs_dir.join("mot-a.yaml.retired").exists());
    }

    #[test]
    fn test_retire_last_motivation_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let mot = TradeoffConfig {
            id: "only-mot".into(),
            name: "Only".into(),
            description: "".into(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        };
        agency::save_tradeoff(&mot, &tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "retire_motivation".into(),
            target_id: Some("only-mot".into()),
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[mot],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("only remaining tradeoff")
        );
    }

    #[test]
    fn test_retire_motivation_not_found_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let mot = TradeoffConfig {
            id: "mot-x".into(),
            name: "X".into(),
            description: "".into(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        };

        let op = EvolverOperation {
            op: "retire_motivation".into(),
            target_id: Some("nonexistent".into()),
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[mot],
            "test-run",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // =======================================================================
    // apply_modify_role: crossover lineage (two parents)
    // =======================================================================

    #[test]
    fn test_apply_modify_role_crossover() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        let parent_a = Role {
            id: "parent-a".into(),
            name: "Developer".into(),
            description: "Writes code".into(),
            component_ids: vec!["coding".to_string()],
            outcome_id: "Working code".into(),
            performance: PerformanceRecord {
                task_count: 10,
                avg_score: Some(0.7),
                evaluations: vec![],
            },
            lineage: Lineage {
                parent_ids: vec![],
                generation: 2,
                created_by: "evolver-run-1".into(),
                created_at: chrono::Utc::now(),
            },
            default_context_scope: None,
            default_exec_mode: None,
        };

        let parent_b = Role {
            id: "parent-b".into(),
            name: "Tester".into(),
            description: "Tests code".into(),
            component_ids: vec!["testing".to_string()],
            outcome_id: "Well-tested code".into(),
            performance: PerformanceRecord {
                task_count: 8,
                avg_score: Some(0.8),
                evaluations: vec![],
            },
            lineage: Lineage {
                parent_ids: vec![],
                generation: 1,
                created_by: "evolver-run-0".into(),
                created_at: chrono::Utc::now(),
            },
            default_context_scope: None,
            default_exec_mode: None,
        };

        let op = EvolverOperation {
            op: "modify_role".into(),
            target_id: Some("parent-a,parent-b".into()), // crossover!
            new_id: Some("crossover-result".into()),
            name: Some("Dev-Tester Hybrid".into()),
            description: Some("Codes and tests".into()),
            component_ids: Some(vec!["coding".into(), "testing".into(), "debugging".into()]),
            outcome_id: Some("Working, well-tested code".into()),
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: Some("Combining best of both".into()),
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[parent_a, parent_b],
            &[],
            "test-run",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");

        // Generation should be max(2, 1) + 1 = 3
        assert_eq!(result["generation"], 3);

        // Parent IDs should include both parents
        let parent_ids: Vec<String> = result["parent_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(parent_ids, vec!["parent-a", "parent-b"]);

        // Verify the content-hash ID
        let new_id = result["new_id"].as_str().unwrap();
        assert_eq!(new_id.len(), 64);

        // Load and verify
        let role = agency::load_role(&roles_dir.join(format!("{}.yaml", new_id))).unwrap();
        assert_eq!(role.name, "Dev-Tester Hybrid");
        assert_eq!(role.component_ids.len(), 3);
        assert_eq!(role.lineage.generation, 3);
        assert_eq!(role.lineage.parent_ids, vec!["parent-a", "parent-b"]);
        assert!(role.lineage.created_by.contains("test-run"));
    }

    #[test]
    fn test_apply_modify_role_parent_not_found_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        let op = EvolverOperation {
            op: "modify_role".into(),
            target_id: Some("nonexistent-parent".into()),
            new_id: None,
            name: Some("X".into()),
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "test-run",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_apply_modify_role_missing_target_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        let op = EvolverOperation {
            op: "modify_role".into(),
            target_id: None, // missing!
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "test-run",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires target_id")
        );
    }

    // =======================================================================
    // apply_operation dispatcher
    // =======================================================================

    #[test]
    fn test_apply_operation_dispatches_create_role() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&roles_dir).unwrap();
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "create_role".into(),
            target_id: None,
            new_id: None,
            name: Some("Dispatcher Test".into()),
            description: Some("Testing dispatch".into()),
            component_ids: Some(vec!["dispatch".into()]),
            outcome_id: Some("Dispatched".into()),
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "run-dispatch",
            &roles_dir,
            &tradeoffs_dir,
            temp_dir.path(),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "applied");
        assert_eq!(result["op"], "create_role");
    }

    #[test]
    fn test_apply_operation_unknown_op_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&roles_dir).unwrap();
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "delete_everything".into(),
            target_id: None,
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[],
            &[],
            "run-bad",
            &roles_dir,
            &tradeoffs_dir,
            temp_dir.path(),
            temp_dir.path(),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unknown operation type")
        );
    }

    // =======================================================================
    // Content-hash ID determinism: same content -> same ID
    // =======================================================================

    #[test]
    fn test_create_role_deterministic_id() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        let op = EvolverOperation {
            op: "create_role".into(),
            target_id: None,
            new_id: None,
            name: Some("Deterministic".into()),
            description: Some("Same description".into()),
            component_ids: Some(vec!["skill-a".into()]),
            outcome_id: Some("Same outcome".into()),
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result1 = apply_operation(
            &op,
            &[],
            &[],
            "run-1",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        let result2 = apply_operation(
            &op,
            &[],
            &[],
            "run-2",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();

        // Same content = same ID (even though run_id differs)
        assert_eq!(result1["id"], result2["id"]);
    }

    #[test]
    fn test_create_motivation_deterministic_id() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let tradeoffs_dir = temp_dir.path().join("motivations");
        fs::create_dir_all(&tradeoffs_dir).unwrap();

        let op = EvolverOperation {
            op: "create_motivation".into(),
            target_id: None,
            new_id: None,
            name: Some("Deterministic".into()),
            description: Some("Same desc".into()),
            component_ids: None,
            outcome_id: None,
            acceptable_tradeoffs: Some(vec!["trade-a".into()]),
            unacceptable_tradeoffs: Some(vec!["trade-b".into()]),
            rationale: None,
            ..Default::default()
        };

        let result1 = apply_operation(
            &op,
            &[],
            &[],
            "run-1",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        let result2 = apply_operation(
            &op,
            &[],
            &[],
            "run-2",
            &temp_dir.path().join("roles"),
            &tradeoffs_dir,
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();

        assert_eq!(result1["id"], result2["id"]);
    }

    // =======================================================================
    // Strategy prompt generation: each strategy produces valid prompt content
    // =======================================================================

    fn make_test_roles() -> Vec<Role> {
        vec![Role {
            id: "test-role".into(),
            name: "Test Role".into(),
            description: "A test role".into(),
            component_ids: vec!["testing".to_string()],
            outcome_id: "Pass tests".into(),
            performance: PerformanceRecord {
                task_count: 5,
                avg_score: Some(0.75),
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        }]
    }

    fn make_test_motivations() -> Vec<TradeoffConfig> {
        vec![TradeoffConfig {
            id: "test-mot".into(),
            name: "Test Motivation".into(),
            description: "A test motivation".into(),
            acceptable_tradeoffs: vec!["Slow".into()],
            unacceptable_tradeoffs: vec!["Broken".into()],
            performance: PerformanceRecord {
                task_count: 3,
                avg_score: Some(0.60),
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        }]
    }

    #[test]
    fn test_build_prompt_mutation_strategy() {
        let roles = make_test_roles();
        let motivations = make_test_motivations();
        let perf = build_performance_summary(&roles, &motivations, &[], &Config::default());
        let config = Config::default();

        let prompt = build_evolver_prompt(
            &perf,
            &[], // no skill docs for unit test
            Strategy::Mutation,
            None,
            &config,
            &roles,
            &motivations,
            Path::new("/tmp/fake"),
        );

        assert!(prompt.contains("Evolver Agent Instructions"));
        assert!(prompt.contains("mutation"));
        assert!(prompt.contains("Focus on the **mutation** strategy"));
        assert!(prompt.contains("Performance Summary"));
        assert!(prompt.contains("Test Role"));
        assert!(prompt.contains("Test Motivation"));
        assert!(prompt.contains("Required Output Format"));
        assert!(prompt.contains("create_role"));
        assert!(prompt.contains("modify_role"));
    }

    #[test]
    fn test_build_prompt_crossover_strategy() {
        let roles = make_test_roles();
        let motivations = make_test_motivations();
        let perf = build_performance_summary(&roles, &motivations, &[], &Config::default());
        let config = Config::default();

        let prompt = build_evolver_prompt(
            &perf,
            &[],
            Strategy::Crossover,
            Some(3),
            &config,
            &roles,
            &motivations,
            Path::new("/tmp/fake"),
        );

        assert!(prompt.contains("Focus on the **crossover** strategy"));
        assert!(prompt.contains("Propose at most 3 operations"));
    }

    #[test]
    fn test_build_prompt_gap_analysis_strategy() {
        let roles = make_test_roles();
        let motivations = make_test_motivations();
        let perf = build_performance_summary(&roles, &motivations, &[], &Config::default());
        let config = Config::default();

        let prompt = build_evolver_prompt(
            &perf,
            &[],
            Strategy::GapAnalysis,
            None,
            &config,
            &roles,
            &motivations,
            Path::new("/tmp/fake"),
        );

        assert!(prompt.contains("Focus on the **gap-analysis** strategy"));
    }

    #[test]
    fn test_build_prompt_retirement_strategy() {
        let roles = make_test_roles();
        let motivations = make_test_motivations();
        let perf = build_performance_summary(&roles, &motivations, &[], &Config::default());
        let config = Config::default();

        let prompt = build_evolver_prompt(
            &perf,
            &[],
            Strategy::Retirement,
            None,
            &config,
            &roles,
            &motivations,
            Path::new("/tmp/fake"),
        );

        assert!(prompt.contains("Focus on the **retirement** strategy"));
    }

    #[test]
    fn test_build_prompt_motivation_tuning_strategy() {
        let roles = make_test_roles();
        let motivations = make_test_motivations();
        let perf = build_performance_summary(&roles, &motivations, &[], &Config::default());
        let config = Config::default();

        let prompt = build_evolver_prompt(
            &perf,
            &[],
            Strategy::MotivationTuning,
            None,
            &config,
            &roles,
            &motivations,
            Path::new("/tmp/fake"),
        );

        assert!(prompt.contains("Focus on the **motivation-tuning** strategy"));
    }

    #[test]
    fn test_build_prompt_all_strategy() {
        let roles = make_test_roles();
        let motivations = make_test_motivations();
        let perf = build_performance_summary(&roles, &motivations, &[], &Config::default());
        let config = Config::default();

        let prompt = build_evolver_prompt(
            &perf,
            &[],
            Strategy::All,
            None,
            &config,
            &roles,
            &motivations,
            Path::new("/tmp/fake"),
        );

        assert!(prompt.contains("Use ALL strategies"));
        // Should NOT contain "Focus on the" since it's "All"
        assert!(!prompt.contains("Focus on the"));
    }

    #[test]
    fn test_build_prompt_includes_skill_docs() {
        let roles = make_test_roles();
        let motivations = make_test_motivations();
        let perf = build_performance_summary(&roles, &motivations, &[], &Config::default());
        let config = Config::default();

        let skill_docs = vec![
            (
                "role-mutation.md".to_string(),
                "Mutation procedure: vary one trait at a time.".to_string(),
            ),
            (
                "gap-analysis.md".to_string(),
                "Identify missing capabilities.".to_string(),
            ),
        ];

        let prompt = build_evolver_prompt(
            &perf,
            &skill_docs,
            Strategy::All,
            None,
            &config,
            &roles,
            &motivations,
            Path::new("/tmp/fake"),
        );

        assert!(prompt.contains("Evolution Skill Documents"));
        assert!(prompt.contains("Skill: role-mutation.md"));
        assert!(prompt.contains("Mutation procedure: vary one trait at a time."));
        assert!(prompt.contains("Skill: gap-analysis.md"));
        assert!(prompt.contains("Identify missing capabilities."));
    }

    #[test]
    fn test_build_prompt_includes_retention_heuristics() {
        let roles = make_test_roles();
        let motivations = make_test_motivations();
        let perf = build_performance_summary(&roles, &motivations, &[], &Config::default());
        let mut config = Config::default();
        config.agency.retention_heuristics =
            Some("Retire roles scoring below 0.3 after 10 evaluations".to_string());

        let prompt = build_evolver_prompt(
            &perf,
            &[],
            Strategy::All,
            None,
            &config,
            &roles,
            &motivations,
            Path::new("/tmp/fake"),
        );

        assert!(prompt.contains("Retention Policy"));
        assert!(prompt.contains("Retire roles scoring below 0.3 after 10 evaluations"));
    }

    // =======================================================================
    // Performance summary: evaluations with dimensions
    // =======================================================================

    #[test]
    fn test_build_performance_summary_with_evaluations_and_synergy() {
        let roles = vec![
            Role {
                id: "r1".into(),
                name: "Dev".into(),
                description: "Developer".into(),
                component_ids: vec!["coding".to_string()],
                outcome_id: "Code".into(),
                performance: PerformanceRecord {
                    task_count: 2,
                    avg_score: Some(0.75),
                    evaluations: vec![],
                },
                lineage: Lineage::default(),
                default_context_scope: None,
                default_exec_mode: None,
            },
            Role {
                id: "r2".into(),
                name: "Tester".into(),
                description: "Tester".into(),
                component_ids: vec!["testing".to_string()],
                outcome_id: "Tests".into(),
                performance: PerformanceRecord {
                    task_count: 1,
                    avg_score: Some(0.90),
                    evaluations: vec![],
                },
                lineage: Lineage::default(),
                default_context_scope: None,
                default_exec_mode: None,
            },
        ];
        let motivations = vec![TradeoffConfig {
            id: "m1".into(),
            name: "Careful".into(),
            description: "Be careful".into(),
            acceptable_tradeoffs: vec!["Slow".into()],
            unacceptable_tradeoffs: vec!["Broken".into()],
            performance: PerformanceRecord {
                task_count: 3,
                avg_score: Some(0.80),
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            former_agents: vec![],
            former_deployments: vec![],
        }];
        let mut dims = HashMap::new();
        dims.insert("correctness".to_string(), 0.9);
        dims.insert("completeness".to_string(), 0.6);

        let evaluations = vec![
            Evaluation {
                id: "e1".into(),
                task_id: "t1".into(),
                agent_id: "".into(),
                role_id: "r1".into(),
                tradeoff_id: "m1".into(),
                score: 0.8,
                dimensions: dims.clone(),
                notes: "Good".into(),
                evaluator: "human".into(),
                timestamp: "2025-01-01T00:00:00Z".into(),
                model: None,
                source: "llm".to_string(),
            },
            Evaluation {
                id: "e2".into(),
                task_id: "t2".into(),
                agent_id: "".into(),
                role_id: "r1".into(),
                tradeoff_id: "m1".into(),
                score: 0.7,
                dimensions: HashMap::new(),
                notes: "OK".into(),
                evaluator: "human".into(),
                timestamp: "2025-01-02T00:00:00Z".into(),
                model: None,
                source: "llm".to_string(),
            },
            Evaluation {
                id: "e3".into(),
                task_id: "t3".into(),
                agent_id: "".into(),
                role_id: "r2".into(),
                tradeoff_id: "m1".into(),
                score: 0.9,
                dimensions: HashMap::new(),
                notes: "Great".into(),
                evaluator: "human".into(),
                timestamp: "2025-01-03T00:00:00Z".into(),
                model: None,
                source: "llm".to_string(),
            },
        ];

        let summary =
            build_performance_summary(&roles, &motivations, &evaluations, &Config::default());

        // Overall stats
        assert!(summary.contains("Total roles: 2"));
        assert!(summary.contains("Total tradeoffs: 1"));
        assert!(summary.contains("Total evaluations: 3"));
        assert!(summary.contains("Overall avg score: 0.800"));

        // Per-role
        assert!(summary.contains("Dev"));
        assert!(summary.contains("Tester"));

        // Dimensions for r1
        assert!(summary.contains("correctness=0.90"));
        assert!(summary.contains("completeness=0.60"));

        // Synergy matrix
        assert!(summary.contains("Synergy Matrix"));
        // r1 x m1 should appear with avg 0.75, r2 x m1 with avg 0.90
        assert!(summary.contains("(r1, m1)"));
        assert!(summary.contains("(r2, m1)"));
    }

    // =======================================================================
    // Lineage metadata correctness
    // =======================================================================

    #[test]
    fn test_mutation_lineage_increments_generation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        // Parent at generation 5
        let parent = Role {
            id: "gen5-parent".into(),
            name: "Gen5".into(),
            description: "Fifth gen".into(),
            component_ids: vec![],
            outcome_id: "Evolve".into(),
            performance: PerformanceRecord::default(),
            lineage: Lineage {
                parent_ids: vec!["gen4-parent".into()],
                generation: 5,
                created_by: "evolver-run-old".into(),
                created_at: chrono::Utc::now(),
            },
            default_context_scope: None,
            default_exec_mode: None,
        };

        let op = EvolverOperation {
            op: "modify_role".into(),
            target_id: Some("gen5-parent".into()),
            new_id: None,
            name: Some("Gen6 Child".into()),
            description: Some("Sixth gen".into()),
            component_ids: Some(vec!["evolved".into()]),
            outcome_id: Some("More evolved".into()),
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[parent],
            &[],
            "run-new",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        assert_eq!(result["generation"], 6);

        let parent_ids: Vec<String> = result["parent_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(parent_ids, vec!["gen5-parent"]);
    }

    #[test]
    fn test_crossover_lineage_uses_max_generation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let roles_dir = temp_dir.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();

        let parent_a = Role {
            id: "pa".into(),
            name: "A".into(),
            description: "".into(),
            component_ids: vec![],
            outcome_id: "".into(),
            performance: PerformanceRecord::default(),
            lineage: Lineage {
                parent_ids: vec![],
                generation: 3,
                created_by: "x".into(),
                created_at: chrono::Utc::now(),
            },
            default_context_scope: None,
            default_exec_mode: None,
        };
        let parent_b = Role {
            id: "pb".into(),
            name: "B".into(),
            description: "".into(),
            component_ids: vec![],
            outcome_id: "".into(),
            performance: PerformanceRecord::default(),
            lineage: Lineage {
                parent_ids: vec![],
                generation: 7,
                created_by: "x".into(),
                created_at: chrono::Utc::now(),
            },
            default_context_scope: None,
            default_exec_mode: None,
        };

        let op = EvolverOperation {
            op: "modify_role".into(),
            target_id: Some("pa,pb".into()),
            new_id: None,
            name: None,
            description: Some("cross".into()),
            component_ids: Some(vec!["merged".into()]),
            outcome_id: Some("merged".into()),
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            rationale: None,
            ..Default::default()
        };

        let result = apply_operation(
            &op,
            &[parent_a, parent_b],
            &[],
            "run-x",
            &roles_dir,
            &temp_dir.path().join("mot"),
            &temp_dir.path().join("agency"),
            temp_dir.path(),
        )
        .unwrap();
        // max(3, 7) + 1 = 8
        assert_eq!(result["generation"], 8);
    }

    // =======================================================================
    // extract_json edge cases
    // =======================================================================

    #[test]
    fn test_extract_json_with_leading_whitespace() {
        let input = "   \n\n  {\"run_id\": \"ws\", \"operations\": []}  \n  ";
        let result = extract_json(input).unwrap();
        assert!(result.contains("ws"));
    }

    #[test]
    fn test_extract_json_nested_braces() {
        let input = r#"{"run_id": "nested", "operations": [{"op": "create_role", "name": "X", "description": "has {braces} in text"}]}"#;
        let result = extract_json(input).unwrap();
        assert!(result.contains("nested"));
    }

    #[test]
    fn test_extract_json_fences_without_json_tag() {
        let input = "```\n{\"run_id\": \"plain-fence\", \"operations\": []}\n```";
        let result = extract_json(input).unwrap();
        assert!(result.contains("plain-fence"));
    }

    #[test]
    fn test_extract_json_inverted_braces_no_panic() {
        // If } appears before { in the text, should return None, not panic
        let input = "some text } then { more text";
        let result = extract_json(input);
        assert!(result.is_none());
    }
}

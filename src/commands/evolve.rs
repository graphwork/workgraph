use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use workgraph::agency::{self, AccessControl, ComponentCategory, Evaluation, Lineage, TradeoffConfig, PerformanceRecord, Role, ContentRef, render_identity_prompt_rich, resolve_all_components, resolve_outcome};
use workgraph::config::Config;
use workgraph::graph::{Node, Status, Task};
use workgraph::{load_graph, save_graph};

/// Strategies the evolver can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Mutation,
    Crossover,
    GapAnalysis,
    Retirement,
    MotivationTuning,
    All,
    // New strategies
    ComponentMutation,
    Randomisation,
    BizarreIdeation,
}

impl Strategy {
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "mutation" => Ok(Self::Mutation),
            "crossover" => Ok(Self::Crossover),
            "gap-analysis" => Ok(Self::GapAnalysis),
            "retirement" => Ok(Self::Retirement),
            "motivation-tuning" => Ok(Self::MotivationTuning),
            "all" => Ok(Self::All),
            "component-mutation" => Ok(Self::ComponentMutation),
            "randomisation" => Ok(Self::Randomisation),
            "bizarre-ideation" => Ok(Self::BizarreIdeation),
            other => bail!(
                "Unknown strategy '{}'. Valid: mutation, crossover, gap-analysis, retirement, \
                 motivation-tuning, component-mutation, randomisation, bizarre-ideation, all",
                other
            ),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Mutation => "mutation",
            Self::Crossover => "crossover",
            Self::GapAnalysis => "gap-analysis",
            Self::Retirement => "retirement",
            Self::MotivationTuning => "motivation-tuning",
            Self::All => "all",
            Self::ComponentMutation => "component-mutation",
            Self::Randomisation => "randomisation",
            Self::BizarreIdeation => "bizarre-ideation",
        }
    }
}

// ---------------------------------------------------------------------------
// Evolver target (Level × Amount)
// ---------------------------------------------------------------------------

/// The level of the primitive hierarchy the evolver targets.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolverLevel {
    Primitives,
    Configurations,
    Agents,
    AgentConfigurations,
}

/// The perturbation magnitude for an evolver operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolverAmount {
    Minimal,
    Moderate,
    Maximal,
}

/// Entity type targeted by an evolver operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolverEntityType {
    Component,
    Outcome,
    Tradeoff,
    Role,
    Agent,
    MetaAgent,
}

/// Two-dimensional evolver targeting: Level × Amount.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvolverTarget {
    pub level: EvolverLevel,
    pub amount: EvolverAmount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<EvolverEntityType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ids: Option<Vec<String>>,
}

impl Default for EvolverTarget {
    fn default() -> Self {
        Self {
            level: EvolverLevel::Configurations,
            amount: EvolverAmount::Moderate,
            entity_type: None,
            target_ids: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Deferred operation types (human oversight gate)
// ---------------------------------------------------------------------------

/// Why an evolver operation was deferred for human review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferralReason {
    /// entity_type = outcome with requires_human_oversight
    ObjectiveChange,
    /// bizarre_ideation on outcome
    BizarreObjective,
    /// trade-off config has protect-objectives
    ProtectObjectivesFlag,
}

/// A human decision on a deferred operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HumanDecision {
    pub approved: bool,
    pub decided_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// An evolver operation placed in the deferred queue for human review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeferredOperation {
    pub id: String,
    pub task_id: String,
    pub operation: EvolverOperation,
    pub deferred_reason: DeferralReason,
    pub proposed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_decision: Option<HumanDecision>,
}

/// A single evolution operation returned by the evolver agent.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EvolverOperation {
    /// Operation type: create_role, modify_role, create_motivation, modify_motivation,
    /// retire_role, retire_motivation, wording_mutation, component_substitution,
    /// config_add_component, config_remove_component, config_swap_outcome,
    /// config_swap_tradeoff, random_compose_role, random_compose_agent, bizarre_ideation,
    /// meta_swap_role, meta_swap_tradeoff, meta_compose_agent
    pub op: String,

    // -- Targeting --
    /// The entity type this operation targets.
    #[serde(default)]
    pub entity_type: Option<String>,

    // -- Source references --
    /// For modify/retire/substitution: the ID of the existing entity to act on.
    #[serde(default)]
    pub target_id: Option<String>,

    // -- Composition changes --
    /// For component_substitution, config_add_component: component hash to add.
    #[serde(default)]
    pub add_component_id: Option<String>,
    /// For component_substitution, config_remove_component: component hash to remove.
    #[serde(default)]
    pub remove_component_id: Option<String>,
    /// For config_swap_outcome: new outcome hash.
    #[serde(default)]
    pub new_outcome_id: Option<String>,
    /// For config_swap_tradeoff: new tradeoff hash.
    #[serde(default)]
    pub new_tradeoff_id: Option<String>,

    // -- Content fields (for wording_mutation, bizarre_ideation) --
    /// New name for created/mutated entity.
    #[serde(default)]
    pub new_name: Option<String>,
    /// New description for created/mutated entity.
    #[serde(default)]
    pub new_description: Option<String>,
    /// New content (serialized ContentRef).
    #[serde(default)]
    pub new_content: Option<String>,
    /// New category: "translated" | "enhanced" | "novel".
    #[serde(default)]
    pub new_category: Option<String>,
    /// New success criteria (for outcome bizarre ideation).
    #[serde(default)]
    pub new_success_criteria: Option<Vec<String>>,
    /// New acceptable tradeoffs (for tradeoff bizarre ideation).
    #[serde(default)]
    pub new_acceptable_tradeoffs: Option<Vec<String>>,
    /// New unacceptable tradeoffs (for tradeoff bizarre ideation).
    #[serde(default)]
    pub new_unacceptable_tradeoffs: Option<Vec<String>>,

    // -- Randomisation --
    /// Selection method: "uniform_random" | "performance_weighted_inverse".
    #[serde(default)]
    pub selection_method: Option<String>,

    // -- Backwards compatibility (legacy role/motivation fields) --
    /// New ID for the created/modified entity.
    #[serde(default)]
    pub new_id: Option<String>,
    /// Legacy name field.
    #[serde(default)]
    pub name: Option<String>,
    /// Legacy description field.
    #[serde(default)]
    pub description: Option<String>,
    /// Component IDs (for roles or random_compose_role).
    #[serde(default, alias = "skills")]
    pub component_ids: Option<Vec<String>>,
    /// Outcome ID (for roles or random_compose_role).
    #[serde(default, alias = "desired_outcome")]
    pub outcome_id: Option<String>,
    /// Role ID (for random_compose_agent).
    #[serde(default)]
    pub role_id: Option<String>,
    /// Tradeoff ID (for random_compose_agent).
    #[serde(default)]
    pub tradeoff_id: Option<String>,
    /// Acceptable trade-offs (for motivations).
    #[serde(default)]
    pub acceptable_tradeoffs: Option<Vec<String>>,
    /// Unacceptable trade-offs (for motivations).
    #[serde(default)]
    pub unacceptable_tradeoffs: Option<Vec<String>>,

    // -- Meta-agent targeting --
    /// For meta_swap_role, meta_swap_tradeoff, meta_compose_agent:
    /// which meta-agent slot to target: "assigner" | "evaluator" | "evolver".
    #[serde(default)]
    pub meta_role: Option<String>,

    // -- Provenance --
    /// Rationale for this operation.
    #[serde(default)]
    pub rationale: Option<String>,
    /// For bizarre ideation: the prompt used to generate the new primitive.
    #[serde(default)]
    pub ideation_prompt: Option<String>,
}

impl Default for EvolverOperation {
    fn default() -> Self {
        Self {
            op: String::new(),
            entity_type: None,
            target_id: None,
            add_component_id: None,
            remove_component_id: None,
            new_outcome_id: None,
            new_tradeoff_id: None,
            new_name: None,
            new_description: None,
            new_content: None,
            new_category: None,
            new_success_criteria: None,
            new_acceptable_tradeoffs: None,
            new_unacceptable_tradeoffs: None,
            selection_method: None,
            new_id: None,
            name: None,
            description: None,
            component_ids: None,
            outcome_id: None,
            role_id: None,
            tradeoff_id: None,
            acceptable_tradeoffs: None,
            unacceptable_tradeoffs: None,
            meta_role: None,
            rationale: None,
            ideation_prompt: None,
        }
    }
}

/// Top-level structured output from the evolver agent.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct EvolverOutput {
    /// Run ID for lineage tracking.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Level × Amount targeting for this run.
    #[serde(default)]
    pub target: Option<EvolverTarget>,
    /// List of proposed operations.
    pub operations: Vec<EvolverOperation>,
    /// Operations placed in deferred queue (not yet applied).
    #[serde(default)]
    pub deferred_operations: Vec<EvolverOperation>,
    /// Optional summary from the evolver.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Run `wg evolve` — trigger an evolution cycle on agency roles and tradeoffs.
pub fn run(
    dir: &Path,
    dry_run: bool,
    strategy: Option<&str>,
    budget: Option<u32>,
    model: Option<&str>,
    json: bool,
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

    // Load evolver skill documents
    let skill_docs = load_evolver_skills(&skills_dir, strategy)?;

    // Load config for evolver identity and model
    let config = Config::load_or_default(dir);

    // Determine model: CLI flag > agency.evolver_model > agent.model
    let model = model
        .map(std::string::ToString::to_string)
        .or(config.agency.evolver_model.clone())
        .unwrap_or_else(|| config.agent.model.clone());

    // Build performance summary
    let perf_summary = build_performance_summary(
        &roles, &tradeoffs, &evaluations, &config,
    );

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
                    "create_role" | "modify_role" | "retire_role"
                        | "component_substitution" | "config_add_component"
                        | "config_remove_component" | "config_swap_outcome"
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
// Performance summary builder
// ---------------------------------------------------------------------------

fn build_performance_summary(
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    evaluations: &[Evaluation],
    config: &Config,
) -> String {
    let _ = config;
    let mut out = String::new();
    out.push_str("## Performance Summary\n\n");
    let total_evals = evaluations.len();
    let overall_avg: Option<f64> = if total_evals > 0 {
        let valid: Vec<f64> = evaluations.iter().map(|e| e.score).filter(|s: &f64| s.is_finite()).collect();
        if valid.is_empty() { None } else { Some(valid.iter().sum::<f64>() / valid.len() as f64) }
    } else { None };
    out.push_str(&format!("Total roles: {}\n", roles.len()));
    out.push_str(&format!("Total tradeoffs: {}\n", tradeoffs.len()));
    out.push_str(&format!("Total evaluations: {}\n", total_evals));
    if let Some(avg) = overall_avg { out.push_str(&format!("Overall avg score: {:.3}\n", avg)); }
    out.push('\n');
    out.push_str("### Role Performance\n\n");
    for role in roles {
        let score_str = role.performance.avg_score.map(|s| format!("{:.3}", s)).unwrap_or_else(|| "-".to_string());
        out.push_str(&format!("- **{}** (id: `{}`): {} evals, score: {}, gen: {}\n",
            role.name, role.id, role.performance.task_count, score_str, role.lineage.generation));
        out.push_str(&format!("  description: {}\n", role.description));
        out.push_str(&format!("  outcome_id: {}\n", role.outcome_id));
        if !role.component_ids.is_empty() { out.push_str(&format!("  component_ids: {}\n", role.component_ids.join(", "))); }
        if !role.lineage.parent_ids.is_empty() { out.push_str(&format!("  parents: {}\n", role.lineage.parent_ids.join(", "))); }
        let role_evals: Vec<&Evaluation> = evaluations.iter().filter(|e| e.role_id == role.id).collect();
        if !role_evals.is_empty() {
            let dims = aggregate_dimensions(&role_evals);
            if !dims.is_empty() {
                let dim_strs: Vec<String> = dims.iter().map(|(k, v)| format!("{}={:.2}", k, v)).collect();
                out.push_str(&format!("  dimensions: {}\n", dim_strs.join(", ")));
            }
        }
        out.push('\n');
    }
    out.push_str("### Tradeoff Performance\n\n");
    for tradeoff in tradeoffs {
        let score_str = tradeoff.performance.avg_score.map(|s| format!("{:.3}", s)).unwrap_or_else(|| "-".to_string());
        out.push_str(&format!("- **{}** (id: `{}`): {} evals, score: {}, gen: {}\n",
            tradeoff.name, tradeoff.id, tradeoff.performance.task_count, score_str, tradeoff.lineage.generation));
        out.push_str(&format!("  description: {}\n", tradeoff.description));
        if !tradeoff.acceptable_tradeoffs.is_empty() { out.push_str(&format!("  acceptable_tradeoffs: {}\n", tradeoff.acceptable_tradeoffs.join("; "))); }
        if !tradeoff.unacceptable_tradeoffs.is_empty() { out.push_str(&format!("  unacceptable_tradeoffs: {}\n", tradeoff.unacceptable_tradeoffs.join("; "))); }
        if !tradeoff.lineage.parent_ids.is_empty() { out.push_str(&format!("  parents: {}\n", tradeoff.lineage.parent_ids.join(", "))); }
        out.push('\n');
    }
    let mut synergy: HashMap<(String, String), Vec<f64>> = HashMap::new();
    for eval in evaluations { synergy.entry((eval.role_id.clone(), eval.tradeoff_id.clone())).or_default().push(eval.score); }
    if !synergy.is_empty() {
        out.push_str("### Synergy Matrix (Role x Tradeoff)\n\n");
        let mut pairs: Vec<_> = synergy.iter().collect();
        pairs.sort_by(|a, b| {
            let avg_a = a.1.iter().sum::<f64>() / a.1.len() as f64;
            let avg_b = b.1.iter().sum::<f64>() / b.1.len() as f64;
            avg_b.partial_cmp(&avg_a).unwrap_or(std::cmp::Ordering::Equal)
        });
        for ((role_id, mot_id), scores) in &pairs {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            out.push_str(&format!("- ({}, {}): avg={:.3}, count={}\n", role_id, mot_id, avg, scores.len()));
        }
        out.push('\n');
    }
    out
}

fn aggregate_dimensions(evals: &[&Evaluation]) -> Vec<(String, f64)> {
    let mut dim_sums: HashMap<String, (f64, usize)> = HashMap::new();
    for eval in evals {
        for (dim, score) in &eval.dimensions {
            let entry = dim_sums.entry(dim.clone()).or_insert((0.0, 0));
            entry.0 += score;
            entry.1 += 1;
        }
    }
    let mut dims: Vec<(String, f64)> = dim_sums
        .into_iter()
        .map(|(k, (sum, count))| (k, sum / count as f64))
        .collect();
    dims.sort_by(|a, b| a.0.cmp(&b.0));
    dims
}

// ---------------------------------------------------------------------------
// Evolver skill loader
// ---------------------------------------------------------------------------

fn load_evolver_skills(skills_dir: &Path, strategy: Strategy) -> Result<Vec<(String, String)>> {
    let mut docs = Vec::new();

    if !skills_dir.exists() {
        eprintln!(
            "Warning: evolver-skills directory not found at {}",
            skills_dir.display()
        );
        return Ok(docs);
    }

    let files_to_load: Vec<&str> = match strategy {
        Strategy::Mutation => vec!["role-mutation.md"],
        Strategy::Crossover => vec!["role-crossover.md"],
        Strategy::GapAnalysis => vec!["gap-analysis.md"],
        Strategy::Retirement => vec!["retirement.md"],
        Strategy::MotivationTuning => vec!["motivation-tuning.md"],
        Strategy::ComponentMutation => vec!["component-mutation.md"],
        Strategy::Randomisation => vec!["randomisation.md"],
        Strategy::BizarreIdeation => vec!["bizarre-ideation.md"],
        Strategy::All => vec![
            "role-mutation.md",
            "role-crossover.md",
            "motivation-tuning.md",
            "gap-analysis.md",
            "retirement.md",
            "component-mutation.md",
            "randomisation.md",
            "bizarre-ideation.md",
        ],
    };

    for filename in &files_to_load {
        let path = skills_dir.join(filename);
        if path.exists() {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read evolver skill: {}", path.display()))?;
            docs.push((filename.to_string(), content));
        } else {
            eprintln!(
                "Warning: evolver skill '{}' not found at {}",
                filename,
                path.display()
            );
        }
    }

    Ok(docs)
}

// ---------------------------------------------------------------------------
// Evolver prompt builder
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_evolver_prompt(
    perf_summary: &str,
    skill_docs: &[(String, String)],
    strategy: Strategy,
    budget: Option<u32>,
    config: &Config,
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    agency_dir: &Path,
) -> String {
    let mut out = String::new();

    // System instructions
    out.push_str("# Evolver Agent Instructions\n\n");
    out.push_str(
        "You are the evolver agent for a workgraph agency system. Your job is to improve \
         the agency's performance by evolving roles and tradeoffs based on performance data.\n\n",
    );

    // Evolver's own identity (if configured via evolver_agent hash)
    if let Some(ref agent_hash) = config.agency.evolver_agent {
        let agents_dir = agency_dir.join("cache/agents");
        let agent_path = agents_dir.join(format!("{}.yaml", agent_hash));
        if let Ok(agent) = agency::load_agent(&agent_path) {
            if let Some(role) = roles.iter().find(|r| r.id == agent.role_id) {
                if let Some(tradeoff) = tradeoffs.iter().find(|m| m.id == agent.tradeoff_id) {
                    // Use the project root (parent of agency dir) for skill resolution
                    let workgraph_root = agency_dir.parent().unwrap_or(agency_dir);
                    let resolved_skills = resolve_all_components(role, workgraph_root, agency_dir);
                    let outcome = resolve_outcome(&role.outcome_id, agency_dir);
                    out.push_str(&render_identity_prompt_rich(role, tradeoff, &resolved_skills, outcome.as_ref()));
                    out.push_str("\n\n");
                }
            }
        }
    }

    // Meta-agent assignments (assigner, evaluator, evolver)
    {
        let agents_dir = agency_dir.join("cache/agents");
        let meta_agents: Vec<(&str, &Option<String>)> = vec![
            ("Assigner", &config.agency.assigner_agent),
            ("Evaluator", &config.agency.evaluator_agent),
            ("Evolver", &config.agency.evolver_agent),
        ];
        let mut has_any = false;
        for (label, agent_hash) in &meta_agents {
            if let Some(hash) = agent_hash {
                if !has_any {
                    out.push_str("## Meta-Agent Assignments\n\n");
                    out.push_str(
                        "These agents fill coordination roles (assigner, evaluator, evolver). \
                         Their underlying roles and tradeoffs are valid mutation targets. \
                         **Evolving the evolver's own role or tradeoff requires human approval.**\n\n",
                    );
                    has_any = true;
                }
                let agent_path = agents_dir.join(format!("{}.yaml", hash));
                if let Ok(agent) = agency::load_agent(&agent_path) {
                    let role_name = roles
                        .iter()
                        .find(|r| r.id == agent.role_id)
                        .map(|r| r.name.as_str())
                        .unwrap_or("unknown");
                    let mot_name = tradeoffs
                        .iter()
                        .find(|m| m.id == agent.tradeoff_id)
                        .map(|m| m.name.as_str())
                        .unwrap_or("unknown");
                    let perf_str = agent
                        .performance
                        .avg_score
                        .map(|s| {
                            format!(
                                ", avg_score: {:.3}, tasks: {}",
                                s, agent.performance.task_count
                            )
                        })
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "- **{}**: agent `{}`, role `{}` ({}), tradeoff `{}` ({}){}\n",
                        label, hash, agent.role_id, role_name, agent.tradeoff_id, mot_name, perf_str,
                    ));
                } else {
                    out.push_str(&format!(
                        "- **{}**: agent `{}` (could not load details)\n",
                        label, hash,
                    ));
                }
            }
        }
        if has_any {
            out.push('\n');
        }
    }

    // Strategy
    out.push_str("## Strategy\n\n");
    match strategy {
        Strategy::All => {
            out.push_str(
                "Use ALL strategies as appropriate: mutation, crossover, gap-analysis, \
                 motivation-tuning, and retirement. Analyze the performance data and choose \
                 the most impactful operations.\n\n",
            );
        }
        other => {
            out.push_str(&format!(
                "Focus on the **{}** strategy. Only propose operations of this type.\n\n",
                other.label()
            ));
        }
    }

    // Budget
    if let Some(max) = budget {
        out.push_str(&format!(
            "**Budget:** Propose at most {} operations.\n\n",
            max
        ));
    }

    // Retention heuristics (prose policy from config)
    if let Some(ref heuristics) = config.agency.retention_heuristics {
        out.push_str("## Retention Policy\n\n");
        out.push_str(heuristics);
        out.push_str("\n\n");
    }

    // Performance data
    out.push_str(perf_summary);

    // Skill documents
    if !skill_docs.is_empty() {
        out.push_str("## Evolution Skill Documents\n\n");
        out.push_str(
            "These documents describe the procedures and guidelines for each evolution strategy. \
             Follow them carefully.\n\n",
        );
        for (name, content) in skill_docs {
            out.push_str(&format!("### Skill: {}\n\n", name));
            out.push_str(content);
            out.push_str("\n\n---\n\n");
        }
    }

    // Output format
    out.push_str("## Required Output Format\n\n");
    out.push_str(
        "Respond with **only** a JSON object (no markdown fences, no commentary before or after):\n\n\
         ```\n\
         {\n  \
           \"run_id\": \"<a short unique id for this evolution run>\",\n  \
           \"operations\": [\n    \
             {\n      \
               \"op\": \"<create_role|modify_role|create_motivation|modify_motivation|retire_role|retire_motivation>\",\n      \
               \"target_id\": \"<existing entity ID, for modify/retire ops>\",\n      \
               \"new_id\": \"<new entity ID>\",\n      \
               \"name\": \"<human-readable name>\",\n      \
               \"description\": \"<entity description>\",\n      \
               \"skills\": [\"skill-name-1\", \"skill-name-2\"],\n      \
               \"desired_outcome\": \"<for roles>\",\n      \
               \"acceptable_tradeoffs\": [\"tradeoff1\"],\n      \
               \"unacceptable_tradeoffs\": [\"constraint1\"],\n      \
               \"rationale\": \"<why this operation>\"\n    \
             }\n  \
           ],\n  \
           \"summary\": \"<brief explanation of overall evolution strategy>\"\n\
         }\n\
         ```\n\n",
    );

    out.push_str("### Operation Types\n\n");
    out.push_str("- **create_role**: Creates a brand new role (from gap-analysis). Requires: new_id, name, description, skills, desired_outcome.\n");
    out.push_str("- **modify_role**: Mutates or crosses over an existing role. Requires: target_id (parent), new_id, name, description, skills, desired_outcome.\n");
    out.push_str("- **create_motivation**: Creates a new motivation (from gap-analysis). Requires: new_id, name, description, acceptable_tradeoffs, unacceptable_tradeoffs.\n");
    out.push_str("- **modify_motivation**: Tunes an existing motivation. Requires: target_id (parent), new_id, name, description, acceptable_tradeoffs, unacceptable_tradeoffs.\n");
    out.push_str("- **retire_role**: Retires a poor-performing role. Requires: target_id.\n");
    out.push_str(
        "- **retire_motivation**: Retires a poor-performing motivation. Requires: target_id.\n\n",
    );

    out.push_str("For modify operations involving crossover (two parents), set target_id to a comma-separated pair like \"parent-a,parent-b\".\n\n");

    // AgentConfigurations-level operations
    out.push_str("### AgentConfigurations-Level Operations (Meta-Agent Evolution)\n\n");
    out.push_str(
        "These operations evolve the special agents that fill coordination roles \
         (assigner, evaluator, evolver). Each requires a `meta_role` field set to \
         one of: `assigner`, `evaluator`, `evolver`.\n\n",
    );
    out.push_str("- **meta_swap_role**: Change which role a meta-agent uses (keeps its tradeoff). Requires: meta_role, role_id (new role hash).\n");
    out.push_str("- **meta_swap_tradeoff**: Change which tradeoff a meta-agent uses (keeps its role). Requires: meta_role, tradeoff_id (new tradeoff hash).\n");
    out.push_str("- **meta_compose_agent**: Compose a new agent for a meta-agent slot from scratch. Requires: meta_role, role_id, tradeoff_id.\n\n");
    out.push_str("**Safety:** Operations targeting `meta_role: \"evolver\"` are automatically deferred for human approval.\n\n");

    out.push_str("**Important:** Each new/modified entity gets lineage tracking automatically. Just provide the IDs.\n");

    out
}

// ---------------------------------------------------------------------------
// Output parser
// ---------------------------------------------------------------------------

fn parse_evolver_output(raw: &str) -> Result<EvolverOutput> {
    // Try to extract JSON from potentially noisy LLM output
    let json_str = extract_json(raw)
        .ok_or_else(|| anyhow::anyhow!("No valid JSON found in evolver output"))?;

    let output: EvolverOutput = serde_json::from_str(&json_str)
        .with_context(|| format!("Failed to parse evolver JSON:\n{}", json_str))?;

    Ok(output)
}

/// Extract a JSON object from potentially noisy LLM output.
fn extract_json(raw: &str) -> Option<String> {
    let trimmed = raw.trim();

    // Try the whole string first
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    // Strip markdown code fences
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
        && start <= end
    {
        let candidate = &stripped[start..=end];
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Evolver self-mutation deferral
// ---------------------------------------------------------------------------

/// Create a verified workgraph task for an evolver self-mutation operation.
/// The task requires human approval before the mutation can be applied.
fn defer_self_mutation(op: &EvolverOperation, dir: &Path, run_id: &str) -> Result<String> {
    let graph_path = super::graph_path(dir);
    let mut graph =
        load_graph(&graph_path).context("Failed to load graph for self-mutation deferral")?;

    let task_id = format!(
        "evolve-review-{}-{}",
        op.op,
        op.target_id.as_deref().unwrap_or("unknown"),
    );

    // Don't create duplicate review tasks
    if graph.get_task(&task_id).is_some() {
        return Ok(task_id);
    }

    let op_json = serde_json::to_string_pretty(op).unwrap_or_else(|_| format!("{:?}", op.op));

    let desc = format!(
        "The evolver (run {run_id}) proposed a mutation targeting its own identity. \
         This requires human review before applying.\n\n\
         ## Proposed Operation\n\n\
         ```json\n{op_json}\n```\n\n\
         ## Instructions\n\n\
         Review the proposed change. If acceptable, apply it manually with \
         `wg evolve` or by editing the role/motivation YAML directly, then \
         `wg approve {task_id}`.",
    );

    let task = Task {
        id: task_id.clone(),
        title: format!(
            "Review evolver self-mutation: {} on {}",
            op.op,
            op.target_id.as_deref().unwrap_or("?")
        ),
        description: Some(desc),
        status: Status::Open,
        assigned: None,
        estimate: None,
        before: vec![],
        after: vec![],
        requires: vec![],
        tags: vec!["evolution".to_string(), "agency".to_string()],
        skills: vec![],
        inputs: vec![],
        deliverables: vec![],
        artifacts: vec![],
        exec: None,
        not_before: None,
        created_at: Some(Utc::now().to_rfc3339()),
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries: None,
        failure_reason: None,
        model: None,
        verify: Some("Human must approve evolver self-mutation before applying.".to_string()),
        agent: None,
        loop_iteration: 0,
        ready_after: None,
        paused: false,
        visibility: "internal".to_string(),
        context_scope: None,
        cycle_config: None,
        token_usage: None,
        exec_mode: None,
    };

    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path)
        .context("Failed to save graph with self-mutation review task")?;
    super::notify_graph_changed(dir);

    Ok(task_id)
}

// ---------------------------------------------------------------------------
// Operation application
// ---------------------------------------------------------------------------

fn apply_operation(
    op: &EvolverOperation,
    existing_roles: &[Role],
    existing_tradeoffs: &[TradeoffConfig],
    run_id: &str,
    roles_dir: &Path,
    tradeoffs_dir: &Path,
    agency_dir: &Path,
    dir: &Path,
) -> Result<serde_json::Value> {
    match op.op.as_str() {
        // Legacy operations
        "create_role" => apply_create_role(op, run_id, roles_dir),
        "modify_role" => apply_modify_role(op, existing_roles, run_id, roles_dir),
        "create_motivation" => apply_create_motivation(op, run_id, tradeoffs_dir),
        "modify_motivation" => {
            apply_modify_motivation(op, existing_tradeoffs, run_id, tradeoffs_dir)
        }
        "retire_role" => apply_retire_role(op, existing_roles, roles_dir),
        "retire_motivation" => apply_retire_motivation(op, existing_tradeoffs, tradeoffs_dir),
        // New mutation operations
        "wording_mutation" => apply_wording_mutation(op, run_id, agency_dir),
        "component_substitution" => {
            apply_component_substitution(op, existing_roles, run_id, roles_dir)
        }
        "config_add_component" => {
            apply_config_add_component(op, existing_roles, run_id, roles_dir)
        }
        "config_remove_component" => {
            apply_config_remove_component(op, existing_roles, run_id, roles_dir)
        }
        "config_swap_outcome" => {
            apply_config_swap_outcome(op, existing_roles, run_id, roles_dir, agency_dir)
        }
        "config_swap_tradeoff" => {
            apply_config_swap_tradeoff(op, run_id, agency_dir)
        }
        // Randomisation operations
        "random_compose_role" => apply_random_compose_role(op, run_id, agency_dir),
        "random_compose_agent" => apply_random_compose_agent(op, run_id, agency_dir),
        // Bizarre ideation
        "bizarre_ideation" => apply_bizarre_ideation(op, run_id, agency_dir),
        // Meta-agent (AgentConfigurations level) operations
        "meta_swap_role" => apply_meta_swap_role(op, run_id, agency_dir, dir),
        "meta_swap_tradeoff" => apply_meta_swap_tradeoff(op, run_id, agency_dir, dir),
        "meta_compose_agent" => apply_meta_compose_agent(op, run_id, agency_dir, dir),
        other => bail!("Unknown operation type: '{}'", other),
    }
}

fn apply_create_role(
    op: &EvolverOperation,
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let name = op
        .name
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("create_role requires name"))?;

    let component_ids: Vec<String> = op
        .component_ids
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|s| {
            agency::content_hash_component(s, &ComponentCategory::Translated, &ContentRef::Name(s.to_string()))
        })
        .collect();

    let description = op.description.clone().unwrap_or_default();
    let outcome_id = op.outcome_id.clone().unwrap_or_default();
    let id = agency::content_hash_role(&component_ids, &outcome_id);

    let role = Role {
        id: id.clone(),
        name: name.to_string(),
        description,
        component_ids,
        outcome_id,
        performance: PerformanceRecord::default(),
        lineage: Lineage {
            parent_ids: vec![],
            generation: 0,
            created_by: format!("evolver-{}", run_id),
            created_at: chrono::Utc::now(),
        },
        default_context_scope: None,
    };

    let path = agency::save_role(&role, roles_dir).context("Failed to save new role")?;

    Ok(serde_json::json!({
        "op": "create_role",
        "id": id,
        "name": name,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_modify_role(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("modify_role requires target_id"))?;

    // Support crossover: target_id may be "parent-a,parent-b"
    let parent_ids: Vec<&str> = target_id
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if parent_ids.is_empty() {
        anyhow::bail!("modify_role target_id produced zero valid parent IDs after parsing");
    }

    // Find parent(s) and compute lineage
    let lineage = if parent_ids.len() == 1 {
        let parent = existing_roles
            .iter()
            .find(|r| r.id == parent_ids[0])
            .ok_or_else(|| anyhow::anyhow!("Parent role '{}' not found", parent_ids[0]))?;
        Lineage::mutation(parent_ids[0], parent.lineage.generation, run_id)
    } else {
        for pid in &parent_ids {
            if !existing_roles.iter().any(|r| r.id == *pid) {
                anyhow::bail!("Parent role '{}' not found for crossover", pid);
            }
        }
        let max_gen = parent_ids
            .iter()
            .filter_map(|pid| existing_roles.iter().find(|r| r.id == *pid))
            .map(|r| r.lineage.generation)
            .max()
            .unwrap_or(0);
        Lineage::crossover(&parent_ids, max_gen, run_id)
    };

    let component_ids: Vec<String> = op
        .component_ids
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|s| {
            agency::content_hash_component(s, &ComponentCategory::Translated, &ContentRef::Name(s.to_string()))
        })
        .collect();

    let description = op.description.clone().unwrap_or_default();
    let outcome_id = op.outcome_id.clone().unwrap_or_default();
    let id = agency::content_hash_role(&component_ids, &outcome_id);

    let role = Role {
        id: id.clone(),
        name: op.name.clone().unwrap_or_else(|| id.clone()),
        description,
        component_ids,
        outcome_id,
        performance: PerformanceRecord::default(),
        lineage,
        default_context_scope: None,
    };

    let path = agency::save_role(&role, roles_dir).context("Failed to save modified role")?;

    Ok(serde_json::json!({
        "op": "modify_role",
        "target_id": target_id,
        "new_id": id,
        "name": role.name,
        "generation": role.lineage.generation,
        "parent_ids": role.lineage.parent_ids,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_create_motivation(
    op: &EvolverOperation,
    run_id: &str,
    tradeoffs_dir: &Path,
) -> Result<serde_json::Value> {
    let name = op
        .name
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("create_motivation requires name"))?;

    let description = op.description.clone().unwrap_or_default();
    let acceptable = op.acceptable_tradeoffs.clone().unwrap_or_default();
    let unacceptable = op.unacceptable_tradeoffs.clone().unwrap_or_default();
    let id = agency::content_hash_tradeoff(&acceptable, &unacceptable, &description);

    let tradeoff = TradeoffConfig {
        id: id.clone(),
        name: name.to_string(),
        description,
        acceptable_tradeoffs: acceptable,
        unacceptable_tradeoffs: unacceptable,
        performance: PerformanceRecord::default(),
        lineage: Lineage {
            parent_ids: vec![],
            generation: 0,
            created_by: format!("evolver-{}", run_id),
            created_at: chrono::Utc::now(),
        },
        access_control: AccessControl::default(),
        former_agents: vec![],
        former_deployments: vec![],
    };

    let path = agency::save_tradeoff(&tradeoff, tradeoffs_dir)
        .context("Failed to save new tradeoff")?;

    Ok(serde_json::json!({
        "op": "create_motivation",
        "id": id,
        "name": name,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_modify_motivation(
    op: &EvolverOperation,
    existing_tradeoffs: &[TradeoffConfig],
    run_id: &str,
    tradeoffs_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("modify_motivation requires target_id"))?;

    // Support crossover: target_id may be "parent-a,parent-b"
    let parent_ids: Vec<&str> = target_id
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if parent_ids.is_empty() {
        anyhow::bail!("modify_motivation target_id produced zero valid parent IDs after parsing");
    }

    let lineage = if parent_ids.len() == 1 {
        let parent = existing_tradeoffs
            .iter()
            .find(|m| m.id == parent_ids[0])
            .ok_or_else(|| anyhow::anyhow!("Parent tradeoff '{}' not found", parent_ids[0]))?;
        Lineage::mutation(parent_ids[0], parent.lineage.generation, run_id)
    } else {
        for pid in &parent_ids {
            if !existing_tradeoffs.iter().any(|m| m.id == *pid) {
                anyhow::bail!("Parent tradeoff '{}' not found for crossover", pid);
            }
        }
        let max_gen = parent_ids
            .iter()
            .filter_map(|pid| existing_tradeoffs.iter().find(|m| m.id == *pid))
            .map(|m| m.lineage.generation)
            .max()
            .unwrap_or(0);
        Lineage::crossover(&parent_ids, max_gen, run_id)
    };

    let description = op.description.clone().unwrap_or_default();
    let acceptable = op.acceptable_tradeoffs.clone().unwrap_or_default();
    let unacceptable = op.unacceptable_tradeoffs.clone().unwrap_or_default();
    let id = agency::content_hash_tradeoff(&acceptable, &unacceptable, &description);

    let tradeoff = TradeoffConfig {
        id: id.clone(),
        name: op.name.clone().unwrap_or_else(|| id.clone()),
        description,
        acceptable_tradeoffs: acceptable,
        unacceptable_tradeoffs: unacceptable,
        performance: PerformanceRecord::default(),
        lineage,
        access_control: AccessControl::default(),
        former_agents: vec![],
        former_deployments: vec![],
    };

    let path = agency::save_tradeoff(&tradeoff, tradeoffs_dir)
        .context("Failed to save modified tradeoff")?;

    Ok(serde_json::json!({
        "op": "modify_motivation",
        "target_id": target_id,
        "new_id": id,
        "name": tradeoff.name,
        "generation": tradeoff.lineage.generation,
        "parent_ids": tradeoff.lineage.parent_ids,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_retire_role(
    op: &EvolverOperation,
    existing_roles: &[Role],
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("retire_role requires target_id"))?;

    // Verify the role exists
    if !existing_roles.iter().any(|r| r.id == target_id) {
        bail!("Role '{}' not found", target_id);
    }

    // Safety: never retire the last role
    if existing_roles.len() <= 1 {
        bail!(
            "Cannot retire '{}': it is the only remaining role. Create a replacement first.",
            target_id
        );
    }

    // Rename .yaml to .yaml.retired
    let yaml_path = roles_dir.join(format!("{}.yaml", target_id));
    let retired_path = roles_dir.join(format!("{}.yaml.retired", target_id));

    if yaml_path.exists() {
        fs::rename(&yaml_path, &retired_path)
            .with_context(|| format!("Failed to retire role '{}'", target_id))?;
    } else {
        bail!("Role file not found: {}", yaml_path.display());
    }

    Ok(serde_json::json!({
        "op": "retire_role",
        "target_id": target_id,
        "retired_path": retired_path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_retire_motivation(
    op: &EvolverOperation,
    existing_tradeoffs: &[TradeoffConfig],
    tradeoffs_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("retire_motivation requires target_id"))?;

    // Verify the tradeoff exists
    if !existing_tradeoffs.iter().any(|m| m.id == target_id) {
        bail!("Tradeoff '{}' not found", target_id);
    }

    // Safety: never retire the last tradeoff
    if existing_tradeoffs.len() <= 1 {
        bail!(
            "Cannot retire '{}': it is the only remaining tradeoff. Create a replacement first.",
            target_id
        );
    }

    // Rename .yaml to .yaml.retired
    let yaml_path = tradeoffs_dir.join(format!("{}.yaml", target_id));
    let retired_path = tradeoffs_dir.join(format!("{}.yaml.retired", target_id));

    if yaml_path.exists() {
        fs::rename(&yaml_path, &retired_path)
            .with_context(|| format!("Failed to retire tradeoff '{}'", target_id))?;
    } else {
        bail!("Tradeoff file not found: {}", yaml_path.display());
    }

    Ok(serde_json::json!({
        "op": "retire_motivation",
        "target_id": target_id,
        "retired_path": retired_path.display().to_string(),
        "status": "applied",
    }))
}

// ---------------------------------------------------------------------------
// New apply functions: mutation operations
// ---------------------------------------------------------------------------

/// Parse entity_type string to ComponentCategory, defaulting to Novel.
fn parse_category(s: Option<&str>) -> ComponentCategory {
    match s {
        Some("translated") => ComponentCategory::Translated,
        Some("enhanced") => ComponentCategory::Enhanced,
        _ => ComponentCategory::Novel,
    }
}

/// Check if an operation should be deferred due to human oversight gates.
fn should_defer(op: &EvolverOperation, agency_dir: &Path) -> Option<DeferralReason> {
    let entity_type = op.entity_type.as_deref().unwrap_or("");

    // bizarre_ideation on outcomes is always deferred
    if op.op == "bizarre_ideation" && entity_type == "outcome" {
        return Some(DeferralReason::BizarreObjective);
    }

    // config_swap_outcome is always deferred (outcome change)
    if op.op == "config_swap_outcome" {
        return Some(DeferralReason::ObjectiveChange);
    }

    // wording_mutation on outcomes: check requires_human_oversight
    if entity_type == "outcome" {
        if let Some(ref target_id) = op.target_id {
            let outcome_path = agency_dir
                .join("primitives/outcomes")
                .join(format!("{}.yaml", target_id));
            if let Ok(outcome) = agency::load_outcome(&outcome_path) {
                if outcome.requires_human_oversight {
                    return Some(DeferralReason::ObjectiveChange);
                }
            }
        }
        // For bizarre_ideation outcomes (already handled above), or new outcomes
        // with requires_human_oversight default = true
        if op.op == "wording_mutation" && op.target_id.is_none() {
            return Some(DeferralReason::ObjectiveChange);
        }
    }

    // random_compose_role: check if the selected outcome has requires_human_oversight
    if op.op == "random_compose_role" {
        if let Some(ref oid) = op.outcome_id {
            let outcome_path = agency_dir
                .join("primitives/outcomes")
                .join(format!("{}.yaml", oid));
            if let Ok(outcome) = agency::load_outcome(&outcome_path) {
                if outcome.requires_human_oversight {
                    return Some(DeferralReason::ObjectiveChange);
                }
            }
        }
    }

    None
}

/// Write a deferred operation to agency/deferred/.
fn defer_operation(
    op: &EvolverOperation,
    reason: DeferralReason,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let deferred_dir = agency_dir.join("deferred");
    fs::create_dir_all(&deferred_dir)?;

    let id = format!(
        "def-{}-{}",
        &run_id,
        op.target_id
            .as_deref()
            .or(op.entity_type.as_deref())
            .unwrap_or("unknown")
    );

    let deferred = DeferredOperation {
        id: id.clone(),
        task_id: run_id.to_string(),
        operation: op.clone(),
        deferred_reason: reason,
        proposed_at: Utc::now().to_rfc3339(),
        human_decision: None,
    };

    let path = deferred_dir.join(format!("{}.json", id));
    fs::write(&path, serde_json::to_string_pretty(&deferred)?)?;

    Ok(serde_json::json!({
        "op": op.op,
        "status": "deferred",
        "deferred_id": id,
        "path": path.display().to_string(),
    }))
}

fn apply_wording_mutation(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let entity_type = op
        .entity_type
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("wording_mutation requires entity_type"))?;
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("wording_mutation requires target_id"))?;

    // Check deferred gate
    if let Some(reason) = should_defer(op, agency_dir) {
        return defer_operation(op, reason, run_id, agency_dir);
    }

    match entity_type {
        "component" => {
            let components_dir = agency_dir.join("primitives/components");
            let source_path = components_dir.join(format!("{}.yaml", target_id));
            let source: agency::RoleComponent =
                agency::load_component(&source_path).context("Source component not found")?;

            let new_desc = op
                .new_description
                .as_deref()
                .unwrap_or(&source.description);
            let new_content = if let Some(ref c) = op.new_content {
                agency::ContentRef::Inline(c.clone())
            } else {
                source.content.clone()
            };
            let category = parse_category(op.new_category.as_deref());

            let new_id =
                agency::content_hash_component(new_desc, &category, &new_content);
            let new_component = agency::RoleComponent {
                id: new_id.clone(),
                name: op.new_name.clone().unwrap_or_else(|| source.name.clone()),
                description: new_desc.to_string(),
                category,
                content: new_content,
                performance: PerformanceRecord::default(),
                lineage: Lineage::mutation(target_id, source.lineage.generation, run_id),
                access_control: source.access_control.clone(),
                former_agents: vec![],
                former_deployments: vec![],
            };

            let path = agency::save_component(&new_component, &components_dir)?;
            Ok(serde_json::json!({
                "op": "wording_mutation",
                "entity_type": "component",
                "source_id": target_id,
                "new_id": new_id,
                "path": path.display().to_string(),
                "status": "applied",
            }))
        }
        "tradeoff" => {
            let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
            let source_path = tradeoffs_dir.join(format!("{}.yaml", target_id));
            let source: agency::TradeoffConfig =
                agency::load_tradeoff(&source_path).context("Source tradeoff not found")?;

            let new_desc = op
                .new_description
                .as_deref()
                .unwrap_or(&source.description);
            let acceptable = op
                .new_acceptable_tradeoffs
                .clone()
                .unwrap_or_else(|| source.acceptable_tradeoffs.clone());
            let unacceptable = op
                .new_unacceptable_tradeoffs
                .clone()
                .unwrap_or_else(|| source.unacceptable_tradeoffs.clone());

            let new_id =
                agency::content_hash_tradeoff(&acceptable, &unacceptable, new_desc);
            let new_tradeoff = agency::TradeoffConfig {
                id: new_id.clone(),
                name: op.new_name.clone().unwrap_or_else(|| source.name.clone()),
                description: new_desc.to_string(),
                acceptable_tradeoffs: acceptable,
                unacceptable_tradeoffs: unacceptable,
                performance: PerformanceRecord::default(),
                lineage: Lineage::mutation(target_id, source.lineage.generation, run_id),
                access_control: source.access_control.clone(),
                former_agents: vec![],
                former_deployments: vec![],
            };

            let path = agency::save_tradeoff(&new_tradeoff, &tradeoffs_dir)?;
            Ok(serde_json::json!({
                "op": "wording_mutation",
                "entity_type": "tradeoff",
                "source_id": target_id,
                "new_id": new_id,
                "path": path.display().to_string(),
                "status": "applied",
            }))
        }
        "outcome" => {
            let outcomes_dir = agency_dir.join("primitives/outcomes");
            let source_path = outcomes_dir.join(format!("{}.yaml", target_id));
            let source: agency::DesiredOutcome =
                agency::load_outcome(&source_path).context("Source outcome not found")?;

            let new_desc = op
                .new_description
                .as_deref()
                .unwrap_or(&source.description);
            let criteria = op
                .new_success_criteria
                .clone()
                .unwrap_or_else(|| source.success_criteria.clone());

            let new_id = agency::content_hash_outcome(new_desc, &criteria);
            let new_outcome = agency::DesiredOutcome {
                id: new_id.clone(),
                name: op.new_name.clone().unwrap_or_else(|| source.name.clone()),
                description: new_desc.to_string(),
                success_criteria: criteria,
                performance: PerformanceRecord::default(),
                lineage: Lineage::mutation(target_id, source.lineage.generation, run_id),
                access_control: source.access_control.clone(),
                requires_human_oversight: source.requires_human_oversight,
                former_agents: vec![],
                former_deployments: vec![],
            };

            let path = agency::save_outcome(&new_outcome, &outcomes_dir)?;
            Ok(serde_json::json!({
                "op": "wording_mutation",
                "entity_type": "outcome",
                "source_id": target_id,
                "new_id": new_id,
                "path": path.display().to_string(),
                "status": "applied",
            }))
        }
        other => bail!("wording_mutation: unsupported entity_type '{}'", other),
    }
}

fn apply_component_substitution(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("component_substitution requires target_id"))?;
    let remove_id = op
        .remove_component_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("component_substitution requires remove_component_id"))?;
    let add_id = op
        .add_component_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("component_substitution requires add_component_id"))?;

    let old_role = existing_roles
        .iter()
        .find(|r| r.id == target_id)
        .ok_or_else(|| anyhow::anyhow!("Role '{}' not found", target_id))?;

    let mut new_comp_ids: Vec<String> = old_role
        .component_ids
        .iter()
        .filter(|c| c.as_str() != remove_id)
        .cloned()
        .collect();
    if !new_comp_ids.contains(&add_id.to_string()) {
        new_comp_ids.push(add_id.to_string());
    }
    new_comp_ids.sort();

    let new_role_id = agency::content_hash_role(&new_comp_ids, &old_role.outcome_id);
    if new_role_id == old_role.id {
        return Ok(serde_json::json!({
            "op": "component_substitution",
            "status": "no_op",
            "reason": "Substitution produces identical role hash",
        }));
    }

    let new_role = Role {
        id: new_role_id.clone(),
        name: op
            .new_name
            .clone()
            .unwrap_or_else(|| old_role.name.clone()),
        description: op
            .new_description
            .clone()
            .unwrap_or_else(|| old_role.description.clone()),
        component_ids: new_comp_ids,
        outcome_id: old_role.outcome_id.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_role.lineage.generation, run_id),
        default_context_scope: old_role.default_context_scope.clone(),
    };

    let path = agency::save_role(&new_role, roles_dir)?;
    Ok(serde_json::json!({
        "op": "component_substitution",
        "target_id": target_id,
        "removed": remove_id,
        "added": add_id,
        "new_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_config_add_component(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_add_component requires target_id"))?;
    let add_id = op
        .add_component_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_add_component requires add_component_id"))?;

    let old_role = existing_roles
        .iter()
        .find(|r| r.id == target_id)
        .ok_or_else(|| anyhow::anyhow!("Role '{}' not found", target_id))?;

    let mut new_comp_ids = old_role.component_ids.clone();
    if !new_comp_ids.contains(&add_id.to_string()) {
        new_comp_ids.push(add_id.to_string());
    }
    new_comp_ids.sort();

    let new_role_id = agency::content_hash_role(&new_comp_ids, &old_role.outcome_id);
    if new_role_id == old_role.id {
        return Ok(serde_json::json!({
            "op": "config_add_component",
            "status": "no_op",
            "reason": "Component already present in role",
        }));
    }

    let new_role = Role {
        id: new_role_id.clone(),
        name: old_role.name.clone(),
        description: old_role.description.clone(),
        component_ids: new_comp_ids,
        outcome_id: old_role.outcome_id.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_role.lineage.generation, run_id),
        default_context_scope: old_role.default_context_scope.clone(),
    };

    let path = agency::save_role(&new_role, roles_dir)?;
    Ok(serde_json::json!({
        "op": "config_add_component",
        "target_id": target_id,
        "added": add_id,
        "new_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_config_remove_component(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_remove_component requires target_id"))?;
    let remove_id = op
        .remove_component_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_remove_component requires remove_component_id"))?;

    let old_role = existing_roles
        .iter()
        .find(|r| r.id == target_id)
        .ok_or_else(|| anyhow::anyhow!("Role '{}' not found", target_id))?;

    let new_comp_ids: Vec<String> = old_role
        .component_ids
        .iter()
        .filter(|c| c.as_str() != remove_id)
        .cloned()
        .collect();

    if new_comp_ids.len() == old_role.component_ids.len() {
        return Ok(serde_json::json!({
            "op": "config_remove_component",
            "status": "no_op",
            "reason": "Component not present in role",
        }));
    }

    let new_role_id = agency::content_hash_role(&new_comp_ids, &old_role.outcome_id);

    let new_role = Role {
        id: new_role_id.clone(),
        name: old_role.name.clone(),
        description: old_role.description.clone(),
        component_ids: new_comp_ids,
        outcome_id: old_role.outcome_id.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_role.lineage.generation, run_id),
        default_context_scope: old_role.default_context_scope.clone(),
    };

    let path = agency::save_role(&new_role, roles_dir)?;
    Ok(serde_json::json!({
        "op": "config_remove_component",
        "target_id": target_id,
        "removed": remove_id,
        "new_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_config_swap_outcome(
    op: &EvolverOperation,
    existing_roles: &[Role],
    run_id: &str,
    roles_dir: &Path,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    // config_swap_outcome is always deferred (outcome change)
    if let Some(reason) = should_defer(op, agency_dir) {
        return defer_operation(op, reason, run_id, agency_dir);
    }

    // This branch executes only if the deferred operation was approved and
    // is being re-applied (should_defer won't fire in that context).
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_swap_outcome requires target_id"))?;
    let new_oid = op
        .new_outcome_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_swap_outcome requires new_outcome_id"))?;

    let old_role = existing_roles
        .iter()
        .find(|r| r.id == target_id)
        .ok_or_else(|| anyhow::anyhow!("Role '{}' not found", target_id))?;

    let new_role_id = agency::content_hash_role(&old_role.component_ids, new_oid);

    let new_role = Role {
        id: new_role_id.clone(),
        name: old_role.name.clone(),
        description: old_role.description.clone(),
        component_ids: old_role.component_ids.clone(),
        outcome_id: new_oid.to_string(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_role.lineage.generation, run_id),
        default_context_scope: old_role.default_context_scope.clone(),
    };

    let path = agency::save_role(&new_role, roles_dir)?;
    Ok(serde_json::json!({
        "op": "config_swap_outcome",
        "target_id": target_id,
        "new_outcome_id": new_oid,
        "new_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_config_swap_tradeoff(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let target_id = op
        .target_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_swap_tradeoff requires target_id"))?;
    let new_tid = op
        .new_tradeoff_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("config_swap_tradeoff requires new_tradeoff_id"))?;

    let agents_dir = agency_dir.join("cache/agents");
    let agent_path = agents_dir.join(format!("{}.yaml", target_id));
    let old_agent: agency::Agent =
        agency::load_agent(&agent_path).context("Target agent not found")?;

    let new_agent_id = agency::content_hash_agent(&old_agent.role_id, new_tid);
    if new_agent_id == old_agent.id {
        return Ok(serde_json::json!({
            "op": "config_swap_tradeoff",
            "status": "no_op",
            "reason": "Agent already has this tradeoff",
        }));
    }

    let new_agent = agency::Agent {
        id: new_agent_id.clone(),
        role_id: old_agent.role_id.clone(),
        tradeoff_id: new_tid.to_string(),
        name: old_agent.name.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(target_id, old_agent.lineage.generation, run_id),
        capabilities: old_agent.capabilities.clone(),
        rate: old_agent.rate,
        capacity: old_agent.capacity,
        trust_level: old_agent.trust_level.clone(),
        contact: old_agent.contact.clone(),
        executor: old_agent.executor.clone(),
        deployment_history: vec![],
        attractor_weight: 0.3, // untested new config
        staleness_flags: vec![],
    };

    let path = agency::save_agent(&new_agent, &agents_dir)?;
    Ok(serde_json::json!({
        "op": "config_swap_tradeoff",
        "target_id": target_id,
        "new_tradeoff_id": new_tid,
        "new_id": new_agent_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

// ---------------------------------------------------------------------------
// Randomisation apply functions
// ---------------------------------------------------------------------------

fn apply_random_compose_role(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    // Check deferred gate for outcome oversight
    if let Some(reason) = should_defer(op, agency_dir) {
        return defer_operation(op, reason, run_id, agency_dir);
    }

    let comp_ids = op
        .component_ids
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("random_compose_role requires component_ids"))?;
    let outcome_id = op
        .outcome_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("random_compose_role requires outcome_id"))?;

    // Verify all components exist
    let components_dir = agency_dir.join("primitives/components");
    for cid in comp_ids {
        if !components_dir.join(format!("{}.yaml", cid)).exists() {
            bail!(
                "random_compose_role: component '{}' not found in store",
                cid
            );
        }
    }
    // Verify outcome exists
    let outcomes_dir = agency_dir.join("primitives/outcomes");
    if !outcomes_dir.join(format!("{}.yaml", outcome_id)).exists() {
        bail!(
            "random_compose_role: outcome '{}' not found in store",
            outcome_id
        );
    }

    let mut sorted_ids = comp_ids.clone();
    sorted_ids.sort();
    let new_role_id = agency::content_hash_role(&sorted_ids, outcome_id);

    // Check if already exists
    let roles_dir = agency_dir.join("cache/roles");
    if roles_dir.join(format!("{}.yaml", new_role_id)).exists() {
        return Ok(serde_json::json!({
            "op": "random_compose_role",
            "status": "no_op",
            "reason": "This composition already exists",
            "existing_id": new_role_id,
        }));
    }

    let new_role = Role {
        id: new_role_id.clone(),
        name: op.new_name.clone().unwrap_or_else(|| {
            format!("random-role-{}", &new_role_id[..8.min(new_role_id.len())])
        }),
        description: op
            .new_description
            .clone()
            .unwrap_or_else(|| "Randomly composed role".to_string()),
        component_ids: sorted_ids,
        outcome_id: outcome_id.to_string(),
        performance: PerformanceRecord::default(),
        lineage: Lineage {
            parent_ids: vec![],
            generation: 0,
            created_by: format!("evolver-randomise-{}", run_id),
            created_at: Utc::now(),
        },
        default_context_scope: None,
    };

    let path = agency::save_role(&new_role, &roles_dir)?;
    Ok(serde_json::json!({
        "op": "random_compose_role",
        "new_id": new_role_id,
        "component_ids": new_role.component_ids,
        "outcome_id": outcome_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

fn apply_random_compose_agent(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let role_id = op
        .role_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("random_compose_agent requires role_id"))?;
    let tradeoff_id = op
        .tradeoff_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("random_compose_agent requires tradeoff_id"))?;

    // Verify role exists
    let roles_dir = agency_dir.join("cache/roles");
    if !roles_dir.join(format!("{}.yaml", role_id)).exists() {
        bail!(
            "random_compose_agent: role '{}' not found in store",
            role_id
        );
    }
    // Verify tradeoff exists
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    if !tradeoffs_dir.join(format!("{}.yaml", tradeoff_id)).exists() {
        bail!(
            "random_compose_agent: tradeoff '{}' not found in store",
            tradeoff_id
        );
    }

    let new_agent_id = agency::content_hash_agent(role_id, tradeoff_id);
    let agents_dir = agency_dir.join("cache/agents");

    // Check if already exists
    if agents_dir.join(format!("{}.yaml", new_agent_id)).exists() {
        return Ok(serde_json::json!({
            "op": "random_compose_agent",
            "status": "no_op",
            "reason": "This agent composition already exists",
            "existing_id": new_agent_id,
        }));
    }

    let new_agent = agency::Agent {
        id: new_agent_id.clone(),
        role_id: role_id.to_string(),
        tradeoff_id: tradeoff_id.to_string(),
        name: op.new_name.clone().unwrap_or_else(|| {
            format!(
                "random-agent-{}",
                &new_agent_id[..8.min(new_agent_id.len())]
            )
        }),
        performance: PerformanceRecord::default(),
        lineage: Lineage {
            parent_ids: vec![],
            generation: 0,
            created_by: format!("evolver-randomise-{}", run_id),
            created_at: Utc::now(),
        },
        capabilities: vec![],
        rate: None,
        capacity: None,
        trust_level: workgraph::graph::TrustLevel::Provisional,
        contact: None,
        executor: "claude".to_string(),
        deployment_history: vec![],
        attractor_weight: 0.3,
        staleness_flags: vec![],
    };

    let path = agency::save_agent(&new_agent, &agents_dir)?;
    Ok(serde_json::json!({
        "op": "random_compose_agent",
        "new_id": new_agent_id,
        "role_id": role_id,
        "tradeoff_id": tradeoff_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

// ---------------------------------------------------------------------------
// Bizarre ideation apply function
// ---------------------------------------------------------------------------

fn apply_bizarre_ideation(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
) -> Result<serde_json::Value> {
    let entity_type = op
        .entity_type
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("bizarre_ideation requires entity_type"))?;

    // Check deferred gate (outcomes are always deferred)
    if let Some(reason) = should_defer(op, agency_dir) {
        return defer_operation(op, reason, run_id, agency_dir);
    }

    match entity_type {
        "component" => {
            let components_dir = agency_dir.join("primitives/components");
            let desc = op
                .new_description
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("bizarre_ideation component requires new_description"))?;
            let content = if let Some(ref c) = op.new_content {
                agency::ContentRef::Inline(c.clone())
            } else {
                agency::ContentRef::Inline(desc.to_string())
            };
            let category = parse_category(op.new_category.as_deref());

            let new_id = agency::content_hash_component(desc, &category, &content);
            let new_component = agency::RoleComponent {
                id: new_id.clone(),
                name: op
                    .new_name
                    .clone()
                    .unwrap_or_else(|| format!("bizarre-{}", &new_id[..8.min(new_id.len())])),
                description: desc.to_string(),
                category,
                content,
                performance: PerformanceRecord::default(),
                lineage: Lineage {
                    parent_ids: vec![],
                    generation: 0,
                    created_by: format!("evolver-bizarre-{}", run_id),
                    created_at: Utc::now(),
                },
                access_control: AccessControl::default(),
                former_agents: vec![],
                former_deployments: vec![],
            };

            let path = agency::save_component(&new_component, &components_dir)?;
            Ok(serde_json::json!({
                "op": "bizarre_ideation",
                "entity_type": "component",
                "new_id": new_id,
                "name": new_component.name,
                "path": path.display().to_string(),
                "status": "applied",
            }))
        }
        "tradeoff" => {
            let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
            let desc = op
                .new_description
                .as_deref()
                .ok_or_else(|| {
                    anyhow::anyhow!("bizarre_ideation tradeoff requires new_description")
                })?;
            let acceptable = op.new_acceptable_tradeoffs.clone().unwrap_or_default();
            let unacceptable = op.new_unacceptable_tradeoffs.clone().unwrap_or_default();

            let new_id =
                agency::content_hash_tradeoff(&acceptable, &unacceptable, desc);
            let new_tradeoff = agency::TradeoffConfig {
                id: new_id.clone(),
                name: op
                    .new_name
                    .clone()
                    .unwrap_or_else(|| format!("bizarre-{}", &new_id[..8.min(new_id.len())])),
                description: desc.to_string(),
                acceptable_tradeoffs: acceptable,
                unacceptable_tradeoffs: unacceptable,
                performance: PerformanceRecord::default(),
                lineage: Lineage {
                    parent_ids: vec![],
                    generation: 0,
                    created_by: format!("evolver-bizarre-{}", run_id),
                    created_at: Utc::now(),
                },
                access_control: AccessControl::default(),
                former_agents: vec![],
                former_deployments: vec![],
            };

            let path = agency::save_tradeoff(&new_tradeoff, &tradeoffs_dir)?;
            Ok(serde_json::json!({
                "op": "bizarre_ideation",
                "entity_type": "tradeoff",
                "new_id": new_id,
                "name": new_tradeoff.name,
                "path": path.display().to_string(),
                "status": "applied",
            }))
        }
        "outcome" => {
            // This should have been caught by should_defer, but handle gracefully
            bail!("bizarre_ideation on outcomes must go through the deferred queue");
        }
        other => bail!("bizarre_ideation: unsupported entity_type '{}'", other),
    }
}


// ---------------------------------------------------------------------------
// Meta-agent (AgentConfigurations level) apply functions
// ---------------------------------------------------------------------------

/// Resolve a meta_role string to the config field accessor names.
/// Returns (slot_label, current_agent_hash) or an error if the slot is invalid.
fn resolve_meta_slot<'a>(
    meta_role: &str,
    config: &'a Config,
) -> Result<(&'static str, Option<&'a String>)> {
    match meta_role {
        "assigner" => Ok(("assigner_agent", config.agency.assigner_agent.as_ref())),
        "evaluator" => Ok(("evaluator_agent", config.agency.evaluator_agent.as_ref())),
        "evolver" => Ok(("evolver_agent", config.agency.evolver_agent.as_ref())),
        other => bail!(
            "Unknown meta_role '{}'. Valid: assigner, evaluator, evolver",
            other
        ),
    }
}

/// Update the config's meta-agent slot with a new agent hash.
fn update_meta_slot(meta_role: &str, new_agent_hash: &str, config: &mut Config) {
    match meta_role {
        "assigner" => config.agency.assigner_agent = Some(new_agent_hash.to_string()),
        "evaluator" => config.agency.evaluator_agent = Some(new_agent_hash.to_string()),
        "evolver" => config.agency.evolver_agent = Some(new_agent_hash.to_string()),
        _ => {}
    }
}

/// Swap the role of a meta-agent (assigner/evaluator/evolver), keeping its tradeoff.
/// Creates a new agent with the new role and updates the config slot.
fn apply_meta_swap_role(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
    dir: &Path,
) -> Result<serde_json::Value> {
    let meta_role = op
        .meta_role
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("meta_swap_role requires meta_role"))?;
    let new_role_id = op
        .role_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("meta_swap_role requires role_id (the new role)"))?;

    let mut config = Config::load_or_default(dir);
    let (slot_label, current_hash) = resolve_meta_slot(meta_role, &config)?;
    let current_hash = current_hash
        .ok_or_else(|| anyhow::anyhow!("meta_swap_role: no {} currently configured", slot_label))?
        .clone();

    // Load current agent to get its tradeoff_id
    let agents_dir = agency_dir.join("cache/agents");
    let old_agent = agency::load_agent(&agents_dir.join(format!("{}.yaml", current_hash)))
        .context("Failed to load current meta-agent")?;

    // Verify new role exists
    let roles_dir = agency_dir.join("cache/roles");
    if !roles_dir.join(format!("{}.yaml", new_role_id)).exists() {
        bail!(
            "meta_swap_role: role '{}' not found in store",
            new_role_id
        );
    }

    if old_agent.role_id == new_role_id {
        return Ok(serde_json::json!({
            "op": "meta_swap_role",
            "meta_role": meta_role,
            "status": "no_op",
            "reason": "Meta-agent already has this role",
        }));
    }

    let new_agent_id = agency::content_hash_agent(new_role_id, &old_agent.tradeoff_id);
    let new_agent = agency::Agent {
        id: new_agent_id.clone(),
        role_id: new_role_id.to_string(),
        tradeoff_id: old_agent.tradeoff_id.clone(),
        name: old_agent.name.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(&current_hash, old_agent.lineage.generation, run_id),
        capabilities: old_agent.capabilities.clone(),
        rate: old_agent.rate,
        capacity: old_agent.capacity,
        trust_level: old_agent.trust_level.clone(),
        contact: old_agent.contact.clone(),
        executor: old_agent.executor.clone(),
        deployment_history: vec![],
        attractor_weight: 0.3,
        staleness_flags: vec![],
    };

    let path = agency::save_agent(&new_agent, &agents_dir)?;
    update_meta_slot(meta_role, &new_agent_id, &mut config);
    config.save(dir).context("Failed to save config after meta_swap_role")?;

    Ok(serde_json::json!({
        "op": "meta_swap_role",
        "meta_role": meta_role,
        "old_agent": current_hash,
        "new_agent": new_agent_id,
        "new_role_id": new_role_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

/// Swap the tradeoff of a meta-agent (assigner/evaluator/evolver), keeping its role.
/// Creates a new agent with the new tradeoff and updates the config slot.
fn apply_meta_swap_tradeoff(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
    dir: &Path,
) -> Result<serde_json::Value> {
    let meta_role = op
        .meta_role
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("meta_swap_tradeoff requires meta_role"))?;
    let new_tradeoff_id = op
        .tradeoff_id
        .as_deref()
        .or(op.new_tradeoff_id.as_deref())
        .ok_or_else(|| anyhow::anyhow!("meta_swap_tradeoff requires tradeoff_id or new_tradeoff_id"))?;

    let mut config = Config::load_or_default(dir);
    let (slot_label, current_hash) = resolve_meta_slot(meta_role, &config)?;
    let current_hash = current_hash
        .ok_or_else(|| anyhow::anyhow!("meta_swap_tradeoff: no {} currently configured", slot_label))?
        .clone();

    // Load current agent
    let agents_dir = agency_dir.join("cache/agents");
    let old_agent = agency::load_agent(&agents_dir.join(format!("{}.yaml", current_hash)))
        .context("Failed to load current meta-agent")?;

    // Verify new tradeoff exists
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    if !tradeoffs_dir.join(format!("{}.yaml", new_tradeoff_id)).exists() {
        bail!(
            "meta_swap_tradeoff: tradeoff '{}' not found in store",
            new_tradeoff_id
        );
    }

    if old_agent.tradeoff_id == new_tradeoff_id {
        return Ok(serde_json::json!({
            "op": "meta_swap_tradeoff",
            "meta_role": meta_role,
            "status": "no_op",
            "reason": "Meta-agent already has this tradeoff",
        }));
    }

    let new_agent_id = agency::content_hash_agent(&old_agent.role_id, new_tradeoff_id);
    let new_agent = agency::Agent {
        id: new_agent_id.clone(),
        role_id: old_agent.role_id.clone(),
        tradeoff_id: new_tradeoff_id.to_string(),
        name: old_agent.name.clone(),
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(&current_hash, old_agent.lineage.generation, run_id),
        capabilities: old_agent.capabilities.clone(),
        rate: old_agent.rate,
        capacity: old_agent.capacity,
        trust_level: old_agent.trust_level.clone(),
        contact: old_agent.contact.clone(),
        executor: old_agent.executor.clone(),
        deployment_history: vec![],
        attractor_weight: 0.3,
        staleness_flags: vec![],
    };

    let path = agency::save_agent(&new_agent, &agents_dir)?;
    update_meta_slot(meta_role, &new_agent_id, &mut config);
    config.save(dir).context("Failed to save config after meta_swap_tradeoff")?;

    Ok(serde_json::json!({
        "op": "meta_swap_tradeoff",
        "meta_role": meta_role,
        "old_agent": current_hash,
        "new_agent": new_agent_id,
        "new_tradeoff_id": new_tradeoff_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

/// Compose a new agent for a meta-agent slot from a role_id + tradeoff_id.
/// Creates the agent if it doesn't exist, then updates the config slot.
fn apply_meta_compose_agent(
    op: &EvolverOperation,
    run_id: &str,
    agency_dir: &Path,
    dir: &Path,
) -> Result<serde_json::Value> {
    let meta_role = op
        .meta_role
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("meta_compose_agent requires meta_role"))?;
    let role_id = op
        .role_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("meta_compose_agent requires role_id"))?;
    let tradeoff_id = op
        .tradeoff_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("meta_compose_agent requires tradeoff_id"))?;

    // Verify role exists
    let roles_dir = agency_dir.join("cache/roles");
    if !roles_dir.join(format!("{}.yaml", role_id)).exists() {
        bail!("meta_compose_agent: role '{}' not found in store", role_id);
    }
    // Verify tradeoff exists
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    if !tradeoffs_dir.join(format!("{}.yaml", tradeoff_id)).exists() {
        bail!(
            "meta_compose_agent: tradeoff '{}' not found in store",
            tradeoff_id
        );
    }

    let new_agent_id = agency::content_hash_agent(role_id, tradeoff_id);
    let agents_dir = agency_dir.join("cache/agents");

    // Create the agent if it doesn't already exist
    let agent_path = agents_dir.join(format!("{}.yaml", new_agent_id));
    let path = if agent_path.exists() {
        agent_path
    } else {
        let config_peek = Config::load_or_default(dir);
        let (_, current_hash) = resolve_meta_slot(meta_role, &config_peek)?;
        let parent_gen = current_hash
            .and_then(|h| agency::load_agent(&agents_dir.join(format!("{}.yaml", h))).ok())
            .map(|a| a.lineage.generation)
            .unwrap_or(0);
        let parent_ids: Vec<String> = current_hash.map(|h| vec![h.clone()]).unwrap_or_default();

        let new_agent = agency::Agent {
            id: new_agent_id.clone(),
            role_id: role_id.to_string(),
            tradeoff_id: tradeoff_id.to_string(),
            name: op.new_name.clone().unwrap_or_else(|| {
                format!(
                    "{}-agent-{}",
                    meta_role,
                    &new_agent_id[..8.min(new_agent_id.len())]
                )
            }),
            performance: PerformanceRecord::default(),
            lineage: Lineage {
                parent_ids,
                generation: parent_gen.saturating_add(1),
                created_by: format!("evolver-meta-{}", run_id),
                created_at: Utc::now(),
            },
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: workgraph::graph::TrustLevel::Provisional,
            contact: None,
            executor: "claude".to_string(),
            deployment_history: vec![],
            attractor_weight: 0.3,
            staleness_flags: vec![],
        };
        agency::save_agent(&new_agent, &agents_dir)?
    };

    let mut config = Config::load_or_default(dir);
    let old_hash = resolve_meta_slot(meta_role, &config)?.1.cloned();
    update_meta_slot(meta_role, &new_agent_id, &mut config);
    config.save(dir).context("Failed to save config after meta_compose_agent")?;

    Ok(serde_json::json!({
        "op": "meta_compose_agent",
        "meta_role": meta_role,
        "old_agent": old_hash,
        "new_agent": new_agent_id,
        "role_id": role_id,
        "tradeoff_id": tradeoff_id,
        "path": path.display().to_string(),
        "status": "applied",
    }))
}

// ---------------------------------------------------------------------------
// Deferred queue management
// ---------------------------------------------------------------------------

/// List pending deferred operations.
pub fn run_deferred_list(dir: &Path, json: bool) -> Result<()> {
    let deferred_dir = dir.join("agency/deferred");
    if !deferred_dir.exists() {
        if json {
            println!("[]");
        } else {
            println!("No deferred operations.");
        }
        return Ok(());
    }

    let mut ops: Vec<DeferredOperation> = Vec::new();
    for entry in fs::read_dir(&deferred_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let contents = fs::read_to_string(&path)?;
            if let Ok(deferred) = serde_json::from_str::<DeferredOperation>(&contents) {
                if deferred.human_decision.is_none() {
                    ops.push(deferred);
                }
            }
        }
    }

    ops.sort_by(|a, b| a.proposed_at.cmp(&b.proposed_at));

    if json {
        println!("{}", serde_json::to_string_pretty(&ops)?);
    } else if ops.is_empty() {
        println!("No pending deferred operations.");
    } else {
        println!("Pending deferred operations:\n");
        for op in &ops {
            println!(
                "  {} — {} on {} ({:?})",
                op.id,
                op.operation.op,
                op.operation
                    .entity_type
                    .as_deref()
                    .unwrap_or("?"),
                op.deferred_reason,
            );
            if let Some(ref rationale) = op.operation.rationale {
                println!("    Rationale: {}", rationale);
            }
            println!("    Proposed: {}", op.proposed_at);
            println!();
        }
        println!("{} pending operation(s).", ops.len());
    }

    Ok(())
}

/// Approve a deferred operation.
pub fn run_deferred_approve(dir: &Path, deferred_id: &str, note: Option<&str>) -> Result<()> {
    let deferred_dir = dir.join("agency/deferred");
    let path = deferred_dir.join(format!("{}.json", deferred_id));
    if !path.exists() {
        bail!("Deferred operation '{}' not found", deferred_id);
    }

    let contents = fs::read_to_string(&path)?;
    let mut deferred: DeferredOperation = serde_json::from_str(&contents)?;

    if deferred.human_decision.is_some() {
        bail!(
            "Deferred operation '{}' already has a decision",
            deferred_id
        );
    }

    deferred.human_decision = Some(HumanDecision {
        approved: true,
        decided_at: Utc::now().to_rfc3339(),
        note: note.map(|s| s.to_string()),
    });

    // Save the updated deferred record
    fs::write(&path, serde_json::to_string_pretty(&deferred)?)?;

    // Now apply the operation
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");

    let roles = agency::load_all_roles(&roles_dir).unwrap_or_default();
    let tradeoffs = agency::load_all_tradeoffs(&tradeoffs_dir).unwrap_or_default();

    let result = apply_operation(
        &deferred.operation,
        &roles,
        &tradeoffs,
        &deferred.task_id,
        &roles_dir,
        &tradeoffs_dir,
        &agency_dir,
        dir,
    );

    match result {
        Ok(res) => {
            println!(
                "Approved and applied '{}': {}",
                deferred_id,
                serde_json::to_string(&res)?
            );
        }
        Err(e) => {
            eprintln!("Approved '{}' but failed to apply: {}", deferred_id, e);
        }
    }

    Ok(())
}

/// Reject a deferred operation.
pub fn run_deferred_reject(dir: &Path, deferred_id: &str, note: Option<&str>) -> Result<()> {
    let deferred_dir = dir.join("agency/deferred");
    let path = deferred_dir.join(format!("{}.json", deferred_id));
    if !path.exists() {
        bail!("Deferred operation '{}' not found", deferred_id);
    }

    let contents = fs::read_to_string(&path)?;
    let mut deferred: DeferredOperation = serde_json::from_str(&contents)?;

    if deferred.human_decision.is_some() {
        bail!(
            "Deferred operation '{}' already has a decision",
            deferred_id
        );
    }

    deferred.human_decision = Some(HumanDecision {
        approved: false,
        decided_at: Utc::now().to_rfc3339(),
        note: note.map(|s| s.to_string()),
    });

    fs::write(&path, serde_json::to_string_pretty(&deferred)?)?;
    println!("Rejected deferred operation '{}'.", deferred_id);

    Ok(())
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn print_operation_result(op: &EvolverOperation, result: &serde_json::Value) {
    let status = result["status"].as_str().unwrap_or("unknown");
    let symbol = if status == "applied" { "+" } else { "!" };

    match op.op.as_str() {
        "create_role" => {
            println!(
                "  [{}] Created role: {} ({})",
                symbol,
                op.name.as_deref().unwrap_or("?"),
                op.new_id.as_deref().unwrap_or("?"),
            );
        }
        "modify_role" => {
            println!(
                "  [{}] Modified role: {} -> {} (gen {})",
                symbol,
                op.target_id.as_deref().unwrap_or("?"),
                op.new_id.as_deref().unwrap_or("?"),
                result["generation"].as_u64().unwrap_or(0),
            );
        }
        "create_motivation" => {
            println!(
                "  [{}] Created motivation: {} ({})",
                symbol,
                op.name.as_deref().unwrap_or("?"),
                op.new_id.as_deref().unwrap_or("?"),
            );
        }
        "modify_motivation" => {
            println!(
                "  [{}] Modified motivation: {} -> {} (gen {})",
                symbol,
                op.target_id.as_deref().unwrap_or("?"),
                op.new_id.as_deref().unwrap_or("?"),
                result["generation"].as_u64().unwrap_or(0),
            );
        }
        "retire_role" => {
            println!(
                "  [{}] Retired role: {}",
                symbol,
                op.target_id.as_deref().unwrap_or("?"),
            );
        }
        "retire_motivation" => {
            println!(
                "  [{}] Retired motivation: {}",
                symbol,
                op.target_id.as_deref().unwrap_or("?"),
            );
        }
        "wording_mutation" => {
            println!(
                "  [{}] Wording mutation ({}) {} -> {}",
                symbol,
                op.entity_type.as_deref().unwrap_or("?"),
                op.target_id.as_deref().unwrap_or("?"),
                result["new_id"].as_str().unwrap_or("?"),
            );
        }
        "component_substitution" => {
            println!(
                "  [{}] Component substitution on {} (-{} +{})",
                symbol,
                op.target_id.as_deref().unwrap_or("?"),
                op.remove_component_id.as_deref().unwrap_or("?"),
                op.add_component_id.as_deref().unwrap_or("?"),
            );
        }
        "config_add_component" | "config_remove_component" => {
            println!(
                "  [{}] {} on {} -> {}",
                symbol,
                op.op,
                op.target_id.as_deref().unwrap_or("?"),
                result["new_id"].as_str().unwrap_or("?"),
            );
        }
        "config_swap_outcome" | "config_swap_tradeoff" => {
            println!(
                "  [{}] {} on {} -> {}",
                symbol,
                op.op,
                op.target_id.as_deref().unwrap_or("?"),
                result["new_id"].as_str().unwrap_or("?"),
            );
        }
        "random_compose_role" | "random_compose_agent" => {
            println!(
                "  [{}] {} -> {}",
                symbol,
                op.op,
                result["new_id"].as_str().unwrap_or("?"),
            );
        }
        "bizarre_ideation" => {
            println!(
                "  [{}] Bizarre ideation ({}) -> {}",
                symbol,
                op.entity_type.as_deref().unwrap_or("?"),
                result["new_id"]
                    .as_str()
                    .or(result["deferred_id"].as_str())
                    .unwrap_or("?"),
            );
        }
        other => {
            println!("  [{}] {}: {:?}", symbol, other, result);
        }
    }

    if let Some(rationale) = &op.rationale {
        println!("        Rationale: {}", rationale);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

        let result = apply_create_role(&op, "test-run", &roles_dir).unwrap();
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

        let result = apply_modify_role(&op, &[parent], "test-run", &roles_dir).unwrap();
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

        let result = apply_retire_role(&op, &[role_a, role_b], &roles_dir).unwrap();
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

        let result = apply_retire_role(&op, &[role], &roles_dir);
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

        let result = apply_create_motivation(&op, "test-run", &tradeoffs_dir).unwrap();
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

        let result = apply_create_motivation(&op, "test-run", &tradeoffs_dir);
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

        let result = apply_modify_motivation(&op, &[parent], "test-run", &tradeoffs_dir).unwrap();
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
        let mot =
            agency::load_tradeoff(&tradeoffs_dir.join(format!("{}.yaml", new_id))).unwrap();
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

        let result = apply_modify_motivation(&op, &[], "test-run", &tradeoffs_dir);
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

        let result = apply_modify_motivation(&op, &[], "test-run", &tradeoffs_dir);
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

        let result =
            apply_modify_motivation(&op, &[parent_a, parent_b], "test-run", &tradeoffs_dir)
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
        let mot =
            agency::load_tradeoff(&tradeoffs_dir.join(format!("{}.yaml", new_id))).unwrap();
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

        let result = apply_modify_motivation(&op, &[parent_a], "test-run", &tradeoffs_dir);
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

        let result = apply_retire_motivation(&op, &[mot_a, mot_b], &tradeoffs_dir).unwrap();
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

        let result = apply_retire_motivation(&op, &[mot], &tradeoffs_dir);
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

        let result = apply_retire_motivation(&op, &[mot], &tradeoffs_dir);
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

        let result = apply_modify_role(&op, &[parent_a, parent_b], "test-run", &roles_dir).unwrap();
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

        let result = apply_modify_role(&op, &[], "test-run", &roles_dir);
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

        let result = apply_modify_role(&op, &[], "test-run", &roles_dir);
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

        let result =
            apply_operation(&op, &[], &[], "run-dispatch", &roles_dir, &tradeoffs_dir, temp_dir.path(), temp_dir.path()).unwrap();
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

        let result = apply_operation(&op, &[], &[], "run-bad", &roles_dir, &tradeoffs_dir, temp_dir.path(), temp_dir.path());
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

        let result1 = apply_create_role(&op, "run-1", &roles_dir).unwrap();
        let result2 = apply_create_role(&op, "run-2", &roles_dir).unwrap();

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

        let result1 = apply_create_motivation(&op, "run-1", &tradeoffs_dir).unwrap();
        let result2 = apply_create_motivation(&op, "run-2", &tradeoffs_dir).unwrap();

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

        let summary = build_performance_summary(&roles, &motivations, &evaluations, &Config::default());

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

        let result = apply_modify_role(&op, &[parent], "run-new", &roles_dir).unwrap();
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

        let result = apply_modify_role(&op, &[parent_a, parent_b], "run-x", &roles_dir).unwrap();
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

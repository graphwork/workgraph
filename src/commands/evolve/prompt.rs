use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use workgraph::agency::{
    self, Evaluation, Role, TradeoffConfig, render_identity_prompt_rich, resolve_all_components,
    resolve_outcome,
};
use workgraph::config::Config;

use super::strategy::Strategy;

pub(crate) fn build_performance_summary(
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
        let valid: Vec<f64> = evaluations
            .iter()
            .map(|e| e.score)
            .filter(|s: &f64| s.is_finite())
            .collect();
        if valid.is_empty() {
            None
        } else {
            Some(valid.iter().sum::<f64>() / valid.len() as f64)
        }
    } else {
        None
    };
    out.push_str(&format!("Total roles: {}\n", roles.len()));
    out.push_str(&format!("Total tradeoffs: {}\n", tradeoffs.len()));
    out.push_str(&format!("Total evaluations: {}\n", total_evals));
    if let Some(avg) = overall_avg {
        out.push_str(&format!("Overall avg score: {:.3}\n", avg));
    }
    out.push('\n');
    out.push_str("### Role Performance\n\n");
    for role in roles {
        let score_str = role
            .performance
            .avg_score
            .map(|s| format!("{:.3}", s))
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "- **{}** (id: `{}`): {} evals, score: {}, gen: {}\n",
            role.name, role.id, role.performance.task_count, score_str, role.lineage.generation
        ));
        out.push_str(&format!("  description: {}\n", role.description));
        out.push_str(&format!("  outcome_id: {}\n", role.outcome_id));
        if !role.component_ids.is_empty() {
            out.push_str(&format!(
                "  component_ids: {}\n",
                role.component_ids.join(", ")
            ));
        }
        if !role.lineage.parent_ids.is_empty() {
            out.push_str(&format!(
                "  parents: {}\n",
                role.lineage.parent_ids.join(", ")
            ));
        }
        let role_evals: Vec<&Evaluation> = evaluations
            .iter()
            .filter(|e| e.role_id == role.id)
            .collect();
        if !role_evals.is_empty() {
            let dims = aggregate_dimensions(&role_evals);
            if !dims.is_empty() {
                let dim_strs: Vec<String> = dims
                    .iter()
                    .map(|(k, v)| format!("{}={:.2}", k, v))
                    .collect();
                out.push_str(&format!("  dimensions: {}\n", dim_strs.join(", ")));
            }
        }
        out.push('\n');
    }
    out.push_str("### Tradeoff Performance\n\n");
    for tradeoff in tradeoffs {
        let score_str = tradeoff
            .performance
            .avg_score
            .map(|s| format!("{:.3}", s))
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "- **{}** (id: `{}`): {} evals, score: {}, gen: {}\n",
            tradeoff.name,
            tradeoff.id,
            tradeoff.performance.task_count,
            score_str,
            tradeoff.lineage.generation
        ));
        out.push_str(&format!("  description: {}\n", tradeoff.description));
        if !tradeoff.acceptable_tradeoffs.is_empty() {
            out.push_str(&format!(
                "  acceptable_tradeoffs: {}\n",
                tradeoff.acceptable_tradeoffs.join("; ")
            ));
        }
        if !tradeoff.unacceptable_tradeoffs.is_empty() {
            out.push_str(&format!(
                "  unacceptable_tradeoffs: {}\n",
                tradeoff.unacceptable_tradeoffs.join("; ")
            ));
        }
        if !tradeoff.lineage.parent_ids.is_empty() {
            out.push_str(&format!(
                "  parents: {}\n",
                tradeoff.lineage.parent_ids.join(", ")
            ));
        }
        out.push('\n');
    }
    let mut synergy: HashMap<(String, String), Vec<f64>> = HashMap::new();
    for eval in evaluations {
        synergy
            .entry((eval.role_id.clone(), eval.tradeoff_id.clone()))
            .or_default()
            .push(eval.score);
    }
    if !synergy.is_empty() {
        out.push_str("### Synergy Matrix (Role x Tradeoff)\n\n");
        let mut pairs: Vec<_> = synergy.iter().collect();
        pairs.sort_by(|a, b| {
            let avg_a = a.1.iter().sum::<f64>() / a.1.len() as f64;
            let avg_b = b.1.iter().sum::<f64>() / b.1.len() as f64;
            avg_b
                .partial_cmp(&avg_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for ((role_id, mot_id), scores) in &pairs {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            out.push_str(&format!(
                "- ({}, {}): avg={:.3}, count={}\n",
                role_id,
                mot_id,
                avg,
                scores.len()
            ));
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

pub(crate) fn load_evolver_skills(
    skills_dir: &Path,
    strategy: Strategy,
) -> Result<Vec<(String, String)>> {
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
        Strategy::CoordinatorEvolution => vec!["coordinator-evolution.md"],
        Strategy::All => vec![
            "role-mutation.md",
            "role-crossover.md",
            "motivation-tuning.md",
            "gap-analysis.md",
            "retirement.md",
            "component-mutation.md",
            "randomisation.md",
            "bizarre-ideation.md",
            "coordinator-evolution.md",
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_evolver_prompt(
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
        if let Ok(agent) = agency::load_agent(&agent_path)
            && let Some(role) = roles.iter().find(|r| r.id == agent.role_id)
            && let Some(tradeoff) = tradeoffs.iter().find(|m| m.id == agent.tradeoff_id)
        {
            // Use the project root (parent of agency dir) for skill resolution
            let workgraph_root = agency_dir.parent().unwrap_or(agency_dir);
            let resolved_skills = resolve_all_components(role, workgraph_root, agency_dir);
            let outcome = resolve_outcome(&role.outcome_id, agency_dir);
            out.push_str(&render_identity_prompt_rich(
                role,
                tradeoff,
                &resolved_skills,
                outcome.as_ref(),
            ));
            out.push_str("\n\n");
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
                        label,
                        hash,
                        agent.role_id,
                        role_name,
                        agent.tradeoff_id,
                        mot_name,
                        perf_str,
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

    // Coordinator prompt evolution section
    out.push_str("### Coordinator Prompt Evolution\n\n");
    out.push_str(
        "The coordinator agent's prompt is composed from files in `.workgraph/agency/coordinator-prompt/`. \
         You can modify the mutable files to improve coordinator behavior based on evaluation data.\n\n",
    );
    out.push_str("- **modify_coordinator_prompt**: Modify a coordinator prompt file. Requires: target_id (\"evolved-amendments\" or \"common-patterns\"), new_content (full file content).\n\n");
    out.push_str(
        "**Immutable files (do NOT target):** `base-system-prompt.md`, `behavioral-rules.md`\n",
    );
    out.push_str("**Mutable files:** `evolved-amendments.md` (add rules/heuristics), `common-patterns.md` (add/update examples)\n\n");

    // Include current coordinator prompt files for context
    let prompt_dir = agency_dir.join("coordinator-prompt");
    if prompt_dir.is_dir() {
        let mutable_files = ["evolved-amendments.md", "common-patterns.md"];
        for filename in &mutable_files {
            let path = prompt_dir.join(filename);
            if let Ok(content) = std::fs::read_to_string(&path) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!(
                        "**Current `{}`:**\n```\n{}\n```\n\n",
                        filename, trimmed
                    ));
                }
            }
        }
    }

    out.push_str("**Important:** Each new/modified entity gets lineage tracking automatically. Just provide the IDs.\n");

    out
}

/// Build a per-strategy analyzer prompt for fan-out mode.
///
/// Unlike `build_evolver_prompt` (which is the monolithic single-shot prompt),
/// this produces a focused prompt for a single strategy analyzer task, including
/// only the operations and instructions relevant to that strategy.
pub(crate) fn build_analyzer_prompt(
    strategy: Strategy,
    run_id: &str,
    skill_doc: &str,
    slice_summary: &str,
    agency_dir: &Path,
) -> String {
    let mut out = String::new();

    // Header
    out.push_str(&format!(
        "# Evolver Analyzer: {}\n\n",
        strategy.label()
    ));
    out.push_str(&format!(
        "You are a specialized evolution analyzer focused on the **{}** strategy.\n\
         Your job is to analyze a pre-filtered data slice and propose concrete operations.\n\n",
        strategy.label()
    ));

    // Strategy-specific analysis instructions
    out.push_str("## Analysis Instructions\n\n");
    out.push_str(&strategy_specific_instructions(strategy));
    out.push('\n');

    // Input data
    out.push_str("## Input Data\n\n");
    out.push_str(&format!(
        "Read your data slice from: `.workgraph/evolve-runs/{}/{}-slice.json`\n\n",
        run_id,
        strategy.label()
    ));
    out.push_str(&format!("Summary: {}\n\n", slice_summary));

    // Skill document
    if !skill_doc.is_empty() {
        out.push_str("## Strategy Skill Document\n\n");
        out.push_str(
            "Follow these guidelines carefully — they define valid operations and guardrails.\n\n",
        );
        out.push_str(skill_doc);
        out.push_str("\n\n");
    }

    // Strategy-specific context (e.g., coordinator prompt files)
    let extra_context = strategy_specific_context(strategy, agency_dir);
    if !extra_context.is_empty() {
        out.push_str("## Additional Context\n\n");
        out.push_str(&extra_context);
        out.push('\n');
    }

    // Available operations for this strategy
    out.push_str("## Available Operations\n\n");
    out.push_str(&strategy_allowed_operations(strategy));
    out.push('\n');

    // Output format
    out.push_str("## Required Output Format\n\n");
    out.push_str(&format!(
        "Write a JSON file to `.workgraph/evolve-runs/{}/{}-proposals.json`:\n\n",
        run_id,
        strategy.label()
    ));
    out.push_str(
        "```json\n\
         {\n  \
           \"strategy\": \"<strategy-label>\",\n  \
           \"run_id\": \"<run-id>\",\n  \
           \"operations\": [\n    \
             {\n      \
               \"op\": \"<operation_type>\",\n      \
               \"target_id\": \"<existing entity ID>\",\n      \
               \"rationale\": \"<why this operation>\",\n      \
               \"confidence\": <0.0-1.0>,\n      \
               \"expected_impact\": \"<what improvement is expected>\"\n      \
               // ... strategy-specific fields per skill doc\n    \
             }\n  \
           ],\n  \
           \"analysis_summary\": \"<brief summary of findings>\"\n\
         }\n\
         ```\n\n",
    );

    // Guardrails
    out.push_str("## Guardrails\n\n");
    out.push_str(&strategy_guardrails(strategy));

    out
}

fn strategy_specific_instructions(strategy: Strategy) -> String {
    match strategy {
        Strategy::Mutation => "\
1. Read the data slice — it contains roles with **moderate** scores (0.25–0.70) that have room for improvement.
2. For each role, examine dimensional scores to find specific weaknesses (fidelity, correctness, etc.).
3. Propose **targeted mutations**: wording changes to descriptions, component swaps, or outcome refinements.
4. Prefer minimal perturbations — change one thing at a time per role so improvements can be attributed.
5. Focus on roles where the improvement signal is clearest (large gap between best and worst dimensions).
".to_string(),

        Strategy::Crossover => "\
1. Read the data slice — it contains **high-performing** roles (≥0.55 avg) that are candidates for crossover.
2. Identify pairs of roles with **complementary strengths**: one excels where the other is weaker.
3. Propose crossover operations that combine the best components/descriptions from each parent.
4. The child role should address a gap that neither parent fills alone.
5. Avoid crossing roles that are too similar — diversity is the goal.
".to_string(),

        Strategy::GapAnalysis => "\
1. Read the data slice — it contains the full role and tradeoff inventory (no raw evaluations).
2. Analyze **coverage gaps**: what task types, skills, or problem domains lack a well-suited role?
3. Look for missing combinations: are there tradeoffs without any high-performing role pairing?
4. Propose **new roles** (`create_role`) or **new tradeoffs** (`create_motivation`) to fill gaps.
5. Consider the project's actual workload — what kind of tasks appear in evaluations?
".to_string(),

        Strategy::Retirement => "\
1. Read the data slice — it contains roles and tradeoffs with **poor** performance (<0.35 avg, ≥5 tasks).
2. Verify the signal is real: enough evaluations, consistent low scores (not just one bad run).
3. Check for dependencies: is this role/tradeoff used by a meta-agent (assigner/evaluator/evolver)?
4. Propose `retire_role` or `retire_motivation` for entities that are clearly underperforming.
5. This is a **conservative** operation — only retire when confident. When in doubt, leave it for mutation instead.
".to_string(),

        Strategy::MotivationTuning => "\
1. Read the data slice — it contains tradeoffs with sufficient evaluation data (≥2 tasks).
2. For each tradeoff, analyze which roles it pairs well with and which it doesn't.
3. Look for tradeoffs whose constraints are too tight (blocking good work) or too loose (not providing enough guidance).
4. Propose `modify_motivation` to adjust acceptable/unacceptable tradeoffs lists.
5. Consider creating new tradeoff variants for specific role pairings that consistently underperform.
".to_string(),

        Strategy::ComponentMutation => "\
1. Read the data slice — it contains roles that have components (skills) and evaluation data.
2. Analyze which components correlate with strong/weak dimensional scores.
3. Look for components that appear in low-scoring roles but not high-scoring ones (and vice versa).
4. Propose `component_substitution`, `config_add_component`, or `config_remove_component`.
5. Prefer swapping one component at a time to isolate the effect.
".to_string(),

        Strategy::Randomisation => "\
1. Read the data slice — it contains the full inventory of roles and tradeoffs.
2. Propose **random compositions**: new agents from existing role+tradeoff combinations not yet tried.
3. Use `random_compose_role` to create roles from random component subsets.
4. Use `random_compose_agent` to create agents from novel role+tradeoff pairings.
5. The goal is exploration — these compositions test unexplored regions of the design space.
".to_string(),

        Strategy::BizarreIdeation => "\
1. Read the data slice — it contains a curated context of top and bottom performers.
2. Generate **novel, unconventional** primitives that break existing patterns.
3. Use `bizarre_ideation` operations to create components, outcomes, or tradeoffs that are deliberately different.
4. Think laterally: what skills, constraints, or goals has the system never considered?
5. High creativity is more important than high confidence — the synthesizer will filter.
".to_string(),

        Strategy::CoordinatorEvolution => "\
1. Read the data slice — it contains coordinator-relevant evaluations and the full role/tradeoff inventory.
2. Analyze patterns in task decomposition quality, dependency accuracy, and description completeness.
3. Look for recurring issues: over-decomposition, missing edges, vague task descriptions, poor prioritization.
4. Propose `modify_coordinator_prompt` operations to update the mutable coordinator prompt files.
5. Can also propose `modify_role` or `create_role` for coordinator-specific role variants.
6. **Mutable files**: `evolved-amendments.md` (rules/heuristics), `common-patterns.md` (examples).
7. **Immutable files**: `base-system-prompt.md`, `behavioral-rules.md` — do NOT target these.
".to_string(),

        Strategy::All => "Analyze the data using all available strategies and propose the most impactful operations.\n".to_string(),
    }
}

fn strategy_allowed_operations(strategy: Strategy) -> String {
    match strategy {
        Strategy::Mutation => "\
- `wording_mutation` — Change a component or outcome description. Fields: `entity_type`, `target_id`, `new_description`, optionally `new_name`, `new_content`, `new_category`.
- `component_substitution` — Swap one component for another. Fields: `target_id` (role), `remove_component_id`, `add_component_id`.
- `config_add_component` — Add a component to a role. Fields: `target_id` (role), `add_component_id`.
- `config_remove_component` — Remove a component from a role. Fields: `target_id` (role), `remove_component_id`.
- `config_swap_outcome` — Change a role's outcome (deferred for human approval). Fields: `target_id` (role), `new_outcome_id`.
- `modify_role` — Create a derived role with updated skills/description. Fields: `target_id`, `name`, `description`, `component_ids`/`skills`, `outcome_id`/`desired_outcome`.
".to_string(),

        Strategy::Crossover => "\
- `modify_role` — Create a child role from two parents. Set `target_id` to `\"parent-a,parent-b\"`. Fields: `target_id`, `name`, `description`, `component_ids`/`skills`, `outcome_id`/`desired_outcome`.
".to_string(),

        Strategy::GapAnalysis => "\
- `create_role` — Create a brand new role. Fields: `new_id`, `name`, `description`, `component_ids`/`skills`, `outcome_id`/`desired_outcome`.
- `create_motivation` — Create a new tradeoff. Fields: `new_id`, `name`, `description`, `acceptable_tradeoffs`, `unacceptable_tradeoffs`.
".to_string(),

        Strategy::Retirement => "\
- `retire_role` — Retire a poorly-performing role. Fields: `target_id`.
- `retire_motivation` — Retire a poorly-performing tradeoff. Fields: `target_id`.
".to_string(),

        Strategy::MotivationTuning => "\
- `modify_motivation` — Adjust a tradeoff's constraints. Fields: `target_id`, `name`, `description`, `acceptable_tradeoffs`, `unacceptable_tradeoffs`.
- `create_motivation` — Create a new tradeoff variant. Fields: `new_id`, `name`, `description`, `acceptable_tradeoffs`, `unacceptable_tradeoffs`.
".to_string(),

        Strategy::ComponentMutation => "\
- `component_substitution` — Swap one component for another in a role. Fields: `target_id` (role), `remove_component_id`, `add_component_id`.
- `config_add_component` — Add a component to a role. Fields: `target_id` (role), `add_component_id`.
- `config_remove_component` — Remove a component from a role. Fields: `target_id` (role), `remove_component_id`.
- `wording_mutation` — Mutate a component's description. Fields: `entity_type: \"component\"`, `target_id`, `new_description`.
".to_string(),

        Strategy::Randomisation => "\
- `random_compose_role` — Create a role from random component subsets. Fields: `component_ids`, `outcome_id`, `selection_method`.
- `random_compose_agent` — Create an agent from a novel role+tradeoff pairing. Fields: `role_id`, `tradeoff_id`, `selection_method`.
".to_string(),

        Strategy::BizarreIdeation => "\
- `bizarre_ideation` — Create a novel primitive. Fields: `entity_type`, `new_name`, `new_description`, `ideation_prompt`, and type-specific fields (`new_content`, `new_category`, `new_success_criteria`, `new_acceptable_tradeoffs`, `new_unacceptable_tradeoffs`).
".to_string(),

        Strategy::CoordinatorEvolution => "\
- `modify_coordinator_prompt` — Modify a mutable coordinator prompt file. Fields: `target_id` (\"evolved-amendments\" or \"common-patterns\"), `new_content` (full file content).
- `modify_role` — Adjust the coordinator's role. Fields: `target_id`, `name`, `description`, `component_ids`/`skills`, `outcome_id`/`desired_outcome`.
- `create_role` — Create a specialized coordinator variant. Fields: `new_id`, `name`, `description`, `component_ids`/`skills`, `outcome_id`/`desired_outcome`.
".to_string(),

        Strategy::All => "\
All operation types are available. See the strategy skill documents for details.\n".to_string(),
    }
}

fn strategy_guardrails(strategy: Strategy) -> String {
    match strategy {
        Strategy::Mutation => "\
- Do not mutate roles with fewer than 3 evaluations.
- Prefer one mutation per role per run.
- Keep descriptions concise (under ~200 words).
- Outcome swaps (`config_swap_outcome`) are automatically deferred for human review.
".to_string(),

        Strategy::Crossover => "\
- Both parents must have at least 3 evaluations each.
- Do not cross roles that share >70% of their components (too similar).
- The child must have a distinct name and description — don't just concatenate parents.
".to_string(),

        Strategy::GapAnalysis => "\
- Only propose new entities, never modifications or retirements.
- New roles need valid component IDs and outcome IDs from the inventory.
- Don't create roles that overlap significantly with existing high performers.
".to_string(),

        Strategy::Retirement => "\
- Only retire entities with at least 5 evaluations and avg score below 0.35.
- Never retire an entity used by a meta-agent (assigner, evaluator, evolver) without flagging it.
- When in doubt, leave for mutation rather than retiring.
".to_string(),

        Strategy::MotivationTuning => "\
- Keep tradeoff lists focused — more than 5 items per list loses specificity.
- Don't remove all constraints — a tradeoff with no unacceptable items provides no guidance.
- Preserve the core intent of the tradeoff when tuning.
".to_string(),

        Strategy::ComponentMutation => "\
- Swap one component at a time to isolate effects.
- Verify the replacement component exists in the inventory.
- Don't leave a role with zero components.
".to_string(),

        Strategy::Randomisation => "\
- Cap at 3 random compositions per run to avoid flooding the system.
- Use `selection_method: \"performance_weighted_inverse\"` to bias toward underexplored areas.
- All referenced component, outcome, role, and tradeoff IDs must exist in the inventory.
".to_string(),

        Strategy::BizarreIdeation => "\
- High creativity is valued, but outputs must still be valid primitives.
- Include an `ideation_prompt` explaining the creative reasoning.
- Cap at 2-3 novel creations per run.
".to_string(),

        Strategy::CoordinatorEvolution => "\
- **Never** target `base-system-prompt.md` or `behavioral-rules.md` — they are immutable.
- Only target `evolved-amendments` or `common-patterns` as `target_id` for `modify_coordinator_prompt`.
- `new_content` must be the full file content (it replaces the entire file).
- Be conservative: small incremental improvements to coordinator behavior are better than sweeping changes.
".to_string(),

        Strategy::All => "Follow the guardrails specified in each strategy's skill document.\n".to_string(),
    }
}

fn strategy_specific_context(strategy: Strategy, agency_dir: &Path) -> String {
    match strategy {
        Strategy::CoordinatorEvolution => {
            let mut ctx = String::new();
            let prompt_dir = agency_dir.join("coordinator-prompt");
            if prompt_dir.is_dir() {
                let mutable_files = ["evolved-amendments.md", "common-patterns.md"];
                for filename in &mutable_files {
                    let path = prompt_dir.join(filename);
                    if let Ok(content) = fs::read_to_string(&path) {
                        let trimmed = content.trim();
                        if !trimmed.is_empty() {
                            ctx.push_str(&format!(
                                "**Current `{}`:**\n```\n{}\n```\n\n",
                                filename, trimmed
                            ));
                        } else {
                            ctx.push_str(&format!(
                                "**Current `{}`:** (empty — you can add initial content)\n\n",
                                filename
                            ));
                        }
                    } else {
                        ctx.push_str(&format!(
                            "**`{}`:** (file does not exist yet — you can create it)\n\n",
                            filename
                        ));
                    }
                }
            } else {
                ctx.push_str("Coordinator prompt directory does not exist yet. `modify_coordinator_prompt` operations will create it.\n\n");
            }
            ctx
        }
        _ => String::new(),
    }
}

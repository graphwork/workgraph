use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use workgraph::agency::{Evaluation, Role, TradeoffConfig};
use workgraph::config::Config;
use workgraph::graph::{CycleConfig, Node, Status, Task};
use workgraph::parser::{load_graph, modify_graph};

use super::partition::{self, AnalyzerSlice};
use super::prompt::{build_analyzer_prompt, load_evolver_skills};
use super::strategy::Strategy;

/// Fan-out threshold: below this eval count, use single-shot mode.
pub const FANOUT_THRESHOLD: usize = 50;

/// Convergence threshold for autopoietic cycle: if the overall score delta
/// falls below this value, the cycle terminates. 0.01 = 1% absolute change.
pub const CONVERGENCE_THRESHOLD: f64 = 0.01;

/// Run the fan-out evolution mode, creating a task graph of analyzers.
#[allow(clippy::too_many_arguments)]
pub fn run_fanout(
    dir: &Path,
    dry_run: bool,
    strategy: Option<&str>,
    budget: Option<u32>,
    _model: Option<&str>,
    json: bool,
    autopoietic: bool,
    max_iterations: Option<u32>,
    cycle_delay: Option<u64>,
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    evaluations: &[Evaluation],
    _config: &Config,
) -> Result<()> {
    let run_id = format!("run-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));

    // Determine which strategies to run
    let strategies_to_run: Vec<Strategy> = match strategy {
        Some(s) => vec![Strategy::from_str(s)?],
        None => Strategy::all_individual(),
    };

    // Create run directory under .workgraph/evolve-runs/
    let run_dir = dir.join(format!("evolve-runs/{}", run_id));
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("Failed to create run directory: {}", run_dir.display()))?;

    // Save run config
    let run_config = serde_json::json!({
        "run_id": run_id,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "strategy": strategy.unwrap_or("all"),
        "budget": budget,
        "autopoietic": autopoietic,
        "max_iterations": max_iterations,
        "cycle_delay": cycle_delay,
        "total_evaluations": evaluations.len(),
        "total_roles": roles.len(),
        "total_tradeoffs": tradeoffs.len(),
    });
    fs::write(
        run_dir.join("config.json"),
        serde_json::to_string_pretty(&run_config)?,
    )?;

    // Save pre-evolution snapshot
    save_snapshot(&run_dir, 0, roles, tradeoffs)?;

    // Partition evaluations
    let agency_dir = dir.join("agency");
    let all_slices =
        partition::partition_evaluations(evaluations, roles, tradeoffs, &agency_dir, &run_id);

    // Filter to requested strategies, skip empty slices (unless strategy needs no evals)
    let slices: Vec<(Strategy, AnalyzerSlice)> = all_slices
        .into_iter()
        .filter(|(s, _)| strategies_to_run.contains(s))
        .filter(|(s, slice)| !slice.evaluations.is_empty() || s.needs_no_evals())
        .collect();

    if slices.is_empty() {
        println!("No strategies have actionable data. Nothing to evolve.");
        return Ok(());
    }

    // Write slice data files
    for (_, slice) in &slices {
        let path = run_dir.join(format!("{}-slice.json", slice.strategy));
        fs::write(&path, serde_json::to_string_pretty(slice)?)?;
    }

    if dry_run {
        print_dry_run(
            &slices,
            &run_id,
            budget,
            autopoietic,
            max_iterations,
            cycle_delay,
            json,
            evaluations.len(),
        );
        // Clean up run dir since this is dry run
        let _ = fs::remove_dir_all(&run_dir);
        return Ok(());
    }

    // Create the task graph
    let graph_path = dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).context("Failed to load graph")?;

    // 1. Create partition task (marks the partitioning as done)
    let partition_task_id = format!(".evolve-partition-{}", run_id);
    let partition_description = if autopoietic {
        format!(
            "## Evolver Partition ({run_id})\n\n\
             ### Iteration 0 (Pre-completed)\n\
             Partitioned {n_evals} evaluations into {n_slices} strategy slices.\n\
             Pre-evolution snapshot: `.workgraph/evolve-runs/{run_id}/snapshot-iter-0.json`\n\n\
             ### On Re-Iteration\n\
             When this task re-opens after a cycle reset:\n\
             1. Read self-assessment from `.workgraph/evolve-runs/{run_id}/self-assessment-latest.json`\n\
             2. Load current agency data (roles, tradeoffs, evaluations from `.workgraph/agency/`)\n\
             3. Re-partition evaluations, prioritizing strategies the self-assessment identified as high-impact\n\
             4. Update slice files in `.workgraph/evolve-runs/{run_id}/` (`<strategy>-slice.json`)\n\
             5. Save new snapshot: `.workgraph/evolve-runs/{run_id}/snapshot-iter-<N>.json` (N = loop_iteration)\n\
             6. If the self-assessment recommends new strategies, create additional analyzer tasks with `wg add`\n\n\
             Run dir: .workgraph/evolve-runs/{run_id}",
            run_id = run_id,
            n_evals = evaluations.len(),
            n_slices = slices.len(),
        )
    } else {
        format!(
            "Partitioned {} evaluations into {} strategy slices.\nRun dir: .workgraph/evolve-runs/{}",
            evaluations.len(),
            slices.len(),
            run_id
        )
    };

    let partition_task = Task {
        id: partition_task_id.clone(),
        title: format!("Evolve partition ({})", run_id),
        description: Some(partition_description),
        status: Status::Done, // Already done — we just did the partitioning
        tags: vec!["evolution".into(), "partition".into()],
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Task::default()
    };
    graph.add_node(Node::Task(partition_task));

    // 2. Create analyzer tasks (depend on partition)
    let skills_dir = agency_dir.join("evolver-skills");
    let mut analyzer_task_ids = Vec::new();
    for (strategy, slice) in &slices {
        let task_id = format!(".evolve-analyze-{}-{}", strategy.label(), run_id);

        let skill_doc = match load_evolver_skills(&skills_dir, *strategy) {
            Ok(docs) if !docs.is_empty() => {
                let mut doc_text = String::new();
                for (name, content) in &docs {
                    doc_text.push_str(&format!("### {}\n{}\n\n", name, content));
                }
                doc_text
            }
            _ => "No skill document available for this strategy.".to_string(),
        };

        let model = slice.model_tier.label();

        let description =
            build_analyzer_prompt(*strategy, &run_id, &skill_doc, &slice.summary, &agency_dir);

        let analyzer_task = Task {
            id: task_id.clone(),
            title: format!("Evolve analyzer: {}", strategy.label()),
            description: Some(description),
            status: Status::Open,
            after: vec![partition_task_id.clone()],
            tags: vec!["evolution".into(), "analyzer".into()],
            model: Some(model.to_string()),
            ..Task::default()
        };
        graph.add_node(Node::Task(analyzer_task));

        // Update partition task's before list for bidirectional consistency
        if let Some(pt) = graph.get_task_mut(&partition_task_id)
            && !pt.before.contains(&task_id)
        {
            pt.before.push(task_id.clone());
        }

        analyzer_task_ids.push(task_id);
    }

    // 3. Create synthesize task (depends on all analyzers)
    let synthesize_task_id = format!(".evolve-synthesize-{}", run_id);
    let synthesize_description = format!(
        r#"## Evolver Synthesizer

Read all analyzer proposals from `.workgraph/evolve-runs/{run_id}/` and produce a unified operation set.

### Input Files
{input_files}

### Instructions
1. Read all `*-proposals.json` files from the run directory
2. Deduplicate operations targeting the same entity
3. Resolve conflicts (e.g., modify vs retire on same entity)
4. Score operations by confidence, signal strength, and expected impact
5. Apply budget limit: max {budget} operations
6. Write unified result

### Output
Write to `.workgraph/evolve-runs/{run_id}/synthesis-result.json`:

```json
{{
  "run_id": "{run_id}",
  "operations": [...],
  "conflicts_resolved": [...],
  "stats": {{
    "total_proposed": <N>,
    "total_accepted": <N>,
    "strategies_represented": [...]
  }}
}}
```

## Validation
- All proposal files are read
- Conflicts are documented
- Budget is respected"#,
        run_id = run_id,
        input_files = analyzer_task_ids
            .iter()
            .map(|id| format!(
                "- `{}`",
                id.replace(
                    &format!("-{}", run_id),
                    &format!("-proposals-{}.json", run_id)
                )
            ))
            .collect::<Vec<_>>()
            .join("\n"),
        budget = budget.map_or("unlimited".to_string(), |b| b.to_string()),
    );

    let synthesize_task = Task {
        id: synthesize_task_id.clone(),
        title: format!("Evolve synthesizer ({})", run_id),
        description: Some(synthesize_description),
        status: Status::Open,
        after: analyzer_task_ids.clone(),
        tags: vec!["evolution".into(), "synthesizer".into()],
        model: Some("sonnet".to_string()),
        ..Task::default()
    };
    graph.add_node(Node::Task(synthesize_task));

    // Update analyzer tasks' before lists
    for aid in &analyzer_task_ids {
        if let Some(at) = graph.get_task_mut(aid)
            && !at.before.contains(&synthesize_task_id)
        {
            at.before.push(synthesize_task_id.clone());
        }
    }

    // 4. Create apply task (depends on synthesize)
    let apply_task_id = format!(".evolve-apply-{}", run_id);
    let apply_description = format!(
        r#"## Evolver Apply

Apply the synthesized evolution operations.

### Input
Read from: `.workgraph/evolve-runs/{run_id}/synthesis-result.json`

### Instructions
1. Read the synthesis result
2. For each operation, call the appropriate apply function
3. Handle deferred operations (self-mutation safety)
4. Write results to `.workgraph/evolve-runs/{run_id}/apply-results.json`

## Validation
- All accepted operations are attempted
- Results are recorded
- Deferred operations are logged"#,
        run_id = run_id,
    );

    let apply_task = Task {
        id: apply_task_id.clone(),
        title: format!("Evolve apply ({})", run_id),
        description: Some(apply_description),
        status: Status::Open,
        after: vec![synthesize_task_id.clone()],
        tags: vec!["evolution".into(), "apply".into()],
        model: Some("sonnet".to_string()),
        ..Task::default()
    };
    graph.add_node(Node::Task(apply_task));

    if let Some(st) = graph.get_task_mut(&synthesize_task_id)
        && !st.before.contains(&apply_task_id)
    {
        st.before.push(apply_task_id.clone());
    }

    // 5. Create evaluate task (depends on apply)
    let evaluate_task_id = format!(".evolve-evaluate-{}", run_id);
    let evaluate_description = if autopoietic {
        format!(
            "## Evolver Evaluate ({run_id})\n\n\
             Evaluate the impact of this evolution iteration and determine convergence.\n\n\
             ### Input\n\
             - Pre-evolution snapshot: `.workgraph/evolve-runs/{run_id}/snapshot-iter-<N>.json` (N = loop_iteration)\n\
             - Apply results: `.workgraph/evolve-runs/{run_id}/apply-results.json`\n\
             - Current agency data: `.workgraph/agency/`\n\n\
             ### Instructions\n\
             1. Determine current iteration from `wg show {evaluate_id}` → `loop_iteration`\n\
             2. Load pre-iteration snapshot (`snapshot-iter-<loop_iteration>.json`)\n\
             3. Load current agency data and compute new performance scores\n\
             4. For each entity modified in this iteration:\n\
                - Compare pre/post evaluation scores\n\
                - Classify as improved, degraded, or neutral\n\
             5. Compute overall score delta (average absolute change across all modified entities)\n\
             6. Write self-assessment to `.workgraph/evolve-runs/{run_id}/self-assessment-latest.json`:\n\
                ```json\n\
                {{\n\
                  \"iteration\": <N>,\n\
                  \"overall_delta\": <float>,\n\
                  \"improved\": [\"entity-id-1\"],\n\
                  \"degraded\": [\"entity-id-2\"],\n\
                  \"neutral\": [\"entity-id-3\"],\n\
                  \"operations_applied\": <count>,\n\
                  \"recommendations\": [\"strategy-level guidance for next iteration\"]\n\
                }}\n\
                ```\n\
             7. Save post-iteration snapshot: `snapshot-iter-<N+1>.json`\n\n\
             ### Convergence\n\
             If the overall score delta is below {threshold} (less than {threshold_pct}% absolute change),\n\
             the evolution has converged. Signal convergence:\n\
             ```\n\
             wg done {evaluate_id} --converged\n\
             ```\n\
             Otherwise, mark done normally to continue the cycle:\n\
             ```\n\
             wg done {evaluate_id}\n\
             ```\n\n\
             ## Validation\n\
             - self-assessment written with all required fields\n\
             - Before/after comparison covers all modified entities\n\
             - Convergence decision is explicitly documented",
            run_id = run_id,
            evaluate_id = evaluate_task_id,
            threshold = CONVERGENCE_THRESHOLD,
            threshold_pct = (CONVERGENCE_THRESHOLD * 100.0) as u32,
        )
    } else {
        format!(
            r#"## Evolver Evaluate

Evaluate the results of the evolution run.

### Input
- Pre-evolution snapshot: `.workgraph/evolve-runs/{run_id}/snapshot-iter-0.json`
- Apply results: `.workgraph/evolve-runs/{run_id}/apply-results.json`

### Instructions
1. Compare pre-evolution performance snapshot with current state
2. Document which operations were applied vs skipped
3. Assess overall impact
4. Write evolution report to `.workgraph/evolve-runs/{run_id}/evolution-report.json`

## Validation
- Report covers all applied operations
- Before/after comparison is included"#,
            run_id = run_id,
        )
    };

    let evaluate_task = Task {
        id: evaluate_task_id.clone(),
        title: format!("Evolve evaluate ({})", run_id),
        description: Some(evaluate_description),
        status: Status::Open,
        after: vec![apply_task_id.clone()],
        tags: vec!["evolution".into(), "evaluate".into()],
        model: Some("sonnet".to_string()),
        ..Task::default()
    };
    graph.add_node(Node::Task(evaluate_task));

    if let Some(at) = graph.get_task_mut(&apply_task_id)
        && !at.before.contains(&evaluate_task_id)
    {
        at.before.push(evaluate_task_id.clone());
    }

    // 6. Wire cycle back-edge if autopoietic
    if autopoietic {
        let max_iter = max_iterations.unwrap_or(3);
        let delay_secs = cycle_delay.unwrap_or(3600);

        // Add back-edge: evaluate -> partition (creates cycle)
        if let Some(eval_task) = graph.get_task_mut(&evaluate_task_id) {
            eval_task.cycle_config = Some(CycleConfig {
                max_iterations: max_iter,
                guard: None,
                delay: if delay_secs > 0 {
                    Some(format!("{}s", delay_secs))
                } else {
                    None
                },
                no_converge: false,
                restart_on_failure: true,
                max_failure_restarts: None,
            });
            if !eval_task.after.contains(&partition_task_id) {
                eval_task.after.push(partition_task_id.clone());
            }
        }
        // Also add the back-edge reference on partition
        if let Some(pt) = graph.get_task_mut(&partition_task_id) {
            if !pt.after.contains(&evaluate_task_id) {
                pt.after.push(evaluate_task_id.clone());
            }
            if !pt.before.contains(&evaluate_task_id) {
                pt.before.push(evaluate_task_id.clone());
            }
        }
    }

    // Save graph atomically
    let graph_snapshot = graph;
    modify_graph(&graph_path, |existing_graph| {
        // Re-apply all nodes we created
        for node in graph_snapshot.nodes() {
            let nid = match node {
                workgraph::graph::Node::Task(t) => t.id.clone(),
                workgraph::graph::Node::Resource(r) => r.id.clone(),
            };
            if existing_graph.get_node(&nid).is_none() {
                existing_graph.add_node(node.clone());
            }
        }
        true
    })?;

    // Print summary
    if json {
        let out = serde_json::json!({
            "mode": "fanout",
            "run_id": run_id,
            "analyzers": analyzer_task_ids,
            "synthesizer": synthesize_task_id,
            "apply": apply_task_id,
            "evaluate": evaluate_task_id,
            "autopoietic": autopoietic,
            "slices": slices.iter().map(|(s, sl)| {
                serde_json::json!({
                    "strategy": s.label(),
                    "evaluations": sl.stats.evaluations_in_slice,
                    "roles": sl.stats.roles_in_slice,
                    "model": sl.model_tier.label(),
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("Evolution task graph created (run: {}):", run_id);
        println!("  Analyzers: {} tasks", analyzer_task_ids.len());
        for (strategy, slice) in &slices {
            println!(
                "    - {} ({} evals, {} roles, model: {})",
                strategy.label(),
                slice.stats.evaluations_in_slice,
                slice.stats.roles_in_slice,
                slice.model_tier.label(),
            );
        }
        println!("  Synthesizer: {}", synthesize_task_id);
        println!("  Apply: {}", apply_task_id);
        println!("  Evaluate: {}", evaluate_task_id);
        if autopoietic {
            println!(
                "  Cycle: {} iterations, {} second delay",
                max_iterations.unwrap_or(3),
                cycle_delay.unwrap_or(3600)
            );
        }
    }

    Ok(())
}

fn save_snapshot(
    run_dir: &Path,
    iteration: u32,
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
) -> Result<()> {
    let mut role_scores = serde_json::Map::new();
    for role in roles {
        role_scores.insert(
            role.id.clone(),
            serde_json::json!({
                "avg_score": role.performance.avg_score,
                "task_count": role.performance.task_count,
            }),
        );
    }

    let mut tradeoff_scores = serde_json::Map::new();
    for tradeoff in tradeoffs {
        tradeoff_scores.insert(
            tradeoff.id.clone(),
            serde_json::json!({
                "avg_score": tradeoff.performance.avg_score,
                "task_count": tradeoff.performance.task_count,
            }),
        );
    }

    let overall_avg = {
        let scores: Vec<f64> = roles
            .iter()
            .filter_map(|r| r.performance.avg_score)
            .collect();
        if scores.is_empty() {
            0.0
        } else {
            scores.iter().sum::<f64>() / scores.len() as f64
        }
    };

    let snapshot = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "iteration": iteration,
        "roles": role_scores,
        "tradeoffs": tradeoff_scores,
        "overall_avg": overall_avg,
    });

    let path = run_dir.join(format!("snapshot-iter-{}.json", iteration));
    fs::write(&path, serde_json::to_string_pretty(&snapshot)?)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_dry_run(
    slices: &[(Strategy, AnalyzerSlice)],
    run_id: &str,
    budget: Option<u32>,
    autopoietic: bool,
    max_iterations: Option<u32>,
    cycle_delay: Option<u64>,
    json: bool,
    total_evals: usize,
) {
    if json {
        let slice_json: Vec<serde_json::Value> = slices
            .iter()
            .map(|(s, sl)| {
                serde_json::json!({
                    "strategy": s.label(),
                    "evaluations": sl.stats.evaluations_in_slice,
                    "roles": sl.stats.roles_in_slice,
                    "model": sl.model_tier.label(),
                    "truncated": sl.stats.truncated,
                })
            })
            .collect();
        let analyzer_ids: Vec<String> = slices
            .iter()
            .map(|(s, _)| format!(".evolve-analyze-{}-{}", s.label(), run_id))
            .collect();
        let task_graph = serde_json::json!({
            "partition": format!(".evolve-partition-{}", run_id),
            "analyzers": analyzer_ids,
            "synthesize": format!(".evolve-synthesize-{}", run_id),
            "apply": format!(".evolve-apply-{}", run_id),
            "evaluate": format!(".evolve-evaluate-{}", run_id),
        });
        let out = serde_json::json!({
            "mode": "dry_run_fanout",
            "run_id": run_id,
            "strategies": slices.len(),
            "total_evaluations": total_evals,
            "autopoietic": autopoietic,
            "budget": budget,
            "slices": slice_json,
            "task_graph": task_graph,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return;
    }

    println!("=== Dry Run: wg evolve (fan-out mode) ===\n");
    println!("Run ID:          {}", run_id);
    println!("Strategies:      {}", slices.len());
    println!("Total evals:     {}", total_evals);
    println!(
        "Budget:          {}",
        budget
            .map(|b| b.to_string())
            .unwrap_or_else(|| "unlimited".into())
    );
    println!(
        "Autopoietic:     {}",
        if autopoietic { "yes" } else { "no" }
    );
    if autopoietic {
        println!("Max iterations:  {}", max_iterations.unwrap_or(3));
        println!("Cycle delay:     {}s", cycle_delay.unwrap_or(3600));
    }

    println!("\nStrategy Slices:");
    for (strategy, slice) in slices {
        let eval_info = if slice.stats.evaluations_in_slice == 0 {
            if strategy.needs_no_evals() {
                match strategy {
                    Strategy::GapAnalysis => "0 evals (summary)".to_string(),
                    Strategy::Randomisation => "0 evals (inventory)".to_string(),
                    Strategy::BizarreIdeation => "0 evals (context)".to_string(),
                    _ => format!("{} evals", slice.stats.evaluations_in_slice),
                }
            } else {
                format!("{} evals", slice.stats.evaluations_in_slice)
            }
        } else {
            format!(
                "{} evals, {} roles",
                slice.stats.evaluations_in_slice, slice.stats.roles_in_slice
            )
        };
        println!(
            "  {:<22} {:>30}   (model: {})",
            format!("{}:", strategy.label()),
            eval_info,
            slice.model_tier.label(),
        );
    }

    println!("\nTask graph:");
    let partition = format!(".evolve-partition-{}", run_id);
    println!("  {}", partition);
    for (i, (strategy, _)) in slices.iter().enumerate() {
        let prefix = if i == slices.len() - 1 {
            "└──"
        } else {
            "├──"
        };
        println!(
            "    {} .evolve-analyze-{}-{}",
            prefix,
            strategy.label(),
            run_id
        );
    }
    println!("         └── .evolve-synthesize-{}", run_id);
    println!("              └── .evolve-apply-{}", run_id);
    println!("                   └── .evolve-evaluate-{}", run_id);
    if autopoietic {
        println!("                        └── [cycle back to {}]", partition);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use workgraph::agency::{AccessControl, Lineage, PerformanceRecord};
    use workgraph::graph::WorkGraph;
    use workgraph::parser::save_graph;

    fn setup_test_env() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();
        let graph_path = wg_dir.join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &graph_path).unwrap();
        (tmp, wg_dir)
    }

    fn make_role(id: &str, avg_score: Option<f64>, task_count: u32) -> Role {
        Role {
            id: id.to_string(),
            name: format!("Role {}", id),
            description: "test role".to_string(),
            component_ids: vec!["comp1".to_string()],
            outcome_id: "outcome1".to_string(),
            performance: PerformanceRecord {
                task_count,
                avg_score,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        }
    }

    fn make_tradeoff(id: &str, avg_score: Option<f64>, task_count: u32) -> TradeoffConfig {
        TradeoffConfig {
            id: id.to_string(),
            name: format!("Tradeoff {}", id),
            description: "test tradeoff".to_string(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord {
                task_count,
                avg_score,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            access_control: AccessControl::default(),
            domain_tags: vec![],
            metadata: HashMap::new(),
            former_agents: vec![],
            former_deployments: vec![],
        }
    }

    fn make_eval(id: &str, role_id: &str, tradeoff_id: &str, score: f64) -> Evaluation {
        Evaluation {
            id: id.to_string(),
            task_id: format!("task-{}", id),
            agent_id: String::new(),
            role_id: role_id.to_string(),
            tradeoff_id: tradeoff_id.to_string(),
            score,
            dimensions: HashMap::new(),
            notes: String::new(),
            evaluator: "test".to_string(),
            timestamp: "2026-03-13T12:00:00Z".to_string(),
            model: None,
            source: "llm".to_string(),
            loop_iteration: 0,
        }
    }

    #[test]
    fn test_fanout_dry_run_creates_no_tasks() {
        let (_tmp, wg_dir) = setup_test_env();
        let agency_dir = wg_dir.join("agency");
        fs::create_dir_all(agency_dir.join("cache/roles")).unwrap();
        fs::create_dir_all(agency_dir.join("primitives/tradeoffs")).unwrap();
        fs::create_dir_all(agency_dir.join("evaluations")).unwrap();
        fs::create_dir_all(agency_dir.join("evolver-skills")).unwrap();

        let roles = vec![make_role("r1", Some(0.5), 5)];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.5), 5)];
        let mut evals = Vec::new();
        for i in 0..60 {
            evals.push(make_eval(&format!("e{}", i), "r1", "t1", 0.5));
        }

        let config = Config::load_or_default(&wg_dir);
        let result = run_fanout(
            &wg_dir, true, // dry_run
            None, None, None, false, false, None, None, &roles, &tradeoffs, &evals, &config,
        );
        assert!(result.is_ok());

        // Graph should be unchanged
        let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();
        assert_eq!(graph.tasks().count(), 0);
    }

    #[test]
    fn test_fanout_creates_task_graph() {
        let (_tmp, wg_dir) = setup_test_env();
        let agency_dir = wg_dir.join("agency");
        fs::create_dir_all(agency_dir.join("cache/roles")).unwrap();
        fs::create_dir_all(agency_dir.join("primitives/tradeoffs")).unwrap();
        fs::create_dir_all(agency_dir.join("evaluations")).unwrap();
        fs::create_dir_all(agency_dir.join("evolver-skills")).unwrap();

        let roles = vec![make_role("r1", Some(0.5), 5)];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.5), 5)];
        let mut evals = Vec::new();
        for i in 0..60 {
            evals.push(make_eval(&format!("e{}", i), "r1", "t1", 0.5));
        }

        let config = Config::load_or_default(&wg_dir);
        let result = run_fanout(
            &wg_dir, false, None, None, None, false, false, None, None, &roles, &tradeoffs, &evals,
            &config,
        );
        assert!(result.is_ok());

        let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();
        // Should have: partition + analyzers + synthesize + apply + evaluate
        let task_count = graph.tasks().count();
        // At minimum: partition + some analyzers + synthesize + apply + evaluate
        assert!(
            task_count >= 5,
            "Expected at least 5 tasks, got {}",
            task_count
        );

        // Verify synthesize depends on analyzers
        let synthesize = graph
            .tasks()
            .find(|t| t.id.contains("evolve-synthesize"))
            .expect("synthesize task should exist");
        assert!(!synthesize.after.is_empty());
    }

    #[test]
    fn test_evolver_cycle_structure() {
        let (_tmp, wg_dir) = setup_test_env();
        let agency_dir = wg_dir.join("agency");
        fs::create_dir_all(agency_dir.join("cache/roles")).unwrap();
        fs::create_dir_all(agency_dir.join("primitives/tradeoffs")).unwrap();
        fs::create_dir_all(agency_dir.join("evaluations")).unwrap();
        fs::create_dir_all(agency_dir.join("evolver-skills")).unwrap();

        let roles = vec![make_role("r1", Some(0.5), 5)];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.5), 5)];
        let mut evals = Vec::new();
        for i in 0..60 {
            evals.push(make_eval(&format!("e{}", i), "r1", "t1", 0.5));
        }

        let config = Config::load_or_default(&wg_dir);
        let result = run_fanout(
            &wg_dir,
            false,
            None,
            Some(10),
            None,
            false,
            true,      // autopoietic = true
            Some(5),   // max_iterations
            Some(600), // cycle_delay
            &roles,
            &tradeoffs,
            &evals,
            &config,
        );
        assert!(result.is_ok(), "run_fanout failed: {:?}", result.err());

        let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

        // Should have: partition + analyzers + synthesize + apply + evaluate
        let task_count = graph.tasks().count();
        assert!(
            task_count >= 5,
            "Expected at least 5 tasks, got {}",
            task_count
        );

        // Find key tasks
        let partition = graph
            .tasks()
            .find(|t| t.id.contains("evolve-partition"))
            .expect("partition task should exist");
        let synthesize = graph
            .tasks()
            .find(|t| t.id.contains("evolve-synthesize"))
            .expect("synthesize task should exist");
        let apply = graph
            .tasks()
            .find(|t| t.id.contains("evolve-apply"))
            .expect("apply task should exist");
        let evaluate = graph
            .tasks()
            .find(|t| t.id.contains("evolve-evaluate"))
            .expect("evaluate task should exist");
        let analyzers: Vec<_> = graph
            .tasks()
            .filter(|t| t.id.contains("evolve-analyze"))
            .collect();
        assert!(!analyzers.is_empty(), "Should have at least one analyzer");

        // 1. Partition is Done for iteration 0 (pre-computed)
        assert_eq!(
            partition.status,
            Status::Done,
            "Partition should be Done for iteration 0"
        );

        // 2. All other tasks are Open
        assert_eq!(synthesize.status, Status::Open);
        assert_eq!(apply.status, Status::Open);
        assert_eq!(evaluate.status, Status::Open);
        for a in &analyzers {
            assert_eq!(a.status, Status::Open, "Analyzer {} should be Open", a.id);
        }

        // 3. Dependency chain: analyzers → partition
        for a in &analyzers {
            assert!(
                a.after.contains(&partition.id),
                "Analyzer {} should depend on partition",
                a.id
            );
        }

        // 4. Synthesize depends on all analyzers
        for a in &analyzers {
            assert!(
                synthesize.after.contains(&a.id),
                "Synthesize should depend on analyzer {}",
                a.id
            );
        }

        // 5. Apply depends on synthesize
        assert!(apply.after.contains(&synthesize.id));

        // 6. Evaluate depends on apply
        assert!(evaluate.after.contains(&apply.id));

        // 7. Cycle config on evaluate
        let cycle_config = evaluate
            .cycle_config
            .as_ref()
            .expect("Evaluate should have CycleConfig");
        assert_eq!(cycle_config.max_iterations, 5);
        assert_eq!(cycle_config.delay, Some("600s".to_string()));
        assert!(cycle_config.restart_on_failure);

        // 8. Back-edge: evaluate depends on partition (creates cycle)
        assert!(
            evaluate.after.contains(&partition.id),
            "Evaluate should have back-edge to partition"
        );

        // 9. Back-edge: partition depends on evaluate (bidirectional cycle)
        assert!(
            partition.after.contains(&evaluate.id),
            "Partition should have back-edge to evaluate"
        );

        // 10. Evaluate description references self-assessment and convergence
        let eval_desc = evaluate.description.as_ref().unwrap();
        assert!(
            eval_desc.contains("self-assessment"),
            "Evaluate should reference self-assessment"
        );
        assert!(
            eval_desc.contains("converg"),
            "Evaluate should mention convergence"
        );
        assert!(
            eval_desc.contains("score delta") || eval_desc.contains("overall_delta"),
            "Evaluate should reference score delta for convergence"
        );

        // 11. Partition description references self-assessment feedback
        let part_desc = partition.description.as_ref().unwrap();
        assert!(
            part_desc.contains("self-assessment"),
            "Partition should reference self-assessment for feedback loop"
        );

        // 12. Partition description covers re-iteration behavior
        assert!(
            part_desc.contains("Re-Iteration") || part_desc.contains("re-partition"),
            "Partition should describe re-iteration behavior"
        );

        // 13. Evaluate description includes convergence threshold
        assert!(
            eval_desc.contains("--converged"),
            "Evaluate should instruct agent to use --converged"
        );

        // 14. Evaluate description includes the threshold value
        assert!(
            eval_desc.contains(&CONVERGENCE_THRESHOLD.to_string()),
            "Evaluate should include the convergence threshold value"
        );
    }

    #[test]
    fn test_evolver_cycle_non_autopoietic_no_cycle() {
        // Non-autopoietic mode should NOT create cycle config or self-assessment
        let (_tmp, wg_dir) = setup_test_env();
        let agency_dir = wg_dir.join("agency");
        fs::create_dir_all(agency_dir.join("cache/roles")).unwrap();
        fs::create_dir_all(agency_dir.join("primitives/tradeoffs")).unwrap();
        fs::create_dir_all(agency_dir.join("evaluations")).unwrap();
        fs::create_dir_all(agency_dir.join("evolver-skills")).unwrap();

        let roles = vec![make_role("r1", Some(0.5), 5)];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.5), 5)];
        let mut evals = Vec::new();
        for i in 0..60 {
            evals.push(make_eval(&format!("e{}", i), "r1", "t1", 0.5));
        }

        let config = Config::load_or_default(&wg_dir);
        run_fanout(
            &wg_dir, false, None, None, None, false, false, // autopoietic = false
            None, None, &roles, &tradeoffs, &evals, &config,
        )
        .unwrap();

        let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

        let evaluate = graph
            .tasks()
            .find(|t| t.id.contains("evolve-evaluate"))
            .unwrap();

        // No CycleConfig in non-autopoietic mode
        assert!(
            evaluate.cycle_config.is_none(),
            "Non-autopoietic evaluate should have no CycleConfig"
        );

        // Evaluate description should NOT reference self-assessment
        let eval_desc = evaluate.description.as_ref().unwrap();
        assert!(
            !eval_desc.contains("self-assessment"),
            "Non-autopoietic evaluate should not reference self-assessment"
        );

        // Partition description should NOT reference self-assessment
        let partition = graph
            .tasks()
            .find(|t| t.id.contains("evolve-partition"))
            .unwrap();
        let part_desc = partition.description.as_ref().unwrap();
        assert!(
            !part_desc.contains("self-assessment"),
            "Non-autopoietic partition should not reference self-assessment"
        );
    }

    #[test]
    fn test_evolver_cycle_default_iterations() {
        // When max_iterations and cycle_delay are not specified, defaults should be used
        let (_tmp, wg_dir) = setup_test_env();
        let agency_dir = wg_dir.join("agency");
        fs::create_dir_all(agency_dir.join("cache/roles")).unwrap();
        fs::create_dir_all(agency_dir.join("primitives/tradeoffs")).unwrap();
        fs::create_dir_all(agency_dir.join("evaluations")).unwrap();
        fs::create_dir_all(agency_dir.join("evolver-skills")).unwrap();

        let roles = vec![make_role("r1", Some(0.5), 5)];
        let tradeoffs = vec![make_tradeoff("t1", Some(0.5), 5)];
        let mut evals = Vec::new();
        for i in 0..60 {
            evals.push(make_eval(&format!("e{}", i), "r1", "t1", 0.5));
        }

        let config = Config::load_or_default(&wg_dir);
        run_fanout(
            &wg_dir, false, None, None, None, false, true, // autopoietic
            None, // default max_iterations
            None, // default cycle_delay
            &roles, &tradeoffs, &evals, &config,
        )
        .unwrap();

        let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();

        let evaluate = graph
            .tasks()
            .find(|t| t.id.contains("evolve-evaluate"))
            .unwrap();

        let cycle_config = evaluate.cycle_config.as_ref().unwrap();
        assert_eq!(
            cycle_config.max_iterations, 3,
            "Default max_iterations should be 3"
        );
        assert_eq!(
            cycle_config.delay,
            Some("3600s".to_string()),
            "Default cycle_delay should be 3600s"
        );
    }
}

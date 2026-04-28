use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use workgraph::agency::{self, Evaluation, Role, TradeoffConfig};
use workgraph::parser::load_graph;

#[derive(Clone)]
#[allow(dead_code)]
struct OutputConfig {
    min_evals: u32,
    by_model: bool,
    by_task_type: bool,
}

/// A (role_id, tradeoff_id) pair used as a key in the synergy matrix.
type Pair = (String, String);

/// Canonical task types for performance breakdown.
const TASK_TYPES: &[&str] = &[
    "research",
    "implementation",
    "fix",
    "design",
    "test",
    "docs",
    "refactor",
    "other",
];

/// Per-entity aggregated stats.
struct EntityStats {
    id: String,
    name: String,
    task_count: u32,
    avg_score: Option<f64>,
    /// Recent scores for trend computation (oldest first).
    recent_scores: Vec<f64>,
}

/// Synergy cell: stats for a specific (role, tradeoff) pair.
struct SynergyCell {
    role_id: String,
    tradeoff_id: String,
    count: u32,
    avg_score: f64,
}

/// Tag breakdown cell: stats for (entity_id, tag).
struct TagCell {
    entity_id: String,
    tag: String,
    count: u32,
    avg_score: f64,
}

/// Per-model aggregated stats.
struct ModelStats {
    model: String,
    count: u32,
    avg_score: f64,
    scores: Vec<f64>,
}

/// Stats for a (task_type, entity) pair.
struct TaskTypeCell {
    task_type: String,
    entity_id: String,
    count: u32,
    avg_score: f64,
}

/// Compute a simple trend indicator from recent scores.
/// Returns "up", "down", "flat", or "-" if insufficient data.
fn trend(scores: &[f64]) -> &'static str {
    if scores.len() < 2 {
        return "-";
    }
    let mid = scores.len() / 2;
    let first_half: f64 = scores[..mid].iter().sum::<f64>() / mid as f64;
    let second_half: f64 = scores[mid..].iter().sum::<f64>() / (scores.len() - mid) as f64;
    let diff = second_half - first_half;
    if diff > 0.03 {
        "up"
    } else if diff < -0.03 {
        "down"
    } else {
        "flat"
    }
}

/// Classify a task into a canonical type based on its title.
///
/// Uses title prefix (e.g., "Research: ...", "Fix: ...") or falls back to
/// keyword matching in the title. System tasks (evaluate, flip, place, assign)
/// are excluded.
fn classify_task_type(title: &str) -> Option<&'static str> {
    let lower = title.to_lowercase();

    // Skip system tasks
    if lower.starts_with("evaluate:")
        || lower.starts_with("flip:")
        || lower.starts_with("place:")
        || lower.starts_with(".evaluate-")
        || lower.starts_with(".flip-")
        || lower.starts_with(".place-")
        || lower.starts_with(".assign-")
        || lower.starts_with(".compact-")
        || lower.starts_with(".quality-pass-")
    {
        return None;
    }

    // Check title prefix first (most reliable)
    if lower.starts_with("research:")
        || lower.starts_with("investigate:")
        || lower.starts_with("audit:")
        || lower.starts_with("analyze:")
        || lower.starts_with("explore:")
    {
        return Some("research");
    }
    if lower.starts_with("implement:")
        || lower.starts_with("wire:")
        || lower.starts_with("add:")
        || lower.starts_with("create:")
        || lower.starts_with("build:")
    {
        return Some("implementation");
    }
    if lower.starts_with("fix:") || lower.starts_with("hotfix:") || lower.starts_with("bugfix:") {
        return Some("fix");
    }
    if lower.starts_with("design:") || lower.starts_with("architect:") || lower.starts_with("plan:")
    {
        return Some("design");
    }
    if lower.starts_with("test:") || lower.starts_with("validate:") {
        return Some("test");
    }
    if lower.starts_with("docs:") || lower.starts_with("document:") || lower.starts_with("doc:") {
        return Some("docs");
    }
    if lower.starts_with("refactor:")
        || lower.starts_with("restructure:")
        || lower.starts_with("cleanup:")
    {
        return Some("refactor");
    }

    // Fallback: classify as "other" for user tasks without a clear type prefix
    Some("other")
}

/// Run `wg agency stats [--json] [--min-evals N] [--by-model] [--by-task-type]`.
pub fn run(
    dir: &Path,
    json: bool,
    min_evals: u32,
    by_model: bool,
    by_task_type: bool,
) -> Result<()> {
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("cache/roles");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let evals_dir = agency_dir.join("evaluations");

    let roles = agency::load_all_roles(&roles_dir).context("Failed to load roles")?;
    let tradeoffs =
        agency::load_all_tradeoffs(&tradeoffs_dir).context("Failed to load tradeoffs")?;
    let evaluations =
        agency::load_all_evaluations(&evals_dir).context("Failed to load evaluations")?;

    // Try to load graph for tag-based and task-type-based breakdowns (non-fatal if missing)
    let graph_path = super::graph_path(dir);
    let (task_tags, task_titles): (HashMap<String, Vec<String>>, HashMap<String, String>) =
        if graph_path.exists() {
            match load_graph(&graph_path) {
                Ok(graph) => {
                    let tags = graph
                        .tasks()
                        .map(|t| (t.id.clone(), t.tags.clone()))
                        .collect();
                    let titles = graph
                        .tasks()
                        .map(|t| (t.id.clone(), t.title.clone()))
                        .collect();
                    (tags, titles)
                }
                Err(_) => (HashMap::new(), HashMap::new()),
            }
        } else {
            (HashMap::new(), HashMap::new())
        };

    // Build task_id -> task_type map
    let task_types: HashMap<String, &str> = task_titles
        .iter()
        .filter_map(|(id, title)| classify_task_type(title).map(|tt| (id.clone(), tt)))
        .collect();

    if json {
        output_json(
            &roles,
            &tradeoffs,
            &evaluations,
            &task_tags,
            &task_types,
            min_evals,
            by_model,
            by_task_type,
        )
    } else {
        output_text(
            &roles,
            &tradeoffs,
            &evaluations,
            &task_tags,
            &task_types,
            min_evals,
            by_model,
            by_task_type,
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Computation helpers
// ---------------------------------------------------------------------------

fn build_role_stats(roles: &[Role]) -> Vec<EntityStats> {
    roles
        .iter()
        .map(|r| {
            let mut scores: Vec<f64> = r.performance.evaluations.iter().map(|e| e.score).collect();
            scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            EntityStats {
                id: r.id.clone(),
                name: r.name.clone(),
                task_count: r.performance.task_count,
                avg_score: r.performance.avg_score,
                recent_scores: r.performance.evaluations.iter().map(|e| e.score).collect(),
            }
        })
        .collect()
}

fn build_tradeoff_stats(tradeoffs: &[TradeoffConfig]) -> Vec<EntityStats> {
    tradeoffs
        .iter()
        .map(|m| EntityStats {
            id: m.id.clone(),
            name: m.name.clone(),
            task_count: m.performance.task_count,
            avg_score: m.performance.avg_score,
            recent_scores: m.performance.evaluations.iter().map(|e| e.score).collect(),
        })
        .collect()
}

fn build_synergy_matrix(evaluations: &[Evaluation]) -> Vec<SynergyCell> {
    let mut map: HashMap<Pair, Vec<f64>> = HashMap::new();
    for eval in evaluations {
        map.entry((eval.role_id.clone(), eval.tradeoff_id.clone()))
            .or_default()
            .push(eval.score);
    }
    let mut cells: Vec<SynergyCell> = map
        .into_iter()
        .map(|((role_id, tradeoff_id), scores)| {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            SynergyCell {
                role_id,
                tradeoff_id,
                count: scores.len() as u32,
                avg_score: avg,
            }
        })
        .collect();
    cells.sort_by(|a, b| {
        b.avg_score
            .partial_cmp(&a.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    cells
}

fn build_tag_breakdown(
    evaluations: &[Evaluation],
    task_tags: &HashMap<String, Vec<String>>,
    by_role: bool,
) -> Vec<TagCell> {
    // Group evaluations by (entity_id, tag)
    let mut map: HashMap<(String, String), Vec<f64>> = HashMap::new();
    for eval in evaluations {
        let entity_id = if by_role {
            &eval.role_id
        } else {
            &eval.tradeoff_id
        };
        if let Some(tags) = task_tags.get(&eval.task_id) {
            for tag in tags {
                map.entry((entity_id.clone(), tag.clone()))
                    .or_default()
                    .push(eval.score);
            }
        }
    }
    let mut cells: Vec<TagCell> = map
        .into_iter()
        .map(|((entity_id, tag), scores)| {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            TagCell {
                entity_id,
                tag,
                count: scores.len() as u32,
                avg_score: avg,
            }
        })
        .collect();
    cells.sort_by(|a, b| a.entity_id.cmp(&b.entity_id).then(a.tag.cmp(&b.tag)));
    cells
}

fn build_model_stats(evaluations: &[Evaluation]) -> Vec<ModelStats> {
    let mut map: HashMap<String, Vec<f64>> = HashMap::new();
    for eval in evaluations {
        let model_key = eval.model.as_deref().unwrap_or("(unknown)").to_string();
        map.entry(model_key).or_default().push(eval.score);
    }
    let mut stats: Vec<ModelStats> = map
        .into_iter()
        .map(|(model, scores)| {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            ModelStats {
                model,
                count: scores.len() as u32,
                avg_score: avg,
                scores,
            }
        })
        .collect();
    stats.sort_by(|a, b| {
        b.avg_score
            .partial_cmp(&a.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    stats
}

/// Build a breakdown of scores by (task_type, role_id).
fn build_task_type_role_breakdown(
    evaluations: &[Evaluation],
    task_types: &HashMap<String, &str>,
) -> Vec<TaskTypeCell> {
    let mut map: HashMap<(String, String), Vec<f64>> = HashMap::new();
    for eval in evaluations {
        if let Some(&task_type) = task_types.get(&eval.task_id) {
            map.entry((task_type.to_string(), eval.role_id.clone()))
                .or_default()
                .push(eval.score);
        }
    }
    let mut cells: Vec<TaskTypeCell> = map
        .into_iter()
        .map(|((task_type, entity_id), scores)| {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            TaskTypeCell {
                task_type,
                entity_id,
                count: scores.len() as u32,
                avg_score: avg,
            }
        })
        .collect();
    cells.sort_by(|a, b| {
        a.task_type.cmp(&b.task_type).then(
            b.avg_score
                .partial_cmp(&a.avg_score)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    cells
}

/// Build a breakdown of scores by (task_type, model).
fn build_task_type_model_breakdown(
    evaluations: &[Evaluation],
    task_types: &HashMap<String, &str>,
) -> Vec<TaskTypeCell> {
    let mut map: HashMap<(String, String), Vec<f64>> = HashMap::new();
    for eval in evaluations {
        if let Some(&task_type) = task_types.get(&eval.task_id) {
            let model_key = eval.model.as_deref().unwrap_or("(unknown)").to_string();
            map.entry((task_type.to_string(), model_key))
                .or_default()
                .push(eval.score);
        }
    }
    let mut cells: Vec<TaskTypeCell> = map
        .into_iter()
        .map(|((task_type, entity_id), scores)| {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            TaskTypeCell {
                task_type,
                entity_id,
                count: scores.len() as u32,
                avg_score: avg,
            }
        })
        .collect();
    cells.sort_by(|a, b| {
        a.task_type.cmp(&b.task_type).then(
            b.avg_score
                .partial_cmp(&a.avg_score)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    cells
}

fn find_underexplored(
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    evaluations: &[Evaluation],
    min_evals: u32,
) -> Vec<(String, String, u32)> {
    // Count evaluations per (role, tradeoff) pair
    let mut counts: HashMap<Pair, u32> = HashMap::new();
    for eval in evaluations {
        *counts
            .entry((eval.role_id.clone(), eval.tradeoff_id.clone()))
            .or_insert(0) += 1;
    }

    let mut under: Vec<(String, String, u32)> = Vec::new();
    for role in roles {
        for tradeoff in tradeoffs {
            let count = counts
                .get(&(role.id.clone(), tradeoff.id.clone()))
                .copied()
                .unwrap_or(0);
            if count < min_evals {
                under.push((role.id.clone(), tradeoff.id.clone(), count));
            }
        }
    }
    under.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    under
}

// ---------------------------------------------------------------------------
// Text output
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn output_text(
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    evaluations: &[Evaluation],
    task_tags: &HashMap<String, Vec<String>>,
    task_types: &HashMap<String, &str>,
    min_evals: u32,
    by_model: bool,
    by_task_type: bool,
) {
    // 1. Overall stats
    let total_roles = roles.len();
    let total_tradeoffs = tradeoffs.len();
    let total_evaluations = evaluations.len();
    let overall_avg = if evaluations.is_empty() {
        None
    } else {
        Some(evaluations.iter().map(|e| e.score).sum::<f64>() / evaluations.len() as f64)
    };

    println!("=== Agency Performance Stats ===\n");
    println!("  Roles:        {}", total_roles);
    println!("  TradeoffConfigs:  {}", total_tradeoffs);
    println!("  Evaluations:  {}", total_evaluations);
    println!(
        "  Avg score:    {}",
        overall_avg
            .map(|s| format!("{:.2}", s))
            .unwrap_or_else(|| "-".to_string())
    );

    if evaluations.is_empty() {
        println!("\nNo evaluations recorded yet. Run 'wg evaluate <task-id>' to generate data.");
        return;
    }

    // 2. Role leaderboard
    let mut role_stats = build_role_stats(roles);
    role_stats.sort_by(|a, b| {
        b.avg_score
            .partial_cmp(&a.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("\n--- Role Leaderboard ---\n");
    println!(
        "  {:<20} {:>8} {:>6} {:>6}",
        "Role", "Avg", "Tasks", "Trend"
    );
    println!("  {}", "-".repeat(44));
    for s in &role_stats {
        let avg_str = s
            .avg_score
            .map(|v| format!("{:.2}", v))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<20} {:>8} {:>6} {:>6}",
            agency::short_hash(&s.id),
            avg_str,
            s.task_count,
            trend(&s.recent_scores),
        );
    }

    // 3. TradeoffConfig leaderboard
    let mut mot_stats = build_tradeoff_stats(tradeoffs);
    mot_stats.sort_by(|a, b| {
        b.avg_score
            .partial_cmp(&a.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("\n--- TradeoffConfig Leaderboard ---\n");
    println!(
        "  {:<20} {:>8} {:>6} {:>6}",
        "TradeoffConfig", "Avg", "Tasks", "Trend"
    );
    println!("  {}", "-".repeat(44));
    for s in &mot_stats {
        let avg_str = s
            .avg_score
            .map(|v| format!("{:.2}", v))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<20} {:>8} {:>6} {:>6}",
            agency::short_hash(&s.id),
            avg_str,
            s.task_count,
            trend(&s.recent_scores),
        );
    }

    // 4. Synergy matrix
    let synergy = build_synergy_matrix(evaluations);
    if !synergy.is_empty() {
        println!("\n--- Synergy Matrix (Role x TradeoffConfig) ---\n");
        println!(
            "  {:<20} {:<20} {:>8} {:>6} {:>8}",
            "Role", "TradeoffConfig", "Avg", "Count", "Rating"
        );
        println!("  {}", "-".repeat(66));
        for cell in &synergy {
            let rating = if cell.avg_score >= 0.8 {
                "HIGH"
            } else if cell.avg_score <= 0.4 {
                "LOW"
            } else {
                ""
            };
            println!(
                "  {:<20} {:<20} {:>8.2} {:>6} {:>8}",
                agency::short_hash(&cell.role_id),
                agency::short_hash(&cell.tradeoff_id),
                cell.avg_score,
                cell.count,
                rating,
            );
        }
    }

    // 5. Tag breakdown (only if we have tags)
    let role_tags = build_tag_breakdown(evaluations, task_tags, true);
    if !role_tags.is_empty() {
        println!("\n--- Score by Role x Tag ---\n");
        println!("  {:<20} {:<20} {:>8} {:>6}", "Role", "Tag", "Avg", "Count");
        println!("  {}", "-".repeat(58));
        for cell in &role_tags {
            println!(
                "  {:<20} {:<20} {:>8.2} {:>6}",
                agency::short_hash(&cell.entity_id),
                cell.tag,
                cell.avg_score,
                cell.count,
            );
        }
    }

    let mot_tags = build_tag_breakdown(evaluations, task_tags, false);
    if !mot_tags.is_empty() {
        println!("\n--- Score by TradeoffConfig x Tag ---\n");
        println!(
            "  {:<20} {:<20} {:>8} {:>6}",
            "TradeoffConfig", "Tag", "Avg", "Count"
        );
        println!("  {}", "-".repeat(58));
        for cell in &mot_tags {
            println!(
                "  {:<20} {:<20} {:>8.2} {:>6}",
                agency::short_hash(&cell.entity_id),
                cell.tag,
                cell.avg_score,
                cell.count,
            );
        }
    }

    // 6. Under-explored combinations
    let under = find_underexplored(roles, tradeoffs, evaluations, min_evals);
    if !under.is_empty() {
        println!(
            "\n--- Under-explored Combinations (< {} evals) ---\n",
            min_evals
        );
        println!("  {:<20} {:<20} {:>6}", "Role", "TradeoffConfig", "Evals");
        println!("  {}", "-".repeat(50));
        for (role_id, mot_id, count) in &under {
            println!(
                "  {:<20} {:<20} {:>6}",
                agency::short_hash(role_id),
                agency::short_hash(mot_id),
                count
            );
        }
    }

    // 7. Model leaderboard (if --by-model)
    if by_model {
        let model_stats = build_model_stats(evaluations);
        println!("\n--- Model Leaderboard ---\n");
        println!(
            "  {:<40} {:>8} {:>6} {:>6}",
            "Model", "Avg", "Evals", "Trend"
        );
        println!("  {}", "-".repeat(64));
        for s in &model_stats {
            println!(
                "  {:<40} {:>8.2} {:>6} {:>6}",
                s.model,
                s.avg_score,
                s.count,
                trend(&s.scores),
            );
        }
    }

    // 8. Task type breakdowns (if --by-task-type)
    if by_task_type {
        let role_by_type = build_task_type_role_breakdown(evaluations, task_types);
        if !role_by_type.is_empty() {
            println!("\n--- Best Role by Task Type ---\n");
            println!(
                "  {:<16} {:<20} {:>8} {:>6}",
                "Task Type", "Role", "Avg", "Evals"
            );
            println!("  {}", "-".repeat(54));
            let mut last_type = String::new();
            for cell in &role_by_type {
                if cell.task_type != last_type {
                    if !last_type.is_empty() {
                        println!();
                    }
                    last_type.clone_from(&cell.task_type);
                }
                println!(
                    "  {:<16} {:<20} {:>8.2} {:>6}",
                    cell.task_type,
                    agency::short_hash(&cell.entity_id),
                    cell.avg_score,
                    cell.count,
                );
            }
        }

        let model_by_type = build_task_type_model_breakdown(evaluations, task_types);
        if !model_by_type.is_empty() {
            println!("\n--- Best Model by Task Type ---\n");
            println!(
                "  {:<16} {:<40} {:>8} {:>6}",
                "Task Type", "Model", "Avg", "Evals"
            );
            println!("  {}", "-".repeat(74));
            let mut last_type = String::new();
            for cell in &model_by_type {
                if cell.task_type != last_type {
                    if !last_type.is_empty() {
                        println!();
                    }
                    last_type.clone_from(&cell.task_type);
                }
                println!(
                    "  {:<16} {:<40} {:>8.2} {:>6}",
                    cell.task_type, cell.entity_id, cell.avg_score, cell.count,
                );
            }
        }

        // Summary: best pick per type
        println!("\n--- Recommendations by Task Type ---\n");
        println!(
            "  {:<16} {:<20} {:<40}",
            "Task Type", "Best Role", "Best Model"
        );
        println!("  {}", "-".repeat(78));
        for &tt in TASK_TYPES {
            let best_role = role_by_type
                .iter()
                .filter(|c| c.task_type == tt && c.count >= 2)
                .max_by(|a, b| {
                    a.avg_score
                        .partial_cmp(&b.avg_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            let best_model = model_by_type
                .iter()
                .filter(|c| c.task_type == tt && c.count >= 2)
                .max_by(|a, b| {
                    a.avg_score
                        .partial_cmp(&b.avg_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

            let role_str = best_role
                .map(|c| format!("{} ({:.2})", agency::short_hash(&c.entity_id), c.avg_score))
                .unwrap_or_else(|| "(insufficient data)".to_string());
            let model_str = best_model
                .map(|c| format!("{} ({:.2})", c.entity_id, c.avg_score))
                .unwrap_or_else(|| "(insufficient data)".to_string());

            println!("  {:<16} {:<20} {:<40}", tt, role_str, model_str);
        }
    }
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn output_json(
    roles: &[Role],
    tradeoffs: &[TradeoffConfig],
    evaluations: &[Evaluation],
    task_tags: &HashMap<String, Vec<String>>,
    task_types: &HashMap<String, &str>,
    min_evals: u32,
    by_model: bool,
    by_task_type: bool,
) -> Result<()> {
    let total_evaluations = evaluations.len();
    let overall_avg = if evaluations.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(
            evaluations.iter().map(|e| e.score).sum::<f64>() / total_evaluations as f64
        )
    };

    // Role leaderboard
    let mut role_stats = build_role_stats(roles);
    role_stats.sort_by(|a, b| {
        b.avg_score
            .partial_cmp(&a.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let role_board: Vec<serde_json::Value> = role_stats
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "name": s.name,
                "avg_score": s.avg_score,
                "task_count": s.task_count,
                "trend": trend(&s.recent_scores),
            })
        })
        .collect();

    // TradeoffConfig leaderboard
    let mut mot_stats = build_tradeoff_stats(tradeoffs);
    mot_stats.sort_by(|a, b| {
        b.avg_score
            .partial_cmp(&a.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mot_board: Vec<serde_json::Value> = mot_stats
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "name": s.name,
                "avg_score": s.avg_score,
                "task_count": s.task_count,
                "trend": trend(&s.recent_scores),
            })
        })
        .collect();

    // Synergy matrix
    let synergy = build_synergy_matrix(evaluations);
    let synergy_json: Vec<serde_json::Value> = synergy
        .iter()
        .map(|c| {
            let rating = if c.avg_score >= 0.8 {
                "high"
            } else if c.avg_score <= 0.4 {
                "low"
            } else {
                "medium"
            };
            serde_json::json!({
                "role_id": c.role_id,
                "tradeoff_id": c.tradeoff_id,
                "avg_score": c.avg_score,
                "count": c.count,
                "rating": rating,
            })
        })
        .collect();

    // Tag breakdowns
    let role_tags = build_tag_breakdown(evaluations, task_tags, true);
    let role_tags_json: Vec<serde_json::Value> = role_tags
        .iter()
        .map(|c| {
            serde_json::json!({
                "role_id": c.entity_id,
                "tag": c.tag,
                "avg_score": c.avg_score,
                "count": c.count,
            })
        })
        .collect();

    let mot_tags = build_tag_breakdown(evaluations, task_tags, false);
    let mot_tags_json: Vec<serde_json::Value> = mot_tags
        .iter()
        .map(|c| {
            serde_json::json!({
                "tradeoff_id": c.entity_id,
                "tag": c.tag,
                "avg_score": c.avg_score,
                "count": c.count,
            })
        })
        .collect();

    // Under-explored
    let under = find_underexplored(roles, tradeoffs, evaluations, min_evals);
    let under_json: Vec<serde_json::Value> = under
        .iter()
        .map(|(r, m, c)| {
            serde_json::json!({
                "role_id": r,
                "tradeoff_id": m,
                "eval_count": c,
            })
        })
        .collect();

    let mut output = serde_json::json!({
        "overview": {
            "total_roles": roles.len(),
            "total_tradeoffs": tradeoffs.len(),
            "total_evaluations": total_evaluations,
            "avg_score": overall_avg,
        },
        "role_leaderboard": role_board,
        "tradeoff_leaderboard": mot_board,
        "synergy_matrix": synergy_json,
        "tag_breakdown": {
            "by_role": role_tags_json,
            "by_tradeoff": mot_tags_json,
        },
        "underexplored": under_json,
    });

    if by_model {
        let model_stats = build_model_stats(evaluations);
        let model_board: Vec<serde_json::Value> = model_stats
            .iter()
            .map(|s| {
                serde_json::json!({
                    "model": s.model,
                    "avg_score": s.avg_score,
                    "eval_count": s.count,
                    "trend": trend(&s.scores),
                })
            })
            .collect();
        output["model_leaderboard"] = serde_json::json!(model_board);
    }

    if by_task_type {
        let role_by_type = build_task_type_role_breakdown(evaluations, task_types);
        let role_by_type_json: Vec<serde_json::Value> = role_by_type
            .iter()
            .map(|c| {
                serde_json::json!({
                    "task_type": c.task_type,
                    "role_id": c.entity_id,
                    "avg_score": c.avg_score,
                    "count": c.count,
                })
            })
            .collect();

        let model_by_type = build_task_type_model_breakdown(evaluations, task_types);
        let model_by_type_json: Vec<serde_json::Value> = model_by_type
            .iter()
            .map(|c| {
                serde_json::json!({
                    "task_type": c.task_type,
                    "model": c.entity_id,
                    "avg_score": c.avg_score,
                    "count": c.count,
                })
            })
            .collect();

        // Build recommendations: best role + model per type
        let mut recommendations: Vec<serde_json::Value> = Vec::new();
        for &tt in TASK_TYPES {
            let best_role = role_by_type
                .iter()
                .filter(|c| c.task_type == tt && c.count >= 2)
                .max_by(|a, b| {
                    a.avg_score
                        .partial_cmp(&b.avg_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            let best_model = model_by_type
                .iter()
                .filter(|c| c.task_type == tt && c.count >= 2)
                .max_by(|a, b| {
                    a.avg_score
                        .partial_cmp(&b.avg_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

            recommendations.push(serde_json::json!({
                "task_type": tt,
                "best_role": best_role.map(|c| serde_json::json!({
                    "role_id": c.entity_id,
                    "avg_score": c.avg_score,
                    "count": c.count,
                })),
                "best_model": best_model.map(|c| serde_json::json!({
                    "model": c.entity_id,
                    "avg_score": c.avg_score,
                    "count": c.count,
                })),
            }));
        }

        output["task_type_breakdown"] = serde_json::json!({
            "by_role": role_by_type_json,
            "by_model": model_by_type_json,
            "recommendations": recommendations,
        });
    }

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trend_insufficient_data() {
        assert_eq!(trend(&[]), "-");
        assert_eq!(trend(&[0.5]), "-");
    }

    #[test]
    fn test_trend_up() {
        assert_eq!(trend(&[0.3, 0.4, 0.7, 0.8]), "up");
    }

    #[test]
    fn test_trend_down() {
        assert_eq!(trend(&[0.8, 0.7, 0.3, 0.2]), "down");
    }

    #[test]
    fn test_trend_flat() {
        assert_eq!(trend(&[0.5, 0.5, 0.5, 0.5]), "flat");
    }

    #[test]
    fn test_build_synergy_matrix() {
        let evals = vec![
            Evaluation {
                id: "e1".into(),
                task_id: "t1".into(),
                agent_id: String::new(),
                role_id: "r1".into(),
                tradeoff_id: "m1".into(),
                score: 0.8,
                dimensions: HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: "2025-01-01T00:00:00Z".into(),
                model: None,
                source: "llm".to_string(),
                loop_iteration: 0,
            },
            Evaluation {
                id: "e2".into(),
                task_id: "t2".into(),
                agent_id: String::new(),
                role_id: "r1".into(),
                tradeoff_id: "m1".into(),
                score: 0.6,
                dimensions: HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: "2025-01-02T00:00:00Z".into(),
                model: None,
                source: "llm".to_string(),
                loop_iteration: 0,
            },
        ];

        let cells = build_synergy_matrix(&evals);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].role_id, "r1");
        assert_eq!(cells[0].tradeoff_id, "m1");
        assert_eq!(cells[0].count, 2);
        assert!((cells[0].avg_score - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_find_underexplored() {
        use workgraph::agency::{Lineage, PerformanceRecord};

        let roles = vec![Role {
            id: "r1".into(),
            name: "Role 1".into(),
            description: String::new(),
            component_ids: vec![],
            outcome_id: String::new(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            default_context_scope: None,
            default_exec_mode: None,
        }];
        let tradeoffs = vec![TradeoffConfig {
            id: "m1".into(),
            name: "Mot 1".into(),
            description: String::new(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            access_control: workgraph::agency::AccessControl::default(),
            domain_tags: vec![],
            metadata: HashMap::new(),
            former_agents: vec![],
            former_deployments: vec![],
        }];

        let under = find_underexplored(&roles, &tradeoffs, &[], 3);
        assert_eq!(under.len(), 1);
        assert_eq!(under[0], ("r1".to_string(), "m1".to_string(), 0));
    }

    #[test]
    fn test_build_tag_breakdown() {
        let evals = vec![Evaluation {
            id: "e1".into(),
            task_id: "t1".into(),
            agent_id: String::new(),
            role_id: "r1".into(),
            tradeoff_id: "m1".into(),
            score: 0.9,
            dimensions: HashMap::new(),
            notes: String::new(),
            evaluator: "test".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
            loop_iteration: 0,
        }];
        let mut tags = HashMap::new();
        tags.insert("t1".to_string(), vec!["cli".to_string()]);

        let cells = build_tag_breakdown(&evals, &tags, true);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].entity_id, "r1");
        assert_eq!(cells[0].tag, "cli");
        assert!((cells[0].avg_score - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn test_classify_task_type_prefixes() {
        assert_eq!(
            classify_task_type("Research: JWT libraries"),
            Some("research")
        );
        assert_eq!(
            classify_task_type("Investigate: auth bug"),
            Some("research")
        );
        assert_eq!(
            classify_task_type("Implement: JWT auth"),
            Some("implementation")
        );
        assert_eq!(
            classify_task_type("Add: new endpoint"),
            Some("implementation")
        );
        assert_eq!(
            classify_task_type("Wire: evaluation feedback"),
            Some("implementation")
        );
        assert_eq!(classify_task_type("Fix: crash on startup"), Some("fix"));
        assert_eq!(classify_task_type("Hotfix: null pointer"), Some("fix"));
        assert_eq!(classify_task_type("Design: API schema"), Some("design"));
        assert_eq!(classify_task_type("Test: auth middleware"), Some("test"));
        assert_eq!(classify_task_type("Docs: update README"), Some("docs"));
        assert_eq!(
            classify_task_type("Refactor: extract helper"),
            Some("refactor")
        );
    }

    #[test]
    fn test_classify_task_type_system_tasks() {
        assert_eq!(classify_task_type("Evaluate: task-123"), None);
        assert_eq!(classify_task_type("FLIP: task-123"), None);
        assert_eq!(classify_task_type(".evaluate-task-123"), None);
        assert_eq!(classify_task_type(".flip-task-123"), None);
        assert_eq!(classify_task_type(".place-task-123"), None);
        assert_eq!(classify_task_type(".assign-task-123"), None);
        assert_eq!(classify_task_type(".compact-0"), None);
        assert_eq!(classify_task_type(".quality-pass-20260402"), None);
    }

    #[test]
    fn test_classify_task_type_no_prefix() {
        assert_eq!(classify_task_type("Some random task"), Some("other"));
        assert_eq!(classify_task_type("add-jwt-auth"), Some("other"));
    }

    #[test]
    fn test_build_task_type_role_breakdown() {
        let evals = vec![
            Evaluation {
                id: "e1".into(),
                task_id: "t1".into(),
                agent_id: String::new(),
                role_id: "r1".into(),
                tradeoff_id: "m1".into(),
                score: 0.9,
                dimensions: HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: "2025-01-01T00:00:00Z".into(),
                model: Some("opus".into()),
                source: "llm".to_string(),
                loop_iteration: 0,
            },
            Evaluation {
                id: "e2".into(),
                task_id: "t2".into(),
                agent_id: String::new(),
                role_id: "r2".into(),
                tradeoff_id: "m1".into(),
                score: 0.7,
                dimensions: HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: "2025-01-02T00:00:00Z".into(),
                model: Some("sonnet".into()),
                source: "llm".to_string(),
                loop_iteration: 0,
            },
        ];

        let mut task_types = HashMap::new();
        task_types.insert("t1".to_string(), "fix");
        task_types.insert("t2".to_string(), "fix");

        let cells = build_task_type_role_breakdown(&evals, &task_types);
        assert_eq!(cells.len(), 2);
        // Sorted by task_type then by avg_score descending
        assert_eq!(cells[0].task_type, "fix");
        assert_eq!(cells[0].entity_id, "r1");
        assert!((cells[0].avg_score - 0.9).abs() < f64::EPSILON);
        assert_eq!(cells[1].entity_id, "r2");
        assert!((cells[1].avg_score - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_build_task_type_model_breakdown() {
        let evals = vec![
            Evaluation {
                id: "e1".into(),
                task_id: "t1".into(),
                agent_id: String::new(),
                role_id: "r1".into(),
                tradeoff_id: "m1".into(),
                score: 0.9,
                dimensions: HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: "2025-01-01T00:00:00Z".into(),
                model: Some("opus".into()),
                source: "llm".to_string(),
                loop_iteration: 0,
            },
            Evaluation {
                id: "e2".into(),
                task_id: "t1".into(),
                agent_id: String::new(),
                role_id: "r1".into(),
                tradeoff_id: "m1".into(),
                score: 0.8,
                dimensions: HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: "2025-01-02T00:00:00Z".into(),
                model: Some("sonnet".into()),
                source: "llm".to_string(),
                loop_iteration: 0,
            },
        ];

        let mut task_types = HashMap::new();
        task_types.insert("t1".to_string(), "research");

        let cells = build_task_type_model_breakdown(&evals, &task_types);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].task_type, "research");
        // opus (0.9) should be first, then sonnet (0.8)
        assert_eq!(cells[0].entity_id, "opus");
        assert!((cells[0].avg_score - 0.9).abs() < f64::EPSILON);
        assert_eq!(cells[1].entity_id, "sonnet");
        assert!((cells[1].avg_score - 0.8).abs() < f64::EPSILON);
    }
}

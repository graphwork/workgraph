use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use workgraph::agency::{self, Evaluation, Motivation, Role};
use workgraph::parser::load_graph;

/// A (role_id, motivation_id) pair used as a key in the synergy matrix.
type Pair = (String, String);

/// Per-entity aggregated stats.
struct EntityStats {
    id: String,
    name: String,
    task_count: u32,
    avg_score: Option<f64>,
    /// Recent scores for trend computation (oldest first).
    recent_scores: Vec<f64>,
}

/// Synergy cell: stats for a specific (role, motivation) pair.
struct SynergyCell {
    role_id: String,
    motivation_id: String,
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

/// Run `wg agency stats [--json] [--min-evals N] [--by-model]`.
pub fn run(dir: &Path, json: bool, min_evals: u32, by_model: bool) -> Result<()> {
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");
    let evals_dir = agency_dir.join("evaluations");

    let roles = agency::load_all_roles(&roles_dir).context("Failed to load roles")?;
    let motivations =
        agency::load_all_motivations(&motivations_dir).context("Failed to load motivations")?;
    let evaluations =
        agency::load_all_evaluations(&evals_dir).context("Failed to load evaluations")?;

    // Try to load graph for tag-based breakdown (non-fatal if missing)
    let graph_path = super::graph_path(dir);
    let task_tags: HashMap<String, Vec<String>> = if graph_path.exists() {
        match load_graph(&graph_path) {
            Ok(graph) => graph
                .tasks()
                .map(|t| (t.id.clone(), t.tags.clone()))
                .collect(),
            Err(_) => HashMap::new(),
        }
    } else {
        HashMap::new()
    };

    if json {
        output_json(
            &roles,
            &motivations,
            &evaluations,
            &task_tags,
            min_evals,
            by_model,
        )
    } else {
        output_text(
            &roles,
            &motivations,
            &evaluations,
            &task_tags,
            min_evals,
            by_model,
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

fn build_motivation_stats(motivations: &[Motivation]) -> Vec<EntityStats> {
    motivations
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
        map.entry((eval.role_id.clone(), eval.motivation_id.clone()))
            .or_default()
            .push(eval.score);
    }
    let mut cells: Vec<SynergyCell> = map
        .into_iter()
        .map(|((role_id, motivation_id), scores)| {
            let avg = scores.iter().sum::<f64>() / scores.len() as f64;
            SynergyCell {
                role_id,
                motivation_id,
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
            &eval.motivation_id
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
        let model_key = eval
            .model
            .as_deref()
            .unwrap_or("(unknown)")
            .to_string();
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

fn find_underexplored(
    roles: &[Role],
    motivations: &[Motivation],
    evaluations: &[Evaluation],
    min_evals: u32,
) -> Vec<(String, String, u32)> {
    // Count evaluations per (role, motivation) pair
    let mut counts: HashMap<Pair, u32> = HashMap::new();
    for eval in evaluations {
        *counts
            .entry((eval.role_id.clone(), eval.motivation_id.clone()))
            .or_insert(0) += 1;
    }

    let mut under: Vec<(String, String, u32)> = Vec::new();
    for role in roles {
        for motivation in motivations {
            let count = counts
                .get(&(role.id.clone(), motivation.id.clone()))
                .copied()
                .unwrap_or(0);
            if count < min_evals {
                under.push((role.id.clone(), motivation.id.clone(), count));
            }
        }
    }
    under.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    under
}

// ---------------------------------------------------------------------------
// Text output
// ---------------------------------------------------------------------------

fn output_text(
    roles: &[Role],
    motivations: &[Motivation],
    evaluations: &[Evaluation],
    task_tags: &HashMap<String, Vec<String>>,
    min_evals: u32,
    by_model: bool,
) {
    // 1. Overall stats
    let total_roles = roles.len();
    let total_motivations = motivations.len();
    let total_evaluations = evaluations.len();
    let overall_avg = if evaluations.is_empty() {
        None
    } else {
        Some(evaluations.iter().map(|e| e.score).sum::<f64>() / evaluations.len() as f64)
    };

    println!("=== Agency Performance Stats ===\n");
    println!("  Roles:        {}", total_roles);
    println!("  Motivations:  {}", total_motivations);
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

    // 3. Motivation leaderboard
    let mut mot_stats = build_motivation_stats(motivations);
    mot_stats.sort_by(|a, b| {
        b.avg_score
            .partial_cmp(&a.avg_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("\n--- Motivation Leaderboard ---\n");
    println!(
        "  {:<20} {:>8} {:>6} {:>6}",
        "Motivation", "Avg", "Tasks", "Trend"
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
        println!("\n--- Synergy Matrix (Role x Motivation) ---\n");
        println!(
            "  {:<20} {:<20} {:>8} {:>6} {:>8}",
            "Role", "Motivation", "Avg", "Count", "Rating"
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
                agency::short_hash(&cell.motivation_id),
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
        println!("\n--- Score by Motivation x Tag ---\n");
        println!(
            "  {:<20} {:<20} {:>8} {:>6}",
            "Motivation", "Tag", "Avg", "Count"
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
    let under = find_underexplored(roles, motivations, evaluations, min_evals);
    if !under.is_empty() {
        println!(
            "\n--- Under-explored Combinations (< {} evals) ---\n",
            min_evals
        );
        println!("  {:<20} {:<20} {:>6}", "Role", "Motivation", "Evals");
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
                s.model, s.avg_score, s.count, trend(&s.scores),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

fn output_json(
    roles: &[Role],
    motivations: &[Motivation],
    evaluations: &[Evaluation],
    task_tags: &HashMap<String, Vec<String>>,
    min_evals: u32,
    by_model: bool,
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

    // Motivation leaderboard
    let mut mot_stats = build_motivation_stats(motivations);
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
                "motivation_id": c.motivation_id,
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
                "motivation_id": c.entity_id,
                "tag": c.tag,
                "avg_score": c.avg_score,
                "count": c.count,
            })
        })
        .collect();

    // Under-explored
    let under = find_underexplored(roles, motivations, evaluations, min_evals);
    let under_json: Vec<serde_json::Value> = under
        .iter()
        .map(|(r, m, c)| {
            serde_json::json!({
                "role_id": r,
                "motivation_id": m,
                "eval_count": c,
            })
        })
        .collect();

    let output = serde_json::json!({
        "overview": {
            "total_roles": roles.len(),
            "total_motivations": motivations.len(),
            "total_evaluations": total_evaluations,
            "avg_score": overall_avg,
        },
        "role_leaderboard": role_board,
        "motivation_leaderboard": mot_board,
        "synergy_matrix": synergy_json,
        "tag_breakdown": {
            "by_role": role_tags_json,
            "by_motivation": mot_tags_json,
        },
        "underexplored": under_json,
    });

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
                motivation_id: "m1".into(),
                score: 0.8,
                dimensions: HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: "2025-01-01T00:00:00Z".into(),
            },
            Evaluation {
                id: "e2".into(),
                task_id: "t2".into(),
                agent_id: String::new(),
                role_id: "r1".into(),
                motivation_id: "m1".into(),
                score: 0.6,
                dimensions: HashMap::new(),
                notes: String::new(),
                evaluator: "test".into(),
                timestamp: "2025-01-02T00:00:00Z".into(),
            },
        ];

        let cells = build_synergy_matrix(&evals);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].role_id, "r1");
        assert_eq!(cells[0].motivation_id, "m1");
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
            skills: vec![],
            desired_outcome: String::new(),
            performance: PerformanceRecord {
                task_count: 0,
                avg_score: None,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
        }];
        let motivations = vec![Motivation {
            id: "m1".into(),
            name: "Mot 1".into(),
            description: String::new(),
            acceptable_tradeoffs: vec![],
            unacceptable_tradeoffs: vec![],
            performance: PerformanceRecord {
                task_count: 0,
                avg_score: None,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
        }];

        let under = find_underexplored(&roles, &motivations, &[], 3);
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
            motivation_id: "m1".into(),
            score: 0.9,
            dimensions: HashMap::new(),
            notes: String::new(),
            evaluator: "test".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
        }];
        let mut tags = HashMap::new();
        tags.insert("t1".to_string(), vec!["cli".to_string()]);

        let cells = build_tag_breakdown(&evals, &tags, true);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].entity_id, "r1");
        assert_eq!(cells[0].tag, "cli");
        assert!((cells[0].avg_score - 0.9).abs() < f64::EPSILON);
    }
}

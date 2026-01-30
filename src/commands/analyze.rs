use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::check::check_all;
use workgraph::graph::{Status, WorkGraph};
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;

use super::graph_path;

// Re-use cycle classification from loops module
use super::loops::{ClassifiedCycle, CycleClassification};

/// Severity level for issues
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Warning,
    Critical,
}

/// Summary statistics
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub total_tasks: usize,
    pub open: usize,
    pub in_progress: usize,
    pub done: usize,
    pub blocked: usize,
    pub ready: usize,
    pub estimated_hours: f64,
    pub estimated_cost: f64,
}

/// A structural health issue
#[derive(Debug, Clone, Serialize)]
pub struct StructuralIssue {
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<String>>,
}

/// Structural health section
#[derive(Debug, Clone, Serialize)]
pub struct StructuralHealth {
    pub issues: Vec<StructuralIssue>,
}

/// Information about a bottleneck
#[derive(Debug, Clone, Serialize)]
pub struct BottleneckInfo {
    pub id: String,
    pub transitive_blocks: usize,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned: Option<String>,
    pub severity: Severity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_in_progress: Option<i64>,
}

/// Information about an actor's workload
#[derive(Debug, Clone, Serialize)]
pub struct WorkloadInfo {
    pub id: String,
    pub load_percent: Option<f64>,
    pub is_overloaded: bool,
}

/// Workload section
#[derive(Debug, Clone, Serialize)]
pub struct WorkloadSection {
    pub total_actors: usize,
    pub balanced_actors: usize,
    pub overloaded: Vec<WorkloadInfo>,
}

/// Aging issue
#[derive(Debug, Clone, Serialize)]
pub struct AgingIssue {
    pub task_id: String,
    pub days: i64,
    pub issue_type: String, // "old_open" or "stale_in_progress"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned: Option<String>,
}

/// Aging section
#[derive(Debug, Clone, Serialize)]
pub struct AgingSection {
    pub old_open_count: usize,
    pub stale_in_progress_count: usize,
    pub issues: Vec<AgingIssue>,
}

/// A recommendation
#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    pub priority: usize,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub reason: String,
}

/// Complete analysis output
#[derive(Debug, Clone, Serialize)]
pub struct AnalysisOutput {
    pub summary: Summary,
    pub structural: StructuralHealth,
    pub bottlenecks: Vec<BottleneckInfo>,
    pub workload: WorkloadSection,
    pub aging: AgingSection,
    pub recommendations: Vec<Recommendation>,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let now = Utc::now();

    // Gather all analysis data
    let summary = compute_summary(&graph);
    let structural = compute_structural_health(&graph);
    let bottlenecks = compute_bottlenecks(&graph, &now);
    let workload = compute_workload(&graph);
    let aging = compute_aging(&graph, &now);
    let recommendations = generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);

    let output = AnalysisOutput {
        summary,
        structural,
        bottlenecks,
        workload,
        aging,
        recommendations,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_human_readable(&output);
    }

    Ok(())
}

/// Compute summary statistics
fn compute_summary(graph: &WorkGraph) -> Summary {
    let ready = ready_tasks(graph);
    let ready_ids: HashSet<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    let mut open = 0;
    let mut in_progress = 0;
    let mut done = 0;
    let mut blocked = 0;
    let mut estimated_hours = 0.0;
    let mut estimated_cost = 0.0;

    for task in graph.tasks() {
        match task.status {
            Status::Open => {
                open += 1;
                if !ready_ids.contains(task.id.as_str()) {
                    blocked += 1;
                }
                // Add estimates for remaining work
                if let Some(ref est) = task.estimate {
                    estimated_hours += est.hours.unwrap_or(0.0);
                    estimated_cost += est.cost.unwrap_or(0.0);
                }
            }
            Status::InProgress => {
                in_progress += 1;
                // Include in-progress tasks in remaining estimates
                if let Some(ref est) = task.estimate {
                    estimated_hours += est.hours.unwrap_or(0.0);
                    estimated_cost += est.cost.unwrap_or(0.0);
                }
            }
            Status::Done => done += 1,
            Status::Blocked => {
                blocked += 1;
                if let Some(ref est) = task.estimate {
                    estimated_hours += est.hours.unwrap_or(0.0);
                    estimated_cost += est.cost.unwrap_or(0.0);
                }
            }
            Status::Failed | Status::Abandoned => {
                // Failed/abandoned tasks not counted in progress metrics
            }
        }
    }

    Summary {
        total_tasks: graph.tasks().count(),
        open,
        in_progress,
        done,
        blocked,
        ready: ready.len(),
        estimated_hours,
        estimated_cost,
    }
}

/// Classify a cycle (duplicated from loops.rs to avoid complex imports)
fn classify_cycle(cycle: &[String], graph: &WorkGraph) -> ClassifiedCycle {
    let len = cycle.len();

    // Check if any task in the cycle has recurring or cycle:intentional tag
    let has_intentional_tag = cycle.iter().any(|node_id| {
        if let Some(task) = graph.get_task(node_id) {
            task.tags
                .iter()
                .any(|tag| tag == "recurring" || tag == "cycle:intentional")
        } else {
            false
        }
    });

    let (classification, reason) = if has_intentional_tag {
        (
            CycleClassification::Intentional,
            "has 'recurring' or 'cycle:intentional' tag".to_string(),
        )
    } else if len <= 2 {
        (
            CycleClassification::Warning,
            "Short cycle without recurrence tag".to_string(),
        )
    } else if len >= 5 {
        (
            CycleClassification::Warning,
            format!("Long cycle ({} nodes) likely unintentional", len),
        )
    } else {
        (
            CycleClassification::Info,
            format!("Medium cycle ({} nodes) needs review", len),
        )
    };

    ClassifiedCycle {
        nodes: cycle.to_vec(),
        classification,
        reason,
    }
}

/// Compute structural health issues
fn compute_structural_health(graph: &WorkGraph) -> StructuralHealth {
    let check_result = check_all(graph);
    let mut issues = Vec::new();

    // Check orphan references
    if check_result.orphan_refs.is_empty() {
        issues.push(StructuralIssue {
            severity: Severity::Ok,
            message: "No orphan references".to_string(),
            details: None,
        });
    } else {
        let details: Vec<String> = check_result
            .orphan_refs
            .iter()
            .map(|o| format!("{} -> {} ({})", o.from, o.to, o.relation))
            .collect();
        issues.push(StructuralIssue {
            severity: Severity::Critical,
            message: format!("{} orphan reference(s) found", check_result.orphan_refs.len()),
            details: Some(details),
        });
    }

    // Check cycles
    if check_result.cycles.is_empty() {
        issues.push(StructuralIssue {
            severity: Severity::Ok,
            message: "No cycles detected".to_string(),
            details: None,
        });
    } else {
        // Classify cycles
        let classified: Vec<ClassifiedCycle> = check_result
            .cycles
            .iter()
            .map(|c| classify_cycle(c, graph))
            .collect();

        let intentional_count = classified
            .iter()
            .filter(|c| c.classification == CycleClassification::Intentional)
            .count();
        let warning_count = classified
            .iter()
            .filter(|c| c.classification == CycleClassification::Warning)
            .count();

        if warning_count > 0 {
            issues.push(StructuralIssue {
                severity: Severity::Warning,
                message: format!(
                    "{} cycle(s) detected ({} warning, {} intentional)",
                    classified.len(),
                    warning_count,
                    intentional_count
                ),
                details: None,
            });
        } else if intentional_count > 0 {
            issues.push(StructuralIssue {
                severity: Severity::Ok,
                message: format!("{} cycle(s) detected (marked as intentional)", intentional_count),
                details: None,
            });
        }
    }

    // Check for dead-end tasks with status=open (may be forgotten)
    let dead_ends = find_dead_end_open_tasks(graph);
    if !dead_ends.is_empty() {
        issues.push(StructuralIssue {
            severity: Severity::Warning,
            message: format!("{} dead-end task(s) with status=open (may be forgotten)", dead_ends.len()),
            details: Some(dead_ends),
        });
    }

    StructuralHealth { issues }
}

/// Find open tasks that nothing depends on (potential forgotten tasks)
fn find_dead_end_open_tasks(graph: &WorkGraph) -> Vec<String> {
    // Build reverse dependency map
    let mut has_dependents: HashSet<&str> = HashSet::new();
    for task in graph.tasks() {
        for blocker_id in &task.blocked_by {
            has_dependents.insert(blocker_id);
        }
    }

    // Find open tasks with no dependents (and not likely final deliverables)
    graph
        .tasks()
        .filter(|t| {
            if t.status != Status::Open {
                return false;
            }
            if has_dependents.contains(t.id.as_str()) {
                return false;
            }
            // Heuristic: exclude likely final deliverables
            let id_lower = t.id.to_lowercase();
            let title_lower = t.title.to_lowercase();
            !(id_lower.contains("deploy")
                || id_lower.contains("release")
                || id_lower.contains("doc")
                || id_lower.contains("final")
                || title_lower.contains("deploy")
                || title_lower.contains("release")
                || title_lower.contains("documentation")
                || title_lower.contains("final"))
        })
        .map(|t| t.id.clone())
        .collect()
}

/// Compute bottleneck information
fn compute_bottlenecks(graph: &WorkGraph, now: &DateTime<Utc>) -> Vec<BottleneckInfo> {
    // Build reverse index: task_id -> list of tasks that depend on it
    let reverse_index = build_reverse_index(graph);

    let total_tasks = graph.tasks().count();
    let mut bottlenecks: Vec<BottleneckInfo> = Vec::new();

    for task in graph.tasks() {
        // Count transitive dependents
        let mut transitive: HashSet<String> = HashSet::new();
        collect_transitive_dependents(&reverse_index, &task.id, &mut transitive);
        let transitive_blocks = transitive.len();

        // Only include tasks that block at least 3 tasks (significant impact)
        if transitive_blocks >= 3 {
            let percentage = if total_tasks > 0 {
                (transitive_blocks as f64 / total_tasks as f64 * 100.0).round() as usize
            } else {
                0
            };

            let severity = match &task.status {
                Status::Done => Severity::Ok,
                Status::Open if percentage >= 20 => Severity::Critical,
                Status::Blocked if percentage >= 10 => Severity::Critical,
                Status::InProgress if percentage >= 20 => Severity::Warning,
                _ if percentage >= 10 => Severity::Warning,
                _ => Severity::Ok,
            };

            // Calculate days in progress
            let days_in_progress = if task.status == Status::InProgress {
                task.started_at.as_ref().and_then(|s| {
                    DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|dt| (*now - dt.with_timezone(&Utc)).num_days())
                })
            } else {
                None
            };

            bottlenecks.push(BottleneckInfo {
                id: task.id.clone(),
                transitive_blocks,
                status: task.status.clone(),
                assigned: task.assigned.clone(),
                severity,
                days_in_progress,
            });
        }
    }

    // Sort by transitive impact (highest first), then by severity
    bottlenecks.sort_by(|a, b| {
        // Critical > Warning > Ok
        let severity_order = |s: &Severity| match s {
            Severity::Critical => 0,
            Severity::Warning => 1,
            Severity::Ok => 2,
        };
        match severity_order(&a.severity).cmp(&severity_order(&b.severity)) {
            std::cmp::Ordering::Equal => b.transitive_blocks.cmp(&a.transitive_blocks),
            other => other,
        }
    });

    // Return top 5
    bottlenecks.truncate(5);
    bottlenecks
}

/// Build a reverse index: for each task, find what tasks list it in their `blocked_by`
fn build_reverse_index(graph: &WorkGraph) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();

    for task in graph.tasks() {
        for blocker_id in &task.blocked_by {
            index
                .entry(blocker_id.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    index
}

/// Recursively collect all transitive dependents
fn collect_transitive_dependents(
    reverse_index: &HashMap<String, Vec<String>>,
    task_id: &str,
    visited: &mut HashSet<String>,
) {
    if let Some(dependents) = reverse_index.get(task_id) {
        for dep_id in dependents {
            if visited.insert(dep_id.clone()) {
                collect_transitive_dependents(reverse_index, dep_id, visited);
            }
        }
    }
}

/// Compute workload information
fn compute_workload(graph: &WorkGraph) -> WorkloadSection {
    let mut actor_hours: HashMap<String, f64> = HashMap::new();
    let mut actor_capacity: HashMap<String, Option<f64>> = HashMap::new();

    // Initialize with known actors
    for actor in graph.actors() {
        actor_hours.insert(actor.id.clone(), 0.0);
        actor_capacity.insert(actor.id.clone(), actor.capacity);
    }

    // Sum up hours for each actor
    for task in graph.tasks() {
        if task.status == Status::Done {
            continue;
        }
        if let Some(ref actor_id) = task.assigned {
            let hours = task
                .estimate
                .as_ref()
                .and_then(|e| e.hours)
                .unwrap_or(0.0);
            *actor_hours.entry(actor_id.clone()).or_default() += hours;
            actor_capacity.entry(actor_id.clone()).or_insert(None);
        }
    }

    let total_actors = actor_hours.len();
    let mut overloaded = Vec::new();
    let mut balanced_count = 0;

    for (actor_id, hours) in &actor_hours {
        let capacity = actor_capacity.get(actor_id).and_then(|c| *c);
        let load_percent = capacity.map(|c| if c > 0.0 { (hours / c) * 100.0 } else { 0.0 });
        let is_overloaded = load_percent.map(|l| l > 100.0).unwrap_or(false);

        if is_overloaded {
            overloaded.push(WorkloadInfo {
                id: actor_id.clone(),
                load_percent,
                is_overloaded,
            });
        } else {
            balanced_count += 1;
        }
    }

    WorkloadSection {
        total_actors,
        balanced_actors: balanced_count,
        overloaded,
    }
}

/// Compute aging information
fn compute_aging(graph: &WorkGraph, now: &DateTime<Utc>) -> AgingSection {
    let mut issues = Vec::new();
    let mut old_open_count = 0;
    let mut stale_in_progress_count = 0;

    for task in graph.tasks() {
        // Check for old open tasks (> 90 days)
        if task.status == Status::Open {
            if let Some(ref created_at) = task.created_at {
                if let Ok(created) = DateTime::parse_from_rfc3339(created_at) {
                    let age = *now - created.with_timezone(&Utc);
                    if age > Duration::days(90) {
                        old_open_count += 1;
                        issues.push(AgingIssue {
                            task_id: task.id.clone(),
                            days: age.num_days(),
                            issue_type: "old_open".to_string(),
                            assigned: task.assigned.clone(),
                        });
                    }
                }
            }
        }

        // Check for stale in-progress tasks (> 14 days)
        if task.status == Status::InProgress {
            if let Some(ref started_at) = task.started_at {
                if let Ok(started) = DateTime::parse_from_rfc3339(started_at) {
                    let age = *now - started.with_timezone(&Utc);
                    if age > Duration::days(14) {
                        stale_in_progress_count += 1;
                        issues.push(AgingIssue {
                            task_id: task.id.clone(),
                            days: age.num_days(),
                            issue_type: "stale_in_progress".to_string(),
                            assigned: task.assigned.clone(),
                        });
                    }
                }
            }
        }
    }

    // Sort by days descending
    issues.sort_by(|a, b| b.days.cmp(&a.days));
    // Keep top 5
    issues.truncate(5);

    AgingSection {
        old_open_count,
        stale_in_progress_count,
        issues,
    }
}

/// Generate recommendations based on analysis
fn generate_recommendations(
    _summary: &Summary,
    structural: &StructuralHealth,
    bottlenecks: &[BottleneckInfo],
    workload: &WorkloadSection,
    aging: &AgingSection,
) -> Vec<Recommendation> {
    let mut recommendations = Vec::new();
    let mut priority = 1;

    // Critical bottlenecks that are open and unassigned
    for bottleneck in bottlenecks {
        if bottleneck.severity == Severity::Critical
            && bottleneck.status == Status::Open
            && bottleneck.assigned.is_none()
        {
            recommendations.push(Recommendation {
                priority,
                action: "assign_and_start".to_string(),
                task: Some(bottleneck.id.clone()),
                reason: format!("critical bottleneck - blocks {} tasks", bottleneck.transitive_blocks),
            });
            priority += 1;
        }
    }

    // Stale in-progress tasks
    for bottleneck in bottlenecks {
        if let Some(days) = bottleneck.days_in_progress {
            if days > 14 {
                recommendations.push(Recommendation {
                    priority,
                    action: "check_on".to_string(),
                    task: Some(bottleneck.id.clone()),
                    reason: format!("in-progress for {} days (stalled)", days),
                });
                priority += 1;
            }
        }
    }

    // Old open tasks from aging
    for issue in &aging.issues {
        if issue.issue_type == "old_open" && issue.days > 90 {
            recommendations.push(Recommendation {
                priority,
                action: "review".to_string(),
                task: Some(issue.task_id.clone()),
                reason: format!("open {} days", issue.days),
            });
            priority += 1;
            if priority > 5 {
                break;
            }
        }
    }

    // Overloaded actors
    for actor in &workload.overloaded {
        if let Some(load) = actor.load_percent {
            if load > 100.0 {
                recommendations.push(Recommendation {
                    priority,
                    action: "redistribute".to_string(),
                    task: None,
                    reason: format!("@{} at {:.0}% capacity", actor.id, load),
                });
                priority += 1;
            }
        }
    }

    // Structural issues
    for issue in &structural.issues {
        if issue.severity == Severity::Critical {
            recommendations.push(Recommendation {
                priority,
                action: "fix_structural".to_string(),
                task: None,
                reason: issue.message.clone(),
            });
            priority += 1;
        }
    }

    // Limit to top 5 recommendations
    recommendations.truncate(5);
    recommendations
}

/// Print human-readable output
fn print_human_readable(output: &AnalysisOutput) {
    println!("\n=== Workgraph Health Report ===\n");

    // Summary
    println!("SUMMARY");
    println!(
        "  Total tasks: {} ({} open, {} in-progress, {} done, {} blocked)",
        output.summary.total_tasks,
        output.summary.open,
        output.summary.in_progress,
        output.summary.done,
        output.summary.blocked
    );
    println!("  Ready to start: {} tasks", output.summary.ready);
    if output.summary.estimated_hours > 0.0 || output.summary.estimated_cost > 0.0 {
        println!(
            "  Estimated remaining: {:.0}h / ${:.0}",
            output.summary.estimated_hours, output.summary.estimated_cost
        );
    }
    println!();

    // Structural Health
    println!("STRUCTURAL HEALTH");
    for issue in &output.structural.issues {
        let indicator = match issue.severity {
            Severity::Ok => "[OK]",
            Severity::Warning => "[WARNING]",
            Severity::Critical => "[CRITICAL]",
        };
        println!("  {} {}", indicator, issue.message);
        if let Some(ref details) = issue.details {
            for detail in details.iter().take(3) {
                println!("      - {}", detail);
            }
            if details.len() > 3 {
                println!("      ... and {} more", details.len() - 3);
            }
        }
    }
    println!();

    // Bottlenecks
    if !output.bottlenecks.is_empty() {
        println!("BOTTLENECKS");
        for bottleneck in &output.bottlenecks {
            let indicator = match bottleneck.severity {
                Severity::Critical => "[CRITICAL]",
                Severity::Warning => "[WARNING]",
                Severity::Ok => continue, // Skip OK bottlenecks in human output
            };

            let status_str = match bottleneck.status {
                Status::Open => "open".to_string(),
                Status::InProgress => {
                    if let Some(days) = bottleneck.days_in_progress {
                        format!("in-progress for {} days", days)
                    } else {
                        "in-progress".to_string()
                    }
                }
                Status::Done => "done".to_string(),
                Status::Blocked => "blocked".to_string(),
                Status::Failed => "failed".to_string(),
                Status::Abandoned => "abandoned".to_string(),
            };

            let assigned_str = bottleneck
                .assigned
                .as_ref()
                .map(|a| format!(", @{}", a))
                .unwrap_or_else(|| ", unassigned".to_string());

            println!(
                "  {} {}: blocks {} tasks, status={}{}",
                indicator,
                bottleneck.id,
                bottleneck.transitive_blocks,
                status_str,
                assigned_str
            );
        }
        println!();
    }

    // Workload
    if output.workload.total_actors > 0 {
        println!("WORKLOAD");
        let balanced_msg = format!(
            "{}/{} actors have balanced workload",
            output.workload.balanced_actors, output.workload.total_actors
        );
        if output.workload.overloaded.is_empty() {
            println!("  [OK] {}", balanced_msg);
        } else {
            println!("  [OK] {}", balanced_msg);
            for actor in &output.workload.overloaded {
                if let Some(load) = actor.load_percent {
                    println!("  [WARNING] @{} at {:.0}% capacity", actor.id, load);
                }
            }
        }
        println!();
    }

    // Aging
    if output.aging.old_open_count > 0 || output.aging.stale_in_progress_count > 0 {
        println!("AGING");
        if output.aging.old_open_count > 0 {
            println!(
                "  [WARNING] {} task(s) open > 3 months",
                output.aging.old_open_count
            );
        }
        if output.aging.stale_in_progress_count > 0 {
            println!(
                "  [WARNING] {} task(s) in-progress > 14 days",
                output.aging.stale_in_progress_count
            );
        }
        println!();
    }

    // Recommendations
    if !output.recommendations.is_empty() {
        println!("RECOMMENDATIONS");
        for rec in &output.recommendations {
            let task_str = rec
                .task
                .as_ref()
                .map(|t| format!(" {}", t))
                .unwrap_or_default();
            let action_str = match rec.action.as_str() {
                "assign_and_start" => "Assign and start",
                "check_on" => "Check on",
                "review" => "Review",
                "redistribute" => "Redistribute tasks from",
                "fix_structural" => "Fix",
                _ => &rec.action,
            };
            println!("  {}. {}{} ({})", rec.priority, action_str, task_str, rec.reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Actor, ActorType, Estimate, Node, Task, TrustLevel, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
        }
    }

    fn make_actor(id: &str, capacity: Option<f64>) -> Actor {
        Actor {
            id: id.to_string(),
            name: Some(format!("{} Name", id)),
            role: None,
            rate: None,
            capacity,
            capabilities: vec![],
            context_limit: None,
            trust_level: TrustLevel::Provisional,
            last_seen: None,
            actor_type: ActorType::Agent,
            matrix_user_id: None,
            response_times: vec![],
        }
    }

    fn setup_test_graph() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let workgraph_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&workgraph_dir).unwrap();
        let graph_file = workgraph_dir.join("graph.jsonl");
        (tmp, graph_file)
    }

    #[test]
    fn test_compute_summary_empty() {
        let graph = WorkGraph::new();
        let summary = compute_summary(&graph);

        assert_eq!(summary.total_tasks, 0);
        assert_eq!(summary.open, 0);
        assert_eq!(summary.in_progress, 0);
        assert_eq!(summary.done, 0);
        assert_eq!(summary.blocked, 0);
        assert_eq!(summary.ready, 0);
    }

    #[test]
    fn test_compute_summary_with_tasks() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(1000.0),
        });
        graph.add_node(Node::Task(t1));

        let mut t2 = make_task("t2", "Task 2");
        t2.status = Status::Done;
        graph.add_node(Node::Task(t2));

        let mut t3 = make_task("t3", "Task 3");
        t3.status = Status::InProgress;
        t3.estimate = Some(Estimate {
            hours: Some(5.0),
            cost: Some(500.0),
        });
        graph.add_node(Node::Task(t3));

        let summary = compute_summary(&graph);

        assert_eq!(summary.total_tasks, 3);
        assert_eq!(summary.open, 1);
        assert_eq!(summary.in_progress, 1);
        assert_eq!(summary.done, 1);
        assert_eq!(summary.estimated_hours, 15.0);
        assert_eq!(summary.estimated_cost, 1500.0);
    }

    #[test]
    fn test_compute_structural_health_clean() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));

        let structural = compute_structural_health(&graph);

        // Should have OK for orphans and cycles
        let ok_count = structural
            .issues
            .iter()
            .filter(|i| i.severity == Severity::Ok)
            .count();
        assert!(ok_count >= 2);
    }

    #[test]
    fn test_compute_structural_health_with_orphan() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["nonexistent".to_string()];
        graph.add_node(Node::Task(t1));

        let structural = compute_structural_health(&graph);

        let has_critical = structural
            .issues
            .iter()
            .any(|i| i.severity == Severity::Critical);
        assert!(has_critical);
    }

    #[test]
    fn test_compute_bottlenecks_with_chain() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // Create a chain: t1 -> t2 -> t3 -> t4
        let t1 = make_task("t1", "Root Task");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t2".to_string()];
        let mut t4 = make_task("t4", "Task 4");
        t4.blocked_by = vec!["t3".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));

        let bottlenecks = compute_bottlenecks(&graph, &now);

        // t1 should be the biggest bottleneck (blocks 3 tasks)
        assert!(!bottlenecks.is_empty());
        assert_eq!(bottlenecks[0].id, "t1");
        assert_eq!(bottlenecks[0].transitive_blocks, 3);
    }

    #[test]
    fn test_compute_workload() {
        let mut graph = WorkGraph::new();

        let actor = make_actor("alice", Some(40.0));
        graph.add_node(Node::Actor(actor));

        let mut t1 = make_task("t1", "Task 1");
        t1.assigned = Some("alice".to_string());
        t1.estimate = Some(Estimate {
            hours: Some(50.0), // Overloaded
            cost: None,
        });
        graph.add_node(Node::Task(t1));

        let workload = compute_workload(&graph);

        assert_eq!(workload.total_actors, 1);
        assert!(!workload.overloaded.is_empty());
        assert!(workload.overloaded[0].is_overloaded);
    }

    #[test]
    fn test_compute_aging() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();
        let old_date = now - Duration::days(100);

        let mut t1 = make_task("t1", "Old Task");
        t1.created_at = Some(old_date.to_rfc3339());
        graph.add_node(Node::Task(t1));

        let aging = compute_aging(&graph, &now);

        assert_eq!(aging.old_open_count, 1);
        assert!(!aging.issues.is_empty());
    }

    #[test]
    fn test_generate_recommendations_with_bottleneck() {
        let summary = Summary {
            total_tasks: 10,
            open: 5,
            in_progress: 2,
            done: 3,
            blocked: 2,
            ready: 3,
            estimated_hours: 50.0,
            estimated_cost: 5000.0,
        };

        let structural = StructuralHealth { issues: vec![] };

        let bottlenecks = vec![BottleneckInfo {
            id: "critical-task".to_string(),
            transitive_blocks: 8,
            status: Status::Open,
            assigned: None,
            severity: Severity::Critical,
            days_in_progress: None,
        }];

        let workload = WorkloadSection {
            total_actors: 2,
            balanced_actors: 2,
            overloaded: vec![],
        };

        let aging = AgingSection {
            old_open_count: 0,
            stale_in_progress_count: 0,
            issues: vec![],
        };

        let recommendations = generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);

        assert!(!recommendations.is_empty());
        assert_eq!(recommendations[0].task, Some("critical-task".to_string()));
    }

    #[test]
    fn test_run_empty_graph() {
        let (_tmp, graph_file) = setup_test_graph();
        let graph = WorkGraph::new();
        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_with_json() {
        let (_tmp, graph_file) = setup_test_graph();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_no_workgraph() {
        let tmp = TempDir::new().unwrap();
        let workgraph_dir = tmp.path().join(".workgraph");
        // Don't create the directory

        let result = run(&workgraph_dir, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_dead_end_open_tasks() {
        let mut graph = WorkGraph::new();

        // t1 is depended on by t2
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        // t3 is a dead end (nothing depends on it)
        let t3 = make_task("t3", "Forgotten task");

        // t4 is a dead end but likely a deliverable
        let t4 = make_task("deploy", "Deploy to production");

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));

        let dead_ends = find_dead_end_open_tasks(&graph);

        // Should include t2 and t3 but not deploy
        assert!(dead_ends.contains(&"t2".to_string()));
        assert!(dead_ends.contains(&"t3".to_string()));
        assert!(!dead_ends.contains(&"deploy".to_string()));
    }
}

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::check::check_all;
use workgraph::graph::{Status, WorkGraph};
use workgraph::parser::load_graph;
use workgraph::query::{build_reverse_index, ready_tasks};

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
    let recommendations =
        generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);

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
            Status::InProgress | Status::PendingReview => {
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
            message: format!(
                "{} orphan reference(s) found",
                check_result.orphan_refs.len()
            ),
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
                message: format!(
                    "{} cycle(s) detected (marked as intentional)",
                    intentional_count
                ),
                details: None,
            });
        }
    }

    // Check for dead-end tasks with status=open (may be forgotten)
    let dead_ends = find_dead_end_open_tasks(graph);
    if !dead_ends.is_empty() {
        issues.push(StructuralIssue {
            severity: Severity::Warning,
            message: format!(
                "{} dead-end task(s) with status=open (may be forgotten)",
                dead_ends.len()
            ),
            details: Some(dead_ends),
        });
    }

    // Loop edge validation
    if !check_result.loop_edge_issues.is_empty() {
        let details: Vec<String> = check_result
            .loop_edge_issues
            .iter()
            .map(|issue| {
                use workgraph::check::LoopEdgeIssueKind;
                match &issue.kind {
                    LoopEdgeIssueKind::TargetNotFound => {
                        format!("{} -> {} (target not found)", issue.from, issue.target)
                    }
                    LoopEdgeIssueKind::ZeroMaxIterations => {
                        format!("{} -> {} (max_iterations=0)", issue.from, issue.target)
                    }
                    LoopEdgeIssueKind::GuardTaskNotFound(guard_task) => {
                        format!(
                            "{} -> {} (guard task '{}' not found)",
                            issue.from, issue.target, guard_task
                        )
                    }
                    LoopEdgeIssueKind::SelfLoop => {
                        format!("{} -> {} (self-loop)", issue.from, issue.target)
                    }
                }
            })
            .collect();
        issues.push(StructuralIssue {
            severity: Severity::Critical,
            message: format!(
                "{} loop edge issue(s) found",
                check_result.loop_edge_issues.len()
            ),
            details: Some(details),
        });
    }

    // Loop edge summary (informational)
    let total_loop_edges: usize = graph.tasks().map(|t| t.loops_to.len()).sum();
    if total_loop_edges > 0 {
        let active_count = graph
            .tasks()
            .flat_map(|t| t.loops_to.iter().map(move |e| (t, e)))
            .filter(|(_t, e)| {
                graph
                    .get_task(&e.target)
                    .map(|target| target.loop_iteration < e.max_iterations)
                    .unwrap_or(false)
            })
            .count();
        let exhausted_count = total_loop_edges - active_count;

        if check_result.loop_edge_issues.is_empty() {
            issues.push(StructuralIssue {
                severity: Severity::Ok,
                message: format!(
                    "{} loop edge(s) ({} active, {} exhausted)",
                    total_loop_edges, active_count, exhausted_count
                ),
                details: None,
            });
        }
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

/// Compute workload information from task assignments
fn compute_workload(graph: &WorkGraph) -> WorkloadSection {
    let mut actor_hours: HashMap<String, f64> = HashMap::new();

    // Sum up hours for each assignee from tasks
    for task in graph.tasks() {
        if task.status == Status::Done {
            continue;
        }
        if let Some(ref actor_id) = task.assigned {
            let hours = task.estimate.as_ref().and_then(|e| e.hours).unwrap_or(0.0);
            *actor_hours.entry(actor_id.clone()).or_default() += hours;
        }
    }

    let total_actors = actor_hours.len();

    WorkloadSection {
        total_actors,
        balanced_actors: total_actors,
        overloaded: vec![],
    }
}

/// Compute aging information
fn compute_aging(graph: &WorkGraph, now: &DateTime<Utc>) -> AgingSection {
    let mut issues = Vec::new();
    let mut old_open_count = 0;
    let mut stale_in_progress_count = 0;

    for task in graph.tasks() {
        // Check for old open tasks (> 90 days)
        if task.status == Status::Open
            && let Some(ref created_at) = task.created_at
            && let Ok(created) = DateTime::parse_from_rfc3339(created_at)
        {
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

        // Check for stale in-progress tasks (> 14 days)
        if task.status == Status::InProgress
            && let Some(ref started_at) = task.started_at
            && let Ok(started) = DateTime::parse_from_rfc3339(started_at)
        {
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
                reason: format!(
                    "critical bottleneck - blocks {} tasks",
                    bottleneck.transitive_blocks
                ),
            });
            priority += 1;
        }
    }

    // Stale in-progress tasks
    for bottleneck in bottlenecks {
        if let Some(days) = bottleneck.days_in_progress
            && days > 14
        {
            recommendations.push(Recommendation {
                priority,
                action: "check_on".to_string(),
                task: Some(bottleneck.id.clone()),
                reason: format!("in-progress for {} days (stalled)", days),
            });
            priority += 1;
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
        if let Some(load) = actor.load_percent
            && load > 100.0
        {
            recommendations.push(Recommendation {
                priority,
                action: "redistribute".to_string(),
                task: None,
                reason: format!("@{} at {:.0}% capacity", actor.id, load),
            });
            priority += 1;
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
                Status::PendingReview => "pending-review".to_string(),
            };

            let assigned_str = bottleneck
                .assigned
                .as_ref()
                .map(|a| format!(", @{}", a))
                .unwrap_or_else(|| ", unassigned".to_string());

            println!(
                "  {} {}: blocks {} tasks, status={}{}",
                indicator, bottleneck.id, bottleneck.transitive_blocks, status_str, assigned_str
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
            println!(
                "  {}. {}{} ({})",
                rec.priority, action_str, task_str, rec.reason
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Estimate, Node, Task, WorkGraph};
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
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
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

        let mut t1 = make_task("t1", "Task 1");
        t1.assigned = Some("alice".to_string());
        t1.estimate = Some(Estimate {
            hours: Some(50.0),
            cost: None,
        });
        graph.add_node(Node::Task(t1));

        let workload = compute_workload(&graph);

        assert_eq!(workload.total_actors, 1);
        assert_eq!(workload.balanced_actors, 1);
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

        let recommendations =
            generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);

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

    // --- Expanded tests for compute_summary ---

    #[test]
    fn test_compute_summary_single_task() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Only task")));
        let summary = compute_summary(&graph);

        assert_eq!(summary.total_tasks, 1);
        assert_eq!(summary.open, 1);
        assert_eq!(summary.ready, 1);
        assert_eq!(summary.blocked, 0);
    }

    #[test]
    fn test_compute_summary_blocked_task_counted() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Blocker");
        let mut t2 = make_task("t2", "Blocked");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let summary = compute_summary(&graph);
        // t2 is open but not ready, so it should be counted as blocked
        assert_eq!(summary.blocked, 1);
        assert_eq!(summary.ready, 1);
        assert_eq!(summary.open, 2);
    }

    #[test]
    fn test_compute_summary_explicit_blocked_status() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Explicitly blocked");
        t1.status = Status::Blocked;
        t1.estimate = Some(Estimate {
            hours: Some(3.0),
            cost: Some(100.0),
        });
        graph.add_node(Node::Task(t1));

        let summary = compute_summary(&graph);
        assert_eq!(summary.blocked, 1);
        assert_eq!(summary.estimated_hours, 3.0);
        assert_eq!(summary.estimated_cost, 100.0);
    }

    #[test]
    fn test_compute_summary_failed_abandoned_not_counted() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Failed");
        t1.status = Status::Failed;
        t1.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(500.0),
        });

        let mut t2 = make_task("t2", "Abandoned");
        t2.status = Status::Abandoned;
        t2.estimate = Some(Estimate {
            hours: Some(20.0),
            cost: Some(1000.0),
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let summary = compute_summary(&graph);
        assert_eq!(summary.total_tasks, 2);
        assert_eq!(summary.open, 0);
        assert_eq!(summary.in_progress, 0);
        assert_eq!(summary.done, 0);
        // Failed/abandoned don't contribute to estimates
        assert_eq!(summary.estimated_hours, 0.0);
        assert_eq!(summary.estimated_cost, 0.0);
    }

    #[test]
    fn test_compute_summary_pending_review_counted_as_in_progress() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Pending review");
        t1.status = Status::PendingReview;
        t1.estimate = Some(Estimate {
            hours: Some(5.0),
            cost: Some(200.0),
        });
        graph.add_node(Node::Task(t1));

        let summary = compute_summary(&graph);
        assert_eq!(summary.in_progress, 1);
        assert_eq!(summary.estimated_hours, 5.0);
        assert_eq!(summary.estimated_cost, 200.0);
    }

    #[test]
    fn test_compute_summary_no_estimates() {
        let mut graph = WorkGraph::new();
        // Tasks with no estimates at all
        graph.add_node(Node::Task(make_task("t1", "No estimate 1")));
        graph.add_node(Node::Task(make_task("t2", "No estimate 2")));

        let summary = compute_summary(&graph);
        assert_eq!(summary.estimated_hours, 0.0);
        assert_eq!(summary.estimated_cost, 0.0);
    }

    // --- Expanded tests for compute_bottlenecks ---

    #[test]
    fn test_compute_bottlenecks_empty_graph() {
        let graph = WorkGraph::new();
        let now = Utc::now();
        let bottlenecks = compute_bottlenecks(&graph, &now);
        assert!(bottlenecks.is_empty());
    }

    #[test]
    fn test_compute_bottlenecks_no_dependencies() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // Independent tasks don't create bottlenecks
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        graph.add_node(Node::Task(make_task("t2", "Task 2")));
        graph.add_node(Node::Task(make_task("t3", "Task 3")));

        let bottlenecks = compute_bottlenecks(&graph, &now);
        assert!(bottlenecks.is_empty());
    }

    #[test]
    fn test_compute_bottlenecks_fan_out() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // t1 blocks 4 tasks directly
        let t1 = make_task("t1", "Root");
        for i in 2..=5 {
            let mut t = make_task(&format!("t{}", i), &format!("Task {}", i));
            t.blocked_by = vec!["t1".to_string()];
            graph.add_node(Node::Task(t));
        }
        graph.add_node(Node::Task(t1));

        let bottlenecks = compute_bottlenecks(&graph, &now);
        assert_eq!(bottlenecks.len(), 1);
        assert_eq!(bottlenecks[0].id, "t1");
        assert_eq!(bottlenecks[0].transitive_blocks, 4);
    }

    #[test]
    fn test_compute_bottlenecks_done_task_severity_ok() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "Done root");
        t1.status = Status::Done;
        for i in 2..=5 {
            let mut t = make_task(&format!("t{}", i), &format!("Task {}", i));
            t.blocked_by = vec!["t1".to_string()];
            graph.add_node(Node::Task(t));
        }
        graph.add_node(Node::Task(t1));

        let bottlenecks = compute_bottlenecks(&graph, &now);
        assert!(!bottlenecks.is_empty());
        assert_eq!(bottlenecks[0].severity, Severity::Ok);
    }

    #[test]
    fn test_compute_bottlenecks_in_progress_with_days() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "In progress root");
        t1.status = Status::InProgress;
        t1.started_at = Some((now - Duration::days(5)).to_rfc3339());
        for i in 2..=5 {
            let mut t = make_task(&format!("t{}", i), &format!("Task {}", i));
            t.blocked_by = vec!["t1".to_string()];
            graph.add_node(Node::Task(t));
        }
        graph.add_node(Node::Task(t1));

        let bottlenecks = compute_bottlenecks(&graph, &now);
        assert!(!bottlenecks.is_empty());
        assert_eq!(bottlenecks[0].days_in_progress, Some(5));
    }

    #[test]
    fn test_compute_bottlenecks_truncated_to_5() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // Create 8 independent bottlenecks each blocking 3 tasks
        for root_idx in 0..8 {
            let root_id = format!("root{}", root_idx);
            graph.add_node(Node::Task(make_task(
                &root_id,
                &format!("Root {}", root_idx),
            )));
            for child_idx in 0..3 {
                let child_id = format!("child{}_{}", root_idx, child_idx);
                let mut t = make_task(&child_id, &format!("Child {}-{}", root_idx, child_idx));
                t.blocked_by = vec![root_id.clone()];
                graph.add_node(Node::Task(t));
            }
        }

        let bottlenecks = compute_bottlenecks(&graph, &now);
        assert!(bottlenecks.len() <= 5);
    }

    #[test]
    fn test_compute_bottlenecks_severity_critical_for_large_open() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // One open task blocking 20%+ of the graph -> Critical
        let t1 = make_task("blocker", "Big blocker");
        graph.add_node(Node::Task(t1));
        // Total: 1 root + 4 children = 5, blocker blocks 4/5 = 80%
        for i in 0..4 {
            let mut t = make_task(&format!("c{}", i), &format!("Child {}", i));
            t.blocked_by = vec!["blocker".to_string()];
            graph.add_node(Node::Task(t));
        }

        let bottlenecks = compute_bottlenecks(&graph, &now);
        assert!(!bottlenecks.is_empty());
        assert_eq!(bottlenecks[0].severity, Severity::Critical);
    }

    // --- Expanded tests for compute_workload ---

    #[test]
    fn test_compute_workload_empty_graph() {
        let graph = WorkGraph::new();
        let workload = compute_workload(&graph);
        assert_eq!(workload.total_actors, 0);
        assert!(workload.overloaded.is_empty());
    }

    #[test]
    fn test_compute_workload_done_tasks_excluded() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Done task");
        t1.status = Status::Done;
        t1.assigned = Some("alice".to_string());
        t1.estimate = Some(Estimate {
            hours: Some(100.0),
            cost: None,
        });
        graph.add_node(Node::Task(t1));

        let workload = compute_workload(&graph);
        // Done tasks are not counted in actor workload
        assert_eq!(workload.total_actors, 0);
    }

    #[test]
    fn test_compute_workload_multiple_actors() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Alice task");
        t1.assigned = Some("alice".to_string());
        t1.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: None,
        });

        let mut t2 = make_task("t2", "Bob task");
        t2.assigned = Some("bob".to_string());
        t2.estimate = Some(Estimate {
            hours: Some(20.0),
            cost: None,
        });

        let mut t3 = make_task("t3", "Alice task 2");
        t3.assigned = Some("alice".to_string());
        t3.estimate = Some(Estimate {
            hours: Some(15.0),
            cost: None,
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let workload = compute_workload(&graph);
        assert_eq!(workload.total_actors, 2);
    }

    #[test]
    fn test_compute_workload_unassigned_not_counted() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Unassigned")));

        let workload = compute_workload(&graph);
        assert_eq!(workload.total_actors, 0);
    }

    // --- Expanded tests for compute_aging ---

    #[test]
    fn test_compute_aging_empty_graph() {
        let graph = WorkGraph::new();
        let now = Utc::now();
        let aging = compute_aging(&graph, &now);

        assert_eq!(aging.old_open_count, 0);
        assert_eq!(aging.stale_in_progress_count, 0);
        assert!(aging.issues.is_empty());
    }

    #[test]
    fn test_compute_aging_recent_task_not_flagged() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "Recent task");
        t1.created_at = Some((now - Duration::days(10)).to_rfc3339());
        graph.add_node(Node::Task(t1));

        let aging = compute_aging(&graph, &now);
        assert_eq!(aging.old_open_count, 0);
        assert!(aging.issues.is_empty());
    }

    #[test]
    fn test_compute_aging_stale_in_progress() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "Stale in-progress");
        t1.status = Status::InProgress;
        t1.started_at = Some((now - Duration::days(20)).to_rfc3339());
        graph.add_node(Node::Task(t1));

        let aging = compute_aging(&graph, &now);
        assert_eq!(aging.stale_in_progress_count, 1);
        assert_eq!(aging.issues.len(), 1);
        assert_eq!(aging.issues[0].issue_type, "stale_in_progress");
        assert!(aging.issues[0].days >= 20);
    }

    #[test]
    fn test_compute_aging_in_progress_not_stale() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "Fresh in-progress");
        t1.status = Status::InProgress;
        t1.started_at = Some((now - Duration::days(5)).to_rfc3339());
        graph.add_node(Node::Task(t1));

        let aging = compute_aging(&graph, &now);
        assert_eq!(aging.stale_in_progress_count, 0);
    }

    #[test]
    fn test_compute_aging_issues_sorted_by_days_desc() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "100 days old");
        t1.created_at = Some((now - Duration::days(100)).to_rfc3339());

        let mut t2 = make_task("t2", "200 days old");
        t2.created_at = Some((now - Duration::days(200)).to_rfc3339());

        let mut t3 = make_task("t3", "150 days old");
        t3.created_at = Some((now - Duration::days(150)).to_rfc3339());

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let aging = compute_aging(&graph, &now);
        assert_eq!(aging.old_open_count, 3);
        // Should be sorted descending by days
        assert!(aging.issues[0].days >= aging.issues[1].days);
        if aging.issues.len() > 2 {
            assert!(aging.issues[1].days >= aging.issues[2].days);
        }
    }

    #[test]
    fn test_compute_aging_truncated_to_5() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // Create 8 old tasks
        for i in 0..8 {
            let mut t = make_task(&format!("t{}", i), &format!("Old task {}", i));
            t.created_at = Some((now - Duration::days(100 + i as i64)).to_rfc3339());
            graph.add_node(Node::Task(t));
        }

        let aging = compute_aging(&graph, &now);
        assert_eq!(aging.old_open_count, 8);
        assert!(aging.issues.len() <= 5);
    }

    #[test]
    fn test_compute_aging_no_created_at_ignored() {
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        // Task with no created_at timestamp
        let t1 = make_task("t1", "No timestamp");
        graph.add_node(Node::Task(t1));

        let aging = compute_aging(&graph, &now);
        assert_eq!(aging.old_open_count, 0);
    }

    // --- Expanded tests for compute_structural_health ---

    #[test]
    fn test_structural_health_dead_end_tasks() {
        let mut graph = WorkGraph::new();

        // t1 and t2 are dead-end open tasks (nothing depends on them)
        let t1 = make_task("orphan1", "Orphan 1");
        let t2 = make_task("orphan2", "Orphan 2");
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let structural = compute_structural_health(&graph);

        let dead_end_warning = structural
            .issues
            .iter()
            .any(|i| i.severity == Severity::Warning && i.message.contains("dead-end"));
        assert!(dead_end_warning);
    }

    #[test]
    fn test_structural_health_done_tasks_not_dead_end() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Done task");
        t1.status = Status::Done;
        graph.add_node(Node::Task(t1));

        let structural = compute_structural_health(&graph);

        // Done tasks should not be flagged as dead-end
        let dead_end_warning = structural
            .issues
            .iter()
            .any(|i| i.severity == Severity::Warning && i.message.contains("dead-end"));
        assert!(!dead_end_warning);
    }

    #[test]
    fn test_structural_health_deploy_not_dead_end() {
        let mut graph = WorkGraph::new();

        // Deliverable-like task names should not be flagged
        let t1 = make_task("deploy-prod", "Deploy to production");
        let t2 = make_task("release-v2", "Release version 2");
        let t3 = make_task("final-review", "Final review");
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let dead_ends = find_dead_end_open_tasks(&graph);
        assert!(!dead_ends.contains(&"deploy-prod".to_string()));
        assert!(!dead_ends.contains(&"release-v2".to_string()));
        assert!(!dead_ends.contains(&"final-review".to_string()));
    }

    // --- Expanded tests for generate_recommendations ---

    #[test]
    fn test_recommendations_stale_in_progress_bottleneck() {
        let summary = Summary {
            total_tasks: 5,
            open: 2,
            in_progress: 1,
            done: 2,
            blocked: 1,
            ready: 1,
            estimated_hours: 0.0,
            estimated_cost: 0.0,
        };
        let structural = StructuralHealth { issues: vec![] };
        let bottlenecks = vec![BottleneckInfo {
            id: "stale-task".to_string(),
            transitive_blocks: 3,
            status: Status::InProgress,
            assigned: Some("alice".to_string()),
            severity: Severity::Warning,
            days_in_progress: Some(20),
        }];
        let workload = WorkloadSection {
            total_actors: 1,
            balanced_actors: 1,
            overloaded: vec![],
        };
        let aging = AgingSection {
            old_open_count: 0,
            stale_in_progress_count: 0,
            issues: vec![],
        };

        let recs = generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);
        assert!(!recs.is_empty());
        let stale_rec = recs.iter().find(|r| r.action == "check_on");
        assert!(stale_rec.is_some());
        assert!(stale_rec.unwrap().reason.contains("20 days"));
    }

    #[test]
    fn test_recommendations_old_open_tasks() {
        let summary = Summary {
            total_tasks: 5,
            open: 3,
            in_progress: 0,
            done: 2,
            blocked: 0,
            ready: 3,
            estimated_hours: 0.0,
            estimated_cost: 0.0,
        };
        let structural = StructuralHealth { issues: vec![] };
        let bottlenecks = vec![];
        let workload = WorkloadSection {
            total_actors: 0,
            balanced_actors: 0,
            overloaded: vec![],
        };
        let aging = AgingSection {
            old_open_count: 2,
            stale_in_progress_count: 0,
            issues: vec![
                AgingIssue {
                    task_id: "old1".to_string(),
                    days: 120,
                    issue_type: "old_open".to_string(),
                    assigned: None,
                },
                AgingIssue {
                    task_id: "old2".to_string(),
                    days: 95,
                    issue_type: "old_open".to_string(),
                    assigned: None,
                },
            ],
        };

        let recs = generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);
        let review_recs: Vec<_> = recs.iter().filter(|r| r.action == "review").collect();
        assert_eq!(review_recs.len(), 2);
    }

    #[test]
    fn test_recommendations_structural_critical() {
        let summary = Summary {
            total_tasks: 1,
            open: 1,
            in_progress: 0,
            done: 0,
            blocked: 0,
            ready: 1,
            estimated_hours: 0.0,
            estimated_cost: 0.0,
        };
        let structural = StructuralHealth {
            issues: vec![StructuralIssue {
                severity: Severity::Critical,
                message: "3 orphan reference(s) found".to_string(),
                details: None,
            }],
        };
        let bottlenecks = vec![];
        let workload = WorkloadSection {
            total_actors: 0,
            balanced_actors: 0,
            overloaded: vec![],
        };
        let aging = AgingSection {
            old_open_count: 0,
            stale_in_progress_count: 0,
            issues: vec![],
        };

        let recs = generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);
        let fix_recs: Vec<_> = recs
            .iter()
            .filter(|r| r.action == "fix_structural")
            .collect();
        assert_eq!(fix_recs.len(), 1);
        assert!(fix_recs[0].reason.contains("orphan"));
    }

    #[test]
    fn test_recommendations_overloaded_actor() {
        let summary = Summary {
            total_tasks: 5,
            open: 3,
            in_progress: 1,
            done: 1,
            blocked: 0,
            ready: 3,
            estimated_hours: 0.0,
            estimated_cost: 0.0,
        };
        let structural = StructuralHealth { issues: vec![] };
        let bottlenecks = vec![];
        let workload = WorkloadSection {
            total_actors: 2,
            balanced_actors: 1,
            overloaded: vec![WorkloadInfo {
                id: "alice".to_string(),
                load_percent: Some(150.0),
                is_overloaded: true,
            }],
        };
        let aging = AgingSection {
            old_open_count: 0,
            stale_in_progress_count: 0,
            issues: vec![],
        };

        let recs = generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);
        let redistribute_recs: Vec<_> =
            recs.iter().filter(|r| r.action == "redistribute").collect();
        assert_eq!(redistribute_recs.len(), 1);
        assert!(redistribute_recs[0].reason.contains("alice"));
        assert!(redistribute_recs[0].reason.contains("150"));
    }

    #[test]
    fn test_recommendations_truncated_to_5() {
        let summary = Summary {
            total_tasks: 20,
            open: 15,
            in_progress: 0,
            done: 5,
            blocked: 0,
            ready: 15,
            estimated_hours: 0.0,
            estimated_cost: 0.0,
        };
        let structural = StructuralHealth {
            issues: vec![
                StructuralIssue {
                    severity: Severity::Critical,
                    message: "issue 1".to_string(),
                    details: None,
                },
                StructuralIssue {
                    severity: Severity::Critical,
                    message: "issue 2".to_string(),
                    details: None,
                },
            ],
        };
        let bottlenecks = vec![
            BottleneckInfo {
                id: "b1".to_string(),
                transitive_blocks: 10,
                status: Status::Open,
                assigned: None,
                severity: Severity::Critical,
                days_in_progress: None,
            },
            BottleneckInfo {
                id: "b2".to_string(),
                transitive_blocks: 8,
                status: Status::Open,
                assigned: None,
                severity: Severity::Critical,
                days_in_progress: None,
            },
        ];
        let workload = WorkloadSection {
            total_actors: 1,
            balanced_actors: 0,
            overloaded: vec![WorkloadInfo {
                id: "alice".to_string(),
                load_percent: Some(200.0),
                is_overloaded: true,
            }],
        };
        let aging = AgingSection {
            old_open_count: 3,
            stale_in_progress_count: 0,
            issues: vec![
                AgingIssue {
                    task_id: "old1".to_string(),
                    days: 200,
                    issue_type: "old_open".to_string(),
                    assigned: None,
                },
                AgingIssue {
                    task_id: "old2".to_string(),
                    days: 150,
                    issue_type: "old_open".to_string(),
                    assigned: None,
                },
                AgingIssue {
                    task_id: "old3".to_string(),
                    days: 120,
                    issue_type: "old_open".to_string(),
                    assigned: None,
                },
            ],
        };

        let recs = generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);
        assert!(recs.len() <= 5);
    }

    #[test]
    fn test_recommendations_empty_when_all_clear() {
        let summary = Summary {
            total_tasks: 3,
            open: 0,
            in_progress: 0,
            done: 3,
            blocked: 0,
            ready: 0,
            estimated_hours: 0.0,
            estimated_cost: 0.0,
        };
        let structural = StructuralHealth { issues: vec![] };
        let bottlenecks = vec![];
        let workload = WorkloadSection {
            total_actors: 0,
            balanced_actors: 0,
            overloaded: vec![],
        };
        let aging = AgingSection {
            old_open_count: 0,
            stale_in_progress_count: 0,
            issues: vec![],
        };

        let recs = generate_recommendations(&summary, &structural, &bottlenecks, &workload, &aging);
        assert!(recs.is_empty());
    }

    // --- JSON output tests ---

    #[test]
    fn test_analysis_output_json_serialization() {
        let output = AnalysisOutput {
            summary: Summary {
                total_tasks: 3,
                open: 1,
                in_progress: 1,
                done: 1,
                blocked: 0,
                ready: 1,
                estimated_hours: 10.0,
                estimated_cost: 500.0,
            },
            structural: StructuralHealth {
                issues: vec![StructuralIssue {
                    severity: Severity::Ok,
                    message: "No orphan references".to_string(),
                    details: None,
                }],
            },
            bottlenecks: vec![BottleneckInfo {
                id: "t1".to_string(),
                transitive_blocks: 5,
                status: Status::Open,
                assigned: Some("alice".to_string()),
                severity: Severity::Warning,
                days_in_progress: None,
            }],
            workload: WorkloadSection {
                total_actors: 1,
                balanced_actors: 1,
                overloaded: vec![],
            },
            aging: AgingSection {
                old_open_count: 0,
                stale_in_progress_count: 0,
                issues: vec![],
            },
            recommendations: vec![Recommendation {
                priority: 1,
                action: "assign_and_start".to_string(),
                task: Some("t1".to_string()),
                reason: "critical bottleneck".to_string(),
            }],
        };

        let json = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["summary"]["total_tasks"], 3);
        assert_eq!(parsed["summary"]["estimated_hours"], 10.0);
        assert_eq!(parsed["structural"]["issues"][0]["severity"], "ok");
        assert_eq!(parsed["bottlenecks"][0]["id"], "t1");
        assert_eq!(parsed["bottlenecks"][0]["assigned"], "alice");
        assert_eq!(parsed["workload"]["total_actors"], 1);
        assert_eq!(parsed["recommendations"][0]["action"], "assign_and_start");
    }

    #[test]
    fn test_analysis_output_json_skips_none_fields() {
        let output = AnalysisOutput {
            summary: Summary {
                total_tasks: 0,
                open: 0,
                in_progress: 0,
                done: 0,
                blocked: 0,
                ready: 0,
                estimated_hours: 0.0,
                estimated_cost: 0.0,
            },
            structural: StructuralHealth {
                issues: vec![StructuralIssue {
                    severity: Severity::Ok,
                    message: "test".to_string(),
                    details: None,
                }],
            },
            bottlenecks: vec![BottleneckInfo {
                id: "t1".to_string(),
                transitive_blocks: 3,
                status: Status::Open,
                assigned: None,
                severity: Severity::Ok,
                days_in_progress: None,
            }],
            workload: WorkloadSection {
                total_actors: 0,
                balanced_actors: 0,
                overloaded: vec![],
            },
            aging: AgingSection {
                old_open_count: 0,
                stale_in_progress_count: 0,
                issues: vec![],
            },
            recommendations: vec![],
        };

        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // "details" should be skipped (None)
        assert!(parsed["structural"]["issues"][0].get("details").is_none());
        // "assigned" should be skipped (None)
        assert!(parsed["bottlenecks"][0].get("assigned").is_none());
        // "days_in_progress" should be skipped (None)
        assert!(parsed["bottlenecks"][0].get("days_in_progress").is_none());
    }

    // --- classify_cycle tests ---

    #[test]
    fn test_classify_cycle_intentional_with_recurring_tag() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Recurring task");
        t1.tags = vec!["recurring".to_string()];
        let t2 = make_task("t2", "Task 2");
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let cycle = vec!["t1".to_string(), "t2".to_string()];
        let classified = classify_cycle(&cycle, &graph);
        assert_eq!(classified.classification, CycleClassification::Intentional);
    }

    #[test]
    fn test_classify_cycle_intentional_with_cycle_tag() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.tags = vec!["cycle:intentional".to_string()];
        let t2 = make_task("t2", "Task 2");
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let cycle = vec!["t1".to_string(), "t2".to_string()];
        let classified = classify_cycle(&cycle, &graph);
        assert_eq!(classified.classification, CycleClassification::Intentional);
    }

    #[test]
    fn test_classify_cycle_short_without_tag_is_warning() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let t2 = make_task("t2", "Task 2");
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let cycle = vec!["t1".to_string(), "t2".to_string()];
        let classified = classify_cycle(&cycle, &graph);
        assert_eq!(classified.classification, CycleClassification::Warning);
    }

    #[test]
    fn test_classify_cycle_long_is_warning() {
        let mut graph = WorkGraph::new();
        let mut cycle = vec![];
        for i in 0..6 {
            let id = format!("t{}", i);
            graph.add_node(Node::Task(make_task(&id, &format!("Task {}", i))));
            cycle.push(id);
        }

        let classified = classify_cycle(&cycle, &graph);
        assert_eq!(classified.classification, CycleClassification::Warning);
        assert!(classified.reason.contains("Long cycle"));
    }

    #[test]
    fn test_classify_cycle_medium_is_info() {
        let mut graph = WorkGraph::new();
        let mut cycle = vec![];
        for i in 0..3 {
            let id = format!("t{}", i);
            graph.add_node(Node::Task(make_task(&id, &format!("Task {}", i))));
            cycle.push(id);
        }

        let classified = classify_cycle(&cycle, &graph);
        assert_eq!(classified.classification, CycleClassification::Info);
        assert!(classified.reason.contains("Medium cycle"));
    }

    // --- collect_transitive_dependents tests ---

    #[test]
    fn test_collect_transitive_dependents_deep_chain() {
        let mut reverse_index: HashMap<String, Vec<String>> = HashMap::new();
        // Chain: t1 <- t2 <- t3 <- t4 <- t5
        reverse_index.insert("t1".to_string(), vec!["t2".to_string()]);
        reverse_index.insert("t2".to_string(), vec!["t3".to_string()]);
        reverse_index.insert("t3".to_string(), vec!["t4".to_string()]);
        reverse_index.insert("t4".to_string(), vec!["t5".to_string()]);

        let mut visited = HashSet::new();
        collect_transitive_dependents(&reverse_index, "t1", &mut visited);
        assert_eq!(visited.len(), 4);
        assert!(visited.contains("t2"));
        assert!(visited.contains("t5"));
    }

    #[test]
    fn test_collect_transitive_dependents_diamond() {
        let mut reverse_index: HashMap<String, Vec<String>> = HashMap::new();
        // Diamond: t1 <- t2, t1 <- t3, t2 <- t4, t3 <- t4
        reverse_index.insert("t1".to_string(), vec!["t2".to_string(), "t3".to_string()]);
        reverse_index.insert("t2".to_string(), vec!["t4".to_string()]);
        reverse_index.insert("t3".to_string(), vec!["t4".to_string()]);

        let mut visited = HashSet::new();
        collect_transitive_dependents(&reverse_index, "t1", &mut visited);
        // t2, t3, t4 — but t4 counted once
        assert_eq!(visited.len(), 3);
    }

    #[test]
    fn test_collect_transitive_dependents_no_dependents() {
        let reverse_index: HashMap<String, Vec<String>> = HashMap::new();
        let mut visited = HashSet::new();
        collect_transitive_dependents(&reverse_index, "t1", &mut visited);
        assert!(visited.is_empty());
    }

    // --- run() integration tests ---

    #[test]
    fn test_run_single_task_graph() {
        let (_tmp, graph_file) = setup_test_graph();
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Only task")));
        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_complex_graph_json() {
        let (_tmp, graph_file) = setup_test_graph();
        let mut graph = WorkGraph::new();
        let now = Utc::now();

        let mut t1 = make_task("t1", "Root task");
        t1.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(500.0),
        });
        t1.assigned = Some("alice".to_string());

        let mut t2 = make_task("t2", "Child task");
        t2.blocked_by = vec!["t1".to_string()];
        t2.estimate = Some(Estimate {
            hours: Some(5.0),
            cost: Some(200.0),
        });

        let mut t3 = make_task("t3", "Old task");
        t3.created_at = Some((now - Duration::days(100)).to_rfc3339());

        let mut t4 = make_task("t4", "Done task");
        t4.status = Status::Done;

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));
        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_graph_with_all_statuses() {
        let (_tmp, graph_file) = setup_test_graph();
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Open");

        let mut t2 = make_task("t2", "In progress");
        t2.status = Status::InProgress;

        let mut t3 = make_task("t3", "Done");
        t3.status = Status::Done;

        let mut t4 = make_task("t4", "Blocked");
        t4.status = Status::Blocked;

        let mut t5 = make_task("t5", "Failed");
        t5.status = Status::Failed;

        let mut t6 = make_task("t6", "Abandoned");
        t6.status = Status::Abandoned;

        let mut t7 = make_task("t7", "Pending review");
        t7.status = Status::PendingReview;

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));
        graph.add_node(Node::Task(t5));
        graph.add_node(Node::Task(t6));
        graph.add_node(Node::Task(t7));
        save_graph(&graph, &graph_file).unwrap();

        // Both human and JSON output should succeed
        let result_human = run(graph_file.parent().unwrap(), false);
        assert!(result_human.is_ok());
        let result_json = run(graph_file.parent().unwrap(), true);
        assert!(result_json.is_ok());
    }

    // --- Severity serialization tests ---

    #[test]
    fn test_severity_serialization() {
        assert_eq!(serde_json::to_string(&Severity::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&Severity::Warning).unwrap(),
            "\"warning\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            "\"critical\""
        );
    }

    // --- find_dead_end_open_tasks edge cases ---

    #[test]
    fn test_dead_end_empty_graph() {
        let graph = WorkGraph::new();
        let dead_ends = find_dead_end_open_tasks(&graph);
        assert!(dead_ends.is_empty());
    }

    #[test]
    fn test_dead_end_all_done() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Done 1");
        t1.status = Status::Done;
        let mut t2 = make_task("t2", "Done 2");
        t2.status = Status::Done;
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let dead_ends = find_dead_end_open_tasks(&graph);
        assert!(dead_ends.is_empty());
    }

    #[test]
    fn test_dead_end_doc_excluded() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("write-docs", "Write documentation for API");
        graph.add_node(Node::Task(t1));

        let dead_ends = find_dead_end_open_tasks(&graph);
        assert!(!dead_ends.contains(&"write-docs".to_string()));
    }
}

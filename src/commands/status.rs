//! Quick status overview command
//!
//! Provides a one-screen summary of the workgraph state:
//! - Service status (running/stopped, PID, uptime, socket)
//! - Coordinator config (max_agents, executor, model, poll_interval)
//! - Agent summary (alive/dead counts, active agents with tasks)
//! - Task summary (in-progress, ready, blocked, done counts)
//! - Recent activity (last 5 task completions)
//!
//! Usage:
//!   wg status         # Human-readable output
//!   wg status --json  # Machine-readable JSON output

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;
use workgraph::service::{AgentRegistry, AgentStatus};

use super::dead_agents::is_process_alive;
use super::graph_path;
use super::service::{CoordinatorState, ServiceState};

/// Service status information
#[derive(Debug, Clone, serde::Serialize)]
struct ServiceStatusInfo {
    running: bool,
    pid: Option<u32>,
    uptime: Option<String>,
    socket: Option<String>,
}

/// Coordinator configuration info
#[derive(Debug, Clone, serde::Serialize)]
struct CoordinatorInfo {
    max_agents: usize,
    executor: String,
    model: Option<String>,
    poll_interval: u64,
}

/// Active agent info (compact)
#[derive(Debug, Clone, serde::Serialize)]
struct ActiveAgentInfo {
    id: String,
    task_id: String,
    uptime: String,
    status: String,
}

/// Agent summary
#[derive(Debug, Clone, serde::Serialize)]
struct AgentSummaryInfo {
    alive: usize,
    dead: usize,
    active: Vec<ActiveAgentInfo>,
}

/// Task summary
#[derive(Debug, Clone, serde::Serialize)]
struct TaskSummaryInfo {
    in_progress: usize,
    ready: usize,
    blocked: usize,
    delayed: usize,
    done_today: usize,
    done_total: usize,
}

/// Recent activity entry
#[derive(Debug, Clone, serde::Serialize)]
struct RecentActivityEntry {
    time: String,
    task_id: String,
    title: String,
}

/// Full status output
#[derive(Debug, Clone, serde::Serialize)]
struct StatusOutput {
    service: ServiceStatusInfo,
    coordinator: CoordinatorInfo,
    agents: AgentSummaryInfo,
    tasks: TaskSummaryInfo,
    recent: Vec<RecentActivityEntry>,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let status = gather_status(dir)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_status(&status);
    }

    Ok(())
}

fn gather_status(dir: &Path) -> Result<StatusOutput> {
    // 1. Service status
    let service = gather_service_status(dir)?;

    // 2. Coordinator config
    let coordinator = gather_coordinator_info(dir);

    // 3. Agent summary
    let agents = gather_agent_summary(dir)?;

    // 4. Task summary
    let tasks = gather_task_summary(dir)?;

    // 5. Recent activity
    let recent = gather_recent_activity(dir)?;

    Ok(StatusOutput {
        service,
        coordinator,
        agents,
        tasks,
        recent,
    })
}

fn gather_service_status(dir: &Path) -> Result<ServiceStatusInfo> {
    let state = ServiceState::load(dir)?;

    match state {
        Some(s) if is_process_running(s.pid) => {
            let uptime = chrono::DateTime::parse_from_rfc3339(&s.started_at)
                .map(|started| {
                    let now = chrono::Utc::now();
                    let duration = now.signed_duration_since(started);
                    format_duration_short(duration.num_seconds())
                })
                .ok();

            Ok(ServiceStatusInfo {
                running: true,
                pid: Some(s.pid),
                uptime,
                socket: Some(s.socket_path),
            })
        }
        _ => Ok(ServiceStatusInfo {
            running: false,
            pid: None,
            uptime: None,
            socket: None,
        }),
    }
}

fn gather_coordinator_info(dir: &Path) -> CoordinatorInfo {
    // Try to get runtime state from coordinator (if daemon is running)
    if let Some(coord) = CoordinatorState::load(dir) {
        return CoordinatorInfo {
            max_agents: coord.max_agents,
            executor: coord.executor,
            model: coord.model,
            poll_interval: coord.poll_interval,
        };
    }

    // Fall back to config file
    let config = workgraph::config::Config::load(dir).unwrap_or_default();
    CoordinatorInfo {
        max_agents: config.coordinator.max_agents,
        executor: config.coordinator.executor,
        model: config.coordinator.model,
        poll_interval: config.coordinator.poll_interval,
    }
}

fn gather_agent_summary(dir: &Path) -> Result<AgentSummaryInfo> {
    let registry = AgentRegistry::load(dir).unwrap_or_default();
    let agents = registry.list_agents();

    let mut alive = 0;
    let mut dead = 0;
    let mut active = Vec::new();

    for agent in &agents {
        let process_alive = is_process_alive(agent.pid);
        let is_alive = agent.is_alive() && process_alive;

        if is_alive {
            alive += 1;
            // Include in active list if working
            if agent.status == AgentStatus::Working || agent.status == AgentStatus::Starting {
                active.push(ActiveAgentInfo {
                    id: agent.id.clone(),
                    task_id: agent.task_id.clone(),
                    uptime: agent.uptime_human(),
                    status: format!("{:?}", agent.status).to_lowercase(),
                });
            }
        } else {
            dead += 1;
        }
    }

    Ok(AgentSummaryInfo {
        alive,
        dead,
        active,
    })
}

fn gather_task_summary(dir: &Path) -> Result<TaskSummaryInfo> {
    let path = graph_path(dir);
    if !path.exists() {
        return Ok(TaskSummaryInfo {
            in_progress: 0,
            ready: 0,
            blocked: 0,
            delayed: 0,
            done_today: 0,
            done_total: 0,
        });
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let ready_tasks_list = ready_tasks(&graph);
    let ready_ids: std::collections::HashSet<&str> =
        ready_tasks_list.iter().map(|t| t.id.as_str()).collect();

    let now = Utc::now();
    let mut in_progress = 0;
    let mut blocked = 0;
    let mut delayed = 0;
    let mut done_today = 0;
    let mut done_total = 0;

    let today_start = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

    for task in graph.tasks() {
        match task.status {
            Status::Open => {
                if !ready_ids.contains(task.id.as_str()) {
                    // Distinguish delayed (waiting on ready_after) from blocked (waiting on deps)
                    let has_future_ready_after = task.ready_after.as_ref().map_or(false, |ra| {
                        ra.parse::<DateTime<Utc>>()
                            .map(|ts| ts > now)
                            .unwrap_or(false)
                    });
                    let all_blockers_done = task.blocked_by.iter().all(|bid| {
                        graph
                            .get_task(bid)
                            .map(|t| t.status == Status::Done)
                            .unwrap_or(true)
                    });
                    if has_future_ready_after && all_blockers_done {
                        delayed += 1;
                    } else {
                        blocked += 1;
                    }
                }
            }
            Status::InProgress | Status::PendingReview => {
                in_progress += 1;
            }
            Status::Done => {
                done_total += 1;
                // Check if completed today
                if let Some(ref completed_at) = task.completed_at {
                    if let Ok(completed) = completed_at.parse::<DateTime<Utc>>() {
                        if completed >= today_start {
                            done_today += 1;
                        }
                    }
                }
            }
            Status::Blocked => {
                blocked += 1;
            }
            Status::Failed | Status::Abandoned => {
                // Terminal states, not counted in summary
            }
        }
    }

    Ok(TaskSummaryInfo {
        in_progress,
        ready: ready_tasks_list.len(),
        blocked,
        delayed,
        done_today,
        done_total,
    })
}

fn gather_recent_activity(dir: &Path) -> Result<Vec<RecentActivityEntry>> {
    let path = graph_path(dir);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Collect done tasks with completion timestamps
    let mut completed: Vec<_> = graph
        .tasks()
        .filter(|t| t.status == Status::Done && t.completed_at.is_some())
        .filter_map(|t| {
            let completed_at = t.completed_at.as_ref()?;
            let ts = completed_at.parse::<DateTime<Utc>>().ok()?;
            Some((ts, t.id.clone(), t.title.clone()))
        })
        .collect();

    // Sort by completion time, most recent first
    completed.sort_by(|a, b| b.0.cmp(&a.0));

    // Take last 5
    let recent: Vec<RecentActivityEntry> = completed
        .into_iter()
        .take(5)
        .map(|(ts, id, title)| {
            let time = ts.format("%H:%M").to_string();
            RecentActivityEntry {
                time,
                task_id: id,
                title,
            }
        })
        .collect();

    Ok(recent)
}

fn print_status(status: &StatusOutput) {
    // Line 1: Service status
    if status.service.running {
        let pid = status.service.pid.unwrap_or(0);
        let uptime = status.service.uptime.as_deref().unwrap_or("?");
        println!("Service: running (PID {}, {} uptime)", pid, uptime);
    } else {
        println!("Service: stopped");
    }

    // Line 2: Coordinator config
    let model_str = status.coordinator.model.as_deref().unwrap_or("default");
    println!(
        "Coordinator: max={}, executor={}, model={}, poll={}s",
        status.coordinator.max_agents,
        status.coordinator.executor,
        model_str,
        status.coordinator.poll_interval
    );

    // Line 3+: Agent summary
    println!();
    if status.agents.alive == 0 && status.agents.dead == 0 {
        println!("Agents: none");
    } else {
        println!(
            "Agents ({} alive, {} dead):",
            status.agents.alive, status.agents.dead
        );
        for agent in &status.agents.active {
            // Truncate task_id if too long
            let task_display = if agent.task_id.len() > 24 {
                format!("{}...", &agent.task_id[..21])
            } else {
                agent.task_id.clone()
            };
            println!(
                "  {:10}  {:24}  {:>5}  {}",
                agent.id, task_display, agent.uptime, agent.status
            );
        }
        if status.agents.active.is_empty() && status.agents.alive > 0 {
            println!("  ({} idle)", status.agents.alive);
        }
    }

    // Line: Task summary
    println!();
    let delayed_str = if status.tasks.delayed > 0 {
        format!(", {} delayed", status.tasks.delayed)
    } else {
        String::new()
    };
    println!(
        "Tasks: {} in-progress, {} ready, {} blocked{}, {} done (today: {})",
        status.tasks.in_progress,
        status.tasks.ready,
        status.tasks.blocked,
        delayed_str,
        status.tasks.done_total,
        status.tasks.done_today
    );

    // Recent activity
    if !status.recent.is_empty() {
        println!();
        println!("Recent:");
        for entry in &status.recent {
            // Truncate title if too long
            let title_display = if entry.title.len() > 50 {
                format!("{}...", &entry.title[..47])
            } else {
                entry.title.clone()
            };
            println!("  {}  {} [done]", entry.time, title_display);
        }
    }
}

/// Format duration in a compact way (e.g., "5h", "1d 2h", "30m")
fn format_duration_short(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins > 0 {
            format!("{}h {}m", hours, mins)
        } else {
            format!("{}h", hours)
        }
    } else {
        let days = secs / 86400;
        let hours = (secs % 86400) / 3600;
        if hours > 0 {
            format!("{}d {}h", days, hours)
        } else {
            format!("{}d", days)
        }
    }
}

/// Check if a process is running (reusing logic from service.rs)
#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_process_running(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
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

    #[test]
    fn test_format_duration_short() {
        assert_eq!(format_duration_short(30), "30s");
        assert_eq!(format_duration_short(90), "1m");
        assert_eq!(format_duration_short(3600), "1h");
        assert_eq!(format_duration_short(3660), "1h 1m");
        assert_eq!(format_duration_short(86400), "1d");
        assert_eq!(format_duration_short(90000), "1d 1h");
    }

    #[test]
    fn test_gather_status_empty() {
        let temp_dir = TempDir::new().unwrap();
        let result = gather_status(temp_dir.path());
        assert!(result.is_ok());
        let status = result.unwrap();
        assert!(!status.service.running);
        assert_eq!(status.tasks.in_progress, 0);
        assert_eq!(status.tasks.ready, 0);
    }

    #[test]
    fn test_gather_task_summary() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        // Open ready task
        graph.add_node(Node::Task(make_task("t1", "Ready Task")));

        // In-progress task
        let mut t2 = make_task("t2", "In Progress");
        t2.status = Status::InProgress;
        graph.add_node(Node::Task(t2));

        // Done task
        let mut t3 = make_task("t3", "Done Task");
        t3.status = Status::Done;
        t3.completed_at = Some(Utc::now().to_rfc3339());
        graph.add_node(Node::Task(t3));

        // Blocked task
        let mut t4 = make_task("t4", "Blocked");
        t4.blocked_by = vec!["t1".to_string()];
        graph.add_node(Node::Task(t4));

        save_graph(&graph, &path).unwrap();

        let summary = gather_task_summary(temp_dir.path()).unwrap();
        assert_eq!(summary.ready, 1);
        assert_eq!(summary.in_progress, 1);
        assert_eq!(summary.done_total, 1);
        assert_eq!(summary.done_today, 1);
        assert_eq!(summary.blocked, 1);
    }

    #[test]
    fn test_gather_recent_activity() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        for i in 1..=7 {
            let mut t = make_task(&format!("t{}", i), &format!("Task {}", i));
            t.status = Status::Done;
            // Stagger completion times
            let ts = Utc::now() - chrono::Duration::hours(i as i64);
            t.completed_at = Some(ts.to_rfc3339());
            graph.add_node(Node::Task(t));
        }

        save_graph(&graph, &path).unwrap();

        let recent = gather_recent_activity(temp_dir.path()).unwrap();
        // Should return 5 most recent
        assert_eq!(recent.len(), 5);
        // Most recent should be first (t1 is most recent)
        assert_eq!(recent[0].task_id, "t1");
    }
}

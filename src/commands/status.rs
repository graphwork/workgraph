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
use workgraph::check::{OrphanRef, check_orphans};
use workgraph::graph::{CycleAnalysis, Status};
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

/// A dangling dependency (task depends on a non-existent task)
#[derive(Debug, Clone, serde::Serialize)]
struct DanglingDep {
    task_id: String,
    missing_dep: String,
    relation: String,
}

/// A task with repeated verify failures
#[derive(Debug, Clone, serde::Serialize)]
struct VerifyFailingTask {
    task_id: String,
    failures: u32,
    verify_command: String,
}

/// Active cycle timing info
#[derive(Debug, Clone, serde::Serialize)]
struct CycleTimingInfo {
    task_id: String,
    iteration: u32,
    max_iterations: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_iteration_completed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_due: Option<String>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay: Option<String>,
}

/// Full status output
#[derive(Debug, Clone, serde::Serialize)]
struct StatusOutput {
    service: ServiceStatusInfo,
    coordinator: CoordinatorInfo,
    agents: AgentSummaryInfo,
    tasks: TaskSummaryInfo,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cycles: Vec<CycleTimingInfo>,
    recent: Vec<RecentActivityEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dangling_deps: Vec<DanglingDep>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    verify_failing: Vec<VerifyFailingTask>,
}

pub fn run(dir: &Path, json: bool, show_all: bool) -> Result<()> {
    let status = gather_status(dir, show_all)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_status(&status);
    }

    Ok(())
}

fn gather_status(dir: &Path, show_all: bool) -> Result<StatusOutput> {
    // 1. Service status
    let service = gather_service_status(dir)?;

    // 2. Coordinator config
    let coordinator = gather_coordinator_info(dir);

    // 3. Agent summary
    let agents = gather_agent_summary(dir);

    // 4. Task summary
    let tasks = gather_task_summary(dir, show_all)?;

    // 5. Cycle timing (legacy compaction widget removed alongside .compact-N retirement)
    let cycles = gather_cycle_timing(dir);

    // 6. Recent activity
    let recent = gather_recent_activity(dir, show_all)?;

    // 7. Dangling dependencies
    let dangling_deps = gather_dangling_deps(dir);

    // 8. Verify-failing tasks
    let verify_failing = gather_verify_failing(dir, show_all);

    Ok(StatusOutput {
        service,
        coordinator,
        agents,
        tasks,
        cycles,
        recent,
        dangling_deps,
        verify_failing,
    })
}

fn gather_service_status(dir: &Path) -> Result<ServiceStatusInfo> {
    let state = ServiceState::load(dir)?;

    match state {
        Some(s) if is_process_alive(s.pid) => {
            let uptime = chrono::DateTime::parse_from_rfc3339(&s.started_at)
                .map(|started| {
                    let now = chrono::Utc::now();
                    let duration = now.signed_duration_since(started);
                    workgraph::format_duration(duration.num_seconds(), false)
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
    if let Some(coord) = CoordinatorState::load_for(dir, 0) {
        return CoordinatorInfo {
            max_agents: coord.max_agents,
            executor: coord.executor,
            model: coord.model,
            poll_interval: coord.poll_interval,
        };
    }

    // Fall back to config file
    let config = workgraph::config::Config::load_or_default(dir);
    CoordinatorInfo {
        max_agents: config.coordinator.max_agents,
        executor: config.coordinator.effective_executor(),
        model: config.coordinator.model,
        poll_interval: config.coordinator.poll_interval,
    }
}

fn gather_agent_summary(dir: &Path) -> AgentSummaryInfo {
    let registry = AgentRegistry::load_or_warn(dir);
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

    AgentSummaryInfo {
        alive,
        dead,
        active,
    }
}

fn gather_task_summary(dir: &Path, show_all: bool) -> Result<TaskSummaryInfo> {
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
    let mut ready_count = 0;
    let mut blocked = 0;
    let mut delayed = 0;
    let mut done_today = 0;
    let mut done_total = 0;

    let today_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always valid")
        .and_utc();

    for task in graph.tasks() {
        if !show_all && task.id.starts_with('.') {
            continue;
        }
        match task.status {
            Status::Open => {
                if ready_ids.contains(task.id.as_str()) {
                    ready_count += 1;
                } else {
                    // Distinguish delayed (waiting on ready_after) from blocked (waiting on deps)
                    let has_future_ready_after = task.ready_after.as_ref().is_some_and(|ra| {
                        ra.parse::<DateTime<Utc>>()
                            .map(|ts| ts > now)
                            .unwrap_or(false)
                    });
                    let all_blockers_done = task.after.iter().all(|bid| {
                        graph
                            .get_task(bid)
                            .map(|t| t.status.is_terminal())
                            .unwrap_or(true)
                    });
                    if has_future_ready_after && all_blockers_done {
                        delayed += 1;
                    } else {
                        blocked += 1;
                    }
                }
            }
            Status::InProgress => {
                in_progress += 1;
            }
            Status::Done => {
                done_total += 1;
                // Check if completed today
                if let Some(ref completed_at) = task.completed_at {
                    match completed_at.parse::<DateTime<Utc>>() {
                        Ok(completed) if completed >= today_start => {
                            done_today += 1;
                        }
                        Ok(_) => {} // completed before today
                        Err(e) => {
                            eprintln!(
                                "warning: task '{}' has malformed completed_at timestamp '{}': {}",
                                task.id, completed_at, e
                            );
                        }
                    }
                }
            }
            Status::Blocked => {
                blocked += 1;
            }
            Status::Incomplete => {
                // Retryable: counted like open work
            }
            Status::Failed | Status::Abandoned | Status::Waiting | Status::PendingValidation => {
                // Terminal/parked states, not counted in summary
            }
        }
    }

    Ok(TaskSummaryInfo {
        in_progress,
        ready: ready_count,
        blocked,
        delayed,
        done_today,
        done_total,
    })
}

fn gather_recent_activity(dir: &Path, show_all: bool) -> Result<Vec<RecentActivityEntry>> {
    let path = graph_path(dir);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Collect done tasks with completion timestamps
    let mut completed: Vec<_> = graph
        .tasks()
        .filter(|t| show_all || !t.id.starts_with('.'))
        .filter(|t| t.status == Status::Done && t.completed_at.is_some())
        .filter_map(|t| {
            let completed_at = t.completed_at.as_ref()?;
            match completed_at.parse::<DateTime<Utc>>() {
                Ok(ts) => Some((ts, t.id.clone(), t.title.clone())),
                Err(e) => {
                    eprintln!(
                        "warning: task '{}' has malformed completed_at timestamp '{}': {}",
                        t.id, completed_at, e
                    );
                    None
                }
            }
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

fn gather_cycle_timing(dir: &Path) -> Vec<CycleTimingInfo> {
    let path = super::graph_path(dir);
    if !path.exists() {
        return Vec::new();
    }

    let graph = match load_graph(&path) {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };

    let cycle_analysis = CycleAnalysis::from_graph(&graph);
    let now = Utc::now();
    let mut results = Vec::new();

    for cycle in &cycle_analysis.cycles {
        // Find the config owner (cycle header with CycleConfig)
        let config_owner = cycle.members.iter().find_map(|mid| {
            let task = graph.get_task(mid)?;
            task.cycle_config.as_ref()?;
            Some(task)
        });

        let Some(owner) = config_owner else {
            continue;
        };
        let cc = owner.cycle_config.as_ref().unwrap();

        // Use last_iteration_completed_at from the config owner, falling back to completed_at
        let last_completed = owner
            .last_iteration_completed_at
            .as_ref()
            .or(owner.completed_at.as_ref())
            .cloned();

        // Next due: use ready_after if present, otherwise compute from last_completed + delay
        let next_due = owner.ready_after.clone().or_else(|| {
            let delay_secs = cc
                .delay
                .as_ref()
                .and_then(|d| workgraph::graph::parse_delay(d))?;
            let last_ts = last_completed.as_ref()?.parse::<DateTime<Utc>>().ok()?;
            let next = last_ts + chrono::Duration::seconds(delay_secs as i64);
            if next > now {
                Some(next.to_rfc3339())
            } else {
                None
            }
        });

        results.push(CycleTimingInfo {
            task_id: owner.id.clone(),
            iteration: owner.loop_iteration + 1, // 1-based display
            max_iterations: cc.max_iterations,
            last_iteration_completed: last_completed,
            next_due,
            status: owner.status.to_string(),
            delay: cc.delay.clone(),
        });
    }

    results
}

fn gather_dangling_deps(dir: &Path) -> Vec<DanglingDep> {
    let path = super::graph_path(dir);
    if !path.exists() {
        return Vec::new();
    }

    let graph = match load_graph(&path) {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };

    let orphans: Vec<OrphanRef> = check_orphans(&graph);

    // Only surface "after" relation orphans — those are the ones that block tasks
    orphans
        .into_iter()
        .filter(|o| o.relation == "after")
        .map(|o| DanglingDep {
            task_id: o.from,
            missing_dep: o.to,
            relation: o.relation,
        })
        .collect()
}

fn gather_verify_failing(dir: &Path, show_all: bool) -> Vec<VerifyFailingTask> {
    let path = super::graph_path(dir);
    if !path.exists() {
        return Vec::new();
    }

    let graph = match load_graph(&path) {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };

    graph
        .tasks()
        .filter(|t| show_all || !t.id.starts_with('.'))
        .filter(|t| {
            t.verify_failures > 0 && t.status != Status::Failed && t.status != Status::Abandoned
        })
        .map(|t| VerifyFailingTask {
            task_id: t.id.clone(),
            failures: t.verify_failures,
            verify_command: t.verify.clone().unwrap_or_default(),
        })
        .collect()
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

    // Line 2: Dispatcher config
    let model_str = status.coordinator.model.as_deref().unwrap_or("default");
    println!(
        "Dispatcher: max={}, executor={}, model={}, poll={}s",
        status.coordinator.max_agents,
        status.coordinator.executor,
        model_str,
        status.coordinator.poll_interval
    );

    // Line 4+: Agent summary
    println!();
    if status.agents.alive == 0 && status.agents.dead == 0 {
        println!("Agents: none");
    } else {
        println!(
            "Agents ({} alive, {} dead):",
            status.agents.alive, status.agents.dead
        );
        for agent in &status.agents.active {
            // Truncate task_id if too long (char-safe to avoid UTF-8 panic)
            let task_display = if agent.task_id.chars().count() > 24 {
                format!("{}...", agent.task_id.chars().take(21).collect::<String>())
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

    // Active cycles
    if !status.cycles.is_empty() {
        println!();
        println!("Cycles:");
        for cycle in &status.cycles {
            let iter_str = if cycle.max_iterations == 0 {
                format!("{}/unlimited", cycle.iteration)
            } else {
                format!("{}/{}", cycle.iteration, cycle.max_iterations)
            };

            let last_str = match cycle.last_iteration_completed {
                Some(ref ts) => {
                    if let Ok(parsed) = ts.parse::<DateTime<Utc>>() {
                        let ago = Utc::now().signed_duration_since(parsed).num_seconds();
                        format!("last: {} ago", workgraph::format_duration(ago, true))
                    } else {
                        "last: unknown".to_string()
                    }
                }
                None => "last: never".to_string(),
            };

            let next_str = match cycle.next_due {
                Some(ref ts) => {
                    if let Ok(parsed) = ts.parse::<DateTime<Utc>>() {
                        let now = Utc::now();
                        if parsed > now {
                            let secs = (parsed - now).num_seconds();
                            format!("next: in {}", workgraph::format_duration(secs, true))
                        } else {
                            "next: ready".to_string()
                        }
                    } else {
                        String::new()
                    }
                }
                None => String::new(),
            };

            let timing = if next_str.is_empty() {
                last_str
            } else {
                format!("{}, {}", last_str, next_str)
            };

            println!(
                "  {} [{}] iter {} — {}",
                cycle.task_id, cycle.status, iter_str, timing
            );
        }
    }

    // Attention: verify-failing tasks
    if !status.verify_failing.is_empty() {
        println!();
        println!(
            "\x1b[33m⚠ Attention:\x1b[0m {} task(s) have verify failures:",
            status.verify_failing.len()
        );
        for vf in &status.verify_failing {
            let cmd_snippet: String = vf.verify_command.chars().take(60).collect();
            println!(
                "  VERIFY FAILING: \x1b[33m{}\x1b[0m — verify command has failed {} times: {}",
                vf.task_id, vf.failures, cmd_snippet
            );
        }
    }

    // Attention: dangling dependencies
    if !status.dangling_deps.is_empty() {
        println!();
        println!(
            "\x1b[33m⚠ Attention:\x1b[0m {} task(s) have unresolved dependencies:",
            status.dangling_deps.len()
        );
        for dep in &status.dangling_deps {
            println!(
                "  {} → \x1b[31m{}\x1b[0m (missing)",
                dep.task_id, dep.missing_dep
            );
        }
        println!("  Run 'wg check' for details.");
    }

    // Recent activity
    if !status.recent.is_empty() {
        println!();
        println!("Recent:");
        for entry in &status.recent {
            // Truncate title if too long (char-safe to avoid UTF-8 panic)
            let title_display = if entry.title.chars().count() > 50 {
                format!("{}...", entry.title.chars().take(47).collect::<String>())
            } else {
                entry.title.clone()
            };
            println!("  {}  {} [done]", entry.time, title_display);
        }
    }
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
            ..Task::default()
        }
    }

    #[test]
    fn test_gather_status_empty() {
        let temp_dir = TempDir::new().unwrap();
        let result = gather_status(temp_dir.path(), true);
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
        t4.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t4));

        save_graph(&graph, &path).unwrap();

        let summary = gather_task_summary(temp_dir.path(), true).unwrap();
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

        let recent = gather_recent_activity(temp_dir.path(), true).unwrap();
        // Should return 5 most recent
        assert_eq!(recent.len(), 5);
        // Most recent should be first (t1 is most recent)
        assert_eq!(recent[0].task_id, "t1");
    }

    #[test]
    fn test_gather_task_summary_empty_graph() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let summary = gather_task_summary(temp_dir.path(), true).unwrap();
        assert_eq!(summary.ready, 0);
        assert_eq!(summary.in_progress, 0);
        assert_eq!(summary.done_total, 0);
        assert_eq!(summary.blocked, 0);
    }

    #[test]
    fn test_gather_task_summary_delayed_vs_blocked() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        // Delayed task: no blockers but has future ready_after
        let mut delayed = make_task("delayed", "Delayed task");
        let future = Utc::now() + chrono::Duration::hours(1);
        delayed.ready_after = Some(future.to_rfc3339());
        graph.add_node(Node::Task(delayed));

        // Blocked task: has unfinished blocker
        let mut blocked = make_task("blocked", "Blocked task");
        blocked.after = vec!["blocker".to_string()];
        graph.add_node(Node::Task(blocked));

        // The blocker itself (open)
        graph.add_node(Node::Task(make_task("blocker", "Blocker")));

        save_graph(&graph, &path).unwrap();

        let summary = gather_task_summary(temp_dir.path(), true).unwrap();
        assert_eq!(summary.delayed, 1);
        assert_eq!(summary.blocked, 1);
        assert_eq!(summary.ready, 1); // blocker is ready
    }

    #[test]
    fn test_gather_recent_activity_empty() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let recent = gather_recent_activity(temp_dir.path(), true).unwrap();
        assert_eq!(recent.len(), 0);
    }

    #[test]
    fn test_display_status_unicode_truncation() {
        // Verify that Unicode task titles and IDs don't panic on truncation.
        // This uses the print_status path indirectly by testing the truncation logic.
        // Need 51+ characters to trigger truncation
        let long_unicode_title = "日本語のタスク名前がとても長いのでトランケートされるべきです。テスト用の文字列をもっと追加しますよ。はい。";
        assert!(
            long_unicode_title.chars().count() > 50,
            "title has {} chars, need >50",
            long_unicode_title.chars().count()
        );
        // Simulate the truncation logic from print_status
        let title_display = if long_unicode_title.chars().count() > 50 {
            format!(
                "{}...",
                long_unicode_title.chars().take(47).collect::<String>()
            )
        } else {
            long_unicode_title.to_string()
        };
        assert!(title_display.ends_with("..."));
        assert!(title_display.chars().count() <= 50);

        // Need 25+ characters to trigger truncation
        let long_unicode_id = "タスクIDが長すぎる場合のテストタスクIDが長すぎる場合";
        assert!(
            long_unicode_id.chars().count() > 24,
            "id has {} chars, need >24",
            long_unicode_id.chars().count()
        );
        let task_display = if long_unicode_id.chars().count() > 24 {
            format!(
                "{}...",
                long_unicode_id.chars().take(21).collect::<String>()
            )
        } else {
            long_unicode_id.to_string()
        };
        assert!(task_display.ends_with("..."));
        assert!(task_display.chars().count() <= 24);
    }

    #[test]
    fn test_gather_dangling_deps_none() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        let dangling = gather_dangling_deps(temp_dir.path());
        assert!(dangling.is_empty());
    }

    #[test]
    fn test_gather_dangling_deps_found() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.after = vec!["nonexistent".to_string()];
        graph.add_node(Node::Task(t1));
        save_graph(&graph, &path).unwrap();

        let dangling = gather_dangling_deps(temp_dir.path());
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].task_id, "t1");
        assert_eq!(dangling[0].missing_dep, "nonexistent");
    }

    #[test]
    fn test_gather_dangling_deps_resolves_when_created() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        // First: create task with dangling dep
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.after = vec!["t2".to_string()];
        graph.add_node(Node::Task(t1));
        save_graph(&graph, &path).unwrap();

        let dangling = gather_dangling_deps(temp_dir.path());
        assert_eq!(dangling.len(), 1);

        // Now create the missing task
        graph.add_node(Node::Task(make_task("t2", "Task 2")));
        save_graph(&graph, &path).unwrap();

        let dangling = gather_dangling_deps(temp_dir.path());
        assert!(
            dangling.is_empty(),
            "dangling dep should auto-resolve when target is created"
        );
    }

    #[test]
    fn test_gather_dangling_deps_no_graph() {
        let temp_dir = TempDir::new().unwrap();
        // No graph file at all
        let dangling = gather_dangling_deps(temp_dir.path());
        assert!(dangling.is_empty());
    }

    #[test]
    fn test_gather_verify_failing_none() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Normal task")));
        save_graph(&graph, &path).unwrap();

        let failing = gather_verify_failing(temp_dir.path(), true);
        assert!(failing.is_empty());
    }

    #[test]
    fn test_gather_verify_failing_found() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Failing verify");
        task.status = Status::InProgress;
        task.verify = Some("cargo test".to_string());
        task.verify_failures = 2;
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let failing = gather_verify_failing(temp_dir.path(), true);
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].task_id, "t1");
        assert_eq!(failing[0].failures, 2);
        assert_eq!(failing[0].verify_command, "cargo test");
    }

    #[test]
    fn test_gather_verify_failing_excludes_failed_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        // Task already failed (circuit breaker tripped) — should not appear
        let mut task = make_task("t1", "Already failed");
        task.status = Status::Failed;
        task.verify_failures = 3;
        task.verify = Some("cargo test".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let failing = gather_verify_failing(temp_dir.path(), true);
        assert!(
            failing.is_empty(),
            "Failed tasks should not be listed as verify-failing"
        );
    }

    #[test]
    fn test_gather_verify_failing_no_graph() {
        let temp_dir = TempDir::new().unwrap();
        let failing = gather_verify_failing(temp_dir.path(), true);
        assert!(failing.is_empty());
    }

    #[test]
    fn test_gather_task_summary_hides_dot_prefixed() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("real-task", "Real task")));

        let mut dot_task = make_task(".assign-real-task", "Assign real task");
        dot_task.status = Status::InProgress;
        graph.add_node(Node::Task(dot_task));

        let mut dot_done = make_task(".flip-real-task", "FLIP real task");
        dot_done.status = Status::Done;
        dot_done.completed_at = Some(Utc::now().to_rfc3339());
        graph.add_node(Node::Task(dot_done));

        save_graph(&graph, &path).unwrap();

        // show_all=false: only count non-dot-prefixed tasks
        let summary = gather_task_summary(temp_dir.path(), false).unwrap();
        assert_eq!(summary.ready, 1);
        assert_eq!(summary.in_progress, 0);
        assert_eq!(summary.done_total, 0);

        // show_all=true: count everything
        let summary = gather_task_summary(temp_dir.path(), true).unwrap();
        assert_eq!(summary.ready, 1);
        assert_eq!(summary.in_progress, 1);
        assert_eq!(summary.done_total, 1);
    }

    #[test]
    fn test_gather_recent_activity_hides_dot_prefixed() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut real_done = make_task("real-done", "Real done");
        real_done.status = Status::Done;
        real_done.completed_at = Some(Utc::now().to_rfc3339());
        graph.add_node(Node::Task(real_done));

        let mut dot_done = make_task(".flip-real-done", "FLIP done");
        dot_done.status = Status::Done;
        dot_done.completed_at = Some(Utc::now().to_rfc3339());
        graph.add_node(Node::Task(dot_done));

        save_graph(&graph, &path).unwrap();

        let recent = gather_recent_activity(temp_dir.path(), false).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].task_id, "real-done");

        let recent = gather_recent_activity(temp_dir.path(), true).unwrap();
        assert_eq!(recent.len(), 2);
    }
}

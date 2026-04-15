//! Mid-turn state injection for the native executor.
//!
//! Before each API call, the agent loop collects dynamic state changes
//! and injects them as an ephemeral `<system-reminder>` block. This block
//! is NOT persisted to the journal — it appears once, informs the current
//! turn, then vanishes.
//!
//! Four injection sources:
//! 1. **Pending messages**: `wg msg` messages from other agents/coordinator/humans
//! 2. **Graph state changes**: Dependency completions, new tasks, blocker changes
//! 3. **Background job completions**: Completed bg jobs since last check
//! 4. **Context pressure**: Warnings when approaching context limits

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::executor::native::background::{Job, JobStatus};
use crate::graph::Status;
use crate::messages;
use crate::parser;

/// Snapshot of dependency statuses, used to detect changes between turns.
#[derive(Debug, Clone, PartialEq)]
pub struct DependencySnapshot {
    /// (task_id, status) pairs for all dependencies (`after` edges).
    pub deps: Vec<(String, Status)>,
}

impl DependencySnapshot {
    /// Load current dependency statuses from the graph file.
    pub fn from_graph(graph_path: &Path, task_id: &str) -> Option<Self> {
        let graph = parser::load_graph(graph_path).ok()?;
        let task = graph.get_task(task_id)?;
        let deps: Vec<(String, Status)> = task
            .after
            .iter()
            .filter_map(|dep_id| graph.get_task(dep_id).map(|t| (dep_id.clone(), t.status)))
            .collect();
        Some(DependencySnapshot { deps })
    }

    /// Compute changes between a previous snapshot and the current one.
    ///
    /// Returns a list of human-readable change descriptions.
    pub fn diff(&self, current: &DependencySnapshot) -> Vec<String> {
        let mut changes = Vec::new();

        for (id, new_status) in &current.deps {
            if let Some((_, old_status)) = self.deps.iter().find(|(old_id, _)| old_id == id) {
                if old_status != new_status {
                    changes.push(format!(
                        "Dependency '{}' changed: {} → {}",
                        id, old_status, new_status
                    ));
                }
            } else {
                // New dependency appeared
                changes.push(format!(
                    "New dependency '{}' appeared (status: {})",
                    id, new_status
                ));
            }
        }

        // Check for removed dependencies
        for (id, _) in &self.deps {
            if !current.deps.iter().any(|(cur_id, _)| cur_id == id) {
                changes.push(format!("Dependency '{}' was removed", id));
            }
        }

        changes
    }
}

/// Collects dynamic state changes and formats them as ephemeral injections.
pub struct StateInjector {
    /// Path to the `.workgraph/` directory.
    workgraph_dir: PathBuf,
    /// Task this agent is working on.
    task_id: String,
    /// Agent ID for message cursor management.
    agent_id: String,
    /// Last-seen dependency snapshot (for detecting changes).
    last_dep_snapshot: Option<DependencySnapshot>,
    /// Set of background job IDs whose completions have already been reported.
    reported_bg_jobs: HashSet<String>,
}

impl StateInjector {
    /// Create a new state injector.
    pub fn new(workgraph_dir: PathBuf, task_id: String, agent_id: String) -> Self {
        // Take initial dependency snapshot so we only report *changes*
        let graph_path = workgraph_dir.join("graph.jsonl");
        let initial_snapshot = DependencySnapshot::from_graph(&graph_path, &task_id);

        Self {
            workgraph_dir,
            task_id,
            agent_id,
            last_dep_snapshot: initial_snapshot,
            reported_bg_jobs: HashSet::new(),
        }
    }

    /// Collect all pending injections and return a formatted system-reminder block.
    ///
    /// Returns `None` if there are no injections to make.
    /// When messages are returned, they are marked as read (cursor advances).
    pub fn collect_injections(
        &mut self,
        context_pressure_warning: Option<String>,
    ) -> Option<String> {
        let mut sections = Vec::new();

        // 1. Pending messages
        if let Some(msg_section) = self.collect_messages() {
            sections.push(msg_section);
        }

        // 2. Graph state changes
        if let Some(graph_section) = self.collect_graph_changes() {
            sections.push(graph_section);
        }

        // 3. Background job completions
        if let Some(bg_section) = self.collect_bg_completions() {
            sections.push(bg_section);
        }

        // 4. Context pressure warning
        if let Some(warning) = context_pressure_warning {
            sections.push(format!("### Context Pressure\n{}", warning));
        }

        // 5. Time budget awareness
        if let Some(time_section) = Self::collect_time_budget() {
            sections.push(time_section);
        }

        if sections.is_empty() {
            return None;
        }

        let body = sections.join("\n\n");
        Some(format!(
            "<system-reminder>\n## Live State Update\n\n{}\n</system-reminder>",
            body
        ))
    }

    /// Check for pending messages and format them.
    ///
    /// Uses `read_unread` which advances the cursor, so messages are
    /// only injected once.
    fn collect_messages(&self) -> Option<String> {
        let msgs =
            messages::read_unread(&self.workgraph_dir, &self.task_id, &self.agent_id).ok()?;

        if msgs.is_empty() {
            return None;
        }

        let mut lines = Vec::with_capacity(msgs.len() + 1);
        lines.push("### New Messages".to_string());
        for msg in &msgs {
            let priority = if msg.priority == "urgent" {
                " [URGENT]"
            } else {
                ""
            };
            lines.push(format!("- **{}**{}: {}", msg.sender, priority, msg.body));
        }
        Some(lines.join("\n"))
    }

    /// Check time budget from environment variables and return a time-awareness section.
    ///
    /// Reads `WG_TASK_TIMEOUT_SECS` and `WG_SPAWN_EPOCH` set at agent spawn time.
    /// Returns a prompt section when both are set, informing the agent of remaining time.
    ///
    /// Disabled under `#[cfg(test)]` because env vars from a parent agent process
    /// leak into `cargo test`, causing unrelated state-injection tests to fail.
    #[cfg(test)]
    fn collect_time_budget() -> Option<String> {
        None
    }

    /// Check time budget from environment variables and return a time-awareness section.
    #[cfg(not(test))]
    fn collect_time_budget() -> Option<String> {
        let timeout_secs: u64 = std::env::var("WG_TASK_TIMEOUT_SECS").ok()?.parse().ok()?;
        let spawn_epoch: u64 = std::env::var("WG_SPAWN_EPOCH").ok()?.parse().ok()?;

        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let elapsed = now_epoch.saturating_sub(spawn_epoch);
        let remaining = timeout_secs.saturating_sub(elapsed);
        let budget_min = timeout_secs / 60;
        let elapsed_min = elapsed / 60;
        let remaining_min = remaining / 60;
        let eighty_pct_min = (timeout_secs * 80 / 100) / 60;
        let pct_used = if timeout_secs > 0 {
            (elapsed * 100 / timeout_secs).min(100)
        } else {
            100
        };

        let mut section = format!(
            "### Time Budget\n\
             - Total budget: {}s ({}min)\n\
             - Elapsed: {}s ({}min) — {}% used\n\
             - Remaining: {}s ({}min)",
            timeout_secs, budget_min, elapsed, elapsed_min, pct_used, remaining, remaining_min,
        );

        if pct_used >= 80 {
            section.push_str(&format!(
                "\n\n**CRITICAL — past {}min (80% of budget).** \
                 Stop iterating and commit your best work NOW:\n\
                 1. `git add <files> && git commit -m \"partial: <description>\"`\n\
                 2. `wg log <task-id> \"Partial progress: <what's done, what remains>\"`\n\
                 3. `wg done <task-id>`\n\n\
                 Do NOT start new iterations, refactoring, or polish. \
                 Committed partial progress is infinitely more valuable than uncommitted perfect work.",
                eighty_pct_min,
            ));
        }

        Some(section)
    }

    /// Check for completed background jobs that haven't been reported yet.
    ///
    /// Scans the jobs directory (`workgraph_dir/jobs/`) for `.json` job files,
    /// identifies jobs in a terminal state that haven't been reported, and
    /// formats a `<context-update>` block with job ID, exit code, duration,
    /// and the last 20 lines of output.
    fn collect_bg_completions(&mut self) -> Option<String> {
        let jobs_dir = self.workgraph_dir.join("jobs");
        if !jobs_dir.exists() {
            return None;
        }

        let entries = std::fs::read_dir(&jobs_dir).ok()?;
        let mut updates = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let job: Job = match serde_json::from_str(&content) {
                Ok(j) => j,
                Err(_) => continue,
            };

            // Only report terminal jobs that haven't been reported yet
            if !job.status.is_terminal() || self.reported_bg_jobs.contains(&job.id) {
                continue;
            }

            // Mark as reported
            self.reported_bg_jobs.insert(job.id.clone());

            // Compute duration
            let duration_str = if let Some(finished) = job.finished_at {
                let dur = finished.signed_duration_since(job.created_at);
                format!("{}s", dur.num_seconds())
            } else {
                "unknown".to_string()
            };

            // Read last 20 lines of output
            let output_tail = Self::read_tail_lines(&job.log_path, 20);

            let status_str = match job.status {
                JobStatus::Completed => "completed",
                JobStatus::Failed => "failed",
                JobStatus::Cancelled => "cancelled",
                JobStatus::Orphaned => "orphaned",
                JobStatus::Running => unreachable!(),
            };

            let exit_code_str = match job.exit_code {
                Some(code) => code.to_string(),
                None => "unknown".to_string(),
            };

            let mut update = format!(
                "<context-update source=\"bg-job\">\n\
                 Job: {} ({})\n\
                 Command: {}\n\
                 Status: {}\n\
                 Exit code: {}\n\
                 Duration: {}",
                job.id, job.name, job.command, status_str, exit_code_str, duration_str,
            );

            if !output_tail.is_empty() {
                update.push_str(&format!(
                    "\nOutput (last 20 lines):\n```\n{}\n```",
                    output_tail
                ));
            }

            update.push_str("\n</context-update>");
            updates.push(update);
        }

        if updates.is_empty() {
            return None;
        }

        let mut section = "### Background Job Completions\n".to_string();
        section.push_str(&updates.join("\n\n"));
        Some(section)
    }

    /// Read the last N lines from a file, returning them as a single string.
    fn read_tail_lines(path: &Path, n: usize) -> String {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return String::new(),
        };
        let reader = BufReader::new(file);
        let all_lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
        if all_lines.len() <= n {
            all_lines.join("\n")
        } else {
            all_lines[all_lines.len() - n..].join("\n")
        }
    }

    /// Check for graph state changes (dependency status changes).
    fn collect_graph_changes(&mut self) -> Option<String> {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let current = DependencySnapshot::from_graph(&graph_path, &self.task_id)?;

        let changes = if let Some(ref prev) = self.last_dep_snapshot {
            prev.diff(&current)
        } else {
            // First time seeing the graph — no "changes" to report
            Vec::new()
        };

        // Update snapshot for next turn
        self.last_dep_snapshot = Some(current);

        if changes.is_empty() {
            return None;
        }

        let mut lines = Vec::with_capacity(changes.len() + 1);
        lines.push("### Graph Changes".to_string());
        for change in &changes {
            lines.push(format!("- {}", change));
        }
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a minimal workgraph directory with a graph and a task.
    fn setup_workgraph(dir: &Path, task_id: &str, deps: &[(&str, &str)]) {
        fs::create_dir_all(dir).unwrap();

        let mut lines = Vec::new();

        // Add dependency tasks (kind=task for JSONL node format)
        for (dep_id, status) in deps {
            lines.push(format!(
                r#"{{"kind":"task","id":"{}","title":"Dep {}","status":"{}"}}"#,
                dep_id, dep_id, status
            ));
        }

        // Add the main task with after edges
        let after: Vec<String> = deps.iter().map(|(id, _)| format!("\"{}\"", id)).collect();
        lines.push(format!(
            r#"{{"kind":"task","id":"{}","title":"Main task","status":"in-progress","after":[{}]}}"#,
            task_id,
            after.join(",")
        ));

        fs::write(dir.join("graph.jsonl"), lines.join("\n")).unwrap();
    }

    /// Write a message file for a task.
    ///
    /// Messages are stored at `workgraph_dir/messages/{task_id}.jsonl`.
    fn write_message(dir: &Path, task_id: &str, msg_id: u64, sender: &str, body: &str) {
        let msg_dir = dir.join("messages");
        fs::create_dir_all(&msg_dir).unwrap();

        let msg = serde_json::json!({
            "id": msg_id,
            "timestamp": "2026-04-03T12:00:00Z",
            "sender": sender,
            "body": body,
            "priority": "normal",
            "status": "sent"
        });

        // Append to {task_id}.jsonl
        let msg_file = msg_dir.join(format!("{}.jsonl", task_id));
        let mut content = fs::read_to_string(&msg_file).unwrap_or_default();
        content.push_str(&serde_json::to_string(&msg).unwrap());
        content.push('\n');
        fs::write(&msg_file, content).unwrap();
    }

    /// Create a completed background job JSON file in the jobs directory.
    fn create_bg_job(
        dir: &Path,
        job_id: &str,
        name: &str,
        command: &str,
        status: &str,
        exit_code: Option<i32>,
        log_content: Option<&str>,
    ) {
        let jobs_dir = dir.join("jobs");
        fs::create_dir_all(&jobs_dir).unwrap();

        let log_path = jobs_dir.join(format!("{}.log", job_id));
        if let Some(content) = log_content {
            fs::write(&log_path, content).unwrap();
        }

        let finished_at = if status != "running" {
            "\"2026-04-13T12:01:00Z\""
        } else {
            "null"
        };
        let exit_code_json = match exit_code {
            Some(c) => c.to_string(),
            None => "null".to_string(),
        };

        let job_json = format!(
            r#"{{
  "id": "{}",
  "name": "{}",
  "command": "{}",
  "status": "{}",
  "pid": 12345,
  "exit_code": {},
  "created_at": "2026-04-13T12:00:00Z",
  "updated_at": "2026-04-13T12:01:00Z",
  "finished_at": {},
  "log_path": "{}",
  "working_dir": "/tmp"
}}"#,
            job_id,
            name,
            command,
            status,
            exit_code_json,
            finished_at,
            log_path.display(),
        );

        fs::write(jobs_dir.join(format!("job-{}.json", job_id)), job_json).unwrap();
    }

    #[test]
    fn test_state_injection_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[("dep-a", "in-progress")]);

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // No messages, no graph changes, no pressure → None
        let result = injector.collect_injections(None);
        assert!(result.is_none(), "Expected no injection, got: {:?}", result);
    }

    #[test]
    fn test_state_injection_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        write_message(wg_dir, "my-task", 1, "coordinator", "Please hurry up");

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let result = injector.collect_injections(None);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("<system-reminder>"));
        assert!(text.contains("New Messages"));
        assert!(text.contains("coordinator"));
        assert!(text.contains("Please hurry up"));

        // Second call — message already read, cursor advanced → no injection
        let result2 = injector.collect_injections(None);
        assert!(result2.is_none(), "Messages should not repeat");
    }

    #[test]
    fn test_state_injection_graph_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(
            wg_dir,
            "my-task",
            &[("dep-a", "in-progress"), ("dep-b", "open")],
        );

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // No changes yet
        let result = injector.collect_injections(None);
        assert!(result.is_none());

        // Now dep-a completes
        setup_workgraph(wg_dir, "my-task", &[("dep-a", "done"), ("dep-b", "open")]);

        let result = injector.collect_injections(None);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("Graph Changes"));
        assert!(text.contains("dep-a"));
        assert!(text.contains("done"));

        // Next call: no new changes
        let result = injector.collect_injections(None);
        assert!(result.is_none());
    }

    #[test]
    fn test_state_injection_context_pressure() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let warning = "You're at 82% context capacity. Consider wrapping up.".to_string();
        let result = injector.collect_injections(Some(warning.clone()));
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("Context Pressure"));
        assert!(text.contains("82%"));
    }

    #[test]
    fn test_state_injection_combined() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[("dep-a", "in-progress")]);

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // First collect to set baseline (captures initial dep snapshot)
        let _ = injector.collect_injections(None);

        // Change graph state: dep-a completes
        setup_workgraph(wg_dir, "my-task", &[("dep-a", "done")]);

        // Add a message (messages dir is separate from graph.jsonl)
        write_message(wg_dir, "my-task", 1, "user", "Check this out");

        let warning = "Context at 85%".to_string();
        let result = injector.collect_injections(Some(warning));
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("New Messages"));
        assert!(text.contains("Graph Changes"));
        assert!(text.contains("Context Pressure"));
    }

    #[test]
    fn test_state_injection_urgent_message() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        // Write an urgent message
        let msg_dir = wg_dir.join("messages");
        fs::create_dir_all(&msg_dir).unwrap();
        let msg = serde_json::json!({
            "id": 1,
            "timestamp": "2026-04-03T12:00:00Z",
            "sender": "coordinator",
            "body": "Stop what you're doing",
            "priority": "urgent",
            "status": "sent"
        });
        fs::write(
            msg_dir.join("my-task.jsonl"),
            format!("{}\n", serde_json::to_string(&msg).unwrap()),
        )
        .unwrap();

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let result = injector.collect_injections(None);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("[URGENT]"));
        assert!(text.contains("Stop what you're doing"));
    }

    #[test]
    fn test_dependency_snapshot_diff() {
        let old = DependencySnapshot {
            deps: vec![
                ("a".into(), Status::InProgress),
                ("b".into(), Status::Open),
                ("c".into(), Status::Open),
            ],
        };

        let new = DependencySnapshot {
            deps: vec![
                ("a".into(), Status::Done),
                ("b".into(), Status::Open),
                // "c" removed, "d" added
                ("d".into(), Status::InProgress),
            ],
        };

        let changes = old.diff(&new);
        assert!(
            changes
                .iter()
                .any(|c| c.contains("'a'") && c.contains("done"))
        );
        assert!(!changes.iter().any(|c| c.contains("'b'")), "b unchanged");
        assert!(
            changes
                .iter()
                .any(|c| c.contains("'c'") && c.contains("removed"))
        );
        assert!(
            changes
                .iter()
                .any(|c| c.contains("'d'") && c.contains("appeared"))
        );
    }

    #[test]
    fn test_state_injection_ephemeral_format() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        write_message(wg_dir, "my-task", 1, "user", "hello");

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let result = injector.collect_injections(None).unwrap();

        // Must be wrapped in system-reminder tags
        assert!(result.starts_with("<system-reminder>"));
        assert!(result.ends_with("</system-reminder>"));
        assert!(result.contains("## Live State Update"));
    }

    #[test]
    fn test_state_injection_multiple_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        write_message(wg_dir, "my-task", 1, "alice", "First message");
        write_message(wg_dir, "my-task", 2, "bob", "Second message");
        write_message(wg_dir, "my-task", 3, "coordinator", "Third message");

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let result = injector.collect_injections(None).unwrap();
        assert!(result.contains("alice"));
        assert!(result.contains("First message"));
        assert!(result.contains("bob"));
        assert!(result.contains("Second message"));
        assert!(result.contains("coordinator"));
        assert!(result.contains("Third message"));

        // All consumed — next call returns None
        let result2 = injector.collect_injections(None);
        assert!(result2.is_none(), "All messages should be consumed");
    }

    #[test]
    fn test_dependency_snapshot_diff_identical() {
        let snap = DependencySnapshot {
            deps: vec![("a".into(), Status::InProgress), ("b".into(), Status::Open)],
        };
        let changes = snap.diff(&snap);
        assert!(
            changes.is_empty(),
            "Identical snapshots should produce no changes"
        );
    }

    #[test]
    fn test_dependency_snapshot_diff_both_empty() {
        let old = DependencySnapshot { deps: vec![] };
        let new = DependencySnapshot { deps: vec![] };
        let changes = old.diff(&new);
        assert!(
            changes.is_empty(),
            "Two empty snapshots should produce no changes"
        );
    }

    #[test]
    fn test_state_injection_nonexistent_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join("does-not-exist");
        // Don't create the directory — graph doesn't exist

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "no-task".into(), "agent-1".into());

        // Should handle missing graph gracefully (no crash, returns None)
        let result = injector.collect_injections(None);
        assert!(
            result.is_none(),
            "Missing graph should produce no injection"
        );
    }

    #[test]
    fn test_state_injection_multiple_dep_changes_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(
            wg_dir,
            "my-task",
            &[
                ("dep-a", "open"),
                ("dep-b", "open"),
                ("dep-c", "in-progress"),
            ],
        );

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // Consume baseline
        let _ = injector.collect_injections(None);

        // Multiple deps change simultaneously
        setup_workgraph(
            wg_dir,
            "my-task",
            &[
                ("dep-a", "done"),
                ("dep-b", "in-progress"),
                ("dep-c", "done"),
            ],
        );

        let result = injector.collect_injections(None).unwrap();
        assert!(result.contains("dep-a"), "Should report dep-a change");
        assert!(result.contains("dep-b"), "Should report dep-b change");
        assert!(result.contains("dep-c"), "Should report dep-c change");
    }

    #[test]
    fn test_state_injection_graph_change_not_re_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[("dep-a", "open")]);

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // Baseline
        let _ = injector.collect_injections(None);

        // dep-a changes to done
        setup_workgraph(wg_dir, "my-task", &[("dep-a", "done")]);

        let r1 = injector.collect_injections(None);
        assert!(r1.is_some(), "First change should be reported");

        // Graph unchanged — call again
        let r2 = injector.collect_injections(None);
        assert!(
            r2.is_none(),
            "Same state should not produce a second report"
        );

        // Call a third time for good measure
        let r3 = injector.collect_injections(None);
        assert!(r3.is_none(), "Still no report when nothing changed");
    }

    // ── Background job completion tests ─────────────────────────────────────

    #[test]
    fn test_bg_no_completions_no_injection() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // No jobs directory at all → no injection
        let result = injector.collect_injections(None);
        assert!(result.is_none(), "No bg jobs dir → no injection");
    }

    #[test]
    fn test_bg_no_completions_only_running() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        // Create a running (non-terminal) job
        create_bg_job(
            wg_dir,
            "job-1",
            "build",
            "cargo build",
            "running",
            None,
            None,
        );

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let result = injector.collect_injections(None);
        assert!(result.is_none(), "Running jobs should not be injected");
    }

    #[test]
    fn test_bg_completion_injected() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        let log_output = (1..=25)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        create_bg_job(
            wg_dir,
            "job-abc",
            "test-run",
            "cargo test",
            "completed",
            Some(0),
            Some(&log_output),
        );

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let result = injector.collect_injections(None);
        assert!(result.is_some(), "Completed job should produce injection");
        let text = result.unwrap();
        assert!(text.contains("Background Job Completions"));
        assert!(text.contains("context-update"));
        assert!(text.contains("job-abc"));
        assert!(text.contains("test-run"));
        assert!(text.contains("cargo test"));
        assert!(text.contains("Exit code: 0"));
        assert!(text.contains("completed"));
        // Should contain only last 20 lines (lines 6-25)
        assert!(text.contains("line 25"));
        assert!(text.contains("line 6"));
        assert!(
            !text.contains("line 5\n"),
            "line 5 should be trimmed (only last 20 lines)"
        );
    }

    #[test]
    fn test_bg_completion_not_re_injected() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        create_bg_job(
            wg_dir,
            "job-xyz",
            "build",
            "cargo build",
            "completed",
            Some(0),
            Some("all good\n"),
        );

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // First call: should report the completion
        let r1 = injector.collect_injections(None);
        assert!(r1.is_some(), "First call should report completion");
        assert!(r1.unwrap().contains("job-xyz"));

        // Second call: same job, already reported → no injection
        let r2 = injector.collect_injections(None);
        assert!(
            r2.is_none(),
            "Already-reported completion should not be re-injected"
        );
    }

    #[test]
    fn test_bg_failed_job_injected() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        create_bg_job(
            wg_dir,
            "job-fail",
            "broken-test",
            "cargo test --broken",
            "failed",
            Some(1),
            Some("test failed: assertion error\n"),
        );

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let result = injector.collect_injections(None);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("failed"));
        assert!(text.contains("Exit code: 1"));
        assert!(text.contains("assertion error"));
    }

    #[test]
    fn test_bg_multiple_completions_one_already_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        create_bg_job(
            wg_dir,
            "job-a",
            "first",
            "echo a",
            "completed",
            Some(0),
            Some("output a\n"),
        );

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // Report first job
        let r1 = injector.collect_injections(None);
        assert!(r1.is_some());
        assert!(r1.unwrap().contains("job-a"));

        // Now a second job completes
        create_bg_job(
            wg_dir,
            "job-b",
            "second",
            "echo b",
            "completed",
            Some(0),
            Some("output b\n"),
        );

        // Should only report the new one
        let r2 = injector.collect_injections(None);
        assert!(r2.is_some());
        let text = r2.unwrap();
        assert!(text.contains("job-b"), "New job should be reported");
        assert!(!text.contains("job-a"), "Old job should not be re-reported");
    }

    #[test]
    fn test_bg_completion_no_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        // Job with no log content (log file doesn't exist)
        create_bg_job(
            wg_dir,
            "job-nolog",
            "no-output",
            "true",
            "completed",
            Some(0),
            None,
        );

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        let result = injector.collect_injections(None);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("job-nolog"));
        assert!(text.contains("Exit code: 0"));
        // No output section since log file doesn't exist
        assert!(!text.contains("Output (last 20 lines)"));
    }

    #[test]
    fn test_state_injection_context_pressure_not_sticky() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path();
        setup_workgraph(wg_dir, "my-task", &[]);

        let mut injector =
            StateInjector::new(wg_dir.to_path_buf(), "my-task".into(), "agent-1".into());

        // First call with pressure
        let r1 = injector.collect_injections(Some("At 85% capacity".into()));
        assert!(r1.is_some());
        assert!(r1.unwrap().contains("85%"));

        // Second call without pressure → no injection
        let r2 = injector.collect_injections(None);
        assert!(r2.is_none(), "No pressure = no injection (not sticky)");
    }
}

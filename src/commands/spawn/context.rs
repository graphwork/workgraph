//! Context assembly for spawned agents.
//!
//! Gathers dependency artifacts, logs, scope-based context (downstream awareness,
//! graph summaries, CLAUDE.md), and resolves the effective context scope.

use std::fs;
use std::path::Path;

use workgraph::config::Config;
use workgraph::context_scope::ContextScope;
use workgraph::graph::{LogEntry, Status};

/// Build context string from dependency artifacts and logs.
///
/// When scope >= Task, includes upstream task titles alongside artifacts (R5).
pub(crate) fn build_task_context(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
) -> String {
    let mut context_parts = Vec::new();

    for dep_id in &task.after {
        if let Some(dep_task) = graph.get_task(dep_id) {
            // R5: Include upstream task title alongside artifacts
            if !dep_task.artifacts.is_empty() {
                context_parts.push(format!(
                    "From {} ({}): artifacts: {}",
                    dep_id,
                    dep_task.title,
                    dep_task.artifacts.join(", ")
                ));
            }

            if dep_task.status == Status::Done && !dep_task.log.is_empty() {
                let logs: Vec<&LogEntry> = dep_task.log.iter().rev().take(5).collect();
                for entry in logs.iter().rev() {
                    context_parts.push(format!(
                        "From {} logs: {} {}",
                        dep_id, entry.timestamp, entry.message
                    ));
                }
            }
        }
    }

    // Inject cycle metadata if this task has cycle_config
    if let Some(ref cc) = task.cycle_config {
        context_parts.push(format!(
            "Cycle status: iteration {} of this task (max {})",
            task.loop_iteration, cc.max_iterations
        ));
        if let Some(ref delay) = cc.delay {
            context_parts.push(format!("  cycle delay: {}", delay));
        }
    }

    // Inject resume context from checkpoint (set by coordinator when waking a Waiting task)
    if let Some(ref checkpoint) = task.checkpoint {
        context_parts.push(checkpoint.clone());
    }

    if context_parts.is_empty() {
        "No context from dependencies".to_string()
    } else {
        context_parts.join("\n")
    }
}

/// Build the ScopeContext for scope-based prompt assembly.
///
/// Gathers R1 (downstream awareness), R4 (tags/skills), project description,
/// graph summaries, and CLAUDE.md content based on the resolved scope.
pub(crate) fn build_scope_context(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
    scope: ContextScope,
    config: &Config,
    workgraph_dir: &Path,
) -> workgraph::service::executor::ScopeContext {
    let mut ctx = workgraph::service::executor::ScopeContext::default();

    // R1: Downstream awareness (task+ scope)
    if scope >= ContextScope::Task {
        let task_id = &task.id;
        let downstream: Vec<_> = graph
            .tasks()
            .filter(|t| t.after.contains(task_id))
            .collect();
        if !downstream.is_empty() {
            let mut lines =
                vec!["## Downstream Consumers\n\nTasks that depend on your work:".to_string()];
            for dt in &downstream {
                lines.push(format!("- **{}**: \"{}\"", dt.id, dt.title));
            }
            ctx.downstream_info = lines.join("\n");
        }
    }

    // R4: Tags and skills (task+ scope)
    if scope >= ContextScope::Task {
        let mut info_parts = Vec::new();
        if !task.tags.is_empty() {
            info_parts.push(format!("- **Tags:** {}", task.tags.join(", ")));
        }
        if !task.skills.is_empty() {
            info_parts.push(format!("- **Skills:** {}", task.skills.join(", ")));
        }
        if !info_parts.is_empty() {
            ctx.tags_skills_info = info_parts.join("\n");
        }
    }

    // Graph+ scope: project description
    if scope >= ContextScope::Graph
        && let Some(ref desc) = config.project.description
        && !desc.is_empty()
    {
        ctx.project_description = desc.clone();
    }

    // Graph+ scope: 1-hop neighborhood subgraph summary
    if scope >= ContextScope::Graph {
        ctx.graph_summary = build_graph_summary(graph, task, workgraph_dir);
    }

    // Full scope: full graph summary
    if scope >= ContextScope::Full {
        ctx.full_graph_summary = build_full_graph_summary(graph);
    }

    // Full scope: CLAUDE.md content
    if scope >= ContextScope::Full {
        ctx.claude_md_content = read_claude_md(workgraph_dir);
    }

    // Task+ scope: queued messages
    if scope >= ContextScope::Task {
        ctx.queued_messages = workgraph::messages::format_queued_messages(workgraph_dir, &task.id);
    }

    // Note: cursor advancement happens after spawn in execution.rs,
    // where the agent_id is known.

    ctx
}

/// Inline artifact content for graph+ scopes.
///
/// - Files under 500 bytes: inline full content
/// - Larger files: first 3 lines + byte count
/// - Non-existent files: note that file was not found
fn inline_artifact_content(artifacts: &[String], workgraph_dir: &Path) -> String {
    if artifacts.is_empty() {
        return String::new();
    }

    let project_root = workgraph_dir
        .canonicalize()
        .ok()
        .and_then(|abs| abs.parent().map(std::path::Path::to_path_buf));

    let project_root = match project_root {
        Some(r) => r,
        None => return String::new(),
    };

    let mut lines = Vec::new();
    for artifact in artifacts {
        let path = project_root.join(artifact);
        match fs::metadata(&path) {
            Ok(meta) => {
                let size = meta.len();
                if size <= 500 {
                    match fs::read_to_string(&path) {
                        Ok(content) => {
                            lines.push(format!(
                                "  {} ({} bytes):\n  ```\n{}\n  ```",
                                artifact, size, content
                            ));
                        }
                        Err(_) => {
                            lines.push(format!("  {} ({} bytes, binary)", artifact, size));
                        }
                    }
                } else {
                    // Large file: first 3 lines + byte count
                    match fs::read_to_string(&path) {
                        Ok(content) => {
                            let preview: String =
                                content.lines().take(3).collect::<Vec<_>>().join("\n");
                            lines.push(format!(
                                "  {} ({} bytes):\n  ```\n{}\n  ...\n  ```",
                                artifact, size, preview
                            ));
                        }
                        Err(_) => {
                            lines.push(format!("  {} ({} bytes, binary)", artifact, size));
                        }
                    }
                }
            }
            Err(_) => {
                lines.push(format!("  {} (not found)", artifact));
            }
        }
    }
    lines.join("\n")
}

/// Build a 1-hop neighborhood graph summary for graph+ scopes.
///
/// Includes: status counts, upstream tasks, downstream tasks, and siblings.
/// Neighbor content is wrapped in XML fencing for prompt injection protection.
/// Hard cap at 4000 chars.
pub(crate) fn build_graph_summary(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
    workgraph_dir: &Path,
) -> String {
    let mut parts = Vec::new();

    // Status counts
    let mut open = 0u32;
    let mut in_progress = 0u32;
    let mut done = 0u32;
    let mut failed = 0u32;
    let mut blocked = 0u32;
    let total = graph.tasks().count() as u32;
    for t in graph.tasks() {
        match t.status {
            Status::Open => open += 1,
            Status::InProgress => in_progress += 1,
            Status::Done => done += 1,
            Status::Failed => failed += 1,
            Status::Blocked => blocked += 1,
            Status::Abandoned | Status::Waiting | Status::PendingValidation => {}
        }
    }
    parts.push(format!(
        "## Graph Status\n\n{} tasks \u{2014} {} done, {} in-progress, {} open, {} blocked, {} failed",
        total, done, in_progress, open, blocked, failed
    ));

    // Upstream tasks (direct dependencies) — XML fenced
    if !task.after.is_empty() {
        let mut lines = vec!["### Upstream (dependencies)".to_string()];
        for dep_id in &task.after {
            if let Some(dep) = graph.get_task(dep_id) {
                let desc_preview = dep
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect::<String>();
                let mut entry = format!(
                    "<neighbor-context source=\"{}\">\n- **{}** [{}]: {} \u{2014} {}",
                    dep.id, dep.id, dep.status, dep.title, desc_preview
                );
                // Inline artifact content for neighbors
                let artifact_content = inline_artifact_content(&dep.artifacts, workgraph_dir);
                if !artifact_content.is_empty() {
                    entry.push_str(&format!("\n  Artifacts:\n{}", artifact_content));
                }
                entry.push_str("\n</neighbor-context>");
                lines.push(entry);
            }
        }
        parts.push(lines.join("\n"));
    }

    // Downstream tasks (tasks that depend on this one) — XML fenced
    let task_id = &task.id;
    let downstream: Vec<_> = graph
        .tasks()
        .filter(|t| t.after.contains(task_id))
        .collect();
    if !downstream.is_empty() {
        let mut lines = vec!["### Downstream (dependents)".to_string()];
        for dt in &downstream {
            let desc_preview = dt
                .description
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>();
            let mut entry = format!(
                "<neighbor-context source=\"{}\">\n- **{}** [{}]: {} \u{2014} {}",
                dt.id, dt.id, dt.status, dt.title, desc_preview
            );
            let artifact_content = inline_artifact_content(&dt.artifacts, workgraph_dir);
            if !artifact_content.is_empty() {
                entry.push_str(&format!("\n  Artifacts:\n{}", artifact_content));
            }
            entry.push_str("\n</neighbor-context>");
            lines.push(entry);
        }
        parts.push(lines.join("\n"));
    }

    // Siblings (tasks sharing the same upstream dependencies)
    if !task.after.is_empty() {
        let siblings: Vec<_> = graph
            .tasks()
            .filter(|t| {
                t.id != task.id
                    && !t.after.is_empty()
                    && t.after.iter().any(|dep| task.after.contains(dep))
            })
            .collect();
        if !siblings.is_empty() {
            let mut lines = vec!["### Siblings (share upstream dependencies)".to_string()];
            for sib in siblings.iter().take(10) {
                lines.push(format!("- **{}** [{}]: {}", sib.id, sib.status, sib.title));
            }
            if siblings.len() > 10 {
                lines.push(format!("- ... and {} more", siblings.len() - 10));
            }
            parts.push(lines.join("\n"));
        }
    }

    let summary = parts.join("\n\n");
    // Hard cap at 4000 chars
    if summary.len() > 4000 {
        let end = summary.floor_char_boundary(3950);
        let mut truncated = summary[..end].to_string();
        truncated.push_str("\n\n... (graph summary truncated)");
        truncated
    } else {
        summary
    }
}

/// Build a full graph summary for full scope.
///
/// Lists all tasks with statuses and dependency edges, with 4000-char budget.
pub(crate) fn build_full_graph_summary(graph: &workgraph::WorkGraph) -> String {
    let mut parts = vec!["## Full Graph Summary\n".to_string()];
    let mut budget = 4000i32;
    let total = graph.tasks().count();

    for (task_count, t) in graph.tasks().enumerate() {
        let deps = if t.after.is_empty() {
            String::new()
        } else {
            format!(" (after: {})", t.after.join(", "))
        };
        let line = format!("- **{}** [{}]: {}{}\n", t.id, t.status, t.title, deps);
        budget -= line.len() as i32;
        if budget < 0 {
            let remaining = total - task_count;
            parts.push(format!("... and {} more tasks", remaining));
            break;
        }
        parts.push(line);
    }

    parts.join("")
}

/// Read CLAUDE.md content from the project root (parent of .workgraph/).
fn read_claude_md(workgraph_dir: &Path) -> String {
    let project_root = workgraph_dir
        .canonicalize()
        .ok()
        .and_then(|abs| abs.parent().map(std::path::Path::to_path_buf));

    let project_root = match project_root {
        Some(r) => r,
        None => return String::new(),
    };

    let claude_md_path = project_root.join("CLAUDE.md");
    std::fs::read_to_string(&claude_md_path).unwrap_or_default()
}

/// Resolve the effective exec_mode for a task using the priority hierarchy:
/// task.exec_mode > role.default_exec_mode > "full".
pub(crate) fn resolve_task_exec_mode(
    task: &workgraph::graph::Task,
    workgraph_dir: &Path,
) -> String {
    if let Some(ref mode) = task.exec_mode {
        return mode.clone();
    }

    // Check role's default_exec_mode if task has an agent
    if let Some(ref agent_hash) = task.agent {
        let agency_dir = workgraph_dir.join("agency");
        let agents_dir = agency_dir.join("cache/agents");
        let roles_dir = agency_dir.join("cache/roles");
        if let Ok(agent) = workgraph::agency::find_agent_by_prefix(&agents_dir, agent_hash)
            && let Ok(role) = workgraph::agency::find_role_by_prefix(&roles_dir, &agent.role_id)
            && let Some(mode) = role.default_exec_mode
        {
            return mode;
        }
    }

    "full".to_string()
}

/// Resolve the context scope for a task using the priority hierarchy:
/// task > role > coordinator config > default ("task").
pub(crate) fn resolve_task_scope(
    task: &workgraph::graph::Task,
    config: &Config,
    workgraph_dir: &Path,
) -> ContextScope {
    // Get role's default_context_scope if task has an agent
    let role_scope = task.agent.as_ref().and_then(|agent_hash| {
        let agency_dir = workgraph_dir.join("agency");
        let agents_dir = agency_dir.join("cache/agents");
        let roles_dir = agency_dir.join("cache/roles");
        let agent = workgraph::agency::find_agent_by_prefix(&agents_dir, agent_hash).ok()?;
        let role = workgraph::agency::find_role_by_prefix(&roles_dir, &agent.role_id).ok()?;
        role.default_context_scope
    });

    workgraph::context_scope::resolve_context_scope(
        task.context_scope.as_deref(),
        role_scope.as_deref(),
        config.coordinator.default_context_scope.as_deref(),
    )
}

/// Build previous attempt context for retry injection.
///
/// When a task has `retry_count > 0`, looks for the most recent archived agent
/// attempt and extracts context in priority order:
/// 1. Checkpoint summary (auto or explicit)
/// 2. Truncated output.log tail
/// 3. Task log entries
///
/// Returns empty string if no previous attempt context is found or retry_count is 0.
pub(crate) fn build_previous_attempt_context(
    task: &workgraph::graph::Task,
    workgraph_dir: &Path,
    max_tokens: u32,
) -> String {
    if task.retry_count == 0 || max_tokens == 0 {
        return String::new();
    }

    // Find the most recent archived agent for this task
    let archive_base = workgraph_dir.join("log").join("agents").join(&task.id);

    if !archive_base.exists() {
        return String::new();
    }

    // Get the most recent archive directory (sorted by timestamp)
    let mut archives: Vec<_> = match fs::read_dir(&archive_base) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect(),
        Err(_) => return String::new(),
    };

    if archives.is_empty() {
        return String::new();
    }

    // Sort by directory name (ISO timestamps sort lexicographically)
    archives.sort_by_key(|e| e.file_name());
    let latest_archive = archives.last().unwrap().path();
    let archive_timestamp = archives
        .last()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .to_string();

    // Estimate max bytes (~4 chars per token as rough heuristic)
    let max_bytes = (max_tokens as usize) * 4;

    // Priority 1: Look for checkpoint summary from the previous agent
    let checkpoint_context = find_checkpoint_for_task(task, workgraph_dir);
    if let Some(summary) = checkpoint_context
        && !summary.is_empty()
    {
        return format_previous_context(&archive_timestamp, &summary, max_bytes);
    }

    // Priority 2: Truncated output.log from the archive
    let output_path = latest_archive.join("output.txt");
    if output_path.exists()
        && let Ok(content) = fs::read_to_string(&output_path)
        && !content.trim().is_empty()
    {
        let tail = truncate_to_tail(&content, max_bytes);
        return format_previous_context(&archive_timestamp, &tail, max_bytes);
    }

    // Priority 3: Task log entries
    if !task.log.is_empty() {
        let log_context = task
            .log
            .iter()
            .map(|entry| {
                format!(
                    "[{}] {}: {}",
                    entry.timestamp,
                    entry.actor.as_deref().unwrap_or("system"),
                    entry.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        if !log_context.is_empty() {
            let truncated = truncate_to_tail(&log_context, max_bytes);
            return format_previous_context(&archive_timestamp, &truncated, max_bytes);
        }
    }

    String::new()
}

/// Find the most recent checkpoint for a task from any previously assigned agent.
fn find_checkpoint_for_task(task: &workgraph::graph::Task, workgraph_dir: &Path) -> Option<String> {
    let mut prev_agents: Vec<String> = Vec::new();
    for entry in &task.log {
        if let Some(ref actor) = entry.actor
            && actor.starts_with("agent-")
            && !prev_agents.contains(actor)
        {
            prev_agents.push(actor.clone());
        }
    }

    for agent_id in prev_agents.iter().rev() {
        if let Ok(Some(checkpoint)) =
            crate::commands::checkpoint::load_latest(workgraph_dir, agent_id)
        {
            return Some(format!(
                "Checkpoint ({:?}, agent {}): {}",
                checkpoint.checkpoint_type, checkpoint.agent_id, checkpoint.summary
            ));
        }
    }

    None
}

/// Truncate a string to its last `max_bytes` bytes, preserving valid UTF-8 boundaries.
fn truncate_to_tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    let start = s.len() - max_bytes;
    let start = s.ceil_char_boundary(start);
    format!("... (truncated)\n{}", &s[start..])
}

/// Format the previous attempt context section for injection into the prompt.
fn format_previous_context(timestamp: &str, content: &str, max_bytes: usize) -> String {
    let truncated_content = if content.len() > max_bytes {
        truncate_to_tail(content, max_bytes)
    } else {
        content.to_string()
    };

    format!(
        "## Previous Attempt Context\n\
         This task was previously attempted (archived at {}).\n\
         Here is context from that attempt:\n\n\
         {}\n\n\
         Continue from where they left off. Do not repeat work already done.",
        timestamp, truncated_content
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn test_build_task_context() {
        let mut graph = WorkGraph::new();

        // Create a dependency task with artifacts and logs
        let mut dep_task = make_task("dep-1", "Dependency");
        dep_task.status = Status::Done;
        dep_task.artifacts = vec!["output.txt".to_string(), "data.json".to_string()];
        dep_task.log = vec![
            LogEntry {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                message: "Started work".to_string(),
            },
            LogEntry {
                timestamp: "2026-01-01T00:01:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                message: "Found important result".to_string(),
            },
            LogEntry {
                timestamp: "2026-01-01T00:02:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                message: "Completed successfully".to_string(),
            },
        ];
        graph.add_node(Node::Task(dep_task));

        // Create main task blocked by dependency
        let mut main_task = make_task("main", "Main Task");
        main_task.after = vec!["dep-1".to_string()];
        graph.add_node(Node::Task(main_task.clone()));

        let context = build_task_context(&graph, &main_task);
        assert!(context.contains("dep-1"));
        // R5: Upstream title included
        assert!(context.contains("(Dependency)"));
        assert!(context.contains("output.txt"));
        assert!(context.contains("data.json"));
        // Verify log entries are included
        assert!(context.contains("From dep-1 logs:"));
        assert!(context.contains("Started work"));
        assert!(context.contains("Found important result"));
        assert!(context.contains("Completed successfully"));
    }

    #[test]
    fn test_build_task_context_no_deps() {
        let graph = WorkGraph::new();
        let task = make_task("t1", "Test Task");

        let context = build_task_context(&graph, &task);
        assert_eq!(context, "No context from dependencies");
        assert!(!context.contains("logs:"));
    }

    #[test]
    fn test_build_task_context_no_loop_metadata_for_normal_tasks() {
        let graph = WorkGraph::new();
        let task = make_task("t1", "Normal Task");
        let context = build_task_context(&graph, &task);
        assert!(!context.contains("Loop status"));
    }

    #[test]
    fn test_build_graph_summary_includes_status_counts() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Done task");
        t1.status = Status::Done;
        graph.add_node(Node::Task(t1));

        let mut t2 = make_task("t2", "Open task");
        t2.status = Status::Open;
        graph.add_node(Node::Task(t2));

        let mut t3 = make_task("t3", "In progress");
        t3.status = Status::InProgress;
        graph.add_node(Node::Task(t3));

        let main = make_task("main", "Main task");
        graph.add_node(Node::Task(main.clone()));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(
            summary.contains("## Graph Status"),
            "Should have status header"
        );
        assert!(summary.contains("4 tasks"), "Should count all tasks");
        assert!(summary.contains("1 done"), "Should count done tasks");
        assert!(
            summary.contains("1 in-progress"),
            "Should count in-progress tasks"
        );
        assert!(
            summary.contains("2 open"),
            "Should count open tasks (main + t2)"
        );
    }

    #[test]
    fn test_build_graph_summary_includes_upstream_and_downstream() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let mut upstream = make_task("upstream", "Upstream task");
        upstream.status = Status::Done;
        upstream.description = Some("Does upstream work".to_string());
        graph.add_node(Node::Task(upstream));

        let mut main = make_task("main", "Main task");
        main.after = vec!["upstream".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let mut downstream = make_task("downstream", "Downstream task");
        downstream.after = vec!["main".to_string()];
        downstream.description = Some("Consumes main output".to_string());
        graph.add_node(Node::Task(downstream));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(
            summary.contains("### Upstream"),
            "Should have upstream section"
        );
        assert!(summary.contains("upstream"), "Should list upstream task");
        assert!(
            summary.contains("### Downstream"),
            "Should have downstream section"
        );
        assert!(
            summary.contains("downstream"),
            "Should list downstream task"
        );
        assert!(
            summary.contains("Consumes main output"),
            "Should include description preview"
        );
    }

    #[test]
    fn test_build_graph_summary_includes_siblings() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let parent = make_task("parent", "Parent task");
        graph.add_node(Node::Task(parent));

        let mut main = make_task("main", "Main task");
        main.after = vec!["parent".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let mut sibling = make_task("sibling", "Sibling task");
        sibling.after = vec!["parent".to_string()];
        graph.add_node(Node::Task(sibling));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(
            summary.contains("### Siblings"),
            "Should have siblings section"
        );
        assert!(summary.contains("sibling"), "Should list sibling task");
    }

    #[test]
    fn test_build_graph_summary_xml_fencing() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let upstream = make_task("dep", "Dependency");
        graph.add_node(Node::Task(upstream));

        let mut main = make_task("main", "Main task");
        main.after = vec!["dep".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(
            summary.contains("<neighbor-context source=\"dep\">"),
            "Upstream should be XML fenced"
        );
        assert!(
            summary.contains("</neighbor-context>"),
            "Should close XML fence"
        );
    }

    #[test]
    fn test_build_graph_summary_truncates_at_4000_chars() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        // Create many tasks to exceed 4000 chars
        for i in 0..200 {
            let mut t = make_task(
                &format!("task-{:03}", i),
                &format!(
                    "A task with a long title to inflate the summary for task number {}",
                    i
                ),
            );
            t.description = Some(format!(
                "Description for task {} with extra words to pad length",
                i
            ));
            if i > 0 {
                t.after = vec!["task-000".to_string()];
            }
            graph.add_node(Node::Task(t));
        }

        let main_task = graph.get_task("task-000").unwrap().clone();
        let summary = build_graph_summary(&graph, &main_task, wg_dir);
        assert!(
            summary.len() <= 4100,
            "Summary should be capped near 4000 chars, got {}",
            summary.len()
        );
        if summary.len() > 3950 {
            assert!(summary.contains("truncated"), "Should indicate truncation");
        }
    }

    #[test]
    fn test_build_full_graph_summary_lists_tasks() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "First task");
        t1.status = Status::Done;
        graph.add_node(Node::Task(t1));

        let mut t2 = make_task("t2", "Second task");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t2));

        let summary = build_full_graph_summary(&graph);
        assert!(
            summary.contains("## Full Graph Summary"),
            "Should have header"
        );
        assert!(summary.contains("t1"), "Should list first task");
        assert!(summary.contains("[done]"), "Should show status");
        assert!(summary.contains("t2"), "Should list second task");
        assert!(summary.contains("(after: t1)"), "Should show dependencies");
    }

    #[test]
    fn test_build_full_graph_summary_truncates_at_budget() {
        let mut graph = WorkGraph::new();
        // Create enough tasks to exceed the 4000-char budget
        for i in 0..200 {
            let t = make_task(
                &format!("task-with-long-id-{:04}", i),
                &format!("A task with a somewhat long title for padding number {}", i),
            );
            graph.add_node(Node::Task(t));
        }

        let summary = build_full_graph_summary(&graph);
        assert!(
            summary.len() <= 4200,
            "Should be bounded by budget, got {}",
            summary.len()
        );
        assert!(summary.contains("more tasks"), "Should indicate truncation");
    }

    #[test]
    fn test_build_scope_context_clean_scope_empty() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let graph = WorkGraph::new();
        let task = make_task("t1", "Test task");
        let config = Config::default();

        let ctx = build_scope_context(&graph, &task, ContextScope::Clean, &config, wg_dir);
        assert!(
            ctx.downstream_info.is_empty(),
            "Clean scope should have no downstream info"
        );
        assert!(
            ctx.tags_skills_info.is_empty(),
            "Clean scope should have no tags info"
        );
        assert!(
            ctx.project_description.is_empty(),
            "Clean scope should have no project description"
        );
        assert!(
            ctx.graph_summary.is_empty(),
            "Clean scope should have no graph summary"
        );
        assert!(
            ctx.full_graph_summary.is_empty(),
            "Clean scope should have no full graph summary"
        );
        assert!(
            ctx.claude_md_content.is_empty(),
            "Clean scope should have no CLAUDE.md content"
        );
    }

    #[test]
    fn test_build_scope_context_task_scope_includes_downstream() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Main task");
        graph.add_node(Node::Task(task.clone()));

        let mut downstream = make_task("d1", "Dependent task");
        downstream.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(downstream));

        let config = Config::default();
        let ctx = build_scope_context(&graph, &task, ContextScope::Task, &config, wg_dir);
        assert!(
            ctx.downstream_info.contains("d1"),
            "Task scope should include downstream"
        );
        assert!(
            ctx.downstream_info.contains("Dependent task"),
            "Should include downstream title"
        );
        // Should NOT include graph-level stuff
        assert!(
            ctx.graph_summary.is_empty(),
            "Task scope should not have graph summary"
        );
    }

    #[test]
    fn test_build_scope_context_task_scope_includes_tags_skills() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let graph = WorkGraph::new();
        let mut task = make_task("t1", "Tagged task");
        task.tags = vec!["rust".to_string(), "backend".to_string()];
        task.skills = vec!["implementation".to_string()];

        let config = Config::default();
        let ctx = build_scope_context(&graph, &task, ContextScope::Task, &config, wg_dir);
        assert!(ctx.tags_skills_info.contains("rust"), "Should include tags");
        assert!(
            ctx.tags_skills_info.contains("implementation"),
            "Should include skills"
        );
    }

    #[test]
    fn test_build_scope_context_graph_scope_includes_summary() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Graph task");
        graph.add_node(Node::Task(task.clone()));

        let mut config = Config::default();
        config.project.description = Some("A test project".to_string());

        let ctx = build_scope_context(&graph, &task, ContextScope::Graph, &config, wg_dir);
        assert!(
            ctx.project_description.contains("A test project"),
            "Graph scope should include project description"
        );
        assert!(
            !ctx.graph_summary.is_empty(),
            "Graph scope should have graph summary"
        );
        // Should NOT include full-scope stuff
        assert!(
            ctx.full_graph_summary.is_empty(),
            "Graph scope should not have full graph summary"
        );
        assert!(
            ctx.claude_md_content.is_empty(),
            "Graph scope should not have CLAUDE.md"
        );
    }

    #[test]
    fn test_build_scope_context_full_scope_includes_everything() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Full task");
        graph.add_node(Node::Task(task.clone()));

        let mut config = Config::default();
        config.project.description = Some("Test project".to_string());

        let ctx = build_scope_context(&graph, &task, ContextScope::Full, &config, wg_dir);
        assert!(
            !ctx.graph_summary.is_empty(),
            "Full scope should have graph summary"
        );
        assert!(
            !ctx.full_graph_summary.is_empty(),
            "Full scope should have full graph summary"
        );
        assert!(
            ctx.full_graph_summary.contains("Full Graph Summary"),
            "Should include full graph summary header"
        );
    }

    #[test]
    fn test_resolve_task_scope_defaults_to_task() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let task = make_task("t1", "Test");
        let config = Config::default();
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(scope, ContextScope::Task, "Default scope should be Task");
    }

    #[test]
    fn test_resolve_task_scope_task_overrides() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut task = make_task("t1", "Test");
        task.context_scope = Some("clean".to_string());
        let mut config = Config::default();
        config.coordinator.default_context_scope = Some("full".to_string());
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(
            scope,
            ContextScope::Clean,
            "Task scope should override config"
        );
    }

    #[test]
    fn test_resolve_task_scope_config_fallback() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let task = make_task("t1", "Test");
        let mut config = Config::default();
        config.coordinator.default_context_scope = Some("graph".to_string());
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(
            scope,
            ContextScope::Graph,
            "Config scope should be used as fallback"
        );
    }

    // =========================================================================
    // Previous attempt context tests
    // =========================================================================

    #[test]
    fn test_build_previous_attempt_context_zero_retry_count() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let task = make_task("t1", "Test task");
        // retry_count is 0 by default
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(result.is_empty(), "Should return empty for retry_count 0");
    }

    #[test]
    fn test_build_previous_attempt_context_disabled_by_zero_tokens() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        let result = build_previous_attempt_context(&task, wg_dir, 0);
        assert!(
            result.is_empty(),
            "Should return empty when max_tokens is 0"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_no_archive() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.is_empty(),
            "Should return empty when no archive exists"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_with_archive_output() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create an archive with output
        let archive_dir = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(
            archive_dir.join("output.txt"),
            "Agent started working on task t1\nCompleted analysis of requirements\nFound 3 issues",
        )
        .unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.contains("Previous Attempt Context"),
            "Should contain header"
        );
        assert!(
            result.contains("2026-03-07T10:00:00Z"),
            "Should contain archive timestamp"
        );
        assert!(
            result.contains("Found 3 issues"),
            "Should contain output content"
        );
        assert!(
            result.contains("Continue from where they left off"),
            "Should contain continuation instruction"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_empty_output_skipped() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create an archive with empty output
        let archive_dir = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(archive_dir.join("output.txt"), "   \n\n  ").unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        // No checkpoint, empty output, no logs => empty result
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.is_empty(),
            "Should return empty for whitespace-only output"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_uses_most_recent_archive() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create two archives (older and newer)
        let old_archive = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-06T10:00:00Z");
        std::fs::create_dir_all(&old_archive).unwrap();
        std::fs::write(old_archive.join("output.txt"), "Old agent output").unwrap();

        let new_archive = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&new_archive).unwrap();
        std::fs::write(new_archive.join("output.txt"), "New agent output").unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 2;
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.contains("New agent output"),
            "Should use most recent archive"
        );
        assert!(
            !result.contains("Old agent output"),
            "Should not use old archive"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_with_checkpoint() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create an archive
        let archive_dir = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(archive_dir.join("output.txt"), "Some output").unwrap();

        // Create a checkpoint for the agent
        let cp_dir = wg_dir.join("agents").join("agent-99").join("checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        let checkpoint = serde_json::json!({
            "task_id": "t1",
            "agent_id": "agent-99",
            "timestamp": "2026-03-07T10:30:00Z",
            "type": "auto",
            "summary": "Completed web search, found 5 relevant docs",
            "files_modified": [],
            "artifacts_registered": []
        });
        std::fs::write(
            cp_dir.join("2026-03-07T10-30-00.000Z.json"),
            serde_json::to_string(&checkpoint).unwrap(),
        )
        .unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        task.log = vec![LogEntry {
            timestamp: "2026-03-07T09:00:00Z".to_string(),
            actor: Some("agent-99".to_string()),
            message: "Spawned by coordinator".to_string(),
        }];

        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.contains("Completed web search"),
            "Should use checkpoint summary. Got: {}",
            result
        );
    }

    #[test]
    fn test_build_previous_attempt_context_falls_back_to_logs() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create an archive directory but NO output.txt
        let archive_dir = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&archive_dir).unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        task.log = vec![
            LogEntry {
                timestamp: "2026-03-07T09:00:00Z".to_string(),
                actor: Some("agent-50".to_string()),
                message: "Started research".to_string(),
            },
            LogEntry {
                timestamp: "2026-03-07T09:30:00Z".to_string(),
                actor: Some("agent-50".to_string()),
                message: "Found key insight about X".to_string(),
            },
        ];

        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.contains("Found key insight"),
            "Should fall back to task log entries. Got: {}",
            result
        );
    }

    #[test]
    fn test_truncate_to_tail() {
        let short = "Hello";
        assert_eq!(truncate_to_tail(short, 100), "Hello");

        let long = "A".repeat(1000);
        let truncated = truncate_to_tail(&long, 500);
        assert!(truncated.len() <= 520, "Should be roughly max_bytes");
        assert!(truncated.starts_with("... (truncated)"));
    }

    #[test]
    fn test_format_previous_context_structure() {
        let result = format_previous_context("2026-03-07T10:00:00Z", "Some work done", 8000);
        assert!(result.starts_with("## Previous Attempt Context"));
        assert!(result.contains("2026-03-07T10:00:00Z"));
        assert!(result.contains("Some work done"));
        assert!(result.contains("Continue from where they left off"));
    }
}

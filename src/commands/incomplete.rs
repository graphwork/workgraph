use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status, parse_token_usage, parse_wg_tokens};
use workgraph::parser::modify_graph;
use workgraph::service::registry::AgentRegistry;

pub fn run(dir: &Path, id: &str, reason: Option<&str>) -> Result<()> {
    {
        let (graph, _path) = super::load_workgraph_mut(dir)?;
        let task = graph.get_task_or_err(id)?;

        if task.status == Status::Incomplete {
            println!("Task '{}' is already incomplete", id);
            return Ok(());
        }

        if task.status.is_terminal() {
            anyhow::bail!(
                "Task '{}' is {} and cannot be marked as incomplete",
                id,
                task.status
            );
        }
    }

    let path = super::graph_path(dir);

    let token_usage = AgentRegistry::load(dir).ok().and_then(|registry| {
        let agent = registry.get_agent_by_task(id)?;
        let output_path = std::path::Path::new(&agent.output_file);
        let abs_path = if output_path.is_absolute() {
            output_path.to_path_buf()
        } else {
            dir.parent().unwrap_or(dir).join(output_path)
        };
        parse_token_usage(&abs_path).or_else(|| parse_wg_tokens(&abs_path))
    });

    let mut agent_id_for_archive = None;

    let id_owned = id.to_string();
    let reason_owned = reason.map(String::from);
    modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(&id_owned) {
            Some(t) => t,
            None => return false,
        };

        if task.status == Status::Incomplete || task.status.is_terminal() {
            return false;
        }

        agent_id_for_archive = task.assigned.clone();

        task.status = Status::Incomplete;
        task.assigned = None;
        task.completed_at = Some(Utc::now().to_rfc3339());

        let log_message = match reason_owned.as_deref() {
            Some(r) => format!("Task marked as incomplete: {}", r),
            None => "Task marked as incomplete".to_string(),
        };
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: agent_id_for_archive.clone(),
            user: Some(workgraph::current_user()),
            message: log_message,
        });

        if task.token_usage.is_none()
            && let Some(ref usage) = token_usage
        {
            task.token_usage = Some(usage.clone());
        }

        true
    })
    .context("Failed to save graph")?;

    super::notify_graph_changed(dir);

    if let Ok(mut locked_registry) = AgentRegistry::load_locked(dir) {
        if let Some(agent) = locked_registry.get_agent_by_task_mut(id) {
            use workgraph::service::registry::AgentStatus;
            agent.status = AgentStatus::Done;
            if agent.completed_at.is_none() {
                agent.completed_at = Some(Utc::now().to_rfc3339());
            }
        }
        let _ = locked_registry.save_ref();
    }

    let config = workgraph::config::Config::load_or_default(dir);
    let detail = match reason {
        Some(r) => serde_json::json!({ "reason": r }),
        None => serde_json::Value::Null,
    };
    let _ = workgraph::provenance::record(
        dir,
        "incomplete",
        Some(id),
        None,
        detail,
        config.log.rotation_threshold,
    );

    let reason_msg = reason.map(|r| format!(" ({})", r)).unwrap_or_default();
    println!("Marked '{}' as incomplete{}", id, reason_msg);
    println!("  Task will appear in 'wg ready' for re-dispatch");

    if let Some(ref agent_id) = agent_id_for_archive {
        match super::log::archive_agent(dir, id, agent_id) {
            Ok(archive_dir) => {
                eprintln!("Agent archived to {}", archive_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: failed to archive agent: {}", e);
            }
        }
    }

    Ok(())
}

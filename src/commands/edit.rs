//! Edit command for modifying existing tasks

use anyhow::{Context, Result};
use std::path::Path;
use workgraph::cycle::{EdgeAddResult, check_edge_addition};
use workgraph::graph::{CycleConfig, parse_delay};
use workgraph::parser::modify_graph;

use super::graph_path;

/// Edit a task's fields
#[allow(clippy::too_many_arguments)]
pub fn run(
    dir: &Path,
    task_id: &str,
    title: Option<&str>,
    description: Option<&str>,
    add_after: &[String],
    remove_after: &[String],
    add_tag: &[String],
    remove_tag: &[String],
    model: Option<&str>,
    provider: Option<&str>,
    add_skill: &[String],
    remove_skill: &[String],
    max_iterations: Option<u32>,
    cycle_guard: Option<&str>,
    cycle_delay: Option<&str>,
    no_converge: bool,
    no_restart_on_failure: bool,
    max_failure_restarts: Option<u32>,
    visibility: Option<&str>,
    context_scope: Option<&str>,
    exec_mode: Option<&str>,
    delay: Option<&str>,
    not_before: Option<&str>,
    verify: Option<&str>,
    cron: Option<&str>,
    allow_phantom: bool,
    allow_cycle: bool,
) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Validate self-blocking (can be done before loading graph)
    for dep in add_after {
        if dep == task_id {
            anyhow::bail!("Task '{}' cannot block itself", task_id);
        }
    }

    // Deprecation warning for --provider flag
    if let Some(p) = provider {
        let suggested_provider = if p == "anthropic" { "claude" } else { p };
        eprintln!(
            "Warning: --provider is deprecated. Use provider:model format in --model instead.\n\
             Example: wg update {} --model {}:MODEL",
            task_id, suggested_provider,
        );
    }

    // Validate model uses provider:model format
    if let Some(m) = model
        && let Err(e) = workgraph::config::parse_model_spec_strict(m)
    {
        anyhow::bail!("Invalid --model format: {}", e);
    }

    let mut changed = false;
    let mut field_changes: Vec<serde_json::Value> = Vec::new();
    let mut error: Option<anyhow::Error> = None;

    modify_graph(&path, |graph| {

    // Validate task exists
    if graph.get_task(task_id).is_none() {
        error = Some(anyhow::anyhow!("Task '{}' not found", task_id));
        return false;
    }

    // Validate add-after dependencies before taking mutable borrow (phantom edge prevention)
    if !allow_phantom {
        for dep in add_after {
            if workgraph::federation::parse_remote_ref(dep).is_some() {
                continue;
            }
            if graph.get_node(dep).is_none() {
                let mut msg = format!("Dependency '{}' does not exist.", dep);
                let all_ids: Vec<&str> = graph.tasks().map(|t| t.id.as_str()).collect();
                if let Some((suggestion, _)) =
                    workgraph::check::fuzzy_match_task_id(dep, all_ids.iter().copied(), 3)
                {
                    msg.push_str(&format!("\n  → Did you mean '{}'?", suggestion));
                }
                msg.push_str(
                    "\n  Hint: Use --allow-phantom to allow forward references.",
                );
                error = Some(anyhow::anyhow!("{}", msg));
                return false;
            }
        }
    }

    // Check for cycles before adding dependencies (unless allow_cycle is set)
    if !allow_cycle && !add_after.is_empty() {
        // Build adjacency list for cycle detection
        let task_ids: Vec<String> = graph.tasks().map(|t| t.id.clone()).collect();
        let mut task_id_to_index = std::collections::HashMap::new();
        for (i, id) in task_ids.iter().enumerate() {
            task_id_to_index.insert(id, i);
        }

        let mut adjacency_list = vec![Vec::new(); task_ids.len()];
        for task in graph.tasks() {
            if let Some(&task_idx) = task_id_to_index.get(&task.id) {
                for dep_id in &task.after {
                    if let Some(&dep_idx) = task_id_to_index.get(dep_id) {
                        adjacency_list[dep_idx].push(task_idx);
                    }
                }
            }
        }

        // Get the current task's dependencies to check what would actually be added
        let current_after = graph.get_task(task_id)
            .map(|t| &t.after)
            .unwrap_or(&vec![])
            .clone();

        // Check each new dependency for cycle creation
        let task_id_string = task_id.to_string();
        if let Some(&task_idx) = task_id_to_index.get(&task_id_string) {
            for dep in add_after {
                if !current_after.contains(dep)
                    && let Some(&dep_idx) = task_id_to_index.get(dep)
                {
                        match check_edge_addition(task_ids.len(), &adjacency_list, dep_idx, task_idx) {
                            EdgeAddResult::CreatesCycle { cycle_members } => {
                                // Check if the cycle would have CycleConfig
                                let has_cycle_config = cycle_members.iter()
                                    .filter_map(|&idx| task_ids.get(idx))
                                    .any(|cycle_task_id| {
                                        // Check if max_iterations will be set on this task
                                        if cycle_task_id == task_id && max_iterations.is_some() {
                                            return true;
                                        }
                                        graph.get_task(cycle_task_id)
                                            .map(|t| t.cycle_config.is_some())
                                            .unwrap_or(false)
                                    });

                                if !has_cycle_config && !allow_cycle {
                                    let cycle_task_names: Vec<String> = cycle_members.iter()
                                        .filter_map(|&idx| task_ids.get(idx))
                                        .cloned()
                                        .collect();
                                    error = Some(anyhow::anyhow!(
                                        "Adding dependency '{}' → '{}' would create a cycle without CycleConfig: [{}]. \
                                         Use --allow-cycle to override, or add --max-iterations to one of the cycle members.",
                                        dep, task_id, cycle_task_names.join(" → ")
                                    ));
                                    return false;
                                }
                            }
                            EdgeAddResult::NoCycle => {
                                // Safe to add - no action needed
                            }
                        }
                    }
            }
        }
    }

    // Modify the task in a block so the mutable borrow is released afterwards
    {
        let task = match graph.get_task_mut(task_id) {
            Some(t) => t,
            None => {
                error = Some(anyhow::anyhow!("Task '{}' not found", task_id));
                return false;
            }
        };

        // Update title
        if let Some(new_title) = title {
            let old = task.title.clone();
            task.title = new_title.to_string();
            field_changes.push(serde_json::json!({"field": "title", "old": old, "new": new_title}));
            println!("Updated title: {}", new_title);
            changed = true;
        }

        // Update description
        if let Some(new_description) = description {
            let old = task.description.clone();
            task.description = Some(new_description.to_string());
            field_changes.push(
                serde_json::json!({"field": "description", "old": old, "new": new_description}),
            );
            println!("Updated description");
            changed = true;
        }

        // Add after dependencies (already validated above)
        for dep in add_after {
            if !task.after.contains(dep) {
                task.after.push(dep.clone());
                println!("Added after: {}", dep);
                changed = true;
            } else {
                println!("Already blocked by: {}", dep);
            }
        }

        // Remove after dependencies
        for dep in remove_after {
            if let Some(pos) = task.after.iter().position(|x| x == dep) {
                task.after.remove(pos);
                println!("Removed after: {}", dep);
                changed = true;
            } else {
                println!("Not blocked by: {}", dep);
            }
        }

        // Add tags
        for tag in add_tag {
            if !task.tags.contains(tag) {
                task.tags.push(tag.clone());
                println!("Added tag: {}", tag);
                changed = true;
            } else {
                println!("Already has tag: {}", tag);
            }
        }

        // Remove tags
        for tag in remove_tag {
            if let Some(pos) = task.tags.iter().position(|x| x == tag) {
                task.tags.remove(pos);
                println!("Removed tag: {}", tag);
                changed = true;
            } else {
                println!("Does not have tag: {}", tag);
            }
        }

        // Update model
        if let Some(new_model) = model {
            task.model = Some(new_model.to_string());
            println!("Updated model: {}", new_model);
            changed = true;
        }

        // Update provider
        if let Some(new_provider) = provider {
            task.provider = Some(new_provider.to_string());
            println!("Updated provider: {}", new_provider);
            changed = true;
        }

        // --verify is deprecated
        if verify.is_some() {
            error = Some(anyhow::anyhow!(
                "--verify is deprecated and no longer accepted.\n\
                 Use --validation=llm or a ## Validation section in the task description instead."
            ));
            return false;
        }

        // Add skills
        for skill in add_skill {
            if !task.skills.contains(skill) {
                task.skills.push(skill.clone());
                println!("Added skill: {}", skill);
                changed = true;
            } else {
                println!("Already has skill: {}", skill);
            }
        }

        // Remove skills
        for skill in remove_skill {
            if let Some(pos) = task.skills.iter().position(|x| x == skill) {
                task.skills.remove(pos);
                println!("Removed skill: {}", skill);
                changed = true;
            } else {
                println!("Does not have skill: {}", skill);
            }
        }

        // Update cycle config
        if let Some(max_iter) = max_iterations {
            let guard = match cycle_guard {
                Some(expr) => match crate::commands::add::parse_guard_expr(expr) {
                    Ok(g) => Some(g),
                    Err(e) => {
                        error = Some(e);
                        return false;
                    }
                },
                None => task.cycle_config.as_ref().and_then(|c| c.guard.clone()),
            };
            let delay = match cycle_delay {
                Some(d) => {
                    if parse_delay(d).is_none() {
                        error = Some(anyhow::anyhow!(
                            "Invalid cycle delay '{}'. Use format: 30s, 5m, 1h, 24h, 7d",
                            d
                        ));
                        return false;
                    }
                    Some(d.to_string())
                }
                None => task.cycle_config.as_ref().and_then(|c| c.delay.clone()),
            };
            task.cycle_config = Some(CycleConfig {
                max_iterations: max_iter,
                guard,
                delay,
                no_converge,
                restart_on_failure: !no_restart_on_failure,
                max_failure_restarts,
            });
            println!(
                "Set cycle_config: max_iterations={}{}",
                max_iter,
                if no_converge { " (no-converge)" } else { "" }
            );
            changed = true;
        } else {
            // Allow updating guard/delay/no_converge on existing cycle config
            if let Some(expr) = cycle_guard {
                if let Some(ref mut config) = task.cycle_config {
                    config.guard = match crate::commands::add::parse_guard_expr(expr) {
                        Ok(g) => Some(g),
                        Err(e) => {
                            error = Some(e);
                            return false;
                        }
                    };
                    println!("Updated cycle guard");
                    changed = true;
                } else {
                    error = Some(anyhow::anyhow!(
                        "Cannot set --cycle-guard without --max-iterations: task has no cycle_config"
                    ));
                    return false;
                }
            }
            if let Some(d) = cycle_delay {
                if let Some(ref mut config) = task.cycle_config {
                    if parse_delay(d).is_none() {
                        error = Some(anyhow::anyhow!(
                            "Invalid cycle delay '{}'. Use format: 30s, 5m, 1h, 24h, 7d",
                            d
                        ));
                        return false;
                    }
                    config.delay = Some(d.to_string());
                    println!("Updated cycle delay: {}", d);
                    changed = true;
                } else {
                    error = Some(anyhow::anyhow!(
                        "Cannot set --cycle-delay without --max-iterations: task has no cycle_config"
                    ));
                    return false;
                }
            }
            if no_converge {
                if let Some(ref mut config) = task.cycle_config {
                    config.no_converge = true;
                    println!("Set no-converge on cycle");
                    changed = true;
                } else {
                    error = Some(anyhow::anyhow!(
                        "Cannot set --no-converge without --max-iterations: task has no cycle_config"
                    ));
                    return false;
                }
            }
            if no_restart_on_failure {
                if let Some(ref mut config) = task.cycle_config {
                    config.restart_on_failure = false;
                    println!("Disabled restart-on-failure for cycle");
                    changed = true;
                } else {
                    error = Some(anyhow::anyhow!(
                        "Cannot set --no-restart-on-failure without --max-iterations: task has no cycle_config"
                    ));
                    return false;
                }
            }
            if let Some(max) = max_failure_restarts {
                if let Some(ref mut config) = task.cycle_config {
                    config.max_failure_restarts = Some(max);
                    println!("Set max-failure-restarts: {}", max);
                    changed = true;
                } else {
                    error = Some(anyhow::anyhow!(
                        "Cannot set --max-failure-restarts without --max-iterations: task has no cycle_config"
                    ));
                    return false;
                }
            }
        }

        // Update visibility
        if let Some(vis) = visibility {
            match vis {
                "internal" | "public" | "peer" => {
                    let old = task.visibility.clone();
                    task.visibility = vis.to_string();
                    field_changes
                        .push(serde_json::json!({"field": "visibility", "old": old, "new": vis}));
                    println!("Updated visibility: {}", vis);
                    changed = true;
                }
                _ => {
                    error = Some(anyhow::anyhow!(
                        "Invalid visibility '{}'. Valid values: internal, public, peer",
                        vis
                    ));
                    return false;
                }
            }
        }

        // Update context scope
        if let Some(scope) = context_scope {
            // Validate
            if let Err(e) = scope.parse::<workgraph::context_scope::ContextScope>() {
                error = Some(anyhow::anyhow!("{}", e));
                return false;
            }
            let old = task.context_scope.clone();
            task.context_scope = Some(scope.to_string());
            field_changes
                .push(serde_json::json!({"field": "context_scope", "old": old, "new": scope}));
            println!("Updated context_scope: {}", scope);
            changed = true;
        }

        // Update exec mode
        if let Some(mode) = exec_mode {
            if let Err(e) = mode.parse::<workgraph::config::ExecMode>() {
                error = Some(anyhow::anyhow!("{}", e));
                return false;
            }
            let old = task.exec_mode.clone();
            task.exec_mode = Some(mode.to_string());
            field_changes.push(serde_json::json!({"field": "exec_mode", "old": old, "new": mode}));
            println!("Updated exec_mode: {}", mode);
            changed = true;
        }

        // Update not_before (from --delay or --not-before)
        if delay.is_some() && not_before.is_some() {
            error = Some(anyhow::anyhow!("Cannot specify both --delay and --not-before"));
            return false;
        }
        if let Some(d) = delay {
            let secs = match workgraph::graph::parse_delay(d) {
                Some(s) => s,
                None => {
                    error = Some(anyhow::anyhow!("Invalid delay '{}'. Use format: 30s, 5m, 1h, 24h, 7d", d));
                    return false;
                }
            };
            let new_ts = (chrono::Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339();
            let old = task.not_before.clone();
            task.not_before = Some(new_ts.clone());
            field_changes
                .push(serde_json::json!({"field": "not_before", "old": old, "new": new_ts}));
            println!("Set not_before: {} (delay {})", new_ts, d);
            changed = true;
        } else if let Some(ts) = not_before {
            if ts.parse::<chrono::DateTime<chrono::Utc>>().is_err()
                && chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S").is_err()
            {
                error = Some(anyhow::anyhow!("Invalid timestamp '{}'. Use ISO 8601 format", ts));
                return false;
            }
            let old = task.not_before.clone();
            task.not_before = Some(ts.to_string());
            field_changes.push(serde_json::json!({"field": "not_before", "old": old, "new": ts}));
            println!("Set not_before: {}", ts);
            changed = true;
        }
        // Update cron schedule
        if let Some(cron_expr) = cron {
            if cron_expr.is_empty() {
                // Clear cron scheduling
                task.cron_schedule = None;
                task.cron_enabled = false;
                task.next_cron_fire = None;
                task.last_cron_fire = None;
                println!("Cleared cron schedule");
                changed = true;
            } else {
                // Set or update cron schedule
                match workgraph::cron::parse_cron_expression(cron_expr) {
                    Ok(schedule) => {
                        task.cron_schedule = Some(cron_expr.to_string());
                        task.cron_enabled = true;
                        task.next_cron_fire = workgraph::cron::calculate_next_fire_with_jitter(
                            &task.id,
                            &schedule,
                            chrono::Utc::now(),
                        )
                        .map(|dt| dt.to_rfc3339());
                        println!(
                            "Set cron schedule: {} (next fire: {})",
                            cron_expr,
                            task.next_cron_fire.as_deref().unwrap_or("unknown")
                        );
                        changed = true;
                    }
                    Err(e) => {
                        error = Some(anyhow::anyhow!(
                            "Invalid cron expression '{}': {}",
                            cron_expr,
                            e
                        ));
                        return false;
                    }
                }
            }
        }

        // Reset spawn failure counter on any edit — the user may have fixed
        // the root cause (e.g., exec_mode mismatch), so the circuit breaker
        // should give the task a fresh set of attempts.
        if changed && task.spawn_failures > 0 {
            task.spawn_failures = 0;
            println!("Reset spawn failure counter");
        }
    } // task borrow released here

    // When new dependencies are added, clear any existing auto-assignment.
    // This prevents the race where a task gets assigned before its real
    // dependencies are wired (e.g., `wg add` then `wg edit --add-after`).
    if !add_after.is_empty() && changed {
        // Check dot-prefix first, fall back to legacy prefix
        let assign_task_id = format!(".assign-{}", task_id);
        let legacy_assign_id = format!("assign-{}", task_id);
        let found_id = if graph.get_task(&assign_task_id).is_some() {
            Some(assign_task_id)
        } else if graph.get_task(&legacy_assign_id).is_some() {
            Some(legacy_assign_id)
        } else {
            None
        };
        if let Some(ref aid) = found_id
            && let Some(assign_task) = graph.get_task_mut(aid)
        {
            match assign_task.status {
                workgraph::graph::Status::Open | workgraph::graph::Status::InProgress => {
                    assign_task.status = workgraph::graph::Status::Abandoned;
                    println!("Abandoned assignment task '{}' (dependencies changed)", aid);
                }
                _ => {}
            }
        }

        // Clear the agent field so the task gets re-assigned when actually ready
        let task = match graph.get_task_mut(task_id) {
            Some(t) => t,
            None => {
                error = Some(anyhow::anyhow!("Task '{}' not found", task_id));
                return false;
            }
        };
        if task.agent.is_some() {
            task.agent = None;
            println!("Cleared agent assignment (dependencies changed, will re-assign when ready)");
        }
    }

    // Maintain bidirectional consistency: update `blocks` on referenced tasks
    let task_id_owned = task_id.to_string();
    for dep in add_after {
        if let Some(blocker) = graph.get_task_mut(dep)
            && !blocker.before.contains(&task_id_owned)
        {
            blocker.before.push(task_id_owned.clone());
        }
    }
    for dep in remove_after {
        if let Some(blocker) = graph.get_task_mut(dep) {
            blocker.before.retain(|b| b != &task_id_owned);
        }
    }

    // Return whether changes were made
    changed
    })
    .context("Failed to modify graph")?;
    if let Some(e) = error {
        return Err(e);
    }

    if changed {
        super::notify_graph_changed(dir);

        // Record operation
        let config = workgraph::config::Config::load_or_default(dir);
        let _ = workgraph::provenance::record(
            dir,
            "edit",
            Some(task_id),
            None,
            serde_json::json!({ "fields": field_changes }),
            config.log.rotation_threshold,
        );

        println!("\nTask '{}' updated successfully", task_id);
    } else {
        println!("No changes made to task '{}'", task_id);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use workgraph::parser::{load_graph, save_graph};

    fn create_test_graph(dir: &Path) -> Result<()> {
        // Create the workgraph directory if it doesn't exist
        fs::create_dir_all(dir)?;

        // Create an empty graph.jsonl file
        let graph_path = graph_path(dir);
        fs::write(&graph_path, "")?;

        // Add a test task using the add command
        crate::commands::add::run(
            dir,
            "Test Task",
            Some("test-task"),
            Some("Original description"),
            &["dep1".to_string()],
            None,
            None,
            None,
            &["tag1".to_string()],
            &["skill1".to_string()],
            &[],
            &[],
            None,
            Some("claude:sonnet"),
            None,
            None, // verify
            None, // verify_timeout
            None, // validation
            None, // validator_agent
            None, // validator_model
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            None,
            None,
            false,
            false,
            &[],
            &[],
            None,
            None,
            true,  // allow_phantom: test graph uses phantom deps
            false, // independent
            false, // no_tier_escalation
            None,  // iteration_config
            None,  // priority
            None,  // cron
            false, // subtask
        )?;

        Ok(())
    }

    fn create_test_graph_with_two_tasks(dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        let graph_path = graph_path(dir);
        fs::write(&graph_path, "")?;

        // Add two independent tasks (no initial dependency between them)
        crate::commands::add::run(
            dir,
            "Blocker Task",
            Some("blocker-task"),
            None,
            &[],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None, // verify
            None, // verify_timeout
            None, // validation
            None, // validator_agent
            None, // validator_model
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            None,
            None,
            false,
            false,
            &[],
            &[],
            None,
            None,
            false,
            false,
            false, // no_tier_escalation
            None,  // iteration_config
            None,  // priority
            None,  // cron
            false, // subtask
        )?;

        crate::commands::add::run(
            dir,
            "Test Task",
            Some("test-task"),
            Some("Original description"),
            &[],
            None,
            None,
            None,
            &["tag1".to_string()],
            &["skill1".to_string()],
            &[],
            &[],
            None,
            Some("claude:sonnet"),
            None,
            None, // verify
            None, // verify_timeout
            None, // validation
            None, // validator_agent
            None, // validator_model
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            None,
            None,
            false,
            false,
            &[],
            &[],
            None,
            None,
            false,
            false,
            false, // no_tier_escalation
            None,
            None,  // priority
            None,  // cron
            false, // subtask
        )?;

        Ok(())
    }

    #[test]
    fn test_edit_title() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            Some("New Title"),
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert_eq!(task.title, "New Title");
    }

    #[test]
    fn test_edit_description() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            Some("New description"),
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert_eq!(task.description, Some("New description".to_string()));
    }

    #[test]
    fn test_add_after() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["dep2".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,  // cron
            true,  // allow_phantom: dep2 doesn't exist in test graph
            false, // allow_cycle: tests should not allow cycles by default
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(task.after.contains(&"dep2".to_string()));
        assert!(task.after.contains(&"dep1".to_string()));
    }

    #[test]
    fn test_remove_after() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &["dep1".to_string()],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(!task.after.contains(&"dep1".to_string()));
    }

    #[test]
    fn test_add_tag() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &["tag2".to_string()],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(task.tags.contains(&"tag2".to_string()));
        assert!(task.tags.contains(&"tag1".to_string()));
    }

    #[test]
    fn test_remove_tag() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &["tag1".to_string()],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(!task.tags.contains(&"tag1".to_string()));
    }

    #[test]
    fn test_edit_model() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            Some("claude:opus"),
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert_eq!(task.model, Some("claude:opus".to_string()));
    }

    #[test]
    fn test_add_skill() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &["skill2".to_string()],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(task.skills.contains(&"skill2".to_string()));
        assert!(task.skills.contains(&"skill1".to_string()));
    }

    #[test]
    fn test_remove_skill() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &["skill1".to_string()],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(!task.skills.contains(&"skill1".to_string()));
    }

    #[test]
    fn test_task_not_found() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "nonexistent-task",
            Some("New Title"),
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_no_changes() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_self_blocking_rejected() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph(temp_dir.path()).unwrap();

        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["test-task".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot block itself")
        );
    }

    #[test]
    fn test_add_after_updates_blocker_blocks() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph_with_two_tasks(temp_dir.path()).unwrap();

        // Add a new after edge
        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["blocker-task".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();

        // Verify bidirectional consistency
        let blocker = graph.get_task("blocker-task").unwrap();
        assert!(
            blocker.before.contains(&"test-task".to_string()),
            "blocker-task.before should contain test-task"
        );
    }

    #[test]
    fn test_remove_after_updates_blocker_blocks() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph_with_two_tasks(temp_dir.path()).unwrap();

        // First add the dependency, then remove it
        run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["blocker-task".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        )
        .unwrap();

        // Remove the after edge
        let result = run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &[],
            &["blocker-task".to_string()],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        );
        assert!(result.is_ok());

        let path = graph_path(temp_dir.path());
        let graph = load_graph(&path).unwrap();

        // Verify bidirectional consistency
        let blocker = graph.get_task("blocker-task").unwrap();
        assert!(
            !blocker.before.contains(&"test-task".to_string()),
            "blocker-task.before should NOT contain test-task after removal"
        );
    }

    #[test]
    fn test_add_after_clears_agent_assignment() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph_with_two_tasks(temp_dir.path()).unwrap();

        // Set an agent on test-task
        let path = graph_path(temp_dir.path());
        {
            let mut graph = load_graph(&path).unwrap();
            let task = graph.get_task_mut("test-task").unwrap();
            task.agent = Some("some-agent-hash".to_string());
            save_graph(&graph, &path).unwrap();
        }

        // Add a new dependency
        run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["blocker-task".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        )
        .unwrap();

        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(
            task.agent.is_none(),
            "agent should be cleared when new dependencies are added"
        );
        assert!(
            task.after.contains(&"blocker-task".to_string()),
            "dependency should be added"
        );
    }

    #[test]
    fn test_add_after_abandons_assign_task() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph_with_two_tasks(temp_dir.path()).unwrap();

        // Create an assign task for test-task
        let path = graph_path(temp_dir.path());
        {
            let mut graph = load_graph(&path).unwrap();
            let assign_task = workgraph::graph::Task {
                id: "assign-test-task".to_string(),
                title: "Assign agent for: Test Task".to_string(),
                status: workgraph::graph::Status::Open,
                tags: vec!["assignment".to_string()],
                before: vec!["test-task".to_string()],
                ..workgraph::graph::Task::default()
            };
            graph.add_node(workgraph::graph::Node::Task(assign_task));
            save_graph(&graph, &path).unwrap();
        }

        // Add a new dependency
        run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["blocker-task".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        )
        .unwrap();

        let graph = load_graph(&path).unwrap();
        let assign = graph.get_task("assign-test-task").unwrap();
        assert_eq!(
            assign.status,
            workgraph::graph::Status::Abandoned,
            "assign task should be abandoned when deps change"
        );
    }

    #[test]
    fn test_add_duplicate_dep_does_not_clear_agent() {
        let temp_dir = TempDir::new().unwrap();
        create_test_graph_with_two_tasks(temp_dir.path()).unwrap();

        // First add the dependency
        run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["blocker-task".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        )
        .unwrap();

        // Now set an agent
        let path = graph_path(temp_dir.path());
        {
            let mut graph = load_graph(&path).unwrap();
            let task = graph.get_task_mut("test-task").unwrap();
            task.agent = Some("some-agent-hash".to_string());
            save_graph(&graph, &path).unwrap();
        }

        // Try to add the same dep again (no actual new dep)
        run(
            temp_dir.path(),
            "test-task",
            None,
            None,
            &["blocker-task".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false,
        )
        .unwrap();

        // Agent should NOT be cleared since no new deps were actually added
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("test-task").unwrap();
        assert!(
            task.agent.is_some(),
            "agent should NOT be cleared when no new deps are actually added"
        );
    }

    #[test]
    fn test_cycle_detection_blocks_unconfigured_cycle() {
        use crate::commands::graph_path;
        use tempfile::TempDir;
        use workgraph::graph::{Node, Status, Task, WorkGraph};
        use workgraph::parser::save_graph;

        let temp_dir = TempDir::new().unwrap();
        let path = graph_path(temp_dir.path());

        // Create a simple graph with: task-a → task-b
        let mut graph = WorkGraph::new();

        let mut task_a = Task::default();
        task_a.id = "task-a".to_string();
        task_a.title = "Task A".to_string();
        task_a.status = Status::Open;

        let mut task_b = Task::default();
        task_b.id = "task-b".to_string();
        task_b.title = "Task B".to_string();
        task_b.status = Status::Open;
        task_b.after.push("task-a".to_string()); // task-b depends on task-a

        graph.add_node(Node::Task(task_a));
        graph.add_node(Node::Task(task_b));
        save_graph(&graph, &path).unwrap();

        // Try to add task-a -> task-b (would create cycle task-a -> task-b -> task-a)
        let result = run(
            temp_dir.path(),
            "task-a",
            None,
            None,
            &["task-b".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            false, // allow_cycle = false
        );

        // Should fail with cycle detection message
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("would create a cycle without CycleConfig"));
        assert!(error_msg.contains("--allow-cycle"));
    }

    #[test]
    fn test_cycle_detection_allows_with_flag() {
        use crate::commands::{graph_path, load_graph};
        use tempfile::TempDir;
        use workgraph::graph::{Node, Status, Task, WorkGraph};
        use workgraph::parser::save_graph;

        let temp_dir = TempDir::new().unwrap();
        let path = graph_path(temp_dir.path());

        // Create a simple graph with: task-a → task-b
        let mut graph = WorkGraph::new();

        let mut task_a = Task::default();
        task_a.id = "task-a".to_string();
        task_a.title = "Task A".to_string();
        task_a.status = Status::Open;

        let mut task_b = Task::default();
        task_b.id = "task-b".to_string();
        task_b.title = "Task B".to_string();
        task_b.status = Status::Open;
        task_b.after.push("task-a".to_string());

        graph.add_node(Node::Task(task_a));
        graph.add_node(Node::Task(task_b));
        save_graph(&graph, &path).unwrap();

        // Try to add task-a -> task-b with --allow-cycle
        let result = run(
            temp_dir.path(),
            "task-a",
            None,
            None,
            &["task-b".to_string()],
            &[],
            &[],
            &[],
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // cron
            false,
            true, // allow_cycle = true
        );

        // Should succeed when allow_cycle is true
        assert!(result.is_ok());

        // Verify the cycle was actually created
        let graph = load_graph(&path).unwrap();
        let task_a = graph.get_task("task-a").unwrap();
        assert!(task_a.after.contains(&"task-b".to_string()));
    }
}

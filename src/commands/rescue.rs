//! `wg rescue` — thin wrapper over `wg insert parallel <target>
//! --replace-edges` that adds the metadata + logging appropriate for
//! the rescue use case.
//!
//! Intended primary caller: the `.evaluate-*` agent, when it judges a
//! task failed and can describe a concrete fix. The evaluator invokes
//! `wg rescue <failed-target> --description "..."`, which:
//!
//! 1. Creates a new task `R` in the graph at the failed task's slot
//!    (via `insert::run(Position::Parallel, ..., replace_edges=true)`).
//! 2. Rewires successors to unblock from R only (not from the failed
//!    target).
//! 3. Writes metadata on both tasks:
//!      target.log: "superseded by R (rescue from eval <eval-id>)"
//!      R.log:     "supersedes target (created from eval <eval-id>)"
//! 4. Appends an `op: "rescue"` entry to `.workgraph/log/operations.jsonl`.
//! 5. Emits a dim stderr line for interactive visibility.
//!
//! Design invariants:
//! - Rescue tasks are **first-class** in the graph — no dot-prefix,
//!   visible in `wg list` / `wg show` / `wg viz`. Matches the project
//!   principle that real work lives in the regular graph, not hidden
//!   shadow tasks.
//! - The failed target stays in the graph for history. Its `before`
//!   list is cleared (rescue took over its successors), but its
//!   `after` list is preserved so you can see what it depended on.
//! - If rescue itself fails (status=Failed), downstream tooling can
//!   decide to rescue-the-rescue, subject to an attempt counter carried
//!   in the description / a future `rescue_depth` field.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::commands::insert::{self, InsertOptions, Position};
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::modify_graph;

/// Run `wg rescue`. Returns the new task's ID.
pub fn run(
    dir: &Path,
    target_id: &str,
    description: &str,
    title: Option<&str>,
    new_id: Option<&str>,
    from_eval: Option<&str>,
    actor: Option<&str>,
) -> Result<String> {
    if description.trim().is_empty() {
        anyhow::bail!(
            "Rescue description is required — it becomes the next agent's assignment. \
             Be specific about what needs to change."
        );
    }

    // Verify target exists up front so we don't create a stranded rescue
    // when the user typo'd the id.
    {
        let (graph, _) = crate::commands::load_workgraph(dir)?;
        let target = graph.get_task(target_id).ok_or_else(|| {
            anyhow::anyhow!("rescue target task '{}' not found in graph", target_id)
        })?;
        // Soft warning if target is already terminal-success — rescuing a
        // done task is unusual but not forbidden (the user might have
        // discovered the "done" was wrong). Log it for visibility.
        if target.status == Status::Done {
            eprintln!(
                "\x1b[33m[rescue] warning: target '{}' is already Done. \
                 Proceeding — new task will route successors around it.\x1b[0m",
                target_id
            );
        }
    }

    // Derive a title from the target if not supplied.
    let rescue_title = title
        .map(String::from)
        .unwrap_or_else(|| format!("Rescue: {}", target_id));

    // The description the evaluator wrote IS the rescue task's
    // description — it's literally the next agent's brief. Stamp it
    // with provenance so the worker knows this is a rescue, of what,
    // and from which eval.
    let stamped_description = format!(
        "## Rescue for `{target}`\n\
         \n\
         This task supersedes `{target}`, which failed evaluation. \
         Your job is to complete the work correctly.\n\
         \n\
         **Source task:** `{target}`  \n\
         **Eval task that spawned this rescue:** {eval}  \n\
         **Rescue attempt created:** {when}\n\
         \n\
         ---\n\
         \n\
         ## What to fix (from the evaluator)\n\
         \n\
         {body}",
        target = target_id,
        eval = from_eval.unwrap_or("(none — invoked directly)"),
        when = Utc::now().to_rfc3339(),
        body = description.trim(),
    );

    // Delegate the actual graph surgery to `insert`. This is the full
    // extent of rescue's graph mutation — all the edge-rewrite
    // bidirectional-consistency stuff lives in `insert::run`.
    let new_task_id = insert::run(
        dir,
        Position::Parallel,
        target_id,
        &rescue_title,
        Some(&stamped_description),
        new_id,
        InsertOptions {
            replace_edges: true,
            ..Default::default()
        },
    )
    .context("insert::run for rescue failed")?;

    // Attach rescue-specific metadata: log entries on both tasks,
    // plus tag the new task with "rescue" so `wg list --tag rescue`
    // surfaces them cheaply.
    let path = crate::commands::graph_path(dir);
    let actor_str = actor.unwrap_or("rescue");
    let target_id_s = target_id.to_string();
    let new_id_s = new_task_id.clone();
    let eval_ref = from_eval.unwrap_or("(direct)").to_string();
    let actor_owned = actor_str.to_string();

    let _ = modify_graph(&path, move |graph| {
        let mut changed = false;

        if let Some(target) = graph.get_task_mut(&target_id_s) {
            target.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some(actor_owned.clone()),
                user: Some(workgraph::current_user()),
                message: format!(
                    "superseded by rescue task '{}' (from eval {})",
                    new_id_s, eval_ref
                ),
            });
            changed = true;
        }

        if let Some(rescue) = graph.get_task_mut(&new_id_s) {
            rescue.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some(actor_owned.clone()),
                user: Some(workgraph::current_user()),
                message: format!(
                    "supersedes '{}'; created from eval {}",
                    target_id_s, eval_ref
                ),
            });
            if !rescue.tags.iter().any(|t| t == "rescue") {
                rescue.tags.push("rescue".to_string());
            }
            changed = true;
        }

        changed
    });

    // Audit entry in the cross-cutting operations log.
    let _ = workgraph::provenance::record(
        dir,
        "rescue",
        Some(&new_task_id),
        Some(actor_str),
        serde_json::json!({
            "target": target_id,
            "from_eval": from_eval,
            "title": rescue_title,
        }),
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    );

    // Interactive visibility — dim line so it reads as telemetry, not
    // primary output.
    eprintln!(
        "\x1b[2m[rescue] created '{}' superseding '{}'{}\x1b[0m",
        new_task_id,
        target_id,
        from_eval
            .map(|e| format!(" (from eval {})", e))
            .unwrap_or_default()
    );

    Ok(new_task_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::Task;
    use workgraph::parser::load_graph;
    use workgraph::test_helpers::{make_task_with_status, setup_workgraph};
    use tempfile::tempdir;

    fn make(id: &str, status: Status) -> Task {
        make_task_with_status(id, id, status)
    }

    fn setup_classic_fan(dir: &Path) {
        // P → T → S
        let mut p = make("p", Status::Done);
        let mut t = make("t", Status::Failed);
        let mut s = make("s", Status::Open);
        p.before = vec!["t".into()];
        t.after = vec!["p".into()];
        t.before = vec!["s".into()];
        s.after = vec!["t".into()];
        setup_workgraph(dir, vec![p, t, s]);
    }

    #[test]
    fn rescue_creates_first_class_task_with_rescue_tag() {
        let dir = tempdir().unwrap();
        setup_classic_fan(dir.path());

        let new_id = run(
            dir.path(),
            "t",
            "Implement the feature correctly — the previous attempt wrote to /tmp, use cwd.",
            None,
            Some("rescue-t"),
            Some(".evaluate-t"),
            Some("evaluator"),
        )
        .unwrap();

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();
        let r = g.get_task(&new_id).unwrap();
        // First-class ID — no dot-prefix
        assert!(!r.id.starts_with('.'), "rescue task must not be dot-prefixed");
        // Tagged for discoverability
        assert!(r.tags.iter().any(|t| t == "rescue"));
        // Description carries provenance
        let desc = r.description.as_deref().unwrap_or("");
        assert!(desc.contains("Rescue for `t`"));
        assert!(desc.contains(".evaluate-t"));
        assert!(desc.contains("What to fix"));
        assert!(desc.contains("wrote to /tmp"));
    }

    #[test]
    fn rescue_reroutes_successors_and_preserves_target() {
        let dir = tempdir().unwrap();
        setup_classic_fan(dir.path());

        let new_id = run(
            dir.path(),
            "t",
            "fix the thing",
            None,
            Some("rescue-t"),
            Some(".evaluate-t"),
            Some("evaluator"),
        )
        .unwrap();

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();

        // Rescue inherits target's edges via insert::run(Parallel, replace_edges=true)
        let r = g.get_task(&new_id).unwrap();
        assert_eq!(r.after, vec!["p".to_string()]);
        assert_eq!(r.before, vec!["s".to_string()]);

        // Successor waits on rescue only
        let s = g.get_task("s").unwrap();
        assert_eq!(s.after, vec![new_id.clone()]);
        assert!(!s.after.contains(&"t".to_string()));

        // Failed target still present for history; outgoing cleared
        let t = g.get_task("t").unwrap();
        assert_eq!(t.status, Status::Failed);
        assert!(t.before.is_empty(), "target's outgoing cleared");
        // Target log records the supersession
        assert!(
            t.log
                .iter()
                .any(|e| e.message.contains("superseded by rescue task")),
            "target log should record supersession"
        );
    }

    #[test]
    fn rescue_rejects_empty_description() {
        let dir = tempdir().unwrap();
        setup_classic_fan(dir.path());

        let result = run(
            dir.path(),
            "t",
            "   ",
            None,
            None,
            Some(".evaluate-t"),
            Some("evaluator"),
        );
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(err.contains("description"));
    }

    #[test]
    fn rescue_errors_on_nonexistent_target() {
        let dir = tempdir().unwrap();
        setup_classic_fan(dir.path());

        let result = run(
            dir.path(),
            "nonexistent",
            "do a thing",
            None,
            None,
            None,
            Some("evaluator"),
        );
        assert!(result.is_err());
        let err = format!("{}", result.err().unwrap());
        assert!(err.contains("not found"));
    }

    #[test]
    fn rescue_writes_operations_log_entry() {
        let dir = tempdir().unwrap();
        setup_classic_fan(dir.path());

        let new_id = run(
            dir.path(),
            "t",
            "fix",
            None,
            Some("rescue-t"),
            Some(".evaluate-t"),
            Some("evaluator"),
        )
        .unwrap();

        let ops_path = workgraph::provenance::operations_path(dir.path());
        assert!(ops_path.exists(), "operations.jsonl should exist");
        let content = std::fs::read_to_string(&ops_path).unwrap();
        assert!(content.contains(r#""op":"rescue""#));
        assert!(content.contains(&new_id));
        assert!(content.contains("\"target\":\"t\""));
        assert!(content.contains(".evaluate-t"));
    }
}

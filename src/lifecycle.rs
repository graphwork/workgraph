//! Task lifecycle helpers — deprecation migrations and shared mutation routines.
//!
//! These helpers operate on an in-memory `WorkGraph` and are exposed at the lib
//! level so both the dispatcher and integration tests can drive them.

use chrono::Utc;

use crate::current_user;
use crate::graph::{LogEntry, Status, WorkGraph};

/// Migrate any tasks in legacy `PendingValidation` status to `Done`.
///
/// `PendingValidation` is deprecated as a routine task lifecycle state. Tasks
/// that opted into the legacy `--validation=llm`, `--validation=external`, or
/// `verify_mode=separate` paths now end up `Done` immediately; agency
/// `.evaluate-X` tasks are the unblock gate for downstream dependents.
///
/// Tasks tagged with `human-review` are exempt — those are the only remaining
/// routine use of `PendingValidation` (e.g. cross-org review on a
/// public-visibility task that explicitly opts in).
///
/// Returns the IDs of tasks that were migrated.
pub fn migrate_pending_validation_tasks(graph: &mut WorkGraph) -> Vec<String> {
    let to_migrate: Vec<String> = graph
        .tasks()
        .filter(|t| t.status == Status::PendingValidation)
        .filter(|t| !t.tags.iter().any(|tag| tag == "human-review"))
        .map(|t| t.id.clone())
        .collect();

    let mut migrated = Vec::with_capacity(to_migrate.len());
    for task_id in to_migrate {
        if let Some(task) = graph.get_task_mut(&task_id) {
            task.status = Status::Done;
            if task.completed_at.is_none() {
                task.completed_at = Some(Utc::now().to_rfc3339());
            }
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                user: Some(current_user()),
                message:
                    "Migrated PendingValidation → Done (deprecate-pending-validation): \
                     agency `.evaluate-*` is now the dependency-unblock gate. \
                     To force re-spawn instead, run `wg reject <task>`."
                        .to_string(),
            });
            migrated.push(task_id);
        }
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Node, Task};

    fn task(id: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            status,
            ..Task::default()
        }
    }

    #[test]
    fn migrates_pending_validation_to_done_with_log() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task("stuck", Status::PendingValidation)));
        graph.add_node(Node::Task(task("other", Status::Open)));

        let migrated = migrate_pending_validation_tasks(&mut graph);
        assert_eq!(migrated, vec!["stuck".to_string()]);

        let stuck = graph.get_task("stuck").unwrap();
        assert_eq!(stuck.status, Status::Done);
        assert!(stuck.completed_at.is_some());
        assert!(
            stuck
                .log
                .iter()
                .any(|e| e.message.contains("Migrated PendingValidation"))
        );

        let other = graph.get_task("other").unwrap();
        assert_eq!(other.status, Status::Open);
    }

    #[test]
    fn idempotent_after_first_sweep() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(task("stuck", Status::PendingValidation)));
        let first = migrate_pending_validation_tasks(&mut graph);
        assert_eq!(first.len(), 1);
        let second = migrate_pending_validation_tasks(&mut graph);
        assert!(second.is_empty(), "no PendingValidation tasks left to migrate");
    }

    #[test]
    fn skips_human_review_opt_in() {
        let mut graph = WorkGraph::new();
        let mut t = task("opt-in", Status::PendingValidation);
        t.tags = vec!["human-review".to_string()];
        graph.add_node(Node::Task(t));

        let migrated = migrate_pending_validation_tasks(&mut graph);
        assert!(migrated.is_empty(), "human-review tasks must not be migrated");
        assert_eq!(
            graph.get_task("opt-in").unwrap().status,
            Status::PendingValidation
        );
    }
}

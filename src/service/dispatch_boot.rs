//! Dispatcher boot-time graph enumeration.
//!
//! Bug A (orphan chat supervisor) regression-guard: the daemon must spawn
//! chat supervisors based on what's in the live graph, not a hardcoded
//! `coordinator-0`. A fresh `wg init` has no `.chat-N` task, so a hardcoded
//! supervisor would call `wg spawn-task .chat-0` against a non-existent ID
//! and burn the restart budget chasing a phantom.
//!
//! See `tests/integration_dispatch_boot.rs` for the regression tests that
//! pin this behavior.

use crate::graph::{Status, WorkGraph};

/// Identifies a chat supervisor that should be spawned at daemon boot.
///
/// `is_legacy` is true when the underlying graph task uses the deprecated
/// `.coordinator-N` prefix (still loaded for one release; emits a one-time
/// warning when the daemon spawns the supervisor). New tasks use `.chat-N`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSupervisorBootSpec {
    pub chat_id: u32,
    pub is_legacy: bool,
}

/// Enumerate the chat supervisors that should be spawned when the daemon boots.
///
/// Reads the graph: every task tagged with a chat-loop tag (`chat-loop` or
/// legacy `coordinator-loop`) whose ID parses as `.chat-N` or `.coordinator-N`,
/// whose status is not `Done`/`Abandoned`, and that is not tagged `archived`
/// becomes one supervisor.
///
/// Returns supervisors sorted by chat ID. Duplicates collapsed: if both
/// `.chat-N` and `.coordinator-N` exist for the same N, only the new-prefix
/// entry is returned.
pub fn enumerate_chat_supervisors_from_graph(graph: &WorkGraph) -> Vec<ChatSupervisorBootSpec> {
    let mut new_ids: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut legacy_ids: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    for task in graph.tasks() {
        let has_chat_tag = task
            .tags
            .iter()
            .any(|t| crate::chat_id::is_chat_loop_tag(t));
        if !has_chat_tag {
            continue;
        }
        if matches!(task.status, Status::Abandoned | Status::Done) {
            continue;
        }
        if task.tags.iter().any(|t| t == "archived") {
            continue;
        }
        let Some(id) = crate::chat_id::parse_chat_task_id(&task.id) else {
            continue;
        };
        if task.id.starts_with(crate::chat_id::LEGACY_COORDINATOR_PREFIX) {
            legacy_ids.insert(id);
        } else {
            new_ids.insert(id);
        }
    }
    let mut out: Vec<ChatSupervisorBootSpec> = Vec::new();
    for id in &new_ids {
        out.push(ChatSupervisorBootSpec {
            chat_id: *id,
            is_legacy: false,
        });
    }
    for id in &legacy_ids {
        if new_ids.contains(id) {
            continue;
        }
        out.push(ChatSupervisorBootSpec {
            chat_id: *id,
            is_legacy: true,
        });
    }
    out.sort_by_key(|s| s.chat_id);
    out
}

/// Convenience: enumerate from disk (loads the graph from `.workgraph/graph.jsonl`).
/// Returns an empty vec if the graph file is missing or unreadable.
pub fn enumerate_chat_supervisors_for_boot(dir: &std::path::Path) -> Vec<ChatSupervisorBootSpec> {
    let gp = dir.join("graph.jsonl");
    match crate::parser::load_graph(&gp) {
        Ok(g) => enumerate_chat_supervisors_from_graph(&g),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_id::{CHAT_LOOP_TAG, LEGACY_COORDINATOR_LOOP_TAG};
    use crate::graph::{Node, Task};

    fn chat_task(id: &str, tag: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            status,
            tags: vec![tag.to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn empty_graph_yields_empty_supervisor_list() {
        let g = WorkGraph::new();
        assert!(enumerate_chat_supervisors_from_graph(&g).is_empty());
    }

    #[test]
    fn graph_without_chat_tagged_tasks_yields_empty() {
        let mut g = WorkGraph::new();
        g.add_node(Node::Task(Task {
            id: "regular-task".to_string(),
            title: "regular".to_string(),
            status: Status::Open,
            ..Default::default()
        }));
        assert!(enumerate_chat_supervisors_from_graph(&g).is_empty());
    }

    #[test]
    fn graph_with_chat_3_yields_supervisor_3_non_legacy() {
        let mut g = WorkGraph::new();
        g.add_node(Node::Task(chat_task(
            ".chat-3",
            CHAT_LOOP_TAG,
            Status::InProgress,
        )));
        let out = enumerate_chat_supervisors_from_graph(&g);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chat_id, 3);
        assert!(!out[0].is_legacy);
    }

    #[test]
    fn graph_with_legacy_coordinator_1_yields_supervisor_1_legacy() {
        let mut g = WorkGraph::new();
        g.add_node(Node::Task(chat_task(
            ".coordinator-1",
            LEGACY_COORDINATOR_LOOP_TAG,
            Status::InProgress,
        )));
        let out = enumerate_chat_supervisors_from_graph(&g);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chat_id, 1);
        assert!(out[0].is_legacy);
    }

    #[test]
    fn abandoned_chat_task_is_excluded() {
        let mut g = WorkGraph::new();
        g.add_node(Node::Task(chat_task(
            ".chat-2",
            CHAT_LOOP_TAG,
            Status::Abandoned,
        )));
        assert!(enumerate_chat_supervisors_from_graph(&g).is_empty());
    }

    #[test]
    fn done_chat_task_is_excluded() {
        let mut g = WorkGraph::new();
        g.add_node(Node::Task(chat_task(".chat-7", CHAT_LOOP_TAG, Status::Done)));
        assert!(enumerate_chat_supervisors_from_graph(&g).is_empty());
    }

    #[test]
    fn archived_chat_task_is_excluded() {
        let mut g = WorkGraph::new();
        let mut t = chat_task(".chat-5", CHAT_LOOP_TAG, Status::Open);
        t.tags.push("archived".to_string());
        g.add_node(Node::Task(t));
        assert!(enumerate_chat_supervisors_from_graph(&g).is_empty());
    }

    #[test]
    fn duplicate_chat_n_and_coordinator_n_collapses_to_chat_n() {
        let mut g = WorkGraph::new();
        g.add_node(Node::Task(chat_task(
            ".chat-4",
            CHAT_LOOP_TAG,
            Status::InProgress,
        )));
        g.add_node(Node::Task(chat_task(
            ".coordinator-4",
            LEGACY_COORDINATOR_LOOP_TAG,
            Status::InProgress,
        )));
        let out = enumerate_chat_supervisors_from_graph(&g);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chat_id, 4);
        assert!(!out[0].is_legacy);
    }

    #[test]
    fn multiple_chat_tasks_sorted_by_id() {
        let mut g = WorkGraph::new();
        g.add_node(Node::Task(chat_task(
            ".chat-9",
            CHAT_LOOP_TAG,
            Status::InProgress,
        )));
        g.add_node(Node::Task(chat_task(
            ".chat-2",
            CHAT_LOOP_TAG,
            Status::InProgress,
        )));
        g.add_node(Node::Task(chat_task(
            ".coordinator-5",
            LEGACY_COORDINATOR_LOOP_TAG,
            Status::InProgress,
        )));
        let out = enumerate_chat_supervisors_from_graph(&g);
        let ids: Vec<u32> = out.iter().map(|s| s.chat_id).collect();
        assert_eq!(ids, vec![2, 5, 9]);
    }
}

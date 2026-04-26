//! Migration commands. Currently supports the chat-rename migration:
//! rewrites legacy `.coordinator-N` task ids to `.chat-N`, fixes up
//! after-edges, renames `coordinator-loop` tags to `chat-loop`, and
//! rewrites `Coordinator: <name>` / `Coordinator N` titles.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use workgraph::chat_id::{
    CHAT_LOOP_TAG, CHAT_PREFIX, LEGACY_COORDINATOR_LOOP_TAG, LEGACY_COORDINATOR_PREFIX,
};
use workgraph::graph::{LogEntry, Node};
use workgraph::parser::modify_graph;

use super::graph_path;

/// Result of a chat-rename migration.
#[derive(Debug, Default, Clone)]
pub struct ChatRenameMigrationResult {
    /// Old `.coordinator-N` ids that were rewritten to `.chat-N`.
    pub renamed_ids: Vec<(String, String)>,
    /// Number of `after`-edges that were rewritten on dependent tasks.
    pub rewritten_edges: usize,
    /// Number of tags renamed from `coordinator-loop` to `chat-loop`.
    pub renamed_tags: usize,
    /// Number of titles rewritten from `Coordinator: …` / `Coordinator N` to the new form.
    pub renamed_titles: usize,
}

impl ChatRenameMigrationResult {
    pub fn is_empty(&self) -> bool {
        self.renamed_ids.is_empty()
            && self.rewritten_edges == 0
            && self.renamed_tags == 0
            && self.renamed_titles == 0
    }
}

fn maybe_new_title(title: &str) -> Option<String> {
    if let Some(rest) = title.strip_prefix("Coordinator: ") {
        return Some(format!("Chat: {}", rest));
    }
    if let Some(rest) = title.strip_prefix("Coordinator ")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("Chat {}", rest));
    }
    None
}

/// Rewrite legacy chat-agent task ids and tags to the new canonical form.
///
/// Runs in-place on `<dir>/graph.jsonl`. Idempotent — running twice on a
/// migrated graph is a no-op.
pub fn run_chat_rename(dir: &Path, dry_run: bool, json: bool) -> Result<()> {
    let graph_path = graph_path(dir);

    let mut result = ChatRenameMigrationResult::default();
    let now = chrono::Utc::now().to_rfc3339();

    if dry_run {
        let graph = workgraph::parser::load_graph(&graph_path)?;
        for task in graph.tasks() {
            if task.id.starts_with(LEGACY_COORDINATOR_PREFIX) {
                let suffix = &task.id[LEGACY_COORDINATOR_PREFIX.len()..];
                let new_id = format!("{}{}", CHAT_PREFIX, suffix);
                result.renamed_ids.push((task.id.clone(), new_id));
            }
            if task.tags.iter().any(|t| t == LEGACY_COORDINATOR_LOOP_TAG) {
                result.renamed_tags += 1;
            }
            if maybe_new_title(&task.title).is_some() {
                result.renamed_titles += 1;
            }
            for after in &task.after {
                if after.starts_with(LEGACY_COORDINATOR_PREFIX) {
                    result.rewritten_edges += 1;
                }
            }
        }
    } else {
        modify_graph(&graph_path, |graph| {
            // Phase 1: build the id remap.
            let id_remap: HashMap<String, String> = graph
                .tasks()
                .filter_map(|t| {
                    t.id.strip_prefix(LEGACY_COORDINATOR_PREFIX)
                        .map(|suffix| (t.id.clone(), format!("{}{}", CHAT_PREFIX, suffix)))
                })
                .collect();
            for (old, new) in &id_remap {
                result.renamed_ids.push((old.clone(), new.clone()));
            }

            // Phase 2: collect all current task ids (keys to iterate).
            let all_ids: Vec<String> = graph.tasks().map(|t| t.id.clone()).collect();

            // Phase 3: rewrite each task's fields in place — at this point
            // the HashMap key still equals the task.id (no re-keying yet),
            // so get_task_mut works with the OLD id.
            for old_key in &all_ids {
                if let Some(t) = graph.get_task_mut(old_key) {
                    // Rewrite after-edges for this task.
                    let mut local_edges = 0usize;
                    for after in t.after.iter_mut() {
                        if let Some(new_id) = id_remap.get(after) {
                            *after = new_id.clone();
                            local_edges += 1;
                        }
                    }
                    if local_edges > 0 {
                        result.rewritten_edges += local_edges;
                    }

                    // Rewrite legacy tags.
                    let mut renamed_tag_in_task = false;
                    for tag in t.tags.iter_mut() {
                        if tag == LEGACY_COORDINATOR_LOOP_TAG {
                            *tag = CHAT_LOOP_TAG.to_string();
                            renamed_tag_in_task = true;
                        }
                    }
                    if renamed_tag_in_task {
                        result.renamed_tags += 1;
                    }

                    // Rewrite legacy titles.
                    if let Some(new_title) = maybe_new_title(&t.title) {
                        t.title = new_title;
                        result.renamed_titles += 1;
                    }

                    // Rewrite this task's own id if it's a legacy coordinator id.
                    if let Some(new_id) = id_remap.get(&t.id) {
                        let old_id = t.id.clone();
                        t.id = new_id.clone();
                        t.log.push(LogEntry {
                            timestamp: now.clone(),
                            actor: Some("migration".to_string()),
                            user: Some(workgraph::current_user()),
                            message: format!(
                                "wg migrate chat-rename: renamed task id {} -> {}",
                                old_id, new_id
                            ),
                        });
                    }
                }
            }

            // Phase 4: re-key the HashMap so lookups by the NEW id work.
            // We pull each renamed task out by its old key and re-add it,
            // which inserts at the new key (add_node uses node.id()).
            for (old_id, _new_id) in &id_remap {
                if let Some(node) = graph.take_node(old_id) {
                    graph.add_node(node);
                }
            }

            true
        })?;
    }

    if json {
        let payload = serde_json::json!({
            "renamed_ids": result.renamed_ids,
            "rewritten_edges": result.rewritten_edges,
            "renamed_tags": result.renamed_tags,
            "renamed_titles": result.renamed_titles,
            "dry_run": dry_run,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if result.is_empty() {
        println!("No legacy coordinator data found — graph is already on the new schema.");
    } else {
        if dry_run {
            println!("Dry run — no changes written:");
        } else {
            println!("Migration complete:");
        }
        println!("  task ids renamed: {}", result.renamed_ids.len());
        for (old, new) in &result.renamed_ids {
            println!("    {} -> {}", old, new);
        }
        println!("  after-edges rewritten: {}", result.rewritten_edges);
        println!(
            "  tags renamed (coordinator-loop -> chat-loop): {}",
            result.renamed_tags
        );
        println!("  titles renamed: {}", result.renamed_titles);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Status, Task, WorkGraph};

    fn write_graph(dir: &Path, tasks: Vec<Task>) {
        let workgraph_dir = dir.join(".workgraph");
        std::fs::create_dir_all(&workgraph_dir).unwrap();
        let graph_path = workgraph_dir.join("graph.jsonl");
        let mut graph = WorkGraph::new();
        for t in tasks {
            graph.add_node(workgraph::graph::Node::Task(t));
        }
        workgraph::parser::save_graph(&graph, &graph_path).unwrap();
    }

    #[test]
    fn migrates_legacy_coordinator_id_to_chat_prefix() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let coord = Task {
            id: ".coordinator-3".to_string(),
            title: "Coordinator: alice".to_string(),
            status: Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        let dependent = Task {
            id: "feature-x".to_string(),
            title: "Feature X".to_string(),
            status: Status::Open,
            after: vec![".coordinator-3".to_string()],
            ..Default::default()
        };
        write_graph(dir, vec![coord, dependent]);

        run_chat_rename(&dir.join(".workgraph"), false, true).unwrap();

        let graph =
            workgraph::parser::load_graph(&dir.join(".workgraph").join("graph.jsonl")).unwrap();

        // .chat-3 exists with renamed title and tag
        let migrated = graph.get_task(".chat-3").expect("chat-3 should exist");
        assert_eq!(migrated.title, "Chat: alice");
        assert!(migrated.tags.iter().any(|t| t == "chat-loop"));
        assert!(!migrated.tags.iter().any(|t| t == "coordinator-loop"));

        // Old key is gone
        assert!(graph.get_task(".coordinator-3").is_none());

        // Dependent task's after-edge was rewritten
        let dep = graph.get_task("feature-x").expect("dependent must exist");
        assert!(dep.after.iter().any(|a| a == ".chat-3"));
        assert!(!dep.after.iter().any(|a| a == ".coordinator-3"));
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let coord = Task {
            id: ".coordinator-0".to_string(),
            title: "Coordinator 0".to_string(),
            status: Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        write_graph(dir, vec![coord]);

        run_chat_rename(&dir.join(".workgraph"), false, true).unwrap();
        run_chat_rename(&dir.join(".workgraph"), false, true).unwrap();

        let graph =
            workgraph::parser::load_graph(&dir.join(".workgraph").join("graph.jsonl")).unwrap();
        assert!(graph.get_task(".chat-0").is_some());
        assert!(graph.get_task(".coordinator-0").is_none());
    }

    #[test]
    fn dry_run_does_not_modify() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let coord = Task {
            id: ".coordinator-1".to_string(),
            title: "Coordinator 1".to_string(),
            status: Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            ..Default::default()
        };
        write_graph(dir, vec![coord]);

        run_chat_rename(&dir.join(".workgraph"), true, true).unwrap();

        let graph =
            workgraph::parser::load_graph(&dir.join(".workgraph").join("graph.jsonl")).unwrap();
        // Legacy id still present, no chat- yet
        assert!(graph.get_task(".coordinator-1").is_some());
        assert!(graph.get_task(".chat-1").is_none());
    }
}

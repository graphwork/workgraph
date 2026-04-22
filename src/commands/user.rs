//! `wg user` subcommands: init, list, archive.
//!
//! Convenience CLI for managing per-user conversation boards (.user-NAME-N).

use anyhow::{Context, Result};
use std::path::Path;

use workgraph::current_user;
use workgraph::graph::{
    Node, create_user_board_task, is_user_board, next_user_board_seq, resolve_user_board_alias,
    user_board_handle, user_board_seq,
};
use workgraph::parser::modify_graph;

/// `wg user init [NAME]` — create a user board (idempotent).
///
/// If an active board already exists for the handle, prints its ID.
/// Otherwise creates `.user-{handle}-0` (or the next available seq).
pub fn run_init(dir: &Path, name: Option<&str>) -> Result<()> {
    let handle = match name {
        Some(n) => n.to_string(),
        None => current_user(),
    };

    let (graph, _path) = super::load_workgraph(dir)?;

    // Check if an active board already exists
    let alias = format!(".user-{}", handle);
    let resolved = resolve_user_board_alias(&graph, &alias);
    if resolved != alias {
        // Active board found
        println!("User board already exists: {}", resolved);
        return Ok(());
    }

    // Check if the resolved ID itself exists (fully-qualified alias returned as-is
    // when no active board found, but maybe it exists as a done board)
    let seq = next_user_board_seq(&graph, &handle);
    let task = create_user_board_task(&handle, seq);
    let task_id = task.id.clone();

    let graph_path = super::graph_path(dir);
    modify_graph(&graph_path, |graph| {
        graph.add_node(Node::Task(task));
        true
    })
    .context("Failed to create user board")?;

    super::notify_graph_changed(dir);
    println!("Created user board '{}'", task_id);

    Ok(())
}

/// `wg user list` — show all user boards (active + archived).
pub fn run_list(dir: &Path, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let mut boards: Vec<_> = graph.tasks().filter(|t| is_user_board(&t.id)).collect();

    boards.sort_by(|a, b| a.id.cmp(&b.id));

    if json {
        let entries: Vec<serde_json::Value> = boards
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "status": format!("{:?}", t.status),
                    "handle": user_board_handle(&t.id),
                    "seq": user_board_seq(&t.id),
                    "archived": t.tags.contains(&"archived".to_string()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if boards.is_empty() {
        println!("No user boards found.");
        println!("  Create one with: wg user init [NAME]");
        return Ok(());
    }

    println!("User boards:");
    println!();
    for board in &boards {
        let status_icon = if board.status.is_terminal() {
            "\u{25cb}" // ○ (archived/done)
        } else {
            "\u{25cf}" // ● (active)
        };
        let handle = user_board_handle(&board.id).unwrap_or("?");
        let seq = user_board_seq(&board.id)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        let archived = if board.tags.contains(&"archived".to_string()) {
            " [archived]"
        } else {
            ""
        };
        println!(
            "  {} {} (handle: {}, seq: {}){} — {:?}",
            status_icon, board.id, handle, seq, archived, board.status
        );
    }

    Ok(())
}

/// `wg user archive [NAME]` — archive the active board and create successor.
///
/// Sugar for `wg done .user-NAME` with the auto-increment behaviour
/// already implemented in `done.rs`.
pub fn run_archive(dir: &Path, name: Option<&str>) -> Result<()> {
    let handle = match name {
        Some(n) => n.to_string(),
        None => current_user(),
    };

    let (graph, _path) = super::load_workgraph(dir)?;

    // Resolve to the active board
    let alias = format!(".user-{}", handle);
    let resolved = resolve_user_board_alias(&graph, &alias);

    if resolved == alias {
        anyhow::bail!(
            "No active user board found for '{}'. Create one with: wg user init {}",
            handle,
            handle
        );
    }

    // Delegate to the done command which already handles user board auto-increment
    super::done::run(dir, &resolved, false)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Status, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

    fn setup_wg_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();
        let graph = WorkGraph::new();
        let path = dir.join("graph.jsonl");
        save_graph(&graph, &path).unwrap();
        tmp
    }

    fn graph_at(dir: &Path) -> WorkGraph {
        load_graph(&dir.join("graph.jsonl")).unwrap()
    }

    #[test]
    fn test_user_init_creates_board() {
        let tmp = setup_wg_dir();
        let dir = tmp.path();

        run_init(dir, Some("testuser")).unwrap();

        let graph = graph_at(dir);
        let board = graph.get_task(".user-testuser-0").unwrap();
        assert_eq!(board.status, Status::InProgress);
        assert!(board.tags.contains(&"user-board".to_string()));
    }

    #[test]
    fn test_user_init_idempotent() {
        let tmp = setup_wg_dir();
        let dir = tmp.path();

        run_init(dir, Some("testuser")).unwrap();
        // Second call should succeed without creating a duplicate
        run_init(dir, Some("testuser")).unwrap();

        let graph = graph_at(dir);
        // Should still only have one board
        let count = graph
            .tasks()
            .filter(|t| t.id.starts_with(".user-testuser-"))
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_user_list_empty() {
        let tmp = setup_wg_dir();
        let dir = tmp.path();

        // Should not error on empty list
        run_list(dir, false).unwrap();
    }

    #[test]
    fn test_user_list_shows_boards() {
        let tmp = setup_wg_dir();
        let dir = tmp.path();

        run_init(dir, Some("alice")).unwrap();
        run_init(dir, Some("bob")).unwrap();

        // Should not error
        run_list(dir, false).unwrap();
        run_list(dir, true).unwrap();

        let graph = graph_at(dir);
        assert!(graph.get_task(".user-alice-0").is_some());
        assert!(graph.get_task(".user-bob-0").is_some());
    }

    #[test]
    fn test_user_archive_creates_successor() {
        let tmp = setup_wg_dir();
        let dir = tmp.path();

        run_init(dir, Some("testuser")).unwrap();
        run_archive(dir, Some("testuser")).unwrap();

        let graph = graph_at(dir);

        // Original should be Done + archived
        let old = graph.get_task(".user-testuser-0").unwrap();
        assert_eq!(old.status, Status::Done);
        assert!(old.tags.contains(&"archived".to_string()));

        // Successor should exist and be active
        let new = graph.get_task(".user-testuser-1").unwrap();
        assert_eq!(new.status, Status::InProgress);
        assert!(new.tags.contains(&"user-board".to_string()));
    }

    #[test]
    fn test_user_archive_no_active_board_fails() {
        let tmp = setup_wg_dir();
        let dir = tmp.path();

        let result = run_archive(dir, Some("nobody"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No active user board")
        );
    }

    #[test]
    fn test_user_init_defaults_to_current_user() {
        let tmp = setup_wg_dir();
        let dir = tmp.path();

        // Set WG_USER so the test is deterministic
        // SAFETY: test-only, single-threaded access to env vars
        unsafe {
            std::env::set_var("WG_USER", "testdefault");
        }
        let result = run_init(dir, None);
        unsafe {
            std::env::remove_var("WG_USER");
        }

        result.unwrap();
        let graph = graph_at(dir);
        assert!(graph.get_task(".user-testdefault-0").is_some());
    }
}

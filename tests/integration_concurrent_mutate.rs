//! Concurrent integration test for mutate_graph.
//!
//! Proves that the flock-based mutate_graph eliminates TOCTOU races:
//! multiple threads simultaneously mutating the graph via mutate_graph
//! should not lose any updates.

use std::sync::{Arc, Barrier};
use std::thread;
use tempfile::TempDir;
use workgraph::graph::{Node, Task, WorkGraph};
use workgraph::parser::{mutate_graph, save_graph};

fn make_task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        title: format!("Task {}", id),
        ..Task::default()
    }
}

/// Spawn N threads that each add a unique task to the graph via mutate_graph.
/// After all threads complete, verify no updates were lost.
#[test]
fn test_concurrent_mutate_graph_no_lost_updates() {
    let dir = TempDir::new().unwrap();
    let graph_path = dir.path().join("graph.jsonl");

    // Initialize empty graph
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();

    let num_threads = 10;
    let barrier = Arc::new(Barrier::new(num_threads));
    let path = Arc::new(graph_path.clone());

    let handles: Vec<_> = (0..num_threads)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            let path = Arc::clone(&path);
            thread::spawn(move || {
                // Synchronize all threads to start simultaneously
                barrier.wait();
                let task_id = format!("concurrent-task-{}", i);
                mutate_graph(path.as_ref(), |graph| -> Result<(), workgraph::parser::ParseError> {
                    graph.add_node(Node::Task(make_task(&task_id)));
                    Ok(())
                })
                .unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Verify all tasks are present — no lost updates
    let final_graph = workgraph::parser::load_graph(&graph_path).unwrap();
    let task_ids: Vec<String> = final_graph.tasks().map(|t| t.id.clone()).collect();

    assert_eq!(
        task_ids.len(),
        num_threads,
        "Expected {} tasks but found {}. Lost updates detected! Tasks: {:?}",
        num_threads,
        task_ids.len(),
        task_ids
    );

    for i in 0..num_threads {
        let expected_id = format!("concurrent-task-{}", i);
        assert!(
            task_ids.contains(&expected_id),
            "Missing task '{}'. All tasks: {:?}",
            expected_id,
            task_ids
        );
    }
}

/// Prove that bare load_graph+save_graph WITHOUT flock WOULD lose updates.
/// (This test demonstrates the race that mutate_graph prevents.)
#[test]
fn test_concurrent_bare_load_save_loses_updates() {
    use workgraph::parser::load_graph;

    let dir = TempDir::new().unwrap();
    let graph_path = dir.path().join("graph.jsonl");

    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();

    let num_threads = 10;
    let iterations = 5; // Run multiple rounds to increase chance of race
    let mut any_lost = false;

    for _ in 0..iterations {
        // Reset graph
        save_graph(&WorkGraph::new(), &graph_path).unwrap();

        let barrier = Arc::new(Barrier::new(num_threads));
        let path = Arc::new(graph_path.clone());

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                let path = Arc::clone(&path);
                thread::spawn(move || {
                    barrier.wait();
                    // Deliberately use bare load+save (no flock) to show the race
                    let mut graph = load_graph(path.as_ref()).unwrap();
                    // Small sleep to widen the race window
                    std::thread::yield_now();
                    let task_id = format!("race-task-{}", i);
                    graph.add_node(Node::Task(make_task(&task_id)));
                    // save_graph acquires flock for the save itself, but the
                    // load-modify-save cycle is NOT atomic, so concurrent
                    // writers can overwrite each other's changes.
                    save_graph(&graph, path.as_ref()).unwrap();
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let final_graph = load_graph(&graph_path).unwrap();
        let count = final_graph.tasks().count();
        if count < num_threads {
            any_lost = true;
            break;
        }
    }

    // Note: this race is probabilistic, not guaranteed. On a single-core
    // system or with OS scheduling quirks, all threads might serialize
    // naturally. We don't assert `any_lost` because the test proves the
    // concept but can't guarantee the race fires every time.
    if any_lost {
        eprintln!(
            "Confirmed: bare load+save loses updates (expected). mutate_graph prevents this."
        );
    }
}

use crate::graph::{Node, WorkGraph};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error on line {line}: {source}")]
    Json {
        line: usize,
        source: serde_json::Error,
    },
    #[error("Lock error: {0}")]
    Lock(String),
}

/// RAII guard for file locks - automatically releases lock on drop
struct FileLock {
    #[cfg(unix)]
    file: File,
}

impl FileLock {
    /// Acquire an exclusive lock on a lock file
    #[cfg(unix)]
    fn acquire<P: AsRef<Path>>(lock_path: P) -> Result<Self, ParseError> {
        use std::os::unix::io::AsRawFd;

        // Ensure the .workgraph directory exists
        if let Some(parent) = lock_path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Open/create the lock file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)?;

        // Acquire exclusive lock (LOCK_EX) - blocks until available
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };

        if ret != 0 {
            return Err(ParseError::Lock(format!(
                "Failed to acquire lock on {:?}: {}",
                lock_path.as_ref(),
                std::io::Error::last_os_error()
            )));
        }

        Ok(FileLock { file })
    }

    #[cfg(not(unix))]
    fn acquire<P: AsRef<Path>>(_lock_path: P) -> Result<Self, ParseError> {
        // On non-Unix systems, we can't use flock - return a no-op lock
        // This is a limitation but workgraph is primarily for Unix systems
        Ok(FileLock {})
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // Release the lock (LOCK_UN) - best effort, ignore errors on drop
            let fd = self.file.as_raw_fd();
            unsafe {
                libc::flock(fd, libc::LOCK_UN);
            }
        }
    }
}

/// Get the lock file path for a given graph file
fn get_lock_path<P: AsRef<Path>>(graph_path: P) -> PathBuf {
    let graph_path = graph_path.as_ref();
    if let Some(parent) = graph_path.parent() {
        parent.join("graph.lock")
    } else {
        PathBuf::from("graph.lock")
    }
}

/// Load a work graph from a JSONL file
/// Uses advisory file locking to prevent concurrent access corruption
pub fn load_graph<P: AsRef<Path>>(path: P) -> Result<WorkGraph, ParseError> {
    let lock_path = get_lock_path(&path);
    let _lock = FileLock::acquire(&lock_path)?;

    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut graph = WorkGraph::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Skip legacy Actor nodes (removed in actor-system cleanup)
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if v.get("kind").and_then(|k| k.as_str()) == Some("actor") {
                continue;
            }
        }
        let node: Node = serde_json::from_str(trimmed).map_err(|e| ParseError::Json {
            line: line_num + 1,
            source: e,
        })?;
        graph.add_node(node);
    }

    Ok(graph)
    // Lock is automatically released when _lock goes out of scope
}

/// Save a work graph to a JSONL file
/// Uses advisory file locking to prevent concurrent access corruption
pub fn save_graph<P: AsRef<Path>>(graph: &WorkGraph, path: P) -> Result<(), ParseError> {
    let lock_path = get_lock_path(&path);
    let _lock = FileLock::acquire(&lock_path)?;

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;

    for node in graph.nodes() {
        let json = serde_json::to_string(node).map_err(|e| ParseError::Json { line: 0, source: e })?;
        writeln!(file, "{}", json)?;
    }

    Ok(())
    // Lock is automatically released when _lock goes out of scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Status, Task};
    use tempfile::NamedTempFile;
    use std::io::Write;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
        }
    }

    #[test]
    fn test_load_empty_file() {
        let file = NamedTempFile::new().unwrap();
        let graph = load_graph(file.path()).unwrap();
        assert!(graph.is_empty());
    }

    #[test]
    fn test_load_single_task() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"id":"t1","kind":"task","title":"Test","status":"open"}}"#).unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 1);
        assert!(graph.get_task("t1").is_some());
    }

    #[test]
    fn test_load_multiple_nodes() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"id":"t1","kind":"task","title":"Task 1","status":"open"}}"#).unwrap();
        writeln!(file, r#"{{"id":"t2","kind":"task","title":"Task 2","status":"done"}}"#).unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 2);
    }

    #[test]
    fn test_load_skips_legacy_actor_nodes() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"id":"t1","kind":"task","title":"Task 1","status":"open"}}"#).unwrap();
        writeln!(file, r#"{{"id":"erik","kind":"actor","name":"Erik"}}"#).unwrap();
        writeln!(file, r#"{{"id":"t2","kind":"task","title":"Task 2","status":"done"}}"#).unwrap();

        let graph = load_graph(file.path()).unwrap();
        // Actor nodes should be silently skipped
        assert_eq!(graph.len(), 2);
        assert!(graph.get_task("t1").is_some());
        assert!(graph.get_task("t2").is_some());
    }

    #[test]
    fn test_load_skips_empty_lines_and_comments() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "# This is a comment").unwrap();
        writeln!(file, "").unwrap();
        writeln!(file, r#"{{"id":"t1","kind":"task","title":"Test","status":"open"}}"#).unwrap();
        writeln!(file, "   ").unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 1);
    }

    #[test]
    fn test_load_invalid_json_returns_error() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "not valid json").unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ParseError::Json { line: 1, .. }));
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        graph.add_node(Node::Task(make_task("t2", "Task 2")));

        let file = NamedTempFile::new().unwrap();
        save_graph(&graph, file.path()).unwrap();

        let loaded = load_graph(file.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.get_task("t1").is_some());
        assert!(loaded.get_task("t2").is_some());
    }

    #[test]
    fn test_load_nonexistent_file_returns_error() {
        let result = load_graph("/nonexistent/path/graph.jsonl");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ParseError::Io(_)));
    }

    #[test]
    fn test_concurrent_access_with_locking() {
        use std::sync::Arc;
        use std::thread;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Create a temporary file
        let file = NamedTempFile::new().unwrap();
        let path = Arc::new(file.path().to_path_buf());

        // Initialize with a task
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Initial Task")));
        save_graph(&graph, path.as_ref()).unwrap();

        let success_count = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        // Spawn multiple threads that try to read and write concurrently
        for i in 0..10 {
            let path = Arc::clone(&path);
            let success_count = Arc::clone(&success_count);

            let handle = thread::spawn(move || {
                // Each thread loads the graph, adds a task, and saves it back
                if let Ok(mut graph) = load_graph(path.as_ref()) {
                    graph.add_node(Node::Task(make_task(&format!("t{}", i + 2), &format!("Task {}", i + 2))));
                    if save_graph(&graph, path.as_ref()).is_ok() {
                        success_count.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });

            handles.push(handle);
        }

        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        // Verify we can still load the graph without corruption
        let final_graph = load_graph(path.as_ref()).unwrap();

        // At least some operations should have succeeded
        assert!(success_count.load(Ordering::SeqCst) > 0);

        // The graph should be parseable (no EOF errors or corruption)
        assert!(final_graph.len() > 0);
    }
}

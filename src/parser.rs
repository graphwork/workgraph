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
            .truncate(false)
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
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
            && v.get("kind").and_then(|k| k.as_str()) == Some("actor")
        {
            continue;
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
        let json =
            serde_json::to_string(node).map_err(|e| ParseError::Json { line: 0, source: e })?;
        writeln!(file, "{}", json)?;
    }

    Ok(())
    // Lock is automatically released when _lock goes out of scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Status, Task};
    use std::io::Write;
    use tempfile::NamedTempFile;

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
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"Test","status":"open"}}"#
        )
        .unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 1);
        assert!(graph.get_task("t1").is_some());
    }

    #[test]
    fn test_load_multiple_nodes() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"Task 1","status":"open"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"id":"t2","kind":"task","title":"Task 2","status":"done"}}"#
        )
        .unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 2);
    }

    #[test]
    fn test_load_skips_legacy_actor_nodes() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"Task 1","status":"open"}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"id":"erik","kind":"actor","name":"Erik"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"id":"t2","kind":"task","title":"Task 2","status":"done"}}"#
        )
        .unwrap();

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
        writeln!(file).unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"Test","status":"open"}}"#
        )
        .unwrap();
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

    // ---- Edge case tests for error handling ----

    #[test]
    fn test_load_mid_record_corruption() {
        // Valid line, then a corrupted line (truncated JSON), then another valid line
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"Good","status":"open"}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"id":"t2","kind":"task","title":"Trun"#).unwrap(); // truncated
        writeln!(
            file,
            r#"{{"id":"t3","kind":"task","title":"Also Good","status":"open"}}"#
        )
        .unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            ParseError::Json { line, .. } => assert_eq!(line, 2),
            other => panic!("Expected ParseError::Json, got: {:?}", other),
        }
    }

    #[test]
    fn test_load_truncated_single_line() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, r#"{{"id":"t1","kind":"task""#).unwrap(); // no newline, truncated
        file.flush().unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ParseError::Json { line: 1, .. }
        ));
    }

    #[test]
    fn test_load_invalid_json_on_specific_lines() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"OK","status":"open"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"id":"t2","kind":"task","title":"OK","status":"open"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"id":"t3","kind":"task","title":"OK","status":"open"}}"#
        )
        .unwrap();
        writeln!(file, "this is not json at all").unwrap(); // line 4
        writeln!(
            file,
            r#"{{"id":"t5","kind":"task","title":"OK","status":"open"}}"#
        )
        .unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            ParseError::Json { line, .. } => assert_eq!(line, 4),
            other => panic!("Expected ParseError::Json on line 4, got: {:?}", other),
        }
    }

    #[test]
    fn test_load_bare_opening_brace() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{{").unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ParseError::Json { line: 1, .. }
        ));
    }

    #[test]
    fn test_load_json_array_instead_of_object() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"[{{"id":"t1","kind":"task","title":"T","status":"open"}}]"#
        )
        .unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ParseError::Json { line: 1, .. }
        ));
    }

    #[test]
    fn test_load_mixed_line_endings_crlf() {
        let mut file = NamedTempFile::new().unwrap();
        // Write with \r\n line endings
        write!(
            file,
            "{{\"id\":\"t1\",\"kind\":\"task\",\"title\":\"CRLF Task 1\",\"status\":\"open\"}}\r\n{{\"id\":\"t2\",\"kind\":\"task\",\"title\":\"CRLF Task 2\",\"status\":\"done\"}}\r\n",
        )
        .unwrap();
        file.flush().unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 2);
        assert!(graph.get_task("t1").is_some());
        assert!(graph.get_task("t2").is_some());
    }

    #[test]
    fn test_load_mixed_line_endings_mixed() {
        let mut file = NamedTempFile::new().unwrap();
        // Mix \n and \r\n in the same file
        write!(
            file,
            "{{\"id\":\"t1\",\"kind\":\"task\",\"title\":\"LF\",\"status\":\"open\"}}\n{{\"id\":\"t2\",\"kind\":\"task\",\"title\":\"CRLF\",\"status\":\"open\"}}\r\n{{\"id\":\"t3\",\"kind\":\"task\",\"title\":\"LF again\",\"status\":\"open\"}}\n",
        )
        .unwrap();
        file.flush().unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 3);
    }

    #[test]
    fn test_load_whitespace_only_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "   \n  \t  \n\n   \n").unwrap();
        file.flush().unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert!(graph.is_empty());
    }

    #[test]
    fn test_load_file_with_trailing_newlines() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"Test","status":"open"}}"#
        )
        .unwrap();
        write!(file, "\n\n\n").unwrap();
        file.flush().unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 1);
    }

    #[test]
    fn test_load_non_utf8_file() {
        let mut file = NamedTempFile::new().unwrap();
        // Write valid JSON followed by invalid UTF-8 bytes
        let valid_start = br#"{"id":"t1","kind":"task","title":""#;
        let invalid_utf8: &[u8] = &[0xFF, 0xFE, 0x80, 0x81];
        let valid_end = br#"","status":"open"}"#;
        file.write_all(valid_start).unwrap();
        file.write_all(invalid_utf8).unwrap();
        file.write_all(valid_end).unwrap();
        writeln!(file).unwrap();
        file.flush().unwrap();

        // BufReader::lines() returns Err for non-UTF8 lines, which becomes ParseError::Io
        let result = load_graph(file.path());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ParseError::Io(_)));
    }

    #[test]
    fn test_load_large_graph_1000_nodes() {
        let mut file = NamedTempFile::new().unwrap();
        for i in 0..1000 {
            writeln!(
                file,
                r#"{{"id":"t{i}","kind":"task","title":"Task {i}","status":"open"}}"#
            )
            .unwrap();
        }
        file.flush().unwrap();

        let start = std::time::Instant::now();
        let graph = load_graph(file.path()).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(graph.len(), 1000);
        // Sanity: loading 1000 tasks should be well under 5 seconds
        assert!(
            elapsed.as_secs() < 5,
            "Loading 1000 nodes took {:?}, which is too slow",
            elapsed
        );
    }

    #[test]
    fn test_save_and_load_large_graph_roundtrip() {
        let mut graph = WorkGraph::new();
        for i in 0..1000 {
            graph.add_node(Node::Task(make_task(
                &format!("t{i}"),
                &format!("Task {i}"),
            )));
        }

        let file = NamedTempFile::new().unwrap();
        save_graph(&graph, file.path()).unwrap();

        let loaded = load_graph(file.path()).unwrap();
        assert_eq!(loaded.len(), 1000);
    }

    #[test]
    fn test_special_characters_in_task_id() {
        let ids = &[
            "task-with-dashes",
            "task_with_underscores",
            "task.with.dots",
            "task/with/slashes",
            "task:with:colons",
            "UPPER_CASE",
            "MiXeD-CaSe_123",
        ];

        for id in ids {
            let mut graph = WorkGraph::new();
            graph.add_node(Node::Task(make_task(id, "Test")));

            let file = NamedTempFile::new().unwrap();
            save_graph(&graph, file.path()).unwrap();
            let loaded = load_graph(file.path()).unwrap();
            assert!(
                loaded.get_task(id).is_some(),
                "Failed roundtrip for id: {}",
                id
            );
        }
    }

    #[test]
    fn test_special_characters_in_task_title() {
        let titles = &[
            "Title with spaces",
            "Title with \"quotes\"",
            "Title with 'apostrophes'",
            "Title with <angle> & brackets",
            "Title with unicode: café résumé naïve",
            "Title with emoji: \u{1F680}\u{1F525}",
            "Title with newline\\nescaped",
            "Title with tab\\tescaped",
            "日本語のタイトル",
            "Título en español",
        ];

        for title in titles {
            let mut graph = WorkGraph::new();
            graph.add_node(Node::Task(make_task("test-id", title)));

            let file = NamedTempFile::new().unwrap();
            save_graph(&graph, file.path()).unwrap();
            let loaded = load_graph(file.path()).unwrap();
            let task = loaded.get_task("test-id").unwrap();
            assert_eq!(&task.title, title, "Failed roundtrip for title: {}", title);
        }
    }

    #[test]
    fn test_load_valid_json_but_wrong_schema() {
        let mut file = NamedTempFile::new().unwrap();
        // Valid JSON object but missing required fields (no "kind" tag)
        writeln!(file, r#"{{"foo":"bar","baz":42}}"#).unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ParseError::Json { line: 1, .. }
        ));
    }

    #[test]
    fn test_load_task_missing_title() {
        let mut file = NamedTempFile::new().unwrap();
        // Has kind but missing required "title" field
        writeln!(file, r#"{{"id":"t1","kind":"task","status":"open"}}"#).unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ParseError::Json { line: 1, .. }
        ));
    }

    #[test]
    fn test_load_task_missing_id() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"kind":"task","title":"No ID","status":"open"}}"#).unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ParseError::Json { line: 1, .. }
        ));
    }

    #[test]
    fn test_load_resource_node() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"id":"r1","kind":"resource","name":"Budget","type":"currency","available":1000.0,"unit":"USD"}}"#).unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 1);
        assert!(graph.get_node("r1").is_some());
    }

    #[test]
    fn test_load_unknown_kind() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"id":"x1","kind":"unknown_type","title":"Mystery"}}"#
        )
        .unwrap();

        let result = load_graph(file.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ParseError::Json { line: 1, .. }
        ));
    }

    #[test]
    fn test_save_to_readonly_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("readonly.jsonl");

        // Create the file, then make it read-only
        File::create(&file_path).unwrap();
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let graph = WorkGraph::new();
        let result = save_graph(&graph, &file_path);
        assert!(result.is_err());
        // Clean up: restore write permission so tempdir can be deleted
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn test_save_to_nonexistent_directory() {
        let graph = WorkGraph::new();
        let result = save_graph(&graph, "/nonexistent/deep/path/graph.jsonl");
        assert!(result.is_err());
    }

    #[test]
    fn test_save_empty_graph() {
        let graph = WorkGraph::new();
        let file = NamedTempFile::new().unwrap();
        save_graph(&graph, file.path()).unwrap();

        let loaded = load_graph(file.path()).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_save_overwrites_existing_content() {
        let file = NamedTempFile::new().unwrap();

        // Save a graph with 3 tasks
        let mut graph1 = WorkGraph::new();
        graph1.add_node(Node::Task(make_task("t1", "Task 1")));
        graph1.add_node(Node::Task(make_task("t2", "Task 2")));
        graph1.add_node(Node::Task(make_task("t3", "Task 3")));
        save_graph(&graph1, file.path()).unwrap();

        // Save a graph with 1 task - should overwrite, not append
        let mut graph2 = WorkGraph::new();
        graph2.add_node(Node::Task(make_task("t4", "Task 4")));
        save_graph(&graph2, file.path()).unwrap();

        let loaded = load_graph(file.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get_task("t4").is_some());
        assert!(loaded.get_task("t1").is_none());
    }

    #[test]
    fn test_load_duplicate_ids_last_wins() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"First","status":"open"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"Second","status":"done"}}"#
        )
        .unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 1);
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.title, "Second");
    }

    #[test]
    fn test_load_extra_json_fields_are_ignored() {
        let mut file = NamedTempFile::new().unwrap();
        // JSON with extra unknown fields - serde should ignore them with default config
        writeln!(file, r#"{{"id":"t1","kind":"task","title":"Test","status":"open","unknown_field":"value","another":42}}"#).unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 1);
        assert!(graph.get_task("t1").is_some());
    }

    #[test]
    fn test_load_comments_only_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "# comment 1").unwrap();
        writeln!(file, "# comment 2").unwrap();
        writeln!(file, "# comment 3").unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert!(graph.is_empty());
    }

    #[test]
    fn test_load_interleaved_comments_and_tasks() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "# Header comment").unwrap();
        writeln!(
            file,
            r#"{{"id":"t1","kind":"task","title":"Task 1","status":"open"}}"#
        )
        .unwrap();
        writeln!(file, "# Mid-file comment").unwrap();
        writeln!(file).unwrap();
        writeln!(
            file,
            r#"{{"id":"t2","kind":"task","title":"Task 2","status":"open"}}"#
        )
        .unwrap();
        writeln!(file, "# Trailing comment").unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 2);
    }

    #[test]
    fn test_load_legacy_identity_migration() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"id":"t1","kind":"task","title":"Legacy","status":"open","identity":{{"role_id":"r1","motivation_id":"m1"}}}}"#).unwrap();

        let graph = load_graph(file.path()).unwrap();
        let task = graph.get_task("t1").unwrap();
        // The legacy identity should be migrated to an agent hash
        assert!(task.agent.is_some());
    }

    #[test]
    fn test_concurrent_access_with_locking() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

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
                    graph.add_node(Node::Task(make_task(
                        &format!("t{}", i + 2),
                        &format!("Task {}", i + 2),
                    )));
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
        assert!(!final_graph.is_empty());
    }
}

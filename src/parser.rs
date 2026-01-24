use crate::graph::{Node, WorkGraph};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error on line {line}: {source}")]
    Json {
        line: usize,
        source: serde_json::Error,
    },
}

/// Load a work graph from a JSONL file
pub fn load_graph<P: AsRef<Path>>(path: P) -> Result<WorkGraph, ParseError> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut graph = WorkGraph::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let node: Node = serde_json::from_str(trimmed).map_err(|e| ParseError::Json {
            line: line_num + 1,
            source: e,
        })?;
        graph.add_node(node);
    }

    Ok(graph)
}

/// Save a work graph to a JSONL file
pub fn save_graph<P: AsRef<Path>>(graph: &WorkGraph, path: P) -> Result<(), ParseError> {
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
        writeln!(file, r#"{{"id":"erik","kind":"actor","name":"Erik"}}"#).unwrap();

        let graph = load_graph(file.path()).unwrap();
        assert_eq!(graph.len(), 3);
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
}

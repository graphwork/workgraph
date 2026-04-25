//! Persistent history of (executor, model, endpoint) combinations used.
//!
//! Append-only JSONL at `~/.workgraph/launcher-history.jsonl`.
//! Dedup on read: query API returns distinct recent combos, newest first.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    pub timestamp: String,
    pub executor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

impl HistoryEntry {
    pub fn new(executor: &str, model: Option<&str>, endpoint: Option<&str>, source: &str) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            executor: executor.to_string(),
            model: model.map(String::from),
            endpoint: endpoint.map(String::from),
            source: source.to_string(),
            project: std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string()),
        }
    }

    fn combo_key(&self) -> (String, Option<String>, Option<String>) {
        (
            self.executor.clone(),
            self.model.clone(),
            self.endpoint.clone(),
        )
    }
}

fn history_path() -> Result<PathBuf> {
    let global_dir = crate::config::Config::global_dir()?;
    Ok(global_dir.join("launcher-history.jsonl"))
}

pub fn record_use(entry: &HistoryEntry) -> Result<()> {
    let path = history_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let line = serde_json::to_string(entry)?;
    writeln!(file, "{}", line)?;
    Ok(())
}

fn load_all(path: &Path) -> Vec<HistoryEntry> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    reader
        .lines()
        .map_while(|line| line.ok())
        .filter_map(|line| serde_json::from_str::<HistoryEntry>(&line).ok())
        .collect()
}

fn dedup_newest_first(entries: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for entry in entries.into_iter().rev() {
        let key = entry.combo_key();
        if seen.insert(key) {
            result.push(entry);
        }
    }
    result
}

pub fn recent_combos(limit: usize) -> Result<Vec<HistoryEntry>> {
    let path = history_path()?;
    let entries = load_all(&path);
    let deduped = dedup_newest_first(entries);
    Ok(deduped.into_iter().take(limit).collect())
}

pub fn recent_executors(limit: usize) -> Result<Vec<String>> {
    let path = history_path()?;
    let entries = load_all(&path);
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for entry in entries.into_iter().rev() {
        if seen.insert(entry.executor.clone()) {
            result.push(entry.executor);
            if result.len() >= limit {
                break;
            }
        }
    }
    Ok(result)
}

pub fn recent_models(limit: usize) -> Result<Vec<String>> {
    let path = history_path()?;
    let entries = load_all(&path);
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for entry in entries.into_iter().rev() {
        if let Some(m) = &entry.model {
            if seen.insert(m.clone()) {
                result.push(m.clone());
                if result.len() >= limit {
                    break;
                }
            }
        }
    }
    Ok(result)
}

pub fn recent_endpoints(executor: &str, limit: usize) -> Result<Vec<String>> {
    let path = history_path()?;
    let entries = load_all(&path);
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for entry in entries.into_iter().rev() {
        if entry.executor == executor {
            if let Some(ep) = &entry.endpoint {
                if seen.insert(ep.clone()) {
                    result.push(ep.clone());
                    if result.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_entries(entries: &[HistoryEntry]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for e in entries {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
        }
        f
    }

    #[test]
    fn test_roundtrip_serialize() {
        let entry = HistoryEntry::new("claude", Some("opus"), None, "cli");
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.executor, "claude");
        assert_eq!(parsed.model.as_deref(), Some("opus"));
        assert!(parsed.endpoint.is_none());
        assert_eq!(parsed.source, "cli");
    }

    #[test]
    fn test_dedup_keeps_newest() {
        let e1 = HistoryEntry {
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            executor: "claude".to_string(),
            model: Some("sonnet".to_string()),
            endpoint: None,
            source: "cli".to_string(),
            project: None,
        };
        let e2 = HistoryEntry {
            timestamp: "2026-01-02T00:00:00Z".to_string(),
            executor: "native".to_string(),
            model: Some("opus".to_string()),
            endpoint: Some("http://localhost:8080".to_string()),
            source: "config".to_string(),
            project: None,
        };
        let e3 = HistoryEntry {
            timestamp: "2026-01-03T00:00:00Z".to_string(),
            executor: "claude".to_string(),
            model: Some("sonnet".to_string()),
            endpoint: None,
            source: "tui".to_string(),
            project: None,
        };

        let deduped = dedup_newest_first(vec![e1.clone(), e2.clone(), e3.clone()]);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].timestamp, "2026-01-03T00:00:00Z");
        assert_eq!(deduped[0].executor, "claude");
        assert_eq!(deduped[1].timestamp, "2026-01-02T00:00:00Z");
        assert_eq!(deduped[1].executor, "native");
    }

    #[test]
    fn test_load_all_from_file() {
        let entries = vec![
            HistoryEntry::new("claude", Some("opus"), None, "cli"),
            HistoryEntry::new(
                "native",
                Some("sonnet"),
                Some("http://localhost:8080"),
                "config",
            ),
        ];
        let f = write_entries(&entries);
        let loaded = load_all(f.path());
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].executor, "claude");
        assert_eq!(loaded[1].executor, "native");
    }

    #[test]
    fn test_load_all_missing_file() {
        let loaded = load_all(Path::new("/tmp/nonexistent-wg-history-test.jsonl"));
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_load_all_malformed_lines() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{{}}").unwrap();
        writeln!(f, "not json at all").unwrap();
        let entry = HistoryEntry::new("claude", Some("opus"), None, "cli");
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        let loaded = load_all(f.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].executor, "claude");
    }

    #[test]
    fn test_recent_executors() {
        let entries = vec![
            HistoryEntry {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                executor: "claude".to_string(),
                model: Some("opus".to_string()),
                endpoint: None,
                source: "cli".to_string(),
                project: None,
            },
            HistoryEntry {
                timestamp: "2026-01-02T00:00:00Z".to_string(),
                executor: "native".to_string(),
                model: Some("sonnet".to_string()),
                endpoint: None,
                source: "config".to_string(),
                project: None,
            },
            HistoryEntry {
                timestamp: "2026-01-03T00:00:00Z".to_string(),
                executor: "claude".to_string(),
                model: Some("haiku".to_string()),
                endpoint: None,
                source: "tui".to_string(),
                project: None,
            },
        ];
        let deduped = dedup_newest_first(entries);
        let mut seen = HashSet::new();
        let executors: Vec<String> = deduped
            .iter()
            .filter_map(|e| {
                if seen.insert(e.executor.clone()) {
                    Some(e.executor.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(executors, vec!["claude", "native"]);
    }

    #[test]
    fn test_recent_endpoints_filtered_by_executor() {
        let entries = vec![
            HistoryEntry {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                executor: "native".to_string(),
                model: None,
                endpoint: Some("http://localhost:8080".to_string()),
                source: "cli".to_string(),
                project: None,
            },
            HistoryEntry {
                timestamp: "2026-01-02T00:00:00Z".to_string(),
                executor: "claude".to_string(),
                model: None,
                endpoint: Some("https://api.anthropic.com".to_string()),
                source: "cli".to_string(),
                project: None,
            },
            HistoryEntry {
                timestamp: "2026-01-03T00:00:00Z".to_string(),
                executor: "native".to_string(),
                model: None,
                endpoint: Some("http://localhost:9090".to_string()),
                source: "config".to_string(),
                project: None,
            },
        ];
        let f = write_entries(&entries);
        let loaded = load_all(f.path());
        let mut seen = HashSet::new();
        let native_eps: Vec<String> = loaded
            .into_iter()
            .rev()
            .filter(|e| e.executor == "native")
            .filter_map(|e| e.endpoint)
            .filter(|ep| seen.insert(ep.clone()))
            .collect();
        assert_eq!(
            native_eps,
            vec!["http://localhost:9090", "http://localhost:8080"]
        );
    }

    #[test]
    fn test_record_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launcher-history.jsonl");

        let e1 = HistoryEntry::new("claude", Some("opus"), None, "cli");
        let e2 = HistoryEntry::new(
            "native",
            Some("sonnet"),
            Some("http://localhost:8080"),
            "tui",
        );

        {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{}", serde_json::to_string(&e1).unwrap()).unwrap();
            writeln!(file, "{}", serde_json::to_string(&e2).unwrap()).unwrap();
        }

        let loaded = load_all(&path);
        assert_eq!(loaded.len(), 2);

        let deduped = dedup_newest_first(loaded);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].executor, "native");
        assert_eq!(deduped[1].executor, "claude");
    }
}

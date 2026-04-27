//! Persistent history of (executor, model, endpoint) combinations used.
//!
//! Append-only JSONL at `~/.workgraph/launcher-history.jsonl`.
//! Dedup on read: query API returns distinct recent combos, newest first.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Cap entries kept per (executor, model, endpoint) tuple. Prevents the
/// JSONL file from growing unboundedly when one combo is invoked many
/// times. Dedup-on-read still collapses duplicates to a single combo,
/// but the file itself shouldn't accumulate forever.
pub const DEFAULT_MAX_PER_TUPLE: usize = 50;

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

/// Path to the launcher history JSONL. Tests can override via the
/// `WG_LAUNCHER_HISTORY_PATH` env var so they don't pollute the real
/// `~/.workgraph/launcher-history.jsonl`.
fn history_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("WG_LAUNCHER_HISTORY_PATH")
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    let global_dir = crate::config::Config::global_dir()?;
    Ok(global_dir.join("launcher-history.jsonl"))
}

pub fn record_use(entry: &HistoryEntry) -> Result<()> {
    let path = history_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = serde_json::to_string(entry)?;
        writeln!(file, "{}", line)?;
    }
    // Keep the JSONL bounded. Cheap to do on every write because the
    // file is tiny in practice; correctness wins over micro-perf.
    let _ = prune_file(&path, DEFAULT_MAX_PER_TUPLE);
    Ok(())
}

/// Keep only the most-recent `max_per_tuple` entries for each
/// (executor, model, endpoint) tuple. Rewrites the file in place.
fn prune_file(path: &Path, max_per_tuple: usize) -> Result<()> {
    let entries = load_all(path);
    let pruned = prune_by_tuple(entries, max_per_tuple);
    // Atomic rewrite: write to a tmp file, rename over the original.
    let tmp_path = path.with_extension("jsonl.tmp");
    {
        let mut tmp = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)?;
        for e in &pruned {
            writeln!(tmp, "{}", serde_json::to_string(e)?)?;
        }
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// For each tuple, keep at most `max_per_tuple` of the most-recent
/// entries. Preserves chronological order in the result.
fn prune_by_tuple(entries: Vec<HistoryEntry>, max_per_tuple: usize) -> Vec<HistoryEntry> {
    let mut counts: HashMap<(String, Option<String>, Option<String>), usize> = HashMap::new();
    let mut keep_reverse = Vec::with_capacity(entries.len());
    for entry in entries.into_iter().rev() {
        let key = entry.combo_key();
        let count = counts.entry(key).or_insert(0);
        if *count < max_per_tuple {
            keep_reverse.push(entry);
            *count += 1;
        }
    }
    keep_reverse.reverse();
    keep_reverse
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

    fn make_entry(ts: &str, exec: &str, model: Option<&str>, endpoint: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            timestamp: ts.to_string(),
            executor: exec.to_string(),
            model: model.map(String::from),
            endpoint: endpoint.map(String::from),
            source: "test".to_string(),
            project: None,
        }
    }

    #[test]
    fn test_history_dedup_by_tuple() {
        // Same (executor, model, endpoint) tuple appears multiple times;
        // dedup_newest_first should collapse to one entry, keeping newest.
        let entries = vec![
            make_entry("2026-01-01T00:00:00Z", "native", Some("qwen3"), Some("http://lambda01")),
            make_entry("2026-01-02T00:00:00Z", "native", Some("qwen3"), Some("http://lambda01")),
            make_entry("2026-01-03T00:00:00Z", "claude", Some("opus"), None),
            make_entry("2026-01-04T00:00:00Z", "native", Some("qwen3"), Some("http://lambda01")),
        ];
        let deduped = dedup_newest_first(entries);
        assert_eq!(deduped.len(), 2, "two distinct tuples expected");
        assert_eq!(deduped[0].timestamp, "2026-01-04T00:00:00Z");
        assert_eq!(deduped[0].executor, "native");
        assert_eq!(deduped[1].timestamp, "2026-01-03T00:00:00Z");
        assert_eq!(deduped[1].executor, "claude");
    }

    #[test]
    fn test_history_pruning_to_max_n() {
        // 6 entries for the same tuple; pruning to 3 keeps only the 3 newest.
        let mut entries = Vec::new();
        for i in 0..6 {
            entries.push(make_entry(
                &format!("2026-01-0{}T00:00:00Z", i + 1),
                "native",
                Some("qwen3"),
                Some("http://lambda01"),
            ));
        }
        // Mix in another tuple, which should be unaffected.
        entries.insert(3, make_entry("2026-02-01T00:00:00Z", "claude", Some("opus"), None));

        let pruned = prune_by_tuple(entries, 3);
        // 3 native + 1 claude = 4 entries kept.
        assert_eq!(pruned.len(), 4);

        let native_kept: Vec<&HistoryEntry> = pruned
            .iter()
            .filter(|e| e.executor == "native")
            .collect();
        assert_eq!(native_kept.len(), 3, "native tuple capped to 3");
        // Newest 3 by timestamp: dates 04, 05, 06.
        let timestamps: Vec<&str> = native_kept.iter().map(|e| e.timestamp.as_str()).collect();
        assert!(timestamps.contains(&"2026-01-04T00:00:00Z"));
        assert!(timestamps.contains(&"2026-01-05T00:00:00Z"));
        assert!(timestamps.contains(&"2026-01-06T00:00:00Z"));
        assert!(!timestamps.contains(&"2026-01-01T00:00:00Z"));
        assert!(!timestamps.contains(&"2026-01-02T00:00:00Z"));
        assert!(!timestamps.contains(&"2026-01-03T00:00:00Z"));

        // Claude tuple untouched.
        let claude_kept: Vec<&HistoryEntry> = pruned
            .iter()
            .filter(|e| e.executor == "claude")
            .collect();
        assert_eq!(claude_kept.len(), 1);
    }

    #[test]
    fn test_prune_file_rewrites_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");

        // Seed with 5 entries for one tuple, all from oldest to newest.
        {
            let mut f = fs::File::create(&path).unwrap();
            for i in 0..5 {
                let e = make_entry(
                    &format!("2026-01-0{}T00:00:00Z", i + 1),
                    "native",
                    Some("qwen3"),
                    None,
                );
                writeln!(f, "{}", serde_json::to_string(&e).unwrap()).unwrap();
            }
        }

        prune_file(&path, 2).unwrap();

        let loaded = load_all(&path);
        assert_eq!(loaded.len(), 2, "should keep only 2 newest");
        // Order preserved (oldest of the kept first).
        assert_eq!(loaded[0].timestamp, "2026-01-04T00:00:00Z");
        assert_eq!(loaded[1].timestamp, "2026-01-05T00:00:00Z");
    }
}

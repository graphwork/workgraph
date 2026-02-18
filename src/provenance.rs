//! Operation log with zstd-compressed rotation.
//!
//! Appends structured JSON operations to `.workgraph/log/operations.jsonl`.
//! When the file exceeds a configurable threshold (default 10 MB), the current
//! file is compressed with zstd and renamed to `<UTC-timestamp>.jsonl.zst`,
//! and a fresh `operations.jsonl` is started.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// Default rotation threshold: 10 MB.
pub const DEFAULT_ROTATION_THRESHOLD: u64 = 10 * 1024 * 1024;

/// A single entry in the operations log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationEntry {
    pub timestamp: String,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

/// Return the log directory: `.workgraph/log/`
pub fn log_dir(workgraph_dir: &Path) -> PathBuf {
    workgraph_dir.join("log")
}

/// Return the path to the current (unrotated) operations log.
pub fn operations_path(workgraph_dir: &Path) -> PathBuf {
    log_dir(workgraph_dir).join("operations.jsonl")
}

/// Append an operation entry, rotating if the file exceeds `threshold` bytes.
pub fn append_operation(
    workgraph_dir: &Path,
    entry: &OperationEntry,
    threshold: u64,
) -> Result<()> {
    let dir = log_dir(workgraph_dir);
    fs::create_dir_all(&dir).context("Failed to create log directory")?;

    let path = operations_path(workgraph_dir);

    // Check if rotation is needed before appending.
    if path.exists() {
        let meta = fs::metadata(&path).context("Failed to stat operations.jsonl")?;
        if meta.len() >= threshold {
            rotate(&path, &dir)?;
        }
    }

    let line = serde_json::to_string(entry).context("Failed to serialize operation entry")?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open operations.jsonl for append")?;

    writeln!(file, "{}", line).context("Failed to write operation entry")?;

    Ok(())
}

/// Record an operation using the config's rotation threshold.
/// This is the primary entry point for recording operations.
pub fn record(
    workgraph_dir: &Path,
    op: &str,
    task_id: Option<&str>,
    actor: Option<&str>,
    detail: serde_json::Value,
    threshold: u64,
) -> Result<()> {
    let entry = OperationEntry {
        timestamp: Utc::now().to_rfc3339(),
        op: op.to_string(),
        task_id: task_id.map(String::from),
        actor: actor.map(String::from),
        detail,
    };
    append_operation(workgraph_dir, &entry, threshold)
}

/// Compress the current operations.jsonl to `<UTC-timestamp>.jsonl.zst`
/// and start a fresh file.
fn rotate(path: &Path, dir: &Path) -> Result<()> {
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
    let rotated_name = format!("{}.jsonl.zst", stamp);
    let rotated_path = dir.join(&rotated_name);

    // Read the current file and compress it.
    let data = fs::read(path).context("Failed to read operations.jsonl for rotation")?;

    let compressed = zstd::encode_all(data.as_slice(), 3).context("zstd compression failed")?;

    fs::write(&rotated_path, compressed).context("Failed to write rotated compressed file")?;

    // Truncate the original to start fresh.
    File::create(path).context("Failed to truncate operations.jsonl after rotation")?;

    Ok(())
}

/// Read all operations across rotated (compressed) and current files,
/// returned in chronological order (oldest first).
pub fn read_all_operations(workgraph_dir: &Path) -> Result<Vec<OperationEntry>> {
    let dir = log_dir(workgraph_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();

    // Collect rotated files, sorted by name (which is chronological).
    let mut rotated: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(&dir).context("Failed to read log directory")? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".jsonl.zst") {
            rotated.push(entry.path());
        }
    }
    rotated.sort();

    // Read rotated (compressed) files.
    for rpath in &rotated {
        let compressed = fs::read(rpath)
            .with_context(|| format!("Failed to read rotated file {}", rpath.display()))?;
        let mut decompressed = Vec::new();
        zstd::stream::read::Decoder::new(compressed.as_slice())
            .context("Failed to create zstd decoder")?
            .read_to_end(&mut decompressed)
            .context("Failed to decompress rotated file")?;

        for line in decompressed.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let entry: OperationEntry = serde_json::from_slice(line).with_context(|| {
                format!("Failed to parse operation entry from {}", rpath.display())
            })?;
            entries.push(entry);
        }
    }

    // Read current (uncompressed) file.
    let current = operations_path(workgraph_dir);
    if current.exists() {
        let file = File::open(&current).context("Failed to open operations.jsonl")?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line.context("Failed to read line from operations.jsonl")?;
            if line.is_empty() {
                continue;
            }
            let entry: OperationEntry =
                serde_json::from_str(&line).context("Failed to parse operation entry")?;
            entries.push(entry);
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entry(op: &str, task_id: Option<&str>) -> OperationEntry {
        OperationEntry {
            timestamp: Utc::now().to_rfc3339(),
            op: op.to_string(),
            task_id: task_id.map(String::from),
            actor: None,
            detail: serde_json::Value::Null,
        }
    }

    #[test]
    fn test_append_creates_file_and_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let entry = make_entry("add_task", Some("t1"));
        append_operation(&dir, &entry, DEFAULT_ROTATION_THRESHOLD).unwrap();

        let path = operations_path(&dir);
        assert!(path.exists());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("add_task"));
    }

    #[test]
    fn test_rotation_triggers_at_threshold() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        // Use a tiny threshold so rotation happens quickly.
        let threshold = 100u64;

        // Write enough entries to exceed the threshold.
        for i in 0..20 {
            let entry = make_entry("bulk_op", Some(&format!("t{}", i)));
            append_operation(&dir, &entry, threshold).unwrap();
        }

        // There should be at least one rotated .zst file.
        let log = log_dir(&dir);
        let rotated: Vec<_> = fs::read_dir(&log)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl.zst"))
            .collect();

        assert!(
            !rotated.is_empty(),
            "Expected at least one rotated .zst file"
        );
    }

    #[test]
    fn test_compressed_files_are_valid_zstd() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let threshold = 50u64;
        for i in 0..20 {
            let entry = make_entry("zstd_test", Some(&format!("t{}", i)));
            append_operation(&dir, &entry, threshold).unwrap();
        }

        let log = log_dir(&dir);
        for entry in fs::read_dir(&log).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().ends_with(".jsonl.zst") {
                let compressed = fs::read(entry.path()).unwrap();
                // zstd magic bytes: 0x28 0xB5 0x2F 0xFD
                assert_eq!(&compressed[..4], &[0x28, 0xB5, 0x2F, 0xFD]);

                // Should decompress without error.
                let mut decompressed = Vec::new();
                zstd::stream::read::Decoder::new(compressed.as_slice())
                    .unwrap()
                    .read_to_end(&mut decompressed)
                    .unwrap();
                assert!(!decompressed.is_empty());

                // Decompressed content should be valid JSONL.
                for line in decompressed.split(|&b| b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    let _: OperationEntry = serde_json::from_slice(line).unwrap();
                }
            }
        }
    }

    #[test]
    fn test_read_across_rotated_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let threshold = 80u64;
        let total = 30;
        for i in 0..total {
            let entry = make_entry("read_test", Some(&format!("t{}", i)));
            append_operation(&dir, &entry, threshold).unwrap();
        }

        // Read all entries back.
        let all = read_all_operations(&dir).unwrap();

        assert_eq!(
            all.len(),
            total,
            "Expected {} entries, got {}",
            total,
            all.len()
        );

        // All should have op = "read_test".
        for entry in &all {
            assert_eq!(entry.op, "read_test");
        }
    }

    #[test]
    fn test_read_empty_log() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let all = read_all_operations(&dir).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn test_read_only_current_no_rotated() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        // Write a few entries, no rotation.
        for i in 0..3 {
            let entry = make_entry("small", Some(&format!("s{}", i)));
            append_operation(&dir, &entry, DEFAULT_ROTATION_THRESHOLD).unwrap();
        }

        let all = read_all_operations(&dir).unwrap();
        assert_eq!(all.len(), 3);
    }
}

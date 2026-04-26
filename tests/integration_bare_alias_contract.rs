//! Contract test: no dated Claude model IDs in source code.
//!
//! Dated model IDs like `claude-sonnet-4-20250514` go stale silently.
//! The bare aliases (opus, sonnet, haiku) are resolved by the claude CLI.

use std::path::PathBuf;

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Recursively collect all `.rs` files under a directory.
fn collect_rs_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
            } else if path.extension().map_or(false, |e| e == "rs") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn test_no_dated_model_ids_anywhere_in_source() {
    let dated_pattern = regex::Regex::new(r"claude-(opus|sonnet|haiku)-\d+-\d{8}").unwrap();

    let mut violations = Vec::new();
    for path in collect_rs_files(&src_dir()) {
        let contents = std::fs::read_to_string(&path).unwrap();
        for (line_num, line) in contents.lines().enumerate() {
            if dated_pattern.is_match(line) {
                violations.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    line_num + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Found dated Claude model IDs in source (use bare aliases instead):\n{}",
        violations.join("\n")
    );
}

#[test]
fn test_constants_are_bare_aliases() {
    assert_eq!(workgraph::config::CLAUDE_OPUS_MODEL_ID, "opus");
    assert_eq!(workgraph::config::CLAUDE_SONNET_MODEL_ID, "sonnet");
    assert_eq!(workgraph::config::CLAUDE_HAIKU_MODEL_ID, "haiku");
}

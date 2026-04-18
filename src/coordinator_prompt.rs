//! Composable system prompt for the workgraph coordinator.
//!
//! Projects customize the coordinator's behavior by dropping
//! markdown files into `.workgraph/agency/coordinator-prompt/`:
//!
//!   - `base-system-prompt.md`    — core role description
//!   - `behavioral-rules.md`      — dos and don'ts
//!   - `common-patterns.md`       — worked examples
//!   - `evolved-amendments.md`    — auto-generated from evolution runs
//!
//! `build_system_prompt` composes these in order, joined by blank
//! lines. If the directory doesn't exist or all the files are empty,
//! it falls back to the bundled hardcoded prompt.
//!
//! This module is the one source of truth for the coordinator prompt
//! — both the daemon (when it spawns a coordinator agent) and
//! `wg nex --role coordinator` (an interactive human-driven
//! coordinator, or a subprocess coordinator) pull from the same
//! composition.

use std::path::Path;

/// Coordinator prompt component file names (in composition order).
pub const COORDINATOR_PROMPT_FILES: &[&str] = &[
    "base-system-prompt.md",
    "behavioral-rules.md",
    "common-patterns.md",
    "evolved-amendments.md",
];

/// Build the coordinator system prompt by composing from the
/// agency coordinator-prompt/ directory. Falls back to the bundled
/// hardcoded prompt if no files are found.
pub fn build_system_prompt(workgraph_dir: &Path) -> String {
    let prompt_dir = workgraph_dir.join("agency/coordinator-prompt");
    if prompt_dir.is_dir() {
        let mut parts = Vec::new();
        for filename in COORDINATOR_PROMPT_FILES {
            let path = prompt_dir.join(filename);
            if let Ok(content) = std::fs::read_to_string(&path) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        if !parts.is_empty() {
            return parts.join("\n\n");
        }
    }
    build_system_prompt_fallback()
}

/// Hardcoded fallback prompt — used when the project hasn't
/// customized its coordinator prompt.
pub fn build_system_prompt_fallback() -> String {
    include_str!("coordinator_prompt_fallback.txt").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fallback_when_no_dir() {
        let dir = tempdir().unwrap();
        let prompt = build_system_prompt(dir.path());
        assert!(prompt.contains("workgraph coordinator"));
    }

    #[test]
    fn composes_from_files() {
        let dir = tempdir().unwrap();
        let pd = dir.path().join("agency/coordinator-prompt");
        std::fs::create_dir_all(&pd).unwrap();
        std::fs::write(pd.join("base-system-prompt.md"), "# Base\nbase text").unwrap();
        std::fs::write(pd.join("behavioral-rules.md"), "# Rules\nrules text").unwrap();
        let prompt = build_system_prompt(dir.path());
        assert!(prompt.contains("base text"));
        assert!(prompt.contains("rules text"));
        // Order: base first, then rules.
        assert!(prompt.find("base text").unwrap() < prompt.find("rules text").unwrap());
    }

    #[test]
    fn empty_files_fall_back() {
        let dir = tempdir().unwrap();
        let pd = dir.path().join("agency/coordinator-prompt");
        std::fs::create_dir_all(&pd).unwrap();
        std::fs::write(pd.join("base-system-prompt.md"), "").unwrap();
        let prompt = build_system_prompt(dir.path());
        assert!(prompt.contains("workgraph coordinator"));
    }
}

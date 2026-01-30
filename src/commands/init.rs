use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Default content for .workgraph/.gitignore
const GITIGNORE_CONTENT: &str = r#"# Workgraph gitignore
# Agent output logs (can be large)
agents/

# Service files
service/

# Never commit credentials (Matrix config should be in ~/.config/workgraph/)
matrix.toml
*.secret
*.credentials
"#;

pub fn run(dir: &Path) -> Result<()> {
    if dir.exists() {
        anyhow::bail!("Workgraph already initialized at {}", dir.display());
    }

    fs::create_dir_all(dir).context("Failed to create workgraph directory")?;

    let graph_path = dir.join("graph.jsonl");
    fs::write(&graph_path, "").context("Failed to create graph.jsonl")?;

    // Create .gitignore to protect against accidental credential commits
    let gitignore_path = dir.join(".gitignore");
    fs::write(&gitignore_path, GITIGNORE_CONTENT).context("Failed to create .gitignore")?;

    println!("Initialized workgraph at {}", dir.display());
    Ok(())
}

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

    // Add .workgraph to repo-level .gitignore
    let repo_gitignore = dir.parent().map(|p| p.join(".gitignore"));
    if let Some(gitignore_path_repo) = repo_gitignore {
        let entry = ".workgraph";
        if gitignore_path_repo.exists() {
            let contents = fs::read_to_string(&gitignore_path_repo)
                .context("Failed to read .gitignore")?;
            let already_present = contents.lines().any(|line| line.trim() == entry);
            if !already_present {
                let separator = if contents.ends_with('\n') || contents.is_empty() {
                    ""
                } else {
                    "\n"
                };
                fs::write(
                    &gitignore_path_repo,
                    format!("{contents}{separator}{entry}\n"),
                )
                .context("Failed to update .gitignore")?;
                println!("Added .workgraph to .gitignore");
            }
        } else {
            fs::write(&gitignore_path_repo, format!("{entry}\n"))
                .context("Failed to create .gitignore")?;
            println!("Added .workgraph to .gitignore");
        }
    }

    let graph_path = dir.join("graph.jsonl");
    fs::write(&graph_path, "").context("Failed to create graph.jsonl")?;

    // Create .gitignore to protect against accidental credential commits
    let gitignore_path = dir.join(".gitignore");
    fs::write(&gitignore_path, GITIGNORE_CONTENT).context("Failed to create .gitignore")?;

    // Seed agency with starter roles and motivations
    let agency_dir = dir.join("agency");
    let (roles, motivations) = workgraph::agency::seed_starters(&agency_dir)
        .context("Failed to seed agency starters")?;

    println!("Initialized workgraph at {}", dir.display());
    println!("Seeded agency with {} roles and {} motivations.", roles, motivations);
    Ok(())
}

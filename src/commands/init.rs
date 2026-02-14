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
            let contents =
                fs::read_to_string(&gitignore_path_repo).context("Failed to read .gitignore")?;
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
    let (roles, motivations) =
        workgraph::agency::seed_starters(&agency_dir).context("Failed to seed agency starters")?;

    println!("Initialized workgraph at {}", dir.display());
    println!(
        "Seeded agency with {} roles and {} motivations.",
        roles, motivations
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_creates_workgraph_directory() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir).unwrap();

        assert!(wg_dir.exists());
        assert!(wg_dir.is_dir());
    }

    #[test]
    fn test_creates_graph_jsonl() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir).unwrap();

        let graph_path = wg_dir.join("graph.jsonl");
        assert!(graph_path.exists());
        let contents = fs::read_to_string(&graph_path).unwrap();
        assert!(contents.is_empty(), "graph.jsonl should be empty on init");
    }

    #[test]
    fn test_creates_inner_gitignore() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir).unwrap();

        let gitignore = wg_dir.join(".gitignore");
        assert!(gitignore.exists());
        let contents = fs::read_to_string(&gitignore).unwrap();
        assert!(contents.contains("agents/"));
        assert!(contents.contains("service/"));
        assert!(contents.contains("*.secret"));
        assert!(contents.contains("*.credentials"));
    }

    #[test]
    fn test_creates_repo_level_gitignore_when_missing() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir).unwrap();

        let repo_gitignore = tmp.path().join(".gitignore");
        assert!(repo_gitignore.exists());
        let contents = fs::read_to_string(&repo_gitignore).unwrap();
        assert!(contents.contains(".workgraph"));
    }

    #[test]
    fn test_appends_to_existing_repo_gitignore() {
        let tmp = TempDir::new().unwrap();
        let repo_gitignore = tmp.path().join(".gitignore");
        fs::write(&repo_gitignore, "node_modules/\n").unwrap();

        let wg_dir = tmp.path().join(".workgraph");
        run(&wg_dir).unwrap();

        let contents = fs::read_to_string(&repo_gitignore).unwrap();
        assert!(contents.contains("node_modules/"));
        assert!(contents.contains(".workgraph"));
    }

    #[test]
    fn test_does_not_duplicate_repo_gitignore_entry() {
        let tmp = TempDir::new().unwrap();
        let repo_gitignore = tmp.path().join(".gitignore");
        fs::write(&repo_gitignore, ".workgraph\n").unwrap();

        let wg_dir = tmp.path().join(".workgraph");
        run(&wg_dir).unwrap();

        let contents = fs::read_to_string(&repo_gitignore).unwrap();
        assert_eq!(
            contents.matches(".workgraph").count(),
            1,
            "should not duplicate .workgraph entry"
        );
    }

    #[test]
    fn test_seeds_agency() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir).unwrap();

        let agency_dir = wg_dir.join("agency");
        assert!(agency_dir.exists());
        let roles_dir = agency_dir.join("roles");
        let motivations_dir = agency_dir.join("motivations");
        assert!(roles_dir.exists(), "agency/roles should be created");
        assert!(
            motivations_dir.exists(),
            "agency/motivations should be created"
        );

        // At least one role and one motivation should be seeded
        let role_count = fs::read_dir(&roles_dir).unwrap().count();
        let motivation_count = fs::read_dir(&motivations_dir).unwrap().count();
        assert!(role_count > 0, "should seed at least one role");
        assert!(motivation_count > 0, "should seed at least one motivation");
    }

    #[test]
    fn test_fails_if_already_initialized() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir).unwrap();
        let result = run(&wg_dir);

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("already initialized"));
    }
}

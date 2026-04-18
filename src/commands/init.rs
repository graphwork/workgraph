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

pub fn run(dir: &Path, no_agency: bool) -> Result<()> {
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

    // Seed `<dir>/executors/` with example configs for the common
    // external-executor backends. The TOMLs mirror the built-in
    // defaults in `ExecutorRegistry::default_config`, so they act as
    // documentation-by-example: users copy the `.example` off to
    // override a specific flag, env var, or timeout without having
    // to reconstruct the whole config from scratch.
    //
    // Templates are bundled into the binary via `include_str!` so
    // `wg init` works regardless of where the binary is run from —
    // no dependency on the source tree being present.
    let executors_dir = dir.join("executors");
    fs::create_dir_all(&executors_dir).context("Failed to create executors directory")?;
    for (name, contents) in [
        (
            "claude.toml.example",
            include_str!("../../templates/executors/claude.toml.example"),
        ),
        (
            "codex.toml.example",
            include_str!("../../templates/executors/codex.toml.example"),
        ),
        (
            "amplifier.toml.example",
            include_str!("../../templates/executors/amplifier.toml.example"),
        ),
    ] {
        fs::write(executors_dir.join(name), contents)
            .with_context(|| format!("Failed to write executor template {}", name))?;
    }

    println!("Initialized workgraph at {}", dir.display());

    // Full agency initialization: roles, tradeoffs, default agents, config
    if !no_agency {
        super::agency_init::run(dir).context("Failed to initialize agency")?;
    }

    // Hint about global config if it doesn't exist
    if let Ok(global_path) = workgraph::config::Config::global_config_path()
        && !global_path.exists()
    {
        println!();
        println!("No global config found. Run `wg setup` to configure defaults.");
    }

    // Check skill/bundle status for the configured executor
    let config = workgraph::config::Config::load_global()?.unwrap_or_default();
    let executor = config.coordinator.effective_executor();
    match executor.as_str() {
        "claude" => {
            if !super::setup::is_claude_skill_installed() {
                println!();
                println!("Hint: The wg skill for Claude Code is not installed.");
                println!("  Spawned agents won't know wg commands without it.");
                println!("  Run: wg skill install");
            }
        }
        "amplifier" => {
            // Check if executor config exists in the newly created .workgraph
            let executor_toml = dir.join("executors/amplifier.toml");
            if !executor_toml.exists() {
                println!();
                println!(
                    "Hint: Amplifier executor is configured but not installed in this project."
                );
                println!("  Spawned agents won't know wg commands without the workgraph bundle.");
                println!("  Run: cd ~/amplifier-bundle-workgraph && ./setup.sh");
            }
        }
        _ => {} // Custom executor — user knows what they're doing
    }

    // Configure project-level CLAUDE.md if using Claude executor
    if executor == "claude"
        && let Some(project_dir) = dir.parent()
    {
        let (status, changed) = super::setup::configure_project_claude_md(project_dir)?;
        if changed {
            println!();
            println!("{}", status);
        }
    }

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

        run(&wg_dir, false).unwrap();

        assert!(wg_dir.exists());
        assert!(wg_dir.is_dir());
    }

    #[test]
    fn test_creates_graph_jsonl() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, false).unwrap();

        let graph_path = wg_dir.join("graph.jsonl");
        assert!(graph_path.exists());
        let contents = fs::read_to_string(&graph_path).unwrap();
        assert!(contents.is_empty(), "graph.jsonl should be empty on init");
    }

    #[test]
    fn test_creates_inner_gitignore() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, false).unwrap();

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

        run(&wg_dir, false).unwrap();

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
        run(&wg_dir, false).unwrap();

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
        run(&wg_dir, false).unwrap();

        let contents = fs::read_to_string(&repo_gitignore).unwrap();
        assert_eq!(
            contents.matches(".workgraph").count(),
            1,
            "should not duplicate .workgraph entry"
        );
    }

    #[test]
    fn test_full_agency_init() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, false).unwrap();

        let agency_dir = wg_dir.join("agency");
        assert!(agency_dir.exists());
        let roles_dir = agency_dir.join("cache/roles");
        let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
        assert!(roles_dir.exists(), "agency/roles should be created");
        assert!(tradeoffs_dir.exists(), "agency/tradeoffs should be created");

        // Full agency init creates roles, tradeoffs, and agents
        let role_count = fs::read_dir(&roles_dir).unwrap().count();
        let tradeoff_count = fs::read_dir(&tradeoffs_dir).unwrap().count();
        assert!(
            role_count >= 8,
            "should seed at least 8 roles (4 starter + 4 special)"
        );
        assert!(tradeoff_count >= 4, "should seed at least 4 tradeoffs");

        // Agents should be created (1 default + 4 special)
        let agents_dir = agency_dir.join("cache/agents");
        assert!(agents_dir.exists(), "agents dir should be created");
        let agent_count = fs::read_dir(&agents_dir).unwrap().count();
        assert_eq!(
            agent_count, 5,
            "should create 5 agents (1 default + 4 special)"
        );

        // Config should have auto_assign and auto_evaluate enabled
        let config = workgraph::config::Config::load(&wg_dir).unwrap();
        assert!(config.agency.auto_assign, "auto_assign should be enabled");
        assert!(
            config.agency.auto_evaluate,
            "auto_evaluate should be enabled"
        );
    }

    #[test]
    fn test_no_agency_flag() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, true).unwrap();

        // Workgraph dir and graph.jsonl should exist
        assert!(wg_dir.exists());
        assert!(wg_dir.join("graph.jsonl").exists());

        // Agency dir should NOT exist
        let agency_dir = wg_dir.join("agency");
        assert!(
            !agency_dir.exists(),
            "agency should not be created with --no-agency"
        );
    }

    #[test]
    fn test_fails_if_already_initialized() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, false).unwrap();
        let result = run(&wg_dir, false);

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("already initialized"));
    }
}

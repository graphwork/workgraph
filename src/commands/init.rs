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

pub fn run(
    dir: &Path,
    no_agency: bool,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<()> {
    if dir.exists() {
        anyhow::bail!("Workgraph already initialized at {}", dir.display());
    }
    // Refuse if the sibling legacy dir exists — we'd silently shadow it.
    // e.g. user asks for `.wg` but `.workgraph` already exists next to it.
    if let Some(parent) = dir.parent()
        && let Some(target_name) = dir.file_name().and_then(|n| n.to_str())
    {
        for sibling in [".wg", ".workgraph"] {
            if sibling == target_name {
                continue;
            }
            let sibling_path = parent.join(sibling);
            if sibling_path.is_dir() {
                anyhow::bail!(
                    "Workgraph already initialized at {} (legacy dir name). \
                     Either use it as-is, or remove/rename it before running `wg init`.",
                    sibling_path.display()
                );
            }
        }
    }

    fs::create_dir_all(dir).context("Failed to create workgraph directory")?;

    // Add the dir name (`.wg` for new projects, `.workgraph` for legacy init
    // targets) to repo-level .gitignore.
    let dir_basename = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".wg")
        .to_string();
    let repo_gitignore = dir.parent().map(|p| p.join(".gitignore"));
    if let Some(gitignore_path_repo) = repo_gitignore {
        let entry = dir_basename.as_str();
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
                println!("Added {} to .gitignore", entry);
            }
        } else {
            fs::write(&gitignore_path_repo, format!("{entry}\n"))
                .context("Failed to create .gitignore")?;
            println!("Added {} to .gitignore", entry);
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

    // If -m / -e were given, seed config.toml so every subsequent
    // command in this project points at the chosen model/endpoint
    // out of the box.
    if model.is_some() || endpoint.is_some() {
        apply_model_endpoint(dir, model, endpoint)
            .context("Failed to write model/endpoint config")?;
    }

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

/// Write an endpoint + model into the project's `config.toml`.
///
/// Inputs:
/// - `model` sets `coordinator.model` and `agent.model` so every spawned
///   agent inherits the choice.
/// - `endpoint` starting with `http://` or `https://` gets written as an
///   oai-compat no-auth endpoint entry marked `is_default`. That mirrors
///   the inline `-e URL` behaviour of `wg nex` at the config layer.
fn apply_model_endpoint(
    dir: &Path,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<()> {
    use workgraph::config::{Config, EndpointConfig};

    let mut config = Config::load(dir).unwrap_or_default();

    // Endpoint implies the `local` provider (oai-compat + no auth), which
    // also fixes the prefix we write into the model fields — the config
    // validator demands `provider:model`, so bare names would refuse to
    // reload.
    let effective_model: Option<String> = if endpoint.is_some() {
        model.map(|m| {
            if m.contains(':') {
                m.to_string()
            } else {
                format!("local:{}", m)
            }
        })
    } else {
        model.map(|m| m.to_string())
    };

    if let Some(url) = endpoint {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            anyhow::bail!(
                "Endpoint must be an http:// or https:// URL (got: {})",
                url
            );
        }
        let name = "default".to_string();
        config
            .llm_endpoints
            .endpoints
            .retain(|e| e.name != name);
        for e in config.llm_endpoints.endpoints.iter_mut() {
            e.is_default = false;
        }
        config.llm_endpoints.endpoints.push(EndpointConfig {
            name,
            provider: "local".to_string(),
            url: Some(url.to_string()),
            // Store the bare model on the endpoint (provider is known
            // from `provider: local`), not the prefixed form.
            model: model.map(|s| s.to_string()),
            api_key: None,
            api_key_file: None,
            api_key_env: None,
            is_default: true,
            context_window: None,
        });
        println!("Configured default endpoint: {}", url);
    }

    if let Some(ref m) = effective_model {
        config.coordinator.model = Some(m.clone());
        config.agent.model = m.clone();
        println!("Set model: {}", m);
    }

    config.save(dir).context("Failed to save config.toml")?;
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

        run(&wg_dir, false, None, None).unwrap();

        assert!(wg_dir.exists());
        assert!(wg_dir.is_dir());
    }

    #[test]
    fn test_creates_graph_jsonl() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, false, None, None).unwrap();

        let graph_path = wg_dir.join("graph.jsonl");
        assert!(graph_path.exists());
        let contents = fs::read_to_string(&graph_path).unwrap();
        assert!(contents.is_empty(), "graph.jsonl should be empty on init");
    }

    #[test]
    fn test_creates_inner_gitignore() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, false, None, None).unwrap();

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

        run(&wg_dir, false, None, None).unwrap();

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
        run(&wg_dir, false, None, None).unwrap();

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
        run(&wg_dir, false, None, None).unwrap();

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

        run(&wg_dir, false, None, None).unwrap();

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

        run(&wg_dir, true, None, None).unwrap();

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

        run(&wg_dir, false, None, None).unwrap();
        let result = run(&wg_dir, false, None, None);

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("already initialized"));
    }

    #[test]
    fn test_new_wg_dir_basename_lands_in_gitignore() {
        // When init targets `.wg` (the new default), the root .gitignore
        // entry should say `.wg` — not the legacy `.workgraph`.
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".wg");
        run(&wg_dir, true, None, None).unwrap();
        let repo_gitignore = tmp.path().join(".gitignore");
        let contents = fs::read_to_string(&repo_gitignore).unwrap();
        assert!(contents.lines().any(|l| l.trim() == ".wg"));
        assert!(!contents.lines().any(|l| l.trim() == ".workgraph"));
    }

    #[test]
    fn test_refuses_when_sibling_workgraph_exists() {
        // Asking for `.wg` but `.workgraph` already sits next door
        // should error — otherwise subsequent commands would silently
        // shadow the legacy dir.
        let tmp = TempDir::new().unwrap();
        let legacy = tmp.path().join(".workgraph");
        fs::create_dir_all(&legacy).unwrap();
        let new_dir = tmp.path().join(".wg");
        let result = run(&new_dir, true, None, None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains(".workgraph"), "error mentions legacy dir: {}", err);
    }

    #[test]
    fn test_model_and_endpoint_write_config() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".wg");
        run(
            &wg_dir,
            true,
            Some("nemotron-h-8b"),
            Some("http://127.0.0.1:8088"),
        )
        .unwrap();

        let config = workgraph::config::Config::load(&wg_dir).unwrap();
        // With an endpoint given, the model fields get the `local:` prefix
        // so the provider:model validator accepts them on reload.
        assert_eq!(
            config.coordinator.model.as_deref(),
            Some("local:nemotron-h-8b"),
            "coordinator.model should be persisted with local: prefix"
        );
        assert_eq!(
            config.agent.model, "local:nemotron-h-8b",
            "agent.model should be persisted with local: prefix"
        );
        let eps = &config.llm_endpoints.endpoints;
        let default_ep = eps
            .iter()
            .find(|e| e.is_default)
            .expect("a default endpoint should be written");
        assert_eq!(default_ep.url.as_deref(), Some("http://127.0.0.1:8088"));
        assert_eq!(default_ep.provider, "local");
        // The endpoint itself carries the bare model name.
        assert_eq!(default_ep.model.as_deref(), Some("nemotron-h-8b"));
    }

    #[test]
    fn test_endpoint_rejects_non_http() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".wg");
        let err = run(&wg_dir, true, None, Some("definitely-not-a-url"))
            .expect_err("non-http endpoint should be rejected");
        // anyhow context wraps the inner bail, so format with `{:#}` to get the chain.
        let chain = format!("{:#}", err);
        assert!(
            chain.contains("http://") || chain.contains("https://"),
            "error chain should mention http(s):// — got: {}",
            chain
        );
    }
}

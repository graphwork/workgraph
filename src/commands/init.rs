use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use workgraph::config_defaults::{config_for_route, RouteParams, SetupRoute};

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

/// Init entry that supports the new `--route <name>` flag and `--dry-run`.
///
/// When `route` is `Some`, the route's complete defaults populate `[tiers]`
/// + endpoint + model registry. When only `executor` is given, the closest
/// matching route is used (so `wg init -x claude` still produces filled
/// tiers, fixing the "empty [tiers]" bug). Falls back to the legacy
/// `executor`-only path when neither is specified.
pub fn run_with_route(
    dir: &Path,
    no_agency: bool,
    executor: Option<&str>,
    model: Option<&str>,
    endpoint: Option<&str>,
    route: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    // Explicit --route always wins. Otherwise, derive a route from
    // --executor — but ONLY when the executor maps to one of the 5
    // routes. Unknown executors (`shell`, `amplifier`, custom names)
    // fall through to the legacy path so we don't clobber them with
    // claude defaults.
    let resolved_route: Option<SetupRoute> = if let Some(name) = route {
        Some(SetupRoute::from_name(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown route '{}'. Valid routes: openrouter, claude-cli, codex-cli, local, nex-custom",
                name,
            )
        })?)
    } else {
        executor.and_then(SetupRoute::try_from_executor)
    };

    if dry_run {
        if let Some(r) = resolved_route {
            let params = RouteParams {
                api_key_env: None,
                api_key_file: None,
                url: endpoint.map(|s| s.to_string()),
                model: model.map(|s| s.to_string()),
            };
            let cfg = config_for_route(r, params);
            let toml = toml::to_string_pretty(&cfg)
                .map_err(|e| anyhow::anyhow!("serialize: {}", e))?;
            println!("# wg init --dry-run (route: {})", r.as_name());
            println!("# Would create: {}", dir.display());
            println!("# Would write the following config.toml:");
            println!("---");
            println!("{}", toml);
            return Ok(());
        }
        anyhow::bail!(
            "--dry-run requires either --route or --executor so the would-be config can be shown."
        );
    }

    // If no route was resolved, fall back to the legacy executor-only path.
    let Some(route) = resolved_route else {
        return run(dir, no_agency, executor, model, endpoint);
    };

    // Route-driven init: create dir, write config from route defaults.
    if dir.exists() {
        anyhow::bail!("Workgraph already initialized at {}", dir.display());
    }
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
    write_repo_gitignore(dir)?;
    let graph_path = dir.join("graph.jsonl");
    fs::write(&graph_path, "").context("Failed to create graph.jsonl")?;
    let gitignore_path = dir.join(".gitignore");
    fs::write(&gitignore_path, GITIGNORE_CONTENT).context("Failed to create .gitignore")?;
    write_executor_templates(dir)?;

    println!("Initialized workgraph at {}", dir.display());

    let params = RouteParams {
        api_key_env: None,
        api_key_file: None,
        url: endpoint.map(|s| s.to_string()),
        model: model.map(|s| s.to_string()),
    };
    let mut config = config_for_route(route, params);

    // Apply -m / -e overrides on top of the route defaults so the user's
    // explicit flags win.
    if model.is_some() || endpoint.is_some() {
        let summary = config
            .apply_model_endpoint(model, endpoint)
            .context("apply -m / -e on top of route defaults")?;
        for line in summary {
            println!("{}", line);
        }
    }

    config.save(dir).context("Failed to save config.toml")?;

    let tier_summary = format!(
        "{}/{}/{}",
        config.tiers.fast.as_deref().unwrap_or("?"),
        config.tiers.standard.as_deref().unwrap_or("?"),
        config.tiers.premium.as_deref().unwrap_or("?"),
    );
    println!(
        "Wrote {}/config.toml: route={}, executor={}, tiers={}",
        dir.display(),
        route.as_name(),
        route.executor(),
        tier_summary,
    );

    if !no_agency {
        super::agency_init::run(dir).context("Failed to initialize agency")?;
    }

    if let Ok(global_path) = workgraph::config::Config::global_config_path()
        && !global_path.exists()
    {
        println!();
        println!("No global config found. Run `wg setup` to configure defaults.");
    }

    if route == SetupRoute::ClaudeCli && !super::setup::is_claude_skill_installed() {
        println!();
        println!("Hint: The wg skill for Claude Code is not installed.");
        println!("  Spawned agents won't know wg commands without it.");
        println!("  Run: wg skill install");
    }

    if route == SetupRoute::ClaudeCli
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

/// Write the repo-level .gitignore entry for the workgraph dir basename.
fn write_repo_gitignore(dir: &Path) -> Result<()> {
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
    Ok(())
}

/// Write `.workgraph/executors/*.example` template files.
fn write_executor_templates(dir: &Path) -> Result<()> {
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
    Ok(())
}

pub fn run(
    dir: &Path,
    no_agency: bool,
    executor: Option<&str>,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<()> {
    let executor = match executor {
        Some(e) => e,
        None => {
            anyhow::bail!(
                "Executor is required for `wg init`.\n\
                \n\
                Choose one of the supported executors and re-run:\n\
                \n\
                  wg init --executor claude      # Anthropic Claude Code (default for most users)\n\
                  wg init --executor amplifier   # Amplifier multi-agent bundles\n\
                  wg init --executor codex       # OpenAI Codex\n\
                  wg init --executor shell       # Plain shell commands\n\
                  wg init --executor nex         # Native executor (wg nex)\n\
                \n\
                Tip: use `wg setup` for an interactive wizard."
            );
        }
    };

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

    // Always write the executor choice to config.toml.
    apply_executor(dir, executor).context("Failed to write executor config")?;

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

    // Check skill/bundle status for the chosen executor.
    match executor {
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

    // Record this invocation so future `wg setup`, the TUI new-coordinator
    // dialog, and any model-picker UI can offer it as a one-click recall.
    // Use the canonical executor name ("native" not "nex") so dedup
    // collapses entries that came in via different aliases.
    let canonical_executor = match executor {
        "nex" => "native",
        other => other,
    };
    let _ = workgraph::launcher_history::record_use(
        &workgraph::launcher_history::HistoryEntry::new(
            canonical_executor,
            model,
            endpoint,
            "cli",
        ),
    );

    Ok(())
}

/// Write the chosen executor into the project's `config.toml`.
///
/// When the executor maps to one of the known routes (claude → claude-cli,
/// codex → codex-cli, nex/native → openrouter), the route's defaults are
/// used to populate `[tiers]` and the model registry — fixing the empty
/// `[tiers]` bug from the old `wg init -x claude` flow.
///
/// For executors with no matching route (`shell`, `amplifier`, custom),
/// only `coordinator.executor` is set and `[tiers]` is left empty so the
/// custom-executor user can decide for themselves.
fn apply_executor(dir: &Path, executor: &str) -> Result<()> {
    let canonical = match executor {
        "nex" => "native",
        other => other,
    };

    let route = match canonical {
        "claude" => Some(SetupRoute::ClaudeCli),
        "codex" => Some(SetupRoute::CodexCli),
        "native" => Some(SetupRoute::Openrouter),
        _ => None,
    };

    let mut config = workgraph::config::Config::load(dir).unwrap_or_default();

    if let Some(route) = route {
        let route_cfg = config_for_route(route, RouteParams::default());
        config.coordinator.executor = route_cfg.coordinator.executor.clone();
        config.agent.executor = route_cfg.agent.executor.clone();
        config.tiers = route_cfg.tiers.clone();
        if !route_cfg.model_registry.is_empty() {
            config.model_registry = route_cfg.model_registry.clone();
        }
        // Only seed the default model if the existing config doesn't have one
        // (i.e. fresh init). Don't clobber a user-set model.
        if config.coordinator.model.is_none() {
            config.coordinator.model = route_cfg.coordinator.model.clone();
        }
        if config.agent.model.is_empty() || config.agent.model == "sonnet" {
            config.agent.model = route_cfg.agent.model.clone();
        }
        // Wire models.evaluator/assigner so eval doesn't fall back to a
        // model the route doesn't actually own.
        config.models = route_cfg.models.clone();
    } else {
        config.coordinator.executor = Some(canonical.to_string());
    }
    config.save(dir).context("Failed to save config.toml")?;
    println!("Set coordinator.executor = \"{}\"", canonical);
    if route.is_some() {
        let tier_summary = format!(
            "{}/{}/{}",
            config.tiers.fast.as_deref().unwrap_or("?"),
            config.tiers.standard.as_deref().unwrap_or("?"),
            config.tiers.premium.as_deref().unwrap_or("?"),
        );
        println!("Populated [tiers]: {}", tier_summary);
    }
    Ok(())
}

/// Write an endpoint + model into the project's `config.toml` on init.
/// Thin wrapper around `Config::apply_model_endpoint` so init shares the
/// same semantics as `wg config -m/-e`.
fn apply_model_endpoint(dir: &Path, model: Option<&str>, endpoint: Option<&str>) -> Result<()> {
    let mut config = workgraph::config::Config::load(dir).unwrap_or_default();
    let summary = config
        .apply_model_endpoint(model, endpoint)
        .context("apply model/endpoint")?;
    for line in &summary {
        println!("{}", line);
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

        run(&wg_dir, false, Some("shell"), None, None).unwrap();

        assert!(wg_dir.exists());
        assert!(wg_dir.is_dir());
    }

    #[test]
    fn test_creates_graph_jsonl() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, false, Some("shell"), None, None).unwrap();

        let graph_path = wg_dir.join("graph.jsonl");
        assert!(graph_path.exists());
        let contents = fs::read_to_string(&graph_path).unwrap();
        assert!(contents.is_empty(), "graph.jsonl should be empty on init");
    }

    #[test]
    fn test_creates_inner_gitignore() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");

        run(&wg_dir, false, Some("shell"), None, None).unwrap();

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

        run(&wg_dir, false, Some("shell"), None, None).unwrap();

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
        run(&wg_dir, false, Some("shell"), None, None).unwrap();

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
        run(&wg_dir, false, Some("shell"), None, None).unwrap();

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

        run(&wg_dir, false, Some("shell"), None, None).unwrap();

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

        run(&wg_dir, true, Some("shell"), None, None).unwrap();

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

        run(&wg_dir, false, Some("shell"), None, None).unwrap();
        let result = run(&wg_dir, false, Some("shell"), None, None);

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
        run(&wg_dir, true, Some("shell"), None, None).unwrap();
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
        let result = run(&new_dir, true, Some("shell"), None, None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains(".workgraph"),
            "error mentions legacy dir: {}",
            err
        );
    }

    #[test]
    fn test_model_and_endpoint_write_config() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".wg");
        run(
            &wg_dir,
            true,
            Some("shell"),
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
        let err = run(&wg_dir, true, Some("shell"), None, Some("definitely-not-a-url"))
            .expect_err("non-http endpoint should be rejected");
        // anyhow context wraps the inner bail, so format with `{:#}` to get the chain.
        let chain = format!("{:#}", err);
        assert!(
            chain.contains("http://") || chain.contains("https://"),
            "error chain should mention http(s):// — got: {}",
            chain
        );
    }

    /// Run a closure with `WG_LAUNCHER_HISTORY_PATH` pointed at a tempfile.
    /// `unsafe` is required because `set_var` is process-global; serial-test
    /// gates these env-var tests so they never race with each other.
    fn with_history_env<F: FnOnce(&Path)>(f: F) {
        let tmp = TempDir::new().unwrap();
        let history_path = tmp.path().join("launcher-history.jsonl");
        unsafe {
            std::env::set_var("WG_LAUNCHER_HISTORY_PATH", &history_path);
        }
        f(&history_path);
        unsafe {
            std::env::remove_var("WG_LAUNCHER_HISTORY_PATH");
        }
    }

    #[test]
    #[serial_test::serial(launcher_history_env)]
    fn test_cli_init_records_to_launcher_history() {
        with_history_env(|history_path| {
            let tmp = TempDir::new().unwrap();
            let wg_dir = tmp.path().join(".wg");
            run(
                &wg_dir,
                true,
                Some("shell"),
                Some("opus"),
                Some("https://example.com:8080"),
            )
            .unwrap();

            let contents = fs::read_to_string(history_path)
                .expect("history file should have been created by init");
            assert!(
                contents.contains("\"executor\":\"shell\""),
                "history should contain executor: {}",
                contents
            );
            // The model gets the `local:` prefix because we passed an
            // endpoint, but the history records the prefixed form (matches
            // what landed in config.toml).
            assert!(
                contents.contains("\"opus\""),
                "history should contain model: {}",
                contents
            );
            assert!(
                contents.contains("https://example.com:8080"),
                "history should contain endpoint: {}",
                contents
            );
            assert!(
                contents.contains("\"source\":\"cli\""),
                "history should mark source as cli: {}",
                contents
            );
        });
    }

    #[test]
    #[serial_test::serial(launcher_history_env)]
    fn test_cli_init_records_canonical_executor_for_nex() {
        // `wg init --executor nex` should be recorded as canonical
        // "native" so the TUI dedup can collapse entries that came in
        // through different aliases.
        with_history_env(|history_path| {
            let tmp = TempDir::new().unwrap();
            let wg_dir = tmp.path().join(".wg");
            run(&wg_dir, true, Some("nex"), None, None).unwrap();

            let contents = fs::read_to_string(history_path).unwrap();
            assert!(
                contents.contains("\"executor\":\"native\""),
                "history should canonicalize nex → native: {}",
                contents
            );
            assert!(
                !contents.contains("\"executor\":\"nex\""),
                "history should not record raw 'nex' alias: {}",
                contents
            );
        });
    }
}

#![warn(clippy::redundant_closure)]

use anyhow::Result;
use clap::{CommandFactory, Parser};
use std::path::{Path, PathBuf};
use workgraph::config::Config;

mod cli;
mod commands;
mod tui;

use cli::*;

/// Resolve the workgraph directory for this invocation.
///
/// Precedence (highest first):
///
/// 1. **Explicit `--dir <path>` CLI flag.** Always wins. Pass-through.
/// 2. **`WG_DIR` environment variable.** Second-highest — lets users
///    script `wg` commands against a specific graph without a flag.
/// 3. **Project discovery.** Walk up from `cwd` looking for a
///    `.workgraph` directory. If found, use it. This matches how
///    `git` finds `.git`, `cargo` finds `Cargo.toml`, etc.
/// 4. **Global fallback `~/.workgraph`.** If the user has a global
///    workgraph directory in their home, use it. This makes `wg nex`
///    usable from any directory without littering `.workgraph` dirs
///    across the filesystem. Primarily for REPL-style interactive
///    commands; project-scoped commands (`wg add`, `wg done`, etc.)
///    will still fail-on-missing when they try to load the graph.
/// 5. **Default `./.workgraph` in current directory.** Backward-
///    compatible final fallback. Same as the old pre-resolver
///    behavior — will error cleanly downstream if the directory
///    doesn't exist and a graph-reading command is run.
///
/// The resolver does NOT create any directories — it only locates
/// one. Auto-creation is the responsibility of individual commands
/// (`wg init`, `wg nex` when the global fallback is used, etc.).
fn resolve_workgraph_dir(
    cli_dir: Option<PathBuf>,
    env_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> PathBuf {
    // 1. Explicit CLI flag
    if let Some(p) = cli_dir {
        return p;
    }

    // 2. WG_DIR env var
    if let Some(p) = env_dir.filter(|p| !p.as_os_str().is_empty()) {
        return p;
    }

    // 3. Walk up from cwd looking for an existing .workgraph directory
    if let Some(start) = cwd.as_ref() {
        let mut cur: &Path = start;
        loop {
            let candidate = cur.join(".workgraph");
            if candidate.is_dir() {
                return candidate;
            }
            match cur.parent() {
                Some(parent) => cur = parent,
                None => break,
            }
        }
    }

    // 4. Global fallback: ~/.workgraph if it exists
    if let Some(home) = home_dir.as_ref() {
        let global = home.join(".workgraph");
        if global.is_dir() {
            return global;
        }
    }

    // 5. Default: ./.workgraph in current directory
    cwd.map(|c| c.join(".workgraph"))
        .unwrap_or_else(|| PathBuf::from(".workgraph"))
}

#[cfg(test)]
mod resolver_tests {
    use super::resolve_workgraph_dir;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn explicit_cli_flag_wins_over_everything() {
        let tmp = TempDir::new().unwrap();
        let explicit = tmp.path().join("explicit/.workgraph");
        let result = resolve_workgraph_dir(
            Some(explicit.clone()),
            Some(PathBuf::from("/should/not/be/used")),
            Some(tmp.path().to_path_buf()),
            Some(PathBuf::from("/fake/home")),
        );
        assert_eq!(result, explicit);
    }

    #[test]
    fn wg_dir_env_var_wins_over_discovery_and_global() {
        let tmp = TempDir::new().unwrap();
        let env = tmp.path().join("from-env/.workgraph");
        let result = resolve_workgraph_dir(
            None,
            Some(env.clone()),
            Some(tmp.path().to_path_buf()),
            Some(PathBuf::from("/fake/home")),
        );
        assert_eq!(result, env);
    }

    #[test]
    fn empty_wg_dir_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_workgraph_dir(
            None,
            Some(PathBuf::from("")),
            Some(tmp.path().to_path_buf()),
            Some(PathBuf::from("/fake/home")),
        );
        // Should fall through to the default
        assert_eq!(result, tmp.path().join(".workgraph"));
    }

    #[test]
    fn project_discovery_finds_workgraph_in_cwd() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let wg = project.join(".workgraph");
        std::fs::create_dir_all(&wg).unwrap();
        let result =
            resolve_workgraph_dir(None, None, Some(project), Some(tmp.path().to_path_buf()));
        assert_eq!(result, wg);
    }

    #[test]
    fn project_discovery_walks_up_from_subdirectory() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let deep = project.join("src/deep/nested");
        let wg = project.join(".workgraph");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(&wg).unwrap();
        let result = resolve_workgraph_dir(None, None, Some(deep), Some(tmp.path().to_path_buf()));
        assert_eq!(result, wg);
    }

    #[test]
    fn global_fallback_used_when_no_project_and_home_has_workgraph() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let global = home.join(".workgraph");
        std::fs::create_dir_all(&global).unwrap();
        let outside = tmp.path().join("somewhere/else");
        std::fs::create_dir_all(&outside).unwrap();
        let result = resolve_workgraph_dir(None, None, Some(outside), Some(home));
        assert_eq!(result, global);
    }

    #[test]
    fn project_beats_global_when_both_exist() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let global = home.join(".workgraph");
        std::fs::create_dir_all(&global).unwrap();
        let project = tmp.path().join("project");
        let project_wg = project.join(".workgraph");
        std::fs::create_dir_all(&project_wg).unwrap();
        let result = resolve_workgraph_dir(None, None, Some(project), Some(home));
        assert_eq!(result, project_wg);
    }

    #[test]
    fn default_fallback_when_nothing_exists() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let home = tmp.path().join("home"); // no .workgraph inside
        let result = resolve_workgraph_dir(None, None, Some(outside.clone()), Some(home));
        assert_eq!(result, outside.join(".workgraph"));
    }
}

/// Print custom help output with usage-based ordering
fn print_help(dir: &Path, show_all: bool, alphabetical: bool) {
    use workgraph::config::Config;
    use workgraph::usage::{self, MAX_HELP_COMMANDS};

    // Get subcommand definitions from clap
    let cmd = Cli::command();
    let subcommands: Vec<_> = cmd
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| {
            let name = c.get_name().to_string();
            let about = c
                .get_about()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            (name, about)
        })
        .collect();

    // Load config for ordering preference
    let config = Config::load_or_default(dir);
    let use_alphabetical = alphabetical || config.help.ordering == "alphabetical";

    println!("wg - workgraph task management\n");

    if use_alphabetical {
        // Simple alphabetical listing
        let mut sorted = subcommands;
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let to_show = if show_all {
            sorted.len()
        } else {
            MAX_HELP_COMMANDS.min(sorted.len())
        };
        println!("Commands:");
        for (name, about) in sorted.iter().take(to_show) {
            println!("  {:15} {}", name, about);
        }
        if !show_all && sorted.len() > MAX_HELP_COMMANDS {
            println!(
                "  ... and {} more (--help-all)",
                sorted.len() - MAX_HELP_COMMANDS
            );
        }
    } else if config.help.ordering == "curated" {
        print_curated_help(&subcommands, show_all);
    } else if let Some(usage_data) = usage::load_command_order(dir) {
        // Use personalized usage-based ordering with tiers
        let (frequent, occasional, mut rare) = usage::group_by_tier(&usage_data);

        // Add commands with zero usage to the rare tier so they still appear in --help-all
        let mut zero_usage: Vec<&str> = subcommands
            .iter()
            .filter(|(n, _)| {
                !frequent.contains(&n.as_str())
                    && !occasional.contains(&n.as_str())
                    && !rare.contains(&n.as_str())
            })
            .map(|(n, _)| n.as_str())
            .collect();
        zero_usage.sort();
        rare.extend(zero_usage);

        let mut shown = 0;
        let max_show = if show_all {
            subcommands.len()
        } else {
            MAX_HELP_COMMANDS
        };

        // Helper to print commands in a tier
        let mut print_tier = |title: &str, tier_cmds: &[&str]| {
            if tier_cmds.is_empty() || shown >= max_show {
                return;
            }
            println!("{}:", title);
            for &cmd_name in tier_cmds {
                if shown >= max_show {
                    break;
                }
                if let Some((_, about)) = subcommands.iter().find(|(n, _)| n == cmd_name) {
                    println!("  {:15} {}", cmd_name, about);
                    shown += 1;
                }
            }
            println!();
        };

        print_tier("Your most-used", &frequent);
        print_tier("Also used", &occasional);

        if show_all {
            print_tier("Less common", &rare);
        } else if shown < max_show && !rare.is_empty() {
            let remaining = max_show - shown;
            let to_show: Vec<&str> = rare.iter().take(remaining).copied().collect();
            if !to_show.is_empty() {
                println!("More commands:");
                for &cmd_name in &to_show {
                    if let Some((_, about)) = subcommands.iter().find(|(n, _)| n == cmd_name) {
                        println!("  {:15} {}", cmd_name, about);
                    }
                }
            }
        }

        let total_cmds = frequent.len() + occasional.len() + rare.len();
        if !show_all && total_cmds > MAX_HELP_COMMANDS {
            // Count commands we didn't show
            let unshown: usize = subcommands
                .iter()
                .filter(|(n, _)| {
                    !frequent.contains(&n.as_str())
                        && !occasional.contains(&n.as_str())
                        && !rare
                            .iter()
                            .take(max_show - frequent.len() - occasional.len())
                            .any(|&r| r == n.as_str())
                })
                .count();
            if unshown > 0 {
                println!("  ... and {} more (--help-all)", unshown);
            }
        }
    } else {
        // No usage data and not curated — fall back to curated ordering
        print_curated_help(&subcommands, show_all);
    }

    println!("\nOptions:");
    println!("  -d, --dir <PATH>    Workgraph directory [default: .workgraph]");
    println!("  -h, --help          Print help (--help-all for all commands)");
    println!("      --alphabetical  Sort commands alphabetically");
    println!("      --json          Output as JSON");
    println!("  -V, --version       Print version");
}

/// Print commands using the curated default ordering, with remaining commands shown alphabetically.
fn print_curated_help(subcommands: &[(String, String)], show_all: bool) {
    use workgraph::usage::{self, MAX_HELP_COMMANDS};

    let mut shown = std::collections::HashSet::new();
    let mut count = 0;

    // Always show core commands first — these are never clipped by MAX_HELP_COMMANDS
    println!("Core commands:");
    for &cmd_name in usage::CORE_COMMANDS {
        if let Some((name, about)) = subcommands.iter().find(|(n, _)| n == cmd_name) {
            println!("  {:15} {}", name, about);
            shown.insert(name.clone());
            count += 1;
        }
    }

    let to_show = if show_all {
        subcommands.len()
    } else {
        MAX_HELP_COMMANDS.min(subcommands.len())
    };

    // Fill remaining slots from DEFAULT_ORDER, then alphabetically
    let remaining_slots = to_show.saturating_sub(count);
    if remaining_slots > 0 || show_all {
        let mut extra = Vec::new();

        // First pull from DEFAULT_ORDER (preserving curated priority)
        for &default_cmd in usage::DEFAULT_ORDER {
            if !shown.contains(default_cmd)
                && let Some(entry) = subcommands.iter().find(|(n, _)| n == default_cmd)
            {
                extra.push(entry);
                shown.insert(entry.0.clone());
            }
        }

        // Then any remaining commands alphabetically
        let mut alpha_rest: Vec<_> = subcommands
            .iter()
            .filter(|(n, _)| !shown.contains(n))
            .collect();
        alpha_rest.sort_by(|a, b| a.0.cmp(&b.0));
        extra.extend(alpha_rest);

        let to_print = if show_all {
            extra.len()
        } else {
            remaining_slots
        };

        if to_print > 0 {
            println!("\nOther commands:");
            for (name, about) in extra.iter().take(to_print) {
                println!("  {:15} {}", name, about);
                count += 1;
            }
        }
    }

    if !show_all && subcommands.len() > count {
        println!(
            "\n  ... and {} more (--help-all)",
            subcommands.len() - count
        );
    }
}

/// Check if the user is requesting help for a specific subcommand (e.g., `wg show --help`
/// or `wg trace extract --help`).
///
/// Because we use `disable_help_flag = true` for the custom top-level help system,
/// clap doesn't intercept `--help` at the subcommand level. This function pre-scans
/// raw args and, if a subcommand + help flag is detected, prints clap's native help
/// for that subcommand. Supports nested subcommands (e.g., `wg trace extract --help`).
fn maybe_print_subcommand_help() -> bool {
    let args: Vec<String> = std::env::args().collect();

    // Check if --help or -h appears alongside a subcommand
    let has_help = args.iter().any(|a| a == "--help" || a == "-h");
    if !has_help {
        return false;
    }

    // Walk the subcommand chain: start from the root command and drill down
    // through non-flag args that match subcommand names at each level.
    let mut current_cmd = Cli::command();
    let mut matched_any = false;

    for arg in args.iter().skip(1) {
        if arg.starts_with('-') {
            continue;
        }
        let maybe_sub = current_cmd
            .get_subcommands()
            .find(|c| c.get_name() == arg)
            .cloned();
        if let Some(sub) = maybe_sub {
            current_cmd = sub;
            matched_any = true;
        }
    }

    if matched_any {
        let mut cmd = current_cmd.disable_help_flag(false);
        cmd.print_help().ok();
        println!();
        std::process::exit(0);
    }

    false
}

fn main() -> Result<()> {
    // Handle subcommand-level help before clap parses (since we disable_help_flag globally)
    maybe_print_subcommand_help();

    // Initialize logging.
    //
    // Default filter: "info" for normal commands, "warn" for nex/tui-nex
    // so the interactive REPL stays quiet. Users can still override by
    // setting RUST_LOG explicitly. We check argv directly here instead
    // of parsing clap first, because clap's own error output depends
    // on the logger already being initialized.
    let is_repl_invocation = std::env::args()
        .skip(1)
        .any(|a| a == "nex" || a == "tui-nex");
    let default_filter = if is_repl_invocation {
        // warn for our code, but suppress html5ever's noisy "node with
        // weird namespace" warnings that fire on every malformed HTML
        // page the web_fetch readability extractor touches.
        "warn,html5ever=error,selectors=error"
    } else {
        "info"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .format_timestamp(None)
        .init();

    let cli = Cli::parse();

    let workgraph_dir = resolve_workgraph_dir(
        cli.dir.clone(),
        std::env::var_os("WG_DIR").map(PathBuf::from),
        std::env::current_dir().ok(),
        dirs::home_dir(),
    );

    // Auto-create the global fallback `~/.workgraph` for REPL-style
    // commands that should Just Work from any directory. Project-
    // scoped commands (wg add, wg list, etc.) still require an
    // existing workgraph dir and will error cleanly downstream if
    // one isn't found. This mirrors how `gh auth` can create
    // `~/.config/gh` on first use without requiring `gh init`.
    let is_repl_style = matches!(
        cli.command,
        Some(Commands::Nex { .. }) | Some(Commands::TuiNex { .. })
    );
    if is_repl_style
        && !workgraph_dir.exists()
        && let Some(home) = dirs::home_dir()
        && workgraph_dir == home.join(".workgraph")
    {
        if let Err(e) = std::fs::create_dir_all(&workgraph_dir) {
            eprintln!(
                "warning: failed to create global workgraph dir {}: {}",
                workgraph_dir.display(),
                e
            );
        } else {
            eprintln!(
                "\x1b[2m[wg] created global workgraph directory: {}\x1b[0m",
                workgraph_dir.display()
            );
        }
    }

    let workgraph_dir = workgraph_dir.canonicalize().unwrap_or(workgraph_dir);

    // Handle help flags (top-level custom help with usage-based ordering)
    if cli.help || cli.help_all || cli.command.is_none() {
        print_help(&workgraph_dir, cli.help_all, cli.alphabetical);
        return Ok(());
    }

    let command = match cli.command {
        Some(c) => c,
        None => return Ok(()),
    };

    // Warn if --json is passed to a command that doesn't support it
    if cli.json && !supports_json(&command) {
        eprintln!(
            "Warning: --json flag is not supported by 'wg {}' and will be ignored",
            command_name(&command)
        );
    }

    // Track command usage (fire-and-forget, ignores errors)
    workgraph::usage::append_usage_log(&workgraph_dir, command_name(&command));

    match command {
        Commands::Init { no_agency, global } => {
            // When --global is set, override the resolver's decision
            // and point init at `~/.workgraph` regardless of cwd.
            let target_dir = if global {
                match dirs::home_dir() {
                    Some(home) => home.join(".workgraph"),
                    None => {
                        anyhow::bail!(
                            "--global requires a resolvable home directory but HOME is not set"
                        );
                    }
                }
            } else {
                workgraph_dir.clone()
            };
            commands::init::run(&target_dir, no_agency)
        }
        Commands::Reset {
            seed,
            seeds,
            direction,
            also_strip_meta,
            dry_run,
            yes,
        } => {
            let dir_parsed: commands::reset::Direction =
                direction.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let mut all_seeds = vec![seed];
            all_seeds.extend(seeds);
            let opts = commands::reset::ResetOptions {
                direction: dir_parsed,
                also_strip_meta,
                dry_run,
                yes,
            };
            commands::reset::run(&workgraph_dir, &all_seeds, opts)?;
            Ok(())
        }
        Commands::Rescue {
            target,
            description,
            title,
            id,
            from_eval,
        } => {
            let actor = std::env::var("WG_ACTOR")
                .ok()
                .or_else(|| std::env::var("WG_AGENT_ID").ok());
            let new_id = commands::rescue::run(
                &workgraph_dir,
                &target,
                &description,
                title.as_deref(),
                id.as_deref(),
                from_eval.as_deref(),
                actor.as_deref(),
            )?;
            println!(
                "Rescue task '{}' created (supersedes '{}').",
                new_id, target
            );
            Ok(())
        }
        Commands::Insert {
            position,
            target,
            title,
            description,
            id,
            splice,
            replace_edges,
        } => {
            let pos: commands::insert::Position =
                position.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            let opts = commands::insert::InsertOptions {
                splice,
                replace_edges,
            };
            let new_id = commands::insert::run(
                &workgraph_dir,
                pos,
                &target,
                &title,
                description.as_deref(),
                id.as_deref(),
                opts,
            )?;
            println!("Inserted task '{}' ({:?} {}).", new_id, pos, target);
            Ok(())
        }
        Commands::Add {
            title,
            id,
            description,
            repo,
            after,
            assign,
            hours,
            cost,
            tag,
            skill,
            input,
            deliverable,
            max_retries,
            model,
            provider,
            verify,
            verify_timeout,
            max_iterations,
            cycle_guard,
            cycle_delay,
            no_converge,
            no_restart_on_failure,
            max_failure_restarts,
            exec,
            timeout,
            visibility,
            context_scope,
            exec_mode,
            paused,
            no_place,
            place_near,
            place_before,
            delay,
            not_before,
            allow_phantom,
            independent,
            propagation,
            retry_strategy,
            cron,
            subtask,
        } => {
            // Determine effective paused/unplaced state:
            // - --paused always pauses (user-managed draft, skips placement)
            // - --no-place: unplaced=true, paused=false (immediate dispatch)
            // - System tasks (dot-prefix): never draft, never placed
            // - Agent context (WG_TASK_ID set): default to --no-place behavior
            // - Default (interactive): paused=true (draft-by-default, needs placement)
            let is_system_task = title.starts_with('.');
            let is_agent_context =
                std::env::var("WG_TASK_ID").is_ok() || std::env::var("WG_AGENT_ID").is_ok();
            let effective_no_place = no_place || is_system_task || is_agent_context;
            let effective_paused = if paused {
                true
            } else if effective_no_place {
                false
            } else {
                // Draft by default for interactive use
                true
            };
            if let Some(ref peer_ref) = repo {
                commands::add::run_remote(
                    &workgraph_dir,
                    peer_ref,
                    &title,
                    id.as_deref(),
                    description.as_deref(),
                    &after,
                    &tag,
                    &skill,
                    &deliverable,
                    model.as_deref(),
                    provider.as_deref(),
                    verify.as_deref(),
                    verify_timeout.as_deref(),
                    cron.as_deref(),
                )
            } else {
                commands::add::run(
                    &workgraph_dir,
                    &title,
                    id.as_deref(),
                    description.as_deref(),
                    &after,
                    assign.as_deref(),
                    hours,
                    cost,
                    &tag,
                    &skill,
                    &input,
                    &deliverable,
                    max_retries,
                    model.as_deref(),
                    provider.as_deref(),
                    verify.as_deref(),
                    verify_timeout.as_deref(),
                    max_iterations,
                    cycle_guard.as_deref(),
                    cycle_delay.as_deref(),
                    no_converge,
                    no_restart_on_failure,
                    max_failure_restarts,
                    &visibility,
                    context_scope.as_deref(),
                    exec.as_deref(),
                    timeout.as_deref(),
                    exec_mode.as_deref(),
                    effective_paused,
                    effective_no_place,
                    &place_near,
                    &place_before,
                    delay.as_deref(),
                    not_before.as_deref(),
                    allow_phantom,
                    independent,
                    parse_iteration_config(propagation.as_deref(), retry_strategy.as_deref()),
                    None, // priority - not yet exposed in CLI
                    cron.as_deref(),
                    subtask,
                )
            }
        }
        Commands::Edit {
            id,
            title,
            description,
            add_after,
            remove_after,
            add_tag,
            remove_tag,
            model,
            provider,
            add_skill,
            remove_skill,
            max_iterations,
            cycle_guard,
            cycle_delay,
            no_converge,
            no_restart_on_failure,
            max_failure_restarts,
            visibility,
            context_scope,
            exec_mode,
            delay,
            not_before,
            verify,
            cron,
            allow_phantom,
            allow_cycle,
        } => commands::edit::run(
            &workgraph_dir,
            &id,
            title.as_deref(),
            description.as_deref(),
            &add_after,
            &remove_after,
            &add_tag,
            &remove_tag,
            model.as_deref(),
            provider.as_deref(),
            &add_skill,
            &remove_skill,
            max_iterations,
            cycle_guard.as_deref(),
            cycle_delay.as_deref(),
            no_converge,
            no_restart_on_failure,
            max_failure_restarts,
            visibility.as_deref(),
            context_scope.as_deref(),
            exec_mode.as_deref(),
            delay.as_deref(),
            not_before.as_deref(),
            verify.as_deref(),
            cron.as_deref(),
            allow_phantom,
            allow_cycle,
        ),
        Commands::Reprioritize { id, priority } => {
            commands::reprioritize::run(&workgraph_dir, &id, &priority)
        }
        Commands::Done {
            id,
            converged,
            skip_verify,
        } => commands::done::run(&workgraph_dir, &id, converged, skip_verify),
        Commands::Fail {
            id,
            reason,
            eval_reject,
        } => {
            if eval_reject {
                commands::fail::run_eval_reject(&workgraph_dir, &id, reason.as_deref())
            } else {
                commands::fail::run(&workgraph_dir, &id, reason.as_deref())
            }
        }
        Commands::Abandon {
            id,
            reason,
            superseded_by,
        } => commands::abandon::run(&workgraph_dir, &id, reason.as_deref(), &superseded_by),
        Commands::Retry { id } => commands::retry::run(&workgraph_dir, &id),
        Commands::Requeue { id, reason } => commands::requeue::run(&workgraph_dir, &id, &reason),
        Commands::Approve { id } => commands::approve::run(&workgraph_dir, &id),
        Commands::Reject { id, reason } => commands::reject::run(&workgraph_dir, &id, &reason),
        Commands::Claim { id, actor } => {
            commands::claim::claim(&workgraph_dir, &id, actor.as_deref())
        }
        Commands::Unclaim { id } => commands::claim::unclaim(&workgraph_dir, &id),
        Commands::Pause { id } => commands::pause::run(&workgraph_dir, &id),
        Commands::Resume { id, only } => commands::resume::run(&workgraph_dir, &id, only),
        Commands::Publish { id, only } => commands::resume::publish(&workgraph_dir, &id, only),
        Commands::Wait {
            id,
            until,
            checkpoint,
        } => commands::wait::run(&workgraph_dir, &id, &until, checkpoint.as_deref()),
        Commands::AddDep { task, dependency } => {
            commands::link::run_link(&workgraph_dir, &task, &dependency)
        }
        Commands::RmDep { task, dependency } => {
            commands::link::run_unlink(&workgraph_dir, &task, &dependency)
        }
        Commands::Reclaim { id, from, to } => {
            commands::reclaim::run(&workgraph_dir, &id, &from, &to)
        }
        Commands::Ready => commands::ready::run(&workgraph_dir, cli.json),
        Commands::Discover {
            since,
            with_artifacts,
        } => commands::discover::run(&workgraph_dir, Some(&since), with_artifacts, cli.json),
        Commands::Blocked { id } => commands::blocked::run(&workgraph_dir, &id, cli.json),
        Commands::WhyBlocked { id } => commands::why_blocked::run(&workgraph_dir, &id, cli.json),
        Commands::Check => commands::check::run(&workgraph_dir, cli.json),
        Commands::Doctor => commands::doctor::run(&workgraph_dir, cli.json),
        Commands::Cleanup { subcmd } => {
            let args = commands::cleanup::CleanupArgs { subcmd };
            commands::cleanup::run(args)
        }
        Commands::Cycles => commands::cycles::run(&workgraph_dir, cli.json),
        Commands::List {
            status,
            paused,
            tags,
            cron,
        } => commands::list::run(
            &workgraph_dir,
            status.as_deref(),
            paused,
            &tags,
            None,
            cron,
            cli.json,
        ),
        Commands::Viz {
            focus,
            all,
            status,
            critical_path,
            dot,
            mermaid,
            graph,
            output,
            show_internal,
            tui: tui_mode,
            no_tui: _no_tui,
            no_mouse,
            layout,
            tags,
            edge_color,
            columns,
        } => {
            let layout_mode: commands::viz::LayoutMode = layout.parse().unwrap_or_default();
            let _explicit_static_format = dot || mermaid || graph || output.is_some();
            let use_tui = tui_mode;

            // Resolve edge color: CLI flag > config > default ("gray")
            let resolved_edge_color = edge_color
                .unwrap_or_else(|| Config::load_or_default(&workgraph_dir).viz.edge_color);

            // Resolve max columns: --columns flag > terminal width > None
            let max_columns =
                columns.or_else(|| crossterm::terminal::size().ok().map(|(cols, _)| cols));

            if use_tui {
                let options = commands::viz::VizOptions {
                    all,
                    status,
                    critical_path,
                    format: commands::viz::OutputFormat::Ascii,
                    output: None,
                    show_internal,
                    show_internal_running_only: false,
                    focus,
                    tui_mode: true,
                    layout: layout_mode,
                    tags: tags.clone(),
                    edge_color: resolved_edge_color,
                    max_columns: None, // TUI handles its own sizing
                };
                let mouse_override = if no_mouse { Some(false) } else { None };
                tui::viz_viewer::run(
                    workgraph_dir,
                    options,
                    mouse_override,
                    false,
                    None,
                    false,
                    None,
                    false,
                )
            } else {
                let fmt = if dot {
                    commands::viz::OutputFormat::Dot
                } else if mermaid {
                    commands::viz::OutputFormat::Mermaid
                } else if graph {
                    commands::viz::OutputFormat::Graph
                } else {
                    commands::viz::OutputFormat::Ascii
                };
                let options = commands::viz::VizOptions {
                    all,
                    status,
                    critical_path,
                    format: fmt,
                    output,
                    show_internal,
                    show_internal_running_only: false,
                    focus,
                    tui_mode: false,
                    layout: layout_mode,
                    tags,
                    edge_color: resolved_edge_color,
                    max_columns,
                };
                commands::viz::run(&workgraph_dir, &options)
            }
        }
        Commands::GraphExport {
            archive,
            since,
            until,
        } => commands::graph::run(&workgraph_dir, archive, since.as_deref(), until.as_deref()),
        Commands::Cost { id } => commands::cost::run(&workgraph_dir, &id, cli.json),
        Commands::Coordinate { max_parallel } => {
            commands::coordinate::run(&workgraph_dir, cli.json, max_parallel)
        }
        Commands::Plan { budget, hours } => {
            commands::plan::run(&workgraph_dir, budget, hours, cli.json)
        }
        Commands::Reschedule { id, after, at } => {
            commands::reschedule::run(&workgraph_dir, &id, after, at.as_deref())
        }
        Commands::Impact { id } => commands::impact::run(&workgraph_dir, &id, cli.json),
        Commands::Structure => commands::structure::run(&workgraph_dir, cli.json),
        Commands::Bottlenecks => commands::bottlenecks::run(&workgraph_dir, cli.json),
        Commands::Velocity { weeks } => commands::velocity::run(&workgraph_dir, cli.json, weeks),
        Commands::Aging => commands::aging::run(&workgraph_dir, cli.json),
        Commands::Forecast => commands::forecast::run(&workgraph_dir, cli.json),
        Commands::Workload => commands::workload::run(&workgraph_dir, cli.json),
        Commands::Worktree(sub) => match sub {
            cli::WorktreeCommand::List => commands::worktree_cmd::list(&workgraph_dir),
            cli::WorktreeCommand::Archive { agent_id, remove } => {
                commands::worktree_cmd::archive(&workgraph_dir, &agent_id, remove)
            }
            cli::WorktreeCommand::Gc {
                execute,
                older,
                dead_only,
            } => commands::worktree_cmd::gc(&workgraph_dir, execute, older.as_deref(), dead_only),
        },
        Commands::Resources => commands::resources::run(&workgraph_dir, cli.json),
        Commands::CriticalPath => commands::critical_path::run(&workgraph_dir, cli.json),
        Commands::Analyze => commands::analyze::run(&workgraph_dir, cli.json),
        Commands::Archive {
            dry_run,
            older,
            list,
            yes,
            undo,
            ids,
            command,
        } => match command {
            Some(cli::ArchiveCommands::Search { query, limit }) => {
                commands::archive::search(&workgraph_dir, &query, limit, cli.json)
            }
            Some(cli::ArchiveCommands::Restore { task_id, reopen }) => {
                commands::archive::restore(&workgraph_dir, &task_id, reopen)
            }
            None => {
                if undo {
                    commands::archive::undo(&workgraph_dir)
                } else {
                    commands::archive::run(
                        &workgraph_dir,
                        dry_run,
                        older.as_deref(),
                        list,
                        yes,
                        &ids,
                        cli.json,
                    )
                }
            }
        },
        Commands::Gc {
            dry_run,
            include_done,
            older,
        } => commands::gc::run(&workgraph_dir, dry_run, include_done, older.as_deref()),
        Commands::Show { id } => commands::show::run(&workgraph_dir, &id, cli.json),
        Commands::Trace { command } => match command {
            TraceCommands::Show {
                id,
                full,
                ops_only,
                recursive,
                timeline,
                graph,
                animate,
                speed,
            } => {
                if animate {
                    commands::trace_animate::run(&workgraph_dir, &id, speed)
                } else if graph {
                    commands::trace::run_graph(&workgraph_dir, &id)
                } else if recursive || timeline {
                    commands::trace::run_recursive(&workgraph_dir, &id, timeline, cli.json)
                } else {
                    let mode = if cli.json {
                        commands::trace::TraceMode::Json
                    } else if full {
                        commands::trace::TraceMode::Full
                    } else if ops_only {
                        commands::trace::TraceMode::OpsOnly
                    } else {
                        commands::trace::TraceMode::Summary
                    };
                    commands::trace::run(&workgraph_dir, &id, mode)
                }
            }
            TraceCommands::Export {
                root,
                visibility,
                output,
            } => commands::trace_export::run(
                &workgraph_dir,
                root.as_deref(),
                &visibility,
                output.as_deref(),
                cli.json,
            ),
            TraceCommands::Import {
                file,
                source,
                dry_run,
            } => commands::trace_import::run(
                &workgraph_dir,
                &file,
                source.as_deref(),
                dry_run,
                cli.json,
            ),
            // Hidden aliases: print deprecation warning then delegate
            TraceCommands::ExtractAlias {
                task_ids,
                name,
                subgraph,
                recursive,
                generalize,
                generative,
                output,
                force,
                include_evaluations,
            } => {
                eprintln!(
                    "Warning: 'wg trace extract' is deprecated. Use 'wg func extract' instead."
                );
                if generative {
                    commands::func_extract::run_generative(
                        &workgraph_dir,
                        &task_ids,
                        name.as_deref(),
                        output.as_deref(),
                        force,
                        include_evaluations,
                    )
                } else {
                    commands::func_extract::run(
                        &workgraph_dir,
                        &task_ids[0],
                        name.as_deref(),
                        subgraph || recursive,
                        generalize,
                        output.as_deref(),
                        force,
                        include_evaluations,
                    )
                }
            }
            TraceCommands::InstantiateAlias {
                function_id,
                from,
                inputs,
                input_file,
                prefix,
                dry_run,
                after,
                model,
            } => {
                eprintln!(
                    "Warning: 'wg trace instantiate' is deprecated. Use 'wg func apply' instead."
                );
                commands::func_apply::run(
                    &workgraph_dir,
                    &function_id,
                    from.as_deref(),
                    &inputs,
                    input_file.as_deref(),
                    prefix.as_deref(),
                    dry_run,
                    &after,
                    model.as_deref(),
                    cli.json,
                )
            }
            TraceCommands::ListFunctionsAlias {
                verbose,
                include_peers,
                visibility,
            } => {
                eprintln!(
                    "Warning: 'wg trace list-functions' is deprecated. Use 'wg func list' instead."
                );
                commands::func_cmd::run_list(
                    &workgraph_dir,
                    cli.json,
                    verbose,
                    include_peers,
                    visibility.as_deref(),
                )
            }
            TraceCommands::ShowFunctionAlias { id } => {
                eprintln!(
                    "Warning: 'wg trace show-function' is deprecated. Use 'wg func show' instead."
                );
                commands::func_cmd::run_show(&workgraph_dir, &id, cli.json)
            }
            TraceCommands::BootstrapAlias { force } => {
                eprintln!(
                    "Warning: 'wg trace bootstrap' is deprecated. Use 'wg func bootstrap' instead."
                );
                commands::func_bootstrap::run(&workgraph_dir, force)
            }
            TraceCommands::MakeAdaptiveAlias {
                function_id,
                max_runs,
            } => {
                eprintln!(
                    "Warning: 'wg trace make-adaptive' is deprecated. Use 'wg func make-adaptive' instead."
                );
                commands::func_make_adaptive::run(&workgraph_dir, &function_id, max_runs)
            }
        },
        Commands::Func { command } => match command {
            FuncCommands::List {
                verbose,
                include_peers,
                visibility,
            } => commands::func_cmd::run_list(
                &workgraph_dir,
                cli.json,
                verbose,
                include_peers,
                visibility.as_deref(),
            ),
            FuncCommands::Show { id } => {
                commands::func_cmd::run_show(&workgraph_dir, &id, cli.json)
            }
            FuncCommands::Extract {
                task_ids,
                name,
                subgraph,
                recursive,
                generalize,
                generative,
                output,
                force,
                include_evaluations,
            } => {
                if generative {
                    commands::func_extract::run_generative(
                        &workgraph_dir,
                        &task_ids,
                        name.as_deref(),
                        output.as_deref(),
                        force,
                        include_evaluations,
                    )
                } else {
                    commands::func_extract::run(
                        &workgraph_dir,
                        &task_ids[0],
                        name.as_deref(),
                        subgraph || recursive,
                        generalize,
                        output.as_deref(),
                        force,
                        include_evaluations,
                    )
                }
            }
            FuncCommands::Apply {
                function_id,
                from,
                inputs,
                input_file,
                prefix,
                dry_run,
                after,
                model,
            } => commands::func_apply::run(
                &workgraph_dir,
                &function_id,
                from.as_deref(),
                &inputs,
                input_file.as_deref(),
                prefix.as_deref(),
                dry_run,
                &after,
                model.as_deref(),
                cli.json,
            ),
            FuncCommands::Bootstrap { force } => {
                commands::func_bootstrap::run(&workgraph_dir, force)
            }
            FuncCommands::MakeAdaptive {
                function_id,
                max_runs,
            } => commands::func_make_adaptive::run(&workgraph_dir, &function_id, max_runs),
        },
        Commands::Replay {
            model,
            failed_only,
            below_score,
            tasks,
            keep_done,
            plan_only,
            subgraph,
        } => {
            let opts = commands::replay::ReplayOptions {
                model,
                failed_only,
                below_score,
                tasks,
                keep_done,
                plan_only,
                subgraph,
            };
            commands::replay::run(&workgraph_dir, &opts, cli.json)
        }
        Commands::Runs { command } => match command {
            RunsCommands::List => commands::runs_cmd::run_list(&workgraph_dir, cli.json),
            RunsCommands::Show { id } => {
                commands::runs_cmd::run_show(&workgraph_dir, &id, cli.json)
            }
            RunsCommands::Restore { id } => {
                commands::runs_cmd::run_restore(&workgraph_dir, &id, cli.json)
            }
            RunsCommands::Diff { id } => {
                commands::runs_cmd::run_diff(&workgraph_dir, &id, cli.json)
            }
        },
        Commands::Log {
            id,
            message,
            actor,
            list,
            agent,
            operations,
        } => {
            if operations {
                commands::log::run_operations(&workgraph_dir, cli.json)
            } else {
                let id = id.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Task ID is required (use --operations to view the operations log)"
                    )
                })?;
                if agent {
                    commands::log::run_agent(&workgraph_dir, id, cli.json)
                } else if let (false, Some(msg)) = (list, &message) {
                    let agent_id = std::env::var("WG_AGENT_ID").ok();
                    commands::log::run_add(
                        &workgraph_dir,
                        id,
                        msg,
                        actor.as_deref(),
                        agent_id.as_deref(),
                    )
                } else {
                    commands::log::run_list(&workgraph_dir, id, cli.json)
                }
            }
        }
        Commands::Tokens { id, json } => commands::tokens::run(&workgraph_dir, &id, &json),
        Commands::Spend { today, json } => commands::spend::run(&workgraph_dir, today, json),
        Commands::Msg { command } => {
            let agent_id_from_env = std::env::var("WG_AGENT_ID").ok();
            match command {
                MsgCommands::Send {
                    task_id,
                    message,
                    from,
                    priority,
                    stdin,
                } => {
                    // Auto-detect sender: if --from is default "user" and WG_TASK_ID
                    // is set, use the task ID (slug) as the sender identity so agents
                    // identify by their task name rather than a generic "user" label.
                    let sender = if from == "user" {
                        std::env::var("WG_TASK_ID").unwrap_or(from)
                    } else {
                        from
                    };
                    commands::msg::run_send(
                        &workgraph_dir,
                        &task_id,
                        message.as_deref(),
                        &sender,
                        &priority,
                        stdin,
                    )
                }
                MsgCommands::List { task_id } => {
                    commands::msg::run_list(&workgraph_dir, &task_id, cli.json)
                }
                MsgCommands::Read { task_id, agent } => {
                    let agent_id = agent
                        .or(agent_id_from_env)
                        .unwrap_or_else(|| "user".to_string());
                    commands::msg::run_read(&workgraph_dir, &task_id, &agent_id, cli.json)
                }
                MsgCommands::Poll { task_id, agent } => {
                    let agent_id = agent
                        .or(agent_id_from_env)
                        .unwrap_or_else(|| "user".to_string());
                    let has_messages =
                        commands::msg::run_poll(&workgraph_dir, &task_id, &agent_id, cli.json)?;
                    if !has_messages {
                        std::process::exit(1);
                    }
                    Ok(())
                }
            }
        }
        Commands::User { command } => match command {
            UserCommands::Init { name } => {
                commands::user::run_init(&workgraph_dir, name.as_deref())
            }
            UserCommands::List => commands::user::run_list(&workgraph_dir, cli.json),
            UserCommands::Archive { name } => {
                commands::user::run_archive(&workgraph_dir, name.as_deref())
            }
        },
        Commands::Chat {
            message,
            interactive,
            history,
            clear,
            timeout,
            attachment,
            coordinator,
            history_depth,
            no_history,
            rotate,
            cleanup,
            compact,
            share_from,
        } => {
            if let Some(from_id) = share_from {
                commands::chat::run_share(&workgraph_dir, from_id, coordinator)
            } else if compact {
                commands::chat::run_compact(&workgraph_dir, coordinator, cli.json)
            } else if rotate {
                commands::chat::run_rotate(&workgraph_dir, coordinator)
            } else if cleanup {
                commands::chat::run_cleanup(&workgraph_dir, coordinator)
            } else if clear {
                commands::chat::run_clear(&workgraph_dir, coordinator)
            } else if history {
                commands::chat::run_history(&workgraph_dir, cli.json, coordinator, history_depth)
            } else if interactive {
                commands::chat::run_interactive(&workgraph_dir, timeout, coordinator)
            } else if let Some(msg) = message {
                let _ = no_history; // no_history only affects display, not send
                commands::chat::run_send(&workgraph_dir, &msg, timeout, &attachment, coordinator)
            } else {
                // No message and no flags → default to interactive
                let _ = no_history;
                commands::chat::run_interactive(&workgraph_dir, timeout, coordinator)
            }
        }
        Commands::Resource { command } => match command {
            ResourceCommands::Add {
                id,
                name,
                resource_type,
                available,
                unit,
            } => commands::resource::run_add(
                &workgraph_dir,
                &id,
                name.as_deref(),
                resource_type.as_deref(),
                available,
                unit.as_deref(),
            ),
            ResourceCommands::List => commands::resource::run_list(&workgraph_dir, cli.json),
        },
        Commands::Skill { command } => match command {
            SkillCommands::List => commands::skills::run_list(&workgraph_dir, cli.json),
            SkillCommands::Task { id } => commands::skills::run_task(&workgraph_dir, &id, cli.json),
            SkillCommands::Find { skill } => {
                commands::skills::run_find(&workgraph_dir, &skill, cli.json)
            }
            SkillCommands::Install => commands::skills::run_install(),
        },
        Commands::Agency { command } => match command {
            AgencyCommands::Init => commands::agency_init::run(&workgraph_dir),
            AgencyCommands::Migrate { dry_run } => {
                commands::agency_migrate::run(&workgraph_dir, dry_run)
            }
            AgencyCommands::Stats {
                min_evals,
                by_model,
                by_task_type,
            } => commands::agency_stats::run(
                &workgraph_dir,
                cli.json,
                min_evals,
                by_model,
                by_task_type,
            ),
            AgencyCommands::Scan { root, max_depth } => {
                let root_path = std::path::PathBuf::from(&root);
                commands::agency_scan::run(&root_path, cli.json, max_depth)
            }
            AgencyCommands::Pull {
                source,
                entity_ids,
                entity_type,
                dry_run,
                no_performance,
                no_evaluations,
                force,
                global,
            } => {
                let opts = commands::agency_pull::PullOptions {
                    source,
                    dry_run,
                    no_performance,
                    no_evaluations,
                    force,
                    global,
                    entity_ids,
                    entity_type,
                    json: cli.json,
                };
                commands::agency_pull::run(&workgraph_dir, &opts)
            }
            AgencyCommands::Merge {
                sources,
                into,
                dry_run,
            } => {
                let opts = commands::agency_merge::MergeOptions {
                    sources,
                    into,
                    dry_run,
                    json: cli.json,
                };
                commands::agency_merge::run(&workgraph_dir, &opts)
            }
            AgencyCommands::Remote { command } => match command {
                RemoteCommands::Add {
                    name,
                    path,
                    description,
                } => commands::agency_remote::run_add(
                    &workgraph_dir,
                    &name,
                    &path,
                    description.as_deref(),
                ),
                RemoteCommands::Remove { name } => {
                    commands::agency_remote::run_remove(&workgraph_dir, &name)
                }
                RemoteCommands::List => commands::agency_remote::run_list(&workgraph_dir, cli.json),
                RemoteCommands::Show { name } => {
                    commands::agency_remote::run_show(&workgraph_dir, &name, cli.json)
                }
            },
            AgencyCommands::Create { model, dry_run } => {
                commands::agency_create::run(&workgraph_dir, model.as_deref(), dry_run, cli.json)
            }
            AgencyCommands::Deferred => {
                commands::evolve::run_deferred_list(&workgraph_dir, cli.json)
            }
            AgencyCommands::Approve { id, note } => {
                commands::evolve::run_deferred_approve(&workgraph_dir, &id, note.as_deref())
            }
            AgencyCommands::Reject { id, note } => {
                commands::evolve::run_deferred_reject(&workgraph_dir, &id, note.as_deref())
            }
            AgencyCommands::Import {
                csv_path,
                url,
                upstream,
                dry_run,
                tag,
                force,
                check,
            } => {
                let opts = commands::agency_import::ImportOptions {
                    csv_path,
                    url,
                    upstream,
                    dry_run,
                    tag,
                    force,
                    check,
                };
                commands::agency_import::run_import(&workgraph_dir, opts).map(|_| ())
            }
            AgencyCommands::Push {
                target,
                entity_ids,
                entity_type,
                dry_run,
                no_performance,
                no_evaluations,
                force,
                global,
            } => commands::agency_push::run(
                &workgraph_dir,
                &commands::agency_push::PushOptions {
                    target: &target,
                    dry_run,
                    no_performance,
                    no_evaluations,
                    force,
                    global,
                    entity_ids: &entity_ids,
                    entity_type: entity_type.as_deref(),
                    json: cli.json,
                },
            ),
        },
        Commands::Peer { command } => match command {
            PeerCommands::Add {
                name,
                path,
                description,
            } => commands::peer::run_add(&workgraph_dir, &name, &path, description.as_deref()),
            PeerCommands::Remove { name } => commands::peer::run_remove(&workgraph_dir, &name),
            PeerCommands::List => commands::peer::run_list(&workgraph_dir, cli.json),
            PeerCommands::Show { name } => {
                commands::peer::run_show(&workgraph_dir, &name, cli.json)
            }
            PeerCommands::Status => commands::peer::run_status(&workgraph_dir, cli.json),
        },
        Commands::Role { command } => match command {
            RoleCommands::Add {
                name,
                outcome,
                skill,
                description,
            } => commands::role::run_add(
                &workgraph_dir,
                &name,
                &outcome,
                &skill,
                description.as_deref(),
            ),
            RoleCommands::List => commands::role::run_list(&workgraph_dir, cli.json),
            RoleCommands::Show { id } => commands::role::run_show(&workgraph_dir, &id, cli.json),
            RoleCommands::Edit { id } => commands::role::run_edit(&workgraph_dir, &id),
            RoleCommands::Rm { id } => commands::role::run_rm(&workgraph_dir, &id),
            RoleCommands::Lineage { id } => {
                commands::role::run_lineage(&workgraph_dir, &id, cli.json)
            }
        },
        Commands::Tradeoff { command } => match command {
            TradeoffCommands::Add {
                name,
                accept,
                reject,
                description,
            } => commands::tradeoff::run_add(
                &workgraph_dir,
                &name,
                &accept,
                &reject,
                description.as_deref(),
            ),
            TradeoffCommands::List => commands::tradeoff::run_list(&workgraph_dir, cli.json),
            TradeoffCommands::Show { id } => {
                commands::tradeoff::run_show(&workgraph_dir, &id, cli.json)
            }
            TradeoffCommands::Edit { id } => commands::tradeoff::run_edit(&workgraph_dir, &id),
            TradeoffCommands::Rm { id } => commands::tradeoff::run_rm(&workgraph_dir, &id),
            TradeoffCommands::Lineage { id } => {
                commands::tradeoff::run_lineage(&workgraph_dir, &id, cli.json)
            }
        },
        Commands::Assign {
            task,
            agent_hash,
            clear,
            auto,
        } => commands::assign::run(&workgraph_dir, &task, agent_hash.as_deref(), clear, auto),
        Commands::Match { task } => commands::match_cmd::run(&workgraph_dir, &task, cli.json),
        Commands::Heartbeat {
            agent,
            check,
            threshold,
            ..
        } => {
            if let (false, Some(a)) = (check, &agent) {
                commands::heartbeat::run_auto(&workgraph_dir, a)
            } else {
                commands::heartbeat::run_check_agents(&workgraph_dir, threshold, cli.json)
            }
        }
        Commands::Checkpoint {
            task,
            summary,
            agent,
            files,
            stream_offset,
            turn_count,
            token_input,
            token_output,
            checkpoint_type,
            list,
        } => {
            if list {
                let agent_id = agent
                    .or_else(|| std::env::var("WG_AGENT_ID").ok())
                    .ok_or_else(|| anyhow::anyhow!("--agent or WG_AGENT_ID required for --list"))?;
                commands::checkpoint::run_list(&workgraph_dir, &agent_id, Some(&task), cli.json)
            } else {
                let cp_type = match checkpoint_type.as_str() {
                    "auto" => commands::checkpoint::CheckpointType::Auto,
                    _ => commands::checkpoint::CheckpointType::Explicit,
                };
                commands::checkpoint::run(
                    &workgraph_dir,
                    &task,
                    &summary,
                    agent.as_deref(),
                    &files,
                    stream_offset,
                    turn_count,
                    token_input,
                    token_output,
                    cp_type,
                    cli.json,
                )
            }
        }
        Commands::Compact => commands::compact::run(&workgraph_dir, cli.json),
        Commands::Session { command } => commands::chat_session::run(&workgraph_dir, command),
        Commands::Artifact { task, path, remove } => {
            if let Some(artifact_path) = path {
                if remove {
                    commands::artifact::run_remove(&workgraph_dir, &task, &artifact_path)
                } else {
                    commands::artifact::run_add(&workgraph_dir, &task, &artifact_path)
                }
            } else {
                commands::artifact::run_list(&workgraph_dir, &task, cli.json)
            }
        }
        Commands::Context { task, dependents } => {
            if dependents {
                commands::context::run_dependents(&workgraph_dir, &task, cli.json)
            } else {
                commands::context::run(&workgraph_dir, &task, cli.json)
            }
        }
        Commands::Next { actor } => commands::next::run(&workgraph_dir, &actor, cli.json),
        Commands::Trajectory { task, actor } => {
            if let Some(actor_id) = actor {
                commands::trajectory::suggest_for_actor(&workgraph_dir, &actor_id, cli.json)
            } else {
                commands::trajectory::run(&workgraph_dir, &task, cli.json)
            }
        }
        Commands::Exec {
            task,
            actor,
            dry_run,
            set,
            clear,
            shell,
            worktree,
            no_worktree: _,
            model,
        } => {
            if let Some(cmd) = set {
                commands::exec::set_exec(&workgraph_dir, &task, &cmd)
            } else if clear {
                commands::exec::clear_exec(&workgraph_dir, &task)
            } else if shell {
                commands::exec::run(&workgraph_dir, &task, actor.as_deref(), dry_run)
            } else {
                commands::exec::run_interactive(
                    &workgraph_dir,
                    &task,
                    actor.as_deref(),
                    dry_run,
                    worktree,
                    model.as_deref(),
                )
            }
        }
        Commands::Agent { command } => match command {
            AgentCommands::Create {
                name,
                role,
                tradeoff,
                capabilities,
                rate,
                capacity,
                trust_level,
                contact,
                executor,
                model,
                provider,
            } => commands::agent_crud::run_create(
                &workgraph_dir,
                &name,
                role.as_deref(),
                tradeoff.as_deref(),
                &capabilities,
                rate,
                capacity,
                trust_level.as_deref(),
                contact.as_deref(),
                &executor,
                model.as_deref(),
                provider.as_deref(),
            ),
            AgentCommands::List => commands::agent_crud::run_list(&workgraph_dir, cli.json),
            AgentCommands::Show { id } => {
                commands::agent_crud::run_show(&workgraph_dir, &id, cli.json)
            }
            AgentCommands::Rm { id } => commands::agent_crud::run_rm(&workgraph_dir, &id),
            AgentCommands::Lineage { id } => {
                commands::agent_crud::run_lineage(&workgraph_dir, &id, cli.json)
            }
            AgentCommands::Performance { id } => {
                commands::agent_crud::run_performance(&workgraph_dir, &id, cli.json)
            }
            AgentCommands::Run {
                actor,
                once,
                interval,
                max_tasks,
                reset_state,
            } => commands::agent::run(
                &workgraph_dir,
                &actor,
                once,
                interval,
                max_tasks,
                reset_state,
                cli.json,
            ),
        },
        Commands::Spawn {
            task,
            executor,
            timeout,
            model,
        } => commands::spawn::run(
            &workgraph_dir,
            &task,
            &executor,
            timeout.as_deref(),
            model.as_deref(),
            cli.json,
        ),
        Commands::Evaluate { command } => match command {
            EvaluateCommands::Run {
                task,
                evaluator_model,
                dry_run,
                flip,
            } => {
                if flip {
                    commands::evaluate::run_flip(
                        &workgraph_dir,
                        &task,
                        evaluator_model.as_deref(),
                        dry_run,
                        cli.json,
                    )
                } else {
                    commands::evaluate::run(
                        &workgraph_dir,
                        &task,
                        evaluator_model.as_deref(),
                        dry_run,
                        cli.json,
                    )
                }
            }
            EvaluateCommands::Record {
                task,
                score,
                source,
                notes,
                dimensions,
            } => commands::evaluate::run_record(
                &workgraph_dir,
                &task,
                score,
                &source,
                notes.as_deref(),
                &dimensions,
                cli.json,
            ),
            EvaluateCommands::Show {
                task_detail,
                task,
                agent,
                source,
                limit,
            } => commands::evaluate::run_show(
                &workgraph_dir,
                task.as_deref(),
                agent.as_deref(),
                source.as_deref(),
                limit,
                cli.json,
                task_detail.as_deref(),
            ),
        },
        Commands::Watch {
            event_types,
            task,
            replay,
        } => commands::watch::run(&workgraph_dir, &event_types, task.as_deref(), replay),
        Commands::Evolve { command } => match command {
            EvolveCommands::Run {
                dry_run,
                strategy,
                budget,
                model,
                autopoietic,
                max_iterations,
                cycle_delay,
                force_fanout,
                single_shot,
            } => commands::evolve::run(
                &workgraph_dir,
                dry_run,
                strategy.as_deref(),
                budget,
                model.as_deref(),
                cli.json,
                autopoietic,
                max_iterations,
                cycle_delay,
                force_fanout,
                single_shot,
            ),
            EvolveCommands::Apply {
                synthesis_file,
                output,
            } => {
                let output_path = output.unwrap_or_else(|| {
                    // Default: place apply-results.json next to synthesis-result.json
                    synthesis_file
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join("apply-results.json")
                });
                commands::evolve::run_apply_synthesis(&workgraph_dir, &synthesis_file, &output_path)
            }
            EvolveCommands::Review {
                command: review_cmd,
            } => match review_cmd {
                EvolveReviewCommands::List => {
                    commands::evolve::run_deferred_list(&workgraph_dir, cli.json)
                }
                EvolveReviewCommands::Approve { id, note } => {
                    commands::evolve::run_deferred_approve(&workgraph_dir, &id, note.as_deref())
                }
                EvolveReviewCommands::Reject { id, note } => {
                    commands::evolve::run_deferred_reject(&workgraph_dir, &id, note.as_deref())
                }
            },
        },
        Commands::Profile { command } => match command {
            ProfileCommands::Set {
                name,
                fast,
                standard,
                premium,
            } => commands::profile_cmd::set(
                &workgraph_dir,
                &name,
                fast.as_deref(),
                standard.as_deref(),
                premium.as_deref(),
            ),
            ProfileCommands::Show { verbose } => {
                commands::profile_cmd::show(&workgraph_dir, cli.json, verbose)
            }
            ProfileCommands::List => commands::profile_cmd::list(&workgraph_dir, cli.json),
            ProfileCommands::Refresh => commands::profile_cmd::refresh(&workgraph_dir),
        },
        Commands::Config {
            show,
            init,
            global,
            local,
            list,
            executor,
            model,
            set_interval,
            max_agents,
            coordinator_interval,
            poll_interval,
            heartbeat_interval,
            coordinator_executor,
            coordinator_model,
            coordinator_provider,
            matrix,
            homeserver,
            username,
            password,
            access_token,
            room,
            auto_evaluate,
            auto_assign,
            assigner_agent,
            evaluator_agent,
            evolver_agent,
            creator_agent,
            retention_heuristics,
            auto_triage,
            auto_place,
            auto_create,
            triage_timeout,
            triage_max_log_bytes,
            max_child_tasks,
            max_task_depth,
            viz_edge_color,
            eval_gate_threshold,
            eval_gate_all,
            flip_enabled,
            flip_inference_model,
            flip_comparison_model,
            flip_model,
            flip_verification_threshold,
            chat_history,
            chat_history_max,
            tui_counters,
            show_registry,
            registry_add,
            registry_remove,
            show_tiers,
            set_tier,
            reg_id,
            reg_provider,
            reg_model,
            reg_tier,
            reg_endpoint,
            reg_context_window,
            cost_input,
            cost_output,
            show_models,
            set_model,
            set_provider,
            set_endpoint,
            role_model,
            role_provider,
            retry_context_tokens,
            set_key,
            key_file,
            check_key,
            install_global,
            force,
            max_coordinators,
        } => {
            // Derive scope from --global/--local flags
            let scope = if global {
                Some(commands::config_cmd::ConfigScope::Global)
            } else if local {
                Some(commands::config_cmd::ConfigScope::Local)
            } else {
                None
            };

            // Handle --set-key <provider> --file <path>
            if let Some(ref provider) = set_key {
                let file = key_file
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--set-key requires --file <path>"))?;
                let write_scope = scope.unwrap_or(commands::config_cmd::ConfigScope::Local);
                return commands::config_cmd::set_key(&workgraph_dir, write_scope, provider, file);
            }

            // Handle --check-key
            if check_key {
                return commands::config_cmd::check_key(&workgraph_dir, cli.json);
            }

            // Handle --install-global
            if install_global {
                return commands::config_cmd::install_global(&workgraph_dir, force);
            }

            // Handle --registry (list)
            if show_registry {
                return commands::config_cmd::show_registry(&workgraph_dir, cli.json);
            }

            // Handle --registry-add
            if registry_add {
                let id = reg_id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--registry-add requires --id <ID>"))?;
                let provider = reg_provider.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("--registry-add requires --provider <PROVIDER>")
                })?;
                let model_name = reg_model.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("--registry-add requires --reg-model <MODEL>")
                })?;
                let tier = reg_tier
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--registry-add requires --reg-tier <TIER>"))?;
                let write_scope = scope.unwrap_or(commands::config_cmd::ConfigScope::Local);
                return commands::config_cmd::add_registry_entry(
                    &workgraph_dir,
                    write_scope,
                    id,
                    provider,
                    model_name,
                    tier,
                    reg_endpoint.as_deref(),
                    reg_context_window,
                    cost_input,
                    cost_output,
                );
            }

            // Handle --registry-remove
            if let Some(ref id) = registry_remove {
                let write_scope = scope.unwrap_or(commands::config_cmd::ConfigScope::Local);
                return commands::config_cmd::remove_registry_entry(
                    &workgraph_dir,
                    write_scope,
                    id,
                    force,
                    cli.json,
                );
            }

            // Handle --tiers (show)
            if show_tiers {
                return commands::config_cmd::show_tiers(&workgraph_dir, cli.json);
            }

            // Handle --tier <tier>=<model-id>
            if let Some(ref tier_spec) = set_tier {
                let write_scope = scope.unwrap_or(commands::config_cmd::ConfigScope::Local);
                return commands::config_cmd::set_tier(&workgraph_dir, write_scope, tier_spec);
            }

            // Handle Matrix configuration
            if matrix
                || homeserver.is_some()
                || username.is_some()
                || password.is_some()
                || access_token.is_some()
                || room.is_some()
            {
                let has_matrix_updates = homeserver.is_some()
                    || username.is_some()
                    || password.is_some()
                    || access_token.is_some()
                    || room.is_some();

                if has_matrix_updates {
                    commands::config_cmd::update_matrix(
                        homeserver.as_deref(),
                        username.as_deref(),
                        password.as_deref(),
                        access_token.as_deref(),
                        room.as_deref(),
                    )
                } else {
                    commands::config_cmd::show_matrix(cli.json)
                }
            } else if show_models {
                commands::config_cmd::show_model_routing(&workgraph_dir, cli.json)
            } else if set_model.is_some()
                || set_provider.is_some()
                || set_endpoint.is_some()
                || role_model.is_some()
                || role_provider.is_some()
                || flip_inference_model.is_some()
                || flip_comparison_model.is_some()
                || flip_model.is_some()
            {
                // Merge --role-model/--role-provider (key=value) into set_model/set_provider format
                let effective_model = if let Some(ref kv) = role_model {
                    let parts: Vec<&str> = kv.splitn(2, '=').collect();
                    if parts.len() != 2 {
                        anyhow::bail!(
                            "--role-model requires format <role>=<model>, got \"{}\"",
                            kv
                        );
                    }
                    Some(vec![parts[0].to_string(), parts[1].to_string()])
                } else {
                    set_model
                };
                let effective_provider = if let Some(ref kv) = role_provider {
                    let parts: Vec<&str> = kv.splitn(2, '=').collect();
                    if parts.len() != 2 {
                        anyhow::bail!(
                            "--role-provider requires format <role>=<provider>, got \"{}\"",
                            kv
                        );
                    }
                    Some(vec![parts[0].to_string(), parts[1].to_string()])
                } else {
                    set_provider
                };
                // Default scope for writes = Local
                let write_scope = scope.unwrap_or(commands::config_cmd::ConfigScope::Local);

                // Handle --flip-model / --flip-inference-model / --flip-comparison-model
                // These are shorthand for --set-model flip_inference/flip_comparison <model>
                let flip_inf = flip_inference_model.or_else(|| flip_model.clone());
                let flip_cmp = flip_comparison_model.or(flip_model);
                if flip_inf.is_some() || flip_cmp.is_some() {
                    commands::config_cmd::update_flip_models(
                        &workgraph_dir,
                        write_scope,
                        flip_inf.as_deref(),
                        flip_cmp.as_deref(),
                    )?;
                }

                // Handle standard model routing if present
                if effective_model.is_some()
                    || effective_provider.is_some()
                    || set_endpoint.is_some()
                {
                    commands::config_cmd::update_model_routing(
                        &workgraph_dir,
                        write_scope,
                        effective_model.as_deref(),
                        effective_provider.as_deref(),
                        set_endpoint.as_deref(),
                    )?;
                }

                Ok(())
            } else if list {
                commands::config_cmd::list(&workgraph_dir, cli.json)
            } else if init {
                commands::config_cmd::init(&workgraph_dir, scope)
            } else if show
                || (executor.is_none()
                    && model.is_none()
                    && set_interval.is_none()
                    && max_agents.is_none()
                    && max_coordinators.is_none()
                    && coordinator_interval.is_none()
                    && poll_interval.is_none()
                    && coordinator_executor.is_none()
                    && coordinator_model.is_none()
                    && coordinator_provider.is_none()
                    && auto_evaluate.is_none()
                    && auto_assign.is_none()
                    && assigner_agent.is_none()
                    && evaluator_agent.is_none()
                    && evolver_agent.is_none()
                    && creator_agent.is_none()
                    && retention_heuristics.is_none()
                    && auto_triage.is_none()
                    && auto_place.is_none()
                    && auto_create.is_none()
                    && triage_timeout.is_none()
                    && triage_max_log_bytes.is_none()
                    && max_child_tasks.is_none()
                    && max_task_depth.is_none()
                    && viz_edge_color.is_none()
                    && eval_gate_threshold.is_none()
                    && eval_gate_all.is_none()
                    && flip_enabled.is_none()
                    && flip_verification_threshold.is_none()
                    && chat_history.is_none()
                    && chat_history_max.is_none()
                    && tui_counters.is_none()
                    && retry_context_tokens.is_none()
                    && heartbeat_interval.is_none())
            {
                commands::config_cmd::show(&workgraph_dir, scope, cli.json)
            } else {
                // Default scope for writes = Local (like git)
                let write_scope = scope.unwrap_or(commands::config_cmd::ConfigScope::Local);
                commands::config_cmd::update(
                    &workgraph_dir,
                    write_scope,
                    executor.as_deref(),
                    model.as_deref(),
                    set_interval,
                    max_agents,
                    max_coordinators,
                    coordinator_interval,
                    poll_interval,
                    coordinator_executor.as_deref(),
                    coordinator_model.as_deref(),
                    coordinator_provider.as_deref(),
                    auto_evaluate,
                    auto_assign,
                    assigner_agent.as_deref(),
                    evaluator_agent.as_deref(),
                    evolver_agent.as_deref(),
                    creator_agent.as_deref(),
                    retention_heuristics.as_deref(),
                    auto_triage,
                    auto_place,
                    auto_create,
                    triage_timeout,
                    triage_max_log_bytes,
                    max_child_tasks,
                    max_task_depth,
                    viz_edge_color.as_deref(),
                    eval_gate_threshold,
                    eval_gate_all,
                    flip_enabled,
                    flip_verification_threshold,
                    chat_history,
                    chat_history_max,
                    tui_counters.as_deref(),
                    retry_context_tokens,
                    heartbeat_interval,
                )
            }
        }
        Commands::DeadAgents {
            cleanup,
            remove,
            processes,
            purge,
            delete_dirs,
            threshold,
        } => {
            if purge {
                commands::dead_agents::run_purge(&workgraph_dir, delete_dirs, cli.json).map(|_| ())
            } else if processes {
                commands::dead_agents::run_check_processes(&workgraph_dir, cli.json)
            } else if remove {
                commands::dead_agents::run_remove_dead(&workgraph_dir, cli.json).map(|_| ())
            } else if cleanup {
                commands::dead_agents::run_cleanup(&workgraph_dir, threshold, cli.json).map(|_| ())
            } else {
                // Default to check
                commands::dead_agents::run_check(&workgraph_dir, threshold, cli.json)
            }
        }
        Commands::Sweep { dry_run } => {
            commands::sweep::run(&workgraph_dir, dry_run, cli.json).map(|_| ())
        }
        Commands::Agents {
            alive,
            dead,
            working,
            idle,
        } => {
            let filter = if alive {
                Some(commands::agents::AgentFilter::Alive)
            } else if dead {
                Some(commands::agents::AgentFilter::Dead)
            } else if working {
                Some(commands::agents::AgentFilter::Working)
            } else if idle {
                Some(commands::agents::AgentFilter::Idle)
            } else {
                None
            };
            commands::agents::run(&workgraph_dir, filter, cli.json)
        }
        Commands::Kill {
            agent,
            force,
            all,
            tree,
            dry_run,
            no_abandon,
        } => {
            if tree {
                if let Some(task_id) = agent {
                    commands::kill::run_tree(
                        &workgraph_dir,
                        &task_id,
                        force,
                        dry_run,
                        no_abandon,
                        cli.json,
                    )
                } else {
                    anyhow::bail!("Must specify a task ID with --tree")
                }
            } else if all {
                commands::kill::run_all(&workgraph_dir, force, cli.json)
            } else if let Some(agent_id) = agent {
                commands::kill::run(&workgraph_dir, &agent_id, force, cli.json)
            } else {
                anyhow::bail!("Must specify an agent ID or use --all")
            }
        }
        Commands::Reap {
            dry_run,
            older_than,
        } => commands::reap::run(&workgraph_dir, dry_run, older_than.as_deref(), cli.json),
        Commands::Service { command } => match command {
            ServiceCommands::Start {
                port,
                socket,
                max_agents,
                executor,
                interval,
                model,
                force,
                no_coordinator_agent,
            } => commands::service::run_start(
                &workgraph_dir,
                socket.as_deref(),
                port,
                max_agents,
                executor.as_deref(),
                interval,
                model.as_deref(),
                cli.json,
                force,
                no_coordinator_agent,
            ),
            ServiceCommands::Stop { force, kill_agents } => {
                commands::service::run_stop(&workgraph_dir, force, kill_agents, cli.json)
            }
            ServiceCommands::Restart => commands::service::run_restart(&workgraph_dir, cli.json),
            ServiceCommands::Status => commands::service::run_status(&workgraph_dir, cli.json),
            ServiceCommands::Reload {
                max_agents,
                executor,
                interval,
                model,
            } => commands::service::run_reload(
                &workgraph_dir,
                max_agents,
                executor.as_deref(),
                interval,
                model.as_deref(),
                cli.json,
            ),
            ServiceCommands::Pause => commands::service::run_pause(&workgraph_dir, cli.json),
            ServiceCommands::Resume => commands::service::run_resume(&workgraph_dir, cli.json),
            ServiceCommands::Freeze => commands::service::run_freeze(&workgraph_dir, cli.json),
            ServiceCommands::Thaw => commands::service::run_thaw(&workgraph_dir, cli.json),
            ServiceCommands::Install => commands::service::generate_systemd_service(&workgraph_dir),
            ServiceCommands::Tick {
                max_agents,
                executor,
                model,
            } => commands::service::run_tick(
                &workgraph_dir,
                max_agents,
                executor.as_deref(),
                model.as_deref(),
            ),
            ServiceCommands::CreateCoordinator {
                name,
                model,
                executor,
            } => commands::service::run_create_coordinator(
                &workgraph_dir,
                name.as_deref(),
                model.as_deref(),
                executor.as_deref(),
                cli.json,
            ),
            ServiceCommands::DeleteCoordinator { id } => {
                commands::service::run_delete_coordinator(&workgraph_dir, id, cli.json)
            }
            ServiceCommands::ArchiveCoordinator { id } => {
                commands::service::run_archive_coordinator(&workgraph_dir, id, cli.json)
            }
            ServiceCommands::StopCoordinator { id } => {
                commands::service::run_stop_coordinator(&workgraph_dir, id, cli.json)
            }
            ServiceCommands::InterruptCoordinator { id } => {
                commands::service::run_interrupt_coordinator(&workgraph_dir, id, cli.json)
            }
            ServiceCommands::Daemon {
                socket,
                max_agents,
                executor,
                interval,
                model,
                no_coordinator_agent,
            } => commands::service::run_daemon(
                &workgraph_dir,
                &socket,
                max_agents,
                executor.as_deref(),
                interval,
                model.as_deref(),
                no_coordinator_agent,
            ),
        },
        Commands::Tui {
            no_mouse,
            recording,
            trace,
            show_keys,
            history_depth,
            no_history,
        } => {
            let config = Config::load_or_default(&workgraph_dir);
            let resolved_edge_color = config.viz.edge_color;
            let options = commands::viz::VizOptions {
                all: true,
                status: None,
                critical_path: false,
                format: commands::viz::OutputFormat::Ascii,
                output: None,
                show_internal: false,
                show_internal_running_only: false,
                focus: vec![],
                tui_mode: true,
                layout: commands::viz::LayoutMode::default(),
                tags: vec![],
                edge_color: resolved_edge_color,
                max_columns: None, // TUI handles its own sizing
            };
            let mouse_override = if no_mouse { Some(false) } else { None };
            let show_keys = show_keys || config.tui.show_keys;
            tui::viz_viewer::run(
                workgraph_dir,
                options,
                mouse_override,
                recording,
                trace,
                show_keys,
                history_depth,
                no_history,
            )
        }
        Commands::TuiDump {} => {
            let snap = tui::viz_viewer::screen_dump::client_dump(&workgraph_dir)?;
            if cli.json {
                let j = serde_json::json!({
                    "width": snap.width,
                    "height": snap.height,
                    "active_tab": snap.active_tab,
                    "focused_panel": snap.focused_panel,
                    "selected_task": snap.selected_task,
                    "input_mode": snap.input_mode,
                    "coordinator_id": snap.coordinator_id,
                    "text": snap.text,
                });
                println!("{}", serde_json::to_string_pretty(&j)?);
            } else {
                println!("{}", snap.text);
            }
            Ok(())
        }
        Commands::Screencast { command } => match command {
            ScreencastCommands::Render {
                trace,
                output,
                compress_idle,
                target_duration,
                width,
                height,
            } => commands::screencast_render::run(
                &workgraph_dir,
                &trace,
                &output,
                &compress_idle,
                target_duration,
                width,
                height,
            ),
            ScreencastCommands::Autopilot {
                output,
                cols,
                rows,
                duration,
            } => commands::screencast_autopilot::run(&workgraph_dir, &output, cols, rows, duration),
        },
        Commands::Server { command } => match command {
            ServerCommands::Init {
                apply,
                group,
                users,
                ttyd,
                caddy,
                ttyd_port,
            } => {
                let opts = commands::server::ServerInitOpts {
                    apply,
                    group: group.as_deref(),
                    users: &users,
                    ttyd,
                    caddy,
                    ttyd_port,
                };
                commands::server::run(&workgraph_dir, &opts)
            }
            ServerCommands::Connect { user } => commands::server::connect(user.as_deref()),
        },
        Commands::Setup {
            provider,
            api_key_file,
            api_key_env,
            url,
            model,
            skip_validation,
        } => {
            let args = commands::setup::SetupArgs {
                provider,
                api_key_file,
                api_key_env,
                url,
                model,
                skip_validation,
            };
            commands::setup::run_with_args(&args)
        }
        Commands::Quickstart => commands::quickstart::run(cli.json),
        Commands::Status => commands::status::run(&workgraph_dir, cli.json),
        Commands::Stats => commands::stats::run(&workgraph_dir, cli.json),
        Commands::Metrics { json } => commands::metrics::run(&workgraph_dir, json),
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Notify {
            task,
            room,
            message,
        } => commands::notify::run(
            &workgraph_dir,
            &task,
            room.as_deref(),
            message.as_deref(),
            cli.json,
        ),
        #[cfg(any(feature = "matrix", feature = "matrix-lite"))]
        Commands::Matrix { command } => match command {
            MatrixCommands::Listen { room } => {
                commands::matrix::run_listen(&workgraph_dir, room.as_deref())
            }
            MatrixCommands::Send { message, room } => {
                commands::matrix::run_send(&workgraph_dir, room.as_deref(), &message)
            }
            MatrixCommands::Status => commands::matrix::run_status(&workgraph_dir, cli.json),
            MatrixCommands::Login => commands::matrix::run_login(&workgraph_dir),
            MatrixCommands::Logout => {
                commands::matrix::run_logout(&workgraph_dir);
                Ok(())
            }
        },
        Commands::Telegram { command } => match command {
            TelegramCommands::Listen { chat_id } => {
                commands::telegram::run_listen(&workgraph_dir, chat_id.as_deref())
            }
            TelegramCommands::Send { message, chat_id } => {
                commands::telegram::run_send(chat_id.as_deref(), &message)
            }
            TelegramCommands::Status => commands::telegram::run_status(cli.json),
            TelegramCommands::Poll { timeout, chat_id } => {
                commands::telegram::run_poll(chat_id.as_deref(), timeout)
            }
            TelegramCommands::Ask {
                message,
                timeout,
                interval,
                chat_id,
                task_id,
            } => commands::telegram::run_ask(
                &message,
                chat_id.as_deref(),
                timeout,
                interval,
                task_id.as_deref(),
            ),
        },
        Commands::Endpoints { command } | Commands::Endpoint { command } => match command {
            EndpointsCommands::List => commands::endpoints::run_list(&workgraph_dir, cli.json),
            EndpointsCommands::Add {
                name,
                provider,
                url,
                model,
                api_key,
                api_key_file,
                key_env,
                default: set_default,
                global,
            } => commands::endpoints::run_add(
                &workgraph_dir,
                &name,
                provider.as_deref(),
                url.as_deref(),
                model.as_deref(),
                api_key.as_deref(),
                api_key_file.as_deref(),
                key_env.as_deref(),
                set_default,
                global,
            ),
            EndpointsCommands::Update {
                name,
                provider,
                url,
                model,
                api_key,
                api_key_file,
                key_env,
                default: set_default,
                global,
            } => commands::endpoints::run_update(
                &workgraph_dir,
                &name,
                provider.as_deref(),
                url.as_deref(),
                model.as_deref(),
                api_key.as_deref(),
                api_key_file.as_deref(),
                key_env.as_deref(),
                set_default,
                global,
            ),
            EndpointsCommands::Remove { name, global } => {
                commands::endpoints::run_remove(&workgraph_dir, &name, global)
            }
            EndpointsCommands::SetDefault { name, global } => {
                commands::endpoints::run_set_default(&workgraph_dir, &name, global)
            }
            EndpointsCommands::Test { name } => {
                commands::endpoints::run_test(&workgraph_dir, &name)
            }
        },
        Commands::Models { command } => match command {
            ModelsCommands::List { tier } => {
                commands::models::run_list(&workgraph_dir, tier.as_deref(), cli.json)
            }
            ModelsCommands::Search {
                query,
                tools,
                no_cache,
                limit,
            } => commands::models::run_search(
                &workgraph_dir,
                &query,
                tools,
                no_cache,
                limit,
                cli.json,
            ),
            ModelsCommands::Remote {
                tools,
                no_cache,
                limit,
            } => {
                commands::models::run_list_remote(&workgraph_dir, tools, no_cache, limit, cli.json)
            }
            ModelsCommands::Add {
                id,
                provider,
                cost_in,
                cost_out,
                context_window,
                capability,
                tier,
            } => commands::models::run_add(
                &workgraph_dir,
                &id,
                provider.as_deref(),
                cost_in,
                cost_out,
                context_window,
                &capability,
                &tier,
            ),
            ModelsCommands::SetDefault { id } => {
                commands::models::run_set_default(&workgraph_dir, &id)
            }
            ModelsCommands::Init => commands::models::run_init(&workgraph_dir),
            ModelsCommands::Fetch { no_cache } => {
                commands::models::run_fetch(&workgraph_dir, no_cache)
            }
            ModelsCommands::Benchmarks { tier, limit } => {
                commands::models::run_benchmarks(&workgraph_dir, tier.as_deref(), limit, cli.json)
            }
        },
        Commands::Model { command } => match command {
            ModelCommands::List { tier } => {
                commands::model_cmd::run_list(&workgraph_dir, tier.as_deref(), cli.json)
            }
            ModelCommands::Add {
                alias,
                provider,
                model_id,
                tier,
                endpoint,
                context_window,
                cost_in,
                cost_out,
                global,
            } => commands::model_cmd::run_add(
                &workgraph_dir,
                &alias,
                &provider,
                model_id.as_deref(),
                &tier,
                endpoint.as_deref(),
                context_window,
                cost_in,
                cost_out,
                global,
            ),
            ModelCommands::Remove {
                alias,
                force,
                global,
            } => commands::model_cmd::run_remove(&workgraph_dir, &alias, force, global, cli.json),
            ModelCommands::SetDefault { alias, global } => {
                commands::model_cmd::run_set_default(&workgraph_dir, &alias, global)
            }
            ModelCommands::Routing => commands::model_cmd::run_routing(&workgraph_dir, cli.json),
            ModelCommands::Set {
                role,
                model,
                provider,
                endpoint,
                tier: _tier,
                global,
            } => commands::model_cmd::run_set(
                &workgraph_dir,
                &role,
                &model,
                provider.as_deref(),
                endpoint.as_deref(),
                global,
            ),
        },
        Commands::Nex {
            model,
            endpoint,
            system_prompt,
            message,
            max_turns,
            chatty,
            verbose,
            read_only,
            resume,
            role,
            chat_id,
            chat_ref,
            autonomous,
            no_mcp,
        } => commands::nex::run(
            &workgraph_dir,
            model.as_deref(),
            endpoint.as_deref(),
            system_prompt.as_deref(),
            message.as_deref(),
            max_turns,
            chatty,
            verbose,
            read_only,
            resume.as_deref(),
            role.as_deref(),
            chat_id,
            chat_ref.as_deref(),
            autonomous,
            no_mcp,
        ),
        Commands::TuiNex { model, endpoint } => {
            commands::tui_nex::run(&workgraph_dir, model.as_deref(), endpoint.as_deref())
        }
        Commands::NativeExec {
            prompt_file,
            exec_mode,
            task_id,
            model,
            provider,
            endpoint_name,
            endpoint_url,
            api_key,
            max_turns,
            no_resume,
        } => commands::native_exec::run(
            &workgraph_dir,
            &prompt_file,
            &exec_mode,
            &task_id,
            model.as_deref(),
            provider.as_deref(),
            endpoint_name.as_deref(),
            endpoint_url.as_deref(),
            api_key.as_deref(),
            max_turns,
            no_resume,
        ),
        Commands::ApplyPlacement {
            output_dir,
            source_task_id,
        } => {
            let output_path = std::path::Path::new(&output_dir);
            let raw_stream = output_path.join("raw_stream.jsonl");
            match commands::placement::parse_and_apply(&raw_stream, &source_task_id, &workgraph_dir)
            {
                Ok(msg) => {
                    eprintln!("[apply-placement] {}", msg);
                    Ok(())
                }
                Err(e) => {
                    eprintln!("[apply-placement] FAILED: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Key { command } => match command {
            KeyCommands::Set {
                provider,
                env,
                file,
                value,
                global,
            } => commands::key::run_set(
                &workgraph_dir,
                &provider,
                env.as_deref(),
                file.as_deref(),
                value.as_deref(),
                global,
            ),
            KeyCommands::Check { provider } => {
                commands::key::run_check(&workgraph_dir, provider.as_deref(), cli.json)
            }
            KeyCommands::List => commands::key::run_list(&workgraph_dir, cli.json),
        },
        cli::Commands::Openrouter { command } => {
            commands::openrouter::run(&workgraph_dir, &command, cli.json)
        }
    }
}

/// Parse --propagation and --retry-strategy into an IterationConfig.
fn parse_iteration_config(
    propagation: Option<&str>,
    retry_strategy: Option<&str>,
) -> Option<workgraph::agency::IterationConfig> {
    use workgraph::agency::{IterationConfig, PropagationPolicy, RetryStrategy};

    let prop = propagation.map(|p| {
        let p = p.trim().to_lowercase();
        if p == "conservative" {
            PropagationPolicy::Conservative
        } else if p == "aggressive" {
            PropagationPolicy::Aggressive
        } else if let Some(threshold) = p.strip_prefix("conditional:") {
            let val: f32 = threshold.parse().unwrap_or(0.0);
            PropagationPolicy::Conditional(val)
        } else {
            PropagationPolicy::Conservative
        }
    });

    let strat = retry_strategy.map(|s| match s.trim().to_lowercase().as_str() {
        "same-model" => RetryStrategy::SameModel,
        "upgrade-model" => RetryStrategy::UpgradeModel,
        "escalate-to-human" => RetryStrategy::EscalateToHuman,
        _ => RetryStrategy::SameModel,
    });

    if prop.is_none() && strat.is_none() {
        return None;
    }
    Some(IterationConfig {
        max_retries: None,
        propagation: prop,
        retry_strategy: strat,
    })
}

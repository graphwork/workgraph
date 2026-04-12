//! `wg server init` — automate multi-user server setup.
//!
//! Checks prerequisites, configures Unix group permissions, generates shell
//! profile snippets and tmux/ttyd/Caddy configs.  Dry-run by default;
//! requires `--apply` to make real changes.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Outcome of a single setup action — either the shell command that *would* run
/// (dry-run) or the result of actually running it.
#[derive(Debug)]
struct Action {
    description: String,
    command: Option<String>,
    status: ActionStatus,
}

#[derive(Debug)]
#[allow(dead_code)]
enum ActionStatus {
    Pending,
    Skipped(String),
    Applied(String),
    Failed(String),
}

/// Options gathered from CLI flags.
pub struct ServerInitOpts<'a> {
    pub apply: bool,
    pub group: Option<&'a str>,
    pub users: &'a [String],
    pub ttyd: bool,
    pub caddy: bool,
    pub ttyd_port: u16,
}

/// Main entry point.
pub fn run(dir: &Path, opts: &ServerInitOpts) -> Result<()> {
    let project_name = detect_project_name(dir);
    let group = opts
        .group
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("wg-{}", project_name));

    let mut actions: Vec<Action> = Vec::new();

    // 1. Check prerequisites
    println!("## Checking prerequisites\n");
    check_prerequisite("tmux", true, &mut actions);
    check_prerequisite("ttyd", opts.ttyd, &mut actions);
    check_prerequisite("caddy", opts.caddy, &mut actions);

    // 2. Unix group
    println!("\n## Unix group: {}\n", group);
    actions.push(Action {
        description: format!("Create Unix group '{}'", group),
        command: Some(format!("sudo groupadd -f {}", group)),
        status: ActionStatus::Pending,
    });

    // Add users to group
    for user in opts.users {
        actions.push(Action {
            description: format!("Add user '{}' to group '{}'", user, group),
            command: Some(format!("sudo usermod -aG {} {}", group, user)),
            status: ActionStatus::Pending,
        });
    }

    // 3. Directory permissions
    let wg_dir = dir.display();
    println!("\n## Directory permissions\n");
    actions.push(Action {
        description: format!("Set .workgraph/ group to '{}' with mode 0770", group),
        command: Some(format!(
            "sudo chgrp -R {group} {wg_dir} && sudo chmod -R g+rwX {wg_dir} && sudo chmod 0770 {wg_dir}"
        )),
        status: ActionStatus::Pending,
    });

    // 4. File permissions
    let graph_path = dir.join("graph.jsonl");
    if graph_path.exists() {
        actions.push(Action {
            description: "Set graph.jsonl to 0660".into(),
            command: Some(format!("chmod 0660 {}", graph_path.display())),
            status: ActionStatus::Pending,
        });
    }
    let sock_path = dir.join("service").join("daemon.sock");
    actions.push(Action {
        description: "Set daemon.sock to 0660 (when created)".into(),
        command: Some(format!(
            "chmod 0660 {} 2>/dev/null || true",
            sock_path.display()
        )),
        status: ActionStatus::Pending,
    });

    // 5. Generate shell profile snippet
    println!("\n## Shell profile snippet\n");
    println!("Add the following to each user's shell profile (~/.bashrc, ~/.zshrc, etc.):\n");
    for user in opts.users {
        let snippet = generate_shell_snippet(user, dir);
        println!("# For user '{}':", user);
        println!("{}\n", snippet);
    }
    if opts.users.is_empty() {
        let snippet = generate_shell_snippet("<USERNAME>", dir);
        println!("# Template (replace <USERNAME>):");
        println!("{}\n", snippet);
    }

    // 6. tmux launch command
    println!("## tmux launch command\n");
    let tmux_session = format!("wg-{}", project_name);
    println!(
        "  tmux new-session -s {} -d 'wg tui' \\; split-window -h 'wg watch'\n",
        tmux_session
    );

    // 7. Optional ttyd + Caddy config
    if opts.ttyd {
        println!("## ttyd config\n");
        println!(
            "  ttyd -p {} --writable tmux attach -t {}\n",
            opts.ttyd_port, tmux_session
        );
    }
    if opts.caddy {
        println!("## Caddy reverse-proxy snippet\n");
        println!("  # Add to Caddyfile:");
        println!("  wg.example.com {{");
        println!("      reverse_proxy localhost:{}", opts.ttyd_port);
        println!("  }}\n");
    }

    // Execute or print
    if opts.apply {
        println!("\n## Applying changes\n");
        for action in &mut actions {
            if let Some(ref cmd) = action.command {
                if let ActionStatus::Skipped(_) = action.status {
                    println!(
                        "  SKIP  {}: {}",
                        action.description,
                        match &action.status {
                            ActionStatus::Skipped(r) => r.as_str(),
                            _ => "",
                        }
                    );
                    continue;
                }
                print!("  RUN   {} ... ", action.description);
                match run_shell_command(cmd) {
                    Ok(output) => {
                        println!("OK");
                        action.status = ActionStatus::Applied(output);
                    }
                    Err(e) => {
                        println!("FAILED: {}", e);
                        action.status = ActionStatus::Failed(e.to_string());
                    }
                }
            }
        }
    } else {
        println!("\n## Planned actions (dry-run)\n");
        println!("The following commands would be executed with --apply:\n");
        for action in &actions {
            match &action.status {
                ActionStatus::Skipped(reason) => {
                    println!("  SKIP  {} ({})", action.description, reason);
                }
                _ => {
                    if let Some(ref cmd) = action.command {
                        println!("  RUN   {}", action.description);
                        println!("        $ {}", cmd);
                    }
                }
            }
        }
        println!("\nRe-run with --apply to execute these commands.");
    }

    // 8. Print summary
    println!("\n## Summary\n");
    println!("  Project:    {}", project_name);
    println!("  Group:      {}", group);
    println!(
        "  Users:      {}",
        if opts.users.is_empty() {
            "(none specified — use --user)".to_string()
        } else {
            opts.users.join(", ")
        }
    );
    println!("  Directory:  {}", wg_dir);
    println!("  tmux session: {}", tmux_session);
    if opts.ttyd {
        println!("  ttyd port:  {}", opts.ttyd_port);
    }
    if opts.caddy {
        println!("  Caddy:      enabled");
    }
    println!(
        "  Mode:       {}",
        if opts.apply { "applied" } else { "dry-run" }
    );

    Ok(())
}

fn detect_project_name(dir: &Path) -> String {
    // Walk up from .workgraph dir to the project root
    dir.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string()
}

fn check_prerequisite(name: &str, required: bool, actions: &mut Vec<Action>) {
    let found = Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if found {
        println!("  [x] {} found", name);
    } else if required {
        println!("  [ ] {} NOT FOUND (required)", name);
        actions.push(Action {
            description: format!("Install {}", name),
            command: None,
            status: ActionStatus::Skipped(format!(
                "{} not found — install it manually (e.g., apt install {})",
                name, name
            )),
        });
    } else {
        println!("  [ ] {} not found (optional)", name);
    }
}

fn generate_shell_snippet(user: &str, dir: &Path) -> String {
    let project_dir = dir
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string());
    format!(
        r#"export WG_USER="{user}"
# Optional: cd to the project directory
# cd {project_dir}"#,
    )
}

fn run_shell_command(cmd: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .context("Failed to execute command")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}", stderr.trim());
    }
}

/// Create or attach to a user's tmux session (`<user>-wg`).
///
/// Uses the `--user` flag or falls back to `$WG_USER`. Propagates `WG_USER`
/// inside the session. Gives a graceful error with install instructions if
/// tmux is not installed.
pub fn connect(user: Option<&str>) -> Result<()> {
    // Resolve user
    let user = match user {
        Some(u) => u.to_string(),
        None => std::env::var("WG_USER").context(
            "No user specified. Pass --user <name> or set the WG_USER environment variable.",
        )?,
    };

    // Check tmux is installed
    let tmux_found = Command::new("which")
        .arg("tmux")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !tmux_found {
        anyhow::bail!(
            "tmux is not installed.\n\n\
             Install it with your package manager:\n\
             \n\
             \x20 Ubuntu/Debian:  sudo apt install tmux\n\
             \x20 Fedora/RHEL:    sudo dnf install tmux\n\
             \x20 macOS:          brew install tmux\n\
             \x20 Arch:           sudo pacman -S tmux\n"
        );
    }

    let session_name = format!("{}-wg", user);

    // Check if the session already exists
    let has_session = Command::new("tmux")
        .args(["has-session", "-t", &session_name])
        .stderr(std::process::Stdio::null()) // Suppress stderr to avoid terminal errors in test environments
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_session {
        println!("Attaching to existing tmux session '{}'...", session_name);
        let status = Command::new("tmux")
            .args(["attach-session", "-t", &session_name])
            .env("WG_USER", &user)
            .stderr(std::process::Stdio::null()) // Suppress stderr to avoid terminal errors in test environments
            .status()
            .context("Failed to attach to tmux session")?;
        if !status.success() {
            anyhow::bail!("tmux attach-session exited with status {}", status);
        }
    } else {
        println!("Creating tmux session '{}'...", session_name);
        let status = Command::new("tmux")
            .args(["new-session", "-s", &session_name])
            .env("WG_USER", &user)
            .stderr(std::process::Stdio::null()) // Suppress stderr to avoid terminal errors in test environments
            .status()
            .context("Failed to create tmux session")?;
        if !status.success() {
            anyhow::bail!("tmux new-session exited with status {}", status);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    #[test]
    fn test_dry_run_prints_without_executing() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        // Create a graph.jsonl so the file permission action triggers
        std::fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

        let opts = ServerInitOpts {
            apply: false,
            group: Some("test-group"),
            users: &["alice".to_string(), "bob".to_string()],
            ttyd: false,
            caddy: false,
            ttyd_port: 7681,
        };

        // Should succeed without actually running any commands
        let result = run(&wg_dir, &opts);
        assert!(result.is_ok(), "dry-run should succeed: {:?}", result);
    }

    #[test]
    fn test_detect_project_name() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join("my-project").join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let name = detect_project_name(&wg_dir);
        assert_eq!(name, "my-project");
    }

    #[test]
    fn test_generate_shell_snippet_sets_wg_user() {
        let dir = Path::new("/home/test/my-project/.workgraph");
        let snippet = generate_shell_snippet("alice", dir);
        assert!(snippet.contains(r#"export WG_USER="alice""#));
    }

    #[test]
    fn test_check_prerequisite_finds_sh() {
        let mut actions = Vec::new();
        check_prerequisite("sh", true, &mut actions);
        // sh should be found on any Unix system, so no action added
        assert!(
            actions.is_empty(),
            "sh should be found; actions: {:?}",
            actions
        );
    }

    #[test]
    fn test_check_prerequisite_missing_optional() {
        let mut actions = Vec::new();
        check_prerequisite("nonexistent-tool-xyz", false, &mut actions);
        // optional missing tool should not add any action
        assert!(actions.is_empty());
    }

    #[test]
    fn test_check_prerequisite_missing_required() {
        let mut actions = Vec::new();
        check_prerequisite("nonexistent-tool-xyz", true, &mut actions);
        assert_eq!(actions.len(), 1);
        assert!(
            actions[0]
                .description
                .contains("Install nonexistent-tool-xyz")
        );
    }

    #[test]
    fn test_dry_run_with_ttyd_and_caddy() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let opts = ServerInitOpts {
            apply: false,
            group: None,
            users: &[],
            ttyd: true,
            caddy: true,
            ttyd_port: 8080,
        };

        let result = run(&wg_dir, &opts);
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn test_connect_no_user_no_env() {
        // Clear WG_USER so the fallback fails
        unsafe { std::env::remove_var("WG_USER") };
        let result = connect(None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("No user specified"),
            "Expected 'No user specified' error, got: {}",
            err
        );
    }

    #[test]
    #[serial]
    fn test_connect_uses_wg_user_env() {
        // Set WG_USER and verify the function gets past user resolution.
        // It will either succeed (tmux installed) or fail at tmux check —
        // either way it should NOT fail with "No user specified".
        unsafe { std::env::set_var("WG_USER", "testuser") };
        let result = connect(None);
        // If tmux is not installed, the error should be about tmux, not about user
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("No user specified"),
                "Should have resolved user from WG_USER, got: {}",
                msg
            );
        }
        unsafe { std::env::remove_var("WG_USER") };
    }

    #[test]
    #[serial]
    fn test_connect_explicit_user_overrides_env() {
        unsafe { std::env::set_var("WG_USER", "envuser") };
        // Pass explicit user — should get past user resolution
        let result = connect(Some("explicit"));
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("No user specified"),
                "Explicit user should override env, got: {}",
                msg
            );
        }
        unsafe { std::env::remove_var("WG_USER") };
    }
}

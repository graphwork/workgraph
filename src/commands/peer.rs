use std::path::Path;

use anyhow::Result;

use workgraph::federation;

/// Add a named peer workgraph instance.
pub fn run_add(
    workgraph_dir: &Path,
    name: &str,
    path: &str,
    description: Option<&str>,
) -> Result<()> {
    let mut config = federation::load_federation_config(workgraph_dir)?;

    if config.peers.contains_key(name) {
        anyhow::bail!(
            "Peer '{}' already exists. Remove it first with 'wg peer remove {}'",
            name,
            name
        );
    }

    // Validate path accessibility (warn but don't block)
    let resolved_path = if let Some(suffix) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(suffix)
        } else {
            Path::new(path).to_path_buf()
        }
    } else {
        Path::new(path).to_path_buf()
    };

    if !resolved_path.exists() {
        eprintln!(
            "Warning: Path '{}' does not exist or is not accessible. \
             The peer will be added anyway.",
            path
        );
    } else if !resolved_path.join(".workgraph").is_dir() {
        eprintln!(
            "Warning: No .workgraph directory found at '{}'. \
             The peer will be added anyway (it may not be initialized yet).",
            path
        );
    }

    config.peers.insert(
        name.to_string(),
        federation::PeerConfig {
            path: path.to_string(),
            description: description.map(String::from),
        },
    );

    federation::save_federation_config(workgraph_dir, &config)?;
    println!("Added peer '{}' -> {}", name, path);

    Ok(())
}

/// Remove a named peer.
pub fn run_remove(workgraph_dir: &Path, name: &str) -> Result<()> {
    let mut config = federation::load_federation_config(workgraph_dir)?;

    if config.peers.remove(name).is_none() {
        anyhow::bail!("Peer '{}' not found", name);
    }

    federation::save_federation_config(workgraph_dir, &config)?;
    println!("Removed peer '{}'", name);

    Ok(())
}

/// List all configured peers.
pub fn run_list(workgraph_dir: &Path, json: bool) -> Result<()> {
    let config = federation::load_federation_config(workgraph_dir)?;

    if config.peers.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No peers configured. Add one with 'wg peer add <name> <path>'");
        }
        return Ok(());
    }

    if json {
        let entries: Vec<serde_json::Value> = config
            .peers
            .iter()
            .map(|(name, peer)| {
                let status = check_peer_status_for_config(peer);
                serde_json::json!({
                    "name": name,
                    "path": peer.path,
                    "description": peer.description,
                    "service_running": status.running,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    for (name, peer) in &config.peers {
        let status = check_peer_status_for_config(peer);
        let service_indicator = if status.running { "running" } else { "stopped" };
        println!(
            "  {:15} {} (service: {})",
            name, peer.path, service_indicator
        );
        if let Some(desc) = &peer.description {
            println!("  {:15} {}", "", desc);
        }
    }

    Ok(())
}

/// Show detailed info about a peer.
pub fn run_show(workgraph_dir: &Path, name: &str, json: bool) -> Result<()> {
    let config = federation::load_federation_config(workgraph_dir)?;

    let peer = config
        .peers
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("Peer '{}' not found", name))?;

    let resolved = resolve_peer_path(&peer.path);
    let service_status = check_peer_status_for_config(peer);

    // Try to count tasks if accessible
    let task_counts = resolved.as_ref().ok().and_then(|wg_dir| {
        let graph_path = wg_dir.join("graph.jsonl");
        if graph_path.exists() {
            count_tasks_in_graph(&graph_path).ok()
        } else {
            None
        }
    });

    if json {
        let mut obj = serde_json::json!({
            "name": name,
            "path": peer.path,
            "description": peer.description,
            "service_running": service_status.running,
            "pid": service_status.pid,
            "socket_path": service_status.socket_path,
            "started_at": service_status.started_at,
        });

        if let Some(wg_dir) = resolved.as_ref().ok() {
            obj["workgraph_dir"] = serde_json::json!(wg_dir.display().to_string());
            obj["accessible"] = serde_json::json!(true);
        } else {
            obj["accessible"] = serde_json::json!(false);
        }

        if let Some(counts) = &task_counts {
            obj["tasks"] = serde_json::json!(counts);
        }

        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }

    println!("Peer: {}", name);
    println!("  Path:        {}", peer.path);
    if let Some(desc) = &peer.description {
        println!("  Description: {}", desc);
    }

    match &resolved {
        Ok(wg_dir) => {
            println!("  Workgraph:   {}", wg_dir.display());
        }
        Err(e) => {
            println!("  Workgraph:   inaccessible ({})", e);
        }
    }

    if service_status.running {
        println!("  Service:     running (PID {})", service_status.pid.unwrap_or(0));
        if let Some(socket) = &service_status.socket_path {
            println!("  Socket:      {}", socket);
        }
        if let Some(started) = &service_status.started_at {
            println!("  Started:     {}", started);
        }
    } else {
        println!("  Service:     not running");
    }

    if let Some(counts) = &task_counts {
        println!(
            "  Tasks:       {} total ({} open, {} in-progress, {} done, {} failed)",
            counts.total, counts.open, counts.in_progress, counts.done, counts.failed
        );
    }

    Ok(())
}

/// Show service status for all configured peers.
pub fn run_status(workgraph_dir: &Path, json: bool) -> Result<()> {
    let config = federation::load_federation_config(workgraph_dir)?;

    if config.peers.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No peers configured. Add one with 'wg peer add <name> <path>'");
        }
        return Ok(());
    }

    if json {
        let entries: Vec<serde_json::Value> = config
            .peers
            .iter()
            .map(|(name, peer)| {
                let status = check_peer_status_for_config(peer);
                let resolved = resolve_peer_path(&peer.path);
                let task_counts = resolved.as_ref().ok().and_then(|wg_dir| {
                    let graph_path = wg_dir.join("graph.jsonl");
                    if graph_path.exists() {
                        count_tasks_in_graph(&graph_path).ok()
                    } else {
                        None
                    }
                });

                let mut obj = serde_json::json!({
                    "name": name,
                    "path": peer.path,
                    "service_running": status.running,
                    "pid": status.pid,
                    "accessible": resolved.is_ok(),
                });
                if let Some(counts) = task_counts {
                    obj["tasks"] = serde_json::json!(counts);
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    println!("Peer Status:");
    for (name, peer) in &config.peers {
        let status = check_peer_status_for_config(peer);
        let resolved = resolve_peer_path(&peer.path);
        let accessible = resolved.is_ok();

        let service_str = if status.running {
            format!("running (PID {})", status.pid.unwrap_or(0))
        } else {
            "stopped".to_string()
        };

        let access_str = if accessible { "" } else { " [inaccessible]" };

        println!(
            "  {:15} service: {:20} {}{}",
            name, service_str, peer.path, access_str
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a peer path to its .workgraph directory.
fn resolve_peer_path(path: &str) -> Result<std::path::PathBuf> {
    let expanded = if let Some(suffix) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(suffix)
        } else {
            std::path::PathBuf::from(path)
        }
    } else {
        std::path::PathBuf::from(path)
    };

    let abs_path = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };

    let abs_path = abs_path.canonicalize().unwrap_or(abs_path);
    let wg_dir = abs_path.join(".workgraph");

    if !wg_dir.is_dir() {
        anyhow::bail!("No .workgraph directory at '{}'", abs_path.display());
    }

    Ok(wg_dir)
}

/// Check peer service status from a PeerConfig.
fn check_peer_status_for_config(peer: &federation::PeerConfig) -> federation::PeerServiceStatus {
    match resolve_peer_path(&peer.path) {
        Ok(wg_dir) => federation::check_peer_service(&wg_dir),
        Err(_) => federation::PeerServiceStatus {
            running: false,
            pid: None,
            socket_path: None,
            started_at: None,
        },
    }
}

/// Task counts from a workgraph.
#[derive(Debug, Clone, serde::Serialize)]
struct TaskCounts {
    total: usize,
    open: usize,
    in_progress: usize,
    done: usize,
    failed: usize,
}

/// Count tasks by status in a graph.jsonl file.
fn count_tasks_in_graph(graph_path: &Path) -> Result<TaskCounts> {
    let graph = workgraph::parser::load_graph(graph_path)?;
    let mut counts = TaskCounts {
        total: 0,
        open: 0,
        in_progress: 0,
        done: 0,
        failed: 0,
    };

    for task in graph.tasks() {
        counts.total += 1;
        match task.status {
            workgraph::graph::Status::Open => counts.open += 1,
            workgraph::graph::Status::InProgress => counts.in_progress += 1,
            workgraph::graph::Status::Done => counts.done += 1,
            workgraph::graph::Status::Failed => counts.failed += 1,
            workgraph::graph::Status::Abandoned | workgraph::graph::Status::Blocked => {}
        }
    }

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_workgraph_dir(tmp: &TempDir) -> std::path::PathBuf {
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        wg_dir
    }

    fn setup_peer_project(tmp: &TempDir, name: &str) -> std::path::PathBuf {
        let project = tmp.path().join(name);
        let wg_dir = project.join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        // Create a minimal graph.jsonl
        std::fs::write(wg_dir.join("graph.jsonl"), "").unwrap();
        project
    }

    #[test]
    fn add_and_list_peer() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let peer_project = setup_peer_project(&tmp, "other-repo");

        run_add(
            &wg_dir,
            "other",
            peer_project.to_str().unwrap(),
            Some("Another project"),
        )
        .unwrap();

        let config = federation::load_federation_config(&wg_dir).unwrap();
        assert_eq!(config.peers.len(), 1);
        assert!(config.peers.contains_key("other"));
        assert_eq!(
            config.peers["other"].description.as_deref(),
            Some("Another project")
        );
    }

    #[test]
    fn add_duplicate_peer_fails() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let peer_project = setup_peer_project(&tmp, "other-repo");

        run_add(&wg_dir, "other", peer_project.to_str().unwrap(), None).unwrap();
        let result = run_add(&wg_dir, "other", "/another/path", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn remove_peer() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        run_add(&wg_dir, "other", "/some/path", None).unwrap();
        run_remove(&wg_dir, "other").unwrap();

        let config = federation::load_federation_config(&wg_dir).unwrap();
        assert!(config.peers.is_empty());
    }

    #[test]
    fn remove_nonexistent_peer_fails() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        let result = run_remove(&wg_dir, "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn show_peer_with_valid_project() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let peer_project = setup_peer_project(&tmp, "other-repo");

        run_add(&wg_dir, "other", peer_project.to_str().unwrap(), None).unwrap();
        // Should not error
        run_show(&wg_dir, "other", false).unwrap();
        run_show(&wg_dir, "other", true).unwrap();
    }

    #[test]
    fn show_nonexistent_peer_fails() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        let result = run_show(&wg_dir, "nonexistent", false);
        assert!(result.is_err());
    }

    #[test]
    fn status_with_no_peers() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        // Should not error
        run_status(&wg_dir, false).unwrap();
        run_status(&wg_dir, true).unwrap();
    }

    #[test]
    fn add_peer_with_inaccessible_path_warns_but_succeeds() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        run_add(&wg_dir, "faraway", "/nonexistent/path", None).unwrap();

        let config = federation::load_federation_config(&wg_dir).unwrap();
        assert!(config.peers.contains_key("faraway"));
    }

    #[test]
    fn list_empty_peers() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        // Should not error even with no federation.yaml
        run_list(&wg_dir, false).unwrap();
        run_list(&wg_dir, true).unwrap();
    }

    #[test]
    fn peers_coexist_with_remotes() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let peer_project = setup_peer_project(&tmp, "other-repo");

        // Add a remote
        let mut config = federation::load_federation_config(&wg_dir).unwrap();
        config.remotes.insert(
            "upstream".to_string(),
            federation::Remote {
                path: "/some/agency/store".to_string(),
                description: None,
                last_sync: None,
            },
        );
        federation::save_federation_config(&wg_dir, &config).unwrap();

        // Add a peer
        run_add(&wg_dir, "other", peer_project.to_str().unwrap(), None).unwrap();

        // Both should exist
        let config = federation::load_federation_config(&wg_dir).unwrap();
        assert_eq!(config.remotes.len(), 1);
        assert_eq!(config.peers.len(), 1);
    }
}

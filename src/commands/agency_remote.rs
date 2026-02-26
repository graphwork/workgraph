use std::path::Path;

use anyhow::Result;

use workgraph::agency::AgencyStore;
use workgraph::federation;

/// Add a named remote.
pub fn run_add(
    workgraph_dir: &Path,
    name: &str,
    path: &str,
    description: Option<&str>,
) -> Result<()> {
    let mut config = federation::load_federation_config(workgraph_dir)?;

    if config.remotes.contains_key(name) {
        anyhow::bail!("Remote '{}' already exists. Remove it first with 'wg agency remote remove {}'", name, name);
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
             The remote will be added anyway (it may be on a different machine or mounted drive).",
            path
        );
    }

    config.remotes.insert(
        name.to_string(),
        federation::Remote {
            path: path.to_string(),
            description: description.map(String::from),
            last_sync: None,
        },
    );

    federation::save_federation_config(workgraph_dir, &config)?;
    println!("Added remote '{}' -> {}", name, path);

    Ok(())
}

/// Remove a named remote.
pub fn run_remove(workgraph_dir: &Path, name: &str) -> Result<()> {
    let mut config = federation::load_federation_config(workgraph_dir)?;

    if config.remotes.remove(name).is_none() {
        anyhow::bail!("Remote '{}' not found", name);
    }

    federation::save_federation_config(workgraph_dir, &config)?;
    println!("Removed remote '{}'", name);

    Ok(())
}

/// List all remotes.
pub fn run_list(workgraph_dir: &Path, json: bool) -> Result<()> {
    let config = federation::load_federation_config(workgraph_dir)?;

    if config.remotes.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No remotes configured. Add one with 'wg agency remote add <name> <path>'");
        }
        return Ok(());
    }

    if json {
        let entries: Vec<serde_json::Value> = config
            .remotes
            .iter()
            .map(|(name, remote)| {
                serde_json::json!({
                    "name": name,
                    "path": remote.path,
                    "description": remote.description,
                    "last_sync": remote.last_sync,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    for (name, remote) in &config.remotes {
        let sync = remote
            .last_sync
            .as_deref()
            .unwrap_or("never");
        println!("  {:15} {} (last sync: {})", name, remote.path, sync);
        if let Some(desc) = &remote.description {
            println!("  {:15} {}", "", desc);
        }
    }

    Ok(())
}

/// Show detailed info about a remote, including entity counts.
pub fn run_show(workgraph_dir: &Path, name: &str, json: bool) -> Result<()> {
    let config = federation::load_federation_config(workgraph_dir)?;

    let remote = config
        .remotes
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("Remote '{}' not found", name))?;

    // Try to load the remote store and count entities
    let store_result = federation::resolve_store(&remote.path);

    if json {
        let mut obj = serde_json::json!({
            "name": name,
            "path": remote.path,
            "description": remote.description,
            "last_sync": remote.last_sync,
        });

        if let Ok(store) = &store_result {
            if store.is_valid() {
                let roles = store.load_roles().unwrap_or_default();
                let motivations = store.load_tradeoffs().unwrap_or_default();
                let agents = store.load_agents().unwrap_or_default();
                let evaluations = store.load_evaluations().unwrap_or_default();

                obj["store_path"] = serde_json::json!(store.store_path().display().to_string());
                obj["entities"] = serde_json::json!({
                    "roles": roles.len(),
                    "motivations": motivations.len(),
                    "agents": agents.len(),
                    "evaluations": evaluations.len(),
                });
                obj["accessible"] = serde_json::json!(true);
            } else {
                obj["accessible"] = serde_json::json!(false);
                obj["error"] = serde_json::json!("Not a valid agency store");
            }
        } else {
            obj["accessible"] = serde_json::json!(false);
            obj["error"] = serde_json::json!(store_result.unwrap_err().to_string());
        }

        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }

    println!("Remote: {}", name);
    println!("  Path:        {}", remote.path);
    if let Some(desc) = &remote.description {
        println!("  Description: {}", desc);
    }
    println!(
        "  Last sync:   {}",
        remote.last_sync.as_deref().unwrap_or("never")
    );

    match store_result {
        Ok(store) if store.is_valid() => {
            println!("  Store:       {}", store.store_path().display());
            let roles = store.load_roles().unwrap_or_default();
            let motivations = store.load_tradeoffs().unwrap_or_default();
            let agents = store.load_agents().unwrap_or_default();
            let evaluations = store.load_evaluations().unwrap_or_default();
            println!(
                "  Entities:    {} roles, {} motivations, {} agents, {} evaluations",
                roles.len(),
                motivations.len(),
                agents.len(),
                evaluations.len()
            );
        }
        Ok(store) => {
            println!(
                "  Store:       {} (not a valid agency store)",
                store.store_path().display()
            );
        }
        Err(e) => {
            println!("  Store:       inaccessible ({})", e);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::agency::LocalStore;

    fn setup_workgraph_dir(tmp: &TempDir) -> std::path::PathBuf {
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        wg_dir
    }

    fn setup_remote_store(tmp: &TempDir, name: &str) -> LocalStore {
        let path = tmp.path().join(name).join("agency");
        workgraph::agency::init(&path).unwrap();
        LocalStore::new(path)
    }

    #[test]
    fn add_and_list_remote() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let store = setup_remote_store(&tmp, "remote-store");

        run_add(
            &wg_dir,
            "upstream",
            store.store_path().to_str().unwrap(),
            Some("test remote"),
        )
        .unwrap();

        let config = federation::load_federation_config(&wg_dir).unwrap();
        assert_eq!(config.remotes.len(), 1);
        assert!(config.remotes.contains_key("upstream"));
        assert_eq!(
            config.remotes["upstream"].description.as_deref(),
            Some("test remote")
        );
        assert!(config.remotes["upstream"].last_sync.is_none());
    }

    #[test]
    fn add_duplicate_remote_fails() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let store = setup_remote_store(&tmp, "remote-store");

        run_add(&wg_dir, "upstream", store.store_path().to_str().unwrap(), None).unwrap();
        let result = run_add(&wg_dir, "upstream", "/other/path", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn remove_remote() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        run_add(&wg_dir, "upstream", "/some/path", None).unwrap();
        run_remove(&wg_dir, "upstream").unwrap();

        let config = federation::load_federation_config(&wg_dir).unwrap();
        assert!(config.remotes.is_empty());
    }

    #[test]
    fn remove_nonexistent_remote_fails() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        let result = run_remove(&wg_dir, "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn show_remote_with_valid_store() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let store = setup_remote_store(&tmp, "remote-store");

        // Add some entities
        use workgraph::agency::{Lineage, PerformanceRecord, Role};
        store
            .save_role(&Role {
                id: "r1".to_string(),
                name: "test-role".to_string(),
                description: "test".to_string(),
                component_ids: Vec::new(),
                outcome_id: "test".to_string(),
                performance: PerformanceRecord::default(),
                lineage: Lineage::default(),
                default_context_scope: None,
            })
            .unwrap();

        run_add(
            &wg_dir,
            "upstream",
            store.store_path().to_str().unwrap(),
            None,
        )
        .unwrap();

        // Just verify show doesn't error
        run_show(&wg_dir, "upstream", false).unwrap();
        run_show(&wg_dir, "upstream", true).unwrap();
    }

    #[test]
    fn show_nonexistent_remote_fails() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        let result = run_show(&wg_dir, "nonexistent", false);
        assert!(result.is_err());
    }

    #[test]
    fn add_remote_with_inaccessible_path_warns_but_succeeds() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        // Path doesn't exist, but should still succeed
        run_add(&wg_dir, "faraway", "/nonexistent/path/to/agency", None).unwrap();

        let config = federation::load_federation_config(&wg_dir).unwrap();
        assert!(config.remotes.contains_key("faraway"));
    }

    #[test]
    fn list_empty_remotes() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);

        // Should not error even with no federation.yaml
        run_list(&wg_dir, false).unwrap();
        run_list(&wg_dir, true).unwrap();
    }

    #[test]
    fn resolve_store_with_remotes_uses_named_remote() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let store = setup_remote_store(&tmp, "remote-store");

        run_add(
            &wg_dir,
            "upstream",
            store.store_path().to_str().unwrap(),
            None,
        )
        .unwrap();

        let resolved = federation::resolve_store_with_remotes("upstream", &wg_dir).unwrap();
        assert_eq!(resolved.store_path(), store.store_path());
    }

    #[test]
    fn resolve_store_with_remotes_falls_back_to_path() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = setup_workgraph_dir(&tmp);
        let store = setup_remote_store(&tmp, "remote-store");

        // No remote named "upstream", should resolve as path
        let resolved = federation::resolve_store_with_remotes(
            store.store_path().to_str().unwrap(),
            &wg_dir,
        )
        .unwrap();
        assert_eq!(resolved.store_path(), store.store_path());
    }
}

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use workgraph::graph::{Node, Resource};
use workgraph::parser::load_graph;

use super::graph_path;

pub fn run_add(
    dir: &Path,
    id: &str,
    name: Option<&str>,
    resource_type: Option<&str>,
    available: Option<f64>,
    unit: Option<&str>,
) -> Result<()> {
    let path = graph_path(dir);

    // Load existing graph to check for ID conflicts
    let graph = if path.exists() {
        load_graph(&path).context("Failed to load graph")?
    } else {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    };

    // Check for ID conflicts
    if graph.get_node(id).is_some() {
        anyhow::bail!("Node with ID '{}' already exists", id);
    }

    let resource = Resource {
        id: id.to_string(),
        name: name.map(String::from),
        resource_type: resource_type.map(String::from),
        available,
        unit: unit.map(String::from),
    };

    // Append to file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open graph.jsonl")?;

    let json = serde_json::to_string(&Node::Resource(resource)).context("Failed to serialize resource")?;
    writeln!(file, "{}", json).context("Failed to write resource")?;

    let display_name = name.unwrap_or(id);
    println!("Added resource: {} ({})", display_name, id);
    Ok(())
}

pub fn run_list(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let resources: Vec<_> = graph.resources().collect();

    if json {
        let output: Vec<_> = resources
            .iter()
            .map(|r| serde_json::json!({
                "id": r.id,
                "name": r.name,
                "type": r.resource_type,
                "available": r.available,
                "unit": r.unit,
            }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if resources.is_empty() {
            println!("No resources found");
        } else {
            for r in resources {
                let name_str = r.name.as_deref().unwrap_or(&r.id);
                let type_str = r.resource_type.as_deref().unwrap_or("unknown");
                let avail_str = match (&r.available, &r.unit) {
                    (Some(avail), Some(unit)) => format!("{} {}", avail, unit),
                    (Some(avail), None) => format!("{}", avail),
                    _ => "N/A".to_string(),
                };
                println!("[{}] {} - {} ({})", type_str, r.id, name_str, avail_str);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_workgraph() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let graph_path = temp_dir.path().join("graph.jsonl");
        fs::write(&graph_path, "").unwrap();
        temp_dir
    }

    #[test]
    fn test_add_resource_basic() {
        let temp_dir = setup_workgraph();

        let result = run_add(
            temp_dir.path(),
            "budget-q1",
            None,
            None,
            None,
            None,
        );

        assert!(result.is_ok());

        // Verify resource was added
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let resource = graph.get_resource("budget-q1");
        assert!(resource.is_some());
        assert_eq!(resource.unwrap().id, "budget-q1");
    }

    #[test]
    fn test_add_resource_with_all_options() {
        let temp_dir = setup_workgraph();

        let result = run_add(
            temp_dir.path(),
            "budget-q1",
            Some("Q1 Budget"),
            Some("money"),
            Some(50000.0),
            Some("usd"),
        );

        assert!(result.is_ok());

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let resource = graph.get_resource("budget-q1").unwrap();
        assert_eq!(resource.id, "budget-q1");
        assert_eq!(resource.name, Some("Q1 Budget".to_string()));
        assert_eq!(resource.resource_type, Some("money".to_string()));
        assert_eq!(resource.available, Some(50000.0));
        assert_eq!(resource.unit, Some("usd".to_string()));
    }

    #[test]
    fn test_add_resource_duplicate_id_fails() {
        let temp_dir = setup_workgraph();

        // Add first resource
        run_add(temp_dir.path(), "budget", None, None, None, None).unwrap();

        // Try to add duplicate
        let result = run_add(temp_dir.path(), "budget", None, None, None, None);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_add_resource_uninitialized_fails() {
        let temp_dir = TempDir::new().unwrap();

        let result = run_add(temp_dir.path(), "budget", None, None, None, None);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_list_resources_empty() {
        let temp_dir = setup_workgraph();

        let result = run_list(temp_dir.path(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_resources_with_data() {
        let temp_dir = setup_workgraph();

        // Add some resources
        run_add(
            temp_dir.path(),
            "budget-q1",
            Some("Q1 Budget"),
            Some("money"),
            Some(50000.0),
            Some("usd"),
        ).unwrap();

        run_add(
            temp_dir.path(),
            "gpu-cluster",
            Some("GPU Cluster"),
            Some("compute"),
            Some(100.0),
            Some("gpu-hours"),
        ).unwrap();

        let result = run_list(temp_dir.path(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_resources_json() {
        let temp_dir = setup_workgraph();

        run_add(
            temp_dir.path(),
            "budget",
            Some("Budget"),
            Some("money"),
            Some(1000.0),
            Some("usd"),
        ).unwrap();

        let result = run_list(temp_dir.path(), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_resources_uninitialized_fails() {
        let temp_dir = TempDir::new().unwrap();

        let result = run_list(temp_dir.path(), false);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }
}

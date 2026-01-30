use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use workgraph::graph::{Actor, ActorType, Node, TrustLevel};
use workgraph::parser::load_graph;

use super::graph_path;

fn parse_trust_level(s: &str) -> Result<TrustLevel> {
    match s.to_lowercase().as_str() {
        "verified" => Ok(TrustLevel::Verified),
        "provisional" => Ok(TrustLevel::Provisional),
        "unknown" => Ok(TrustLevel::Unknown),
        _ => anyhow::bail!("Invalid trust level: '{}'. Use 'verified', 'provisional', or 'unknown'", s),
    }
}

fn parse_actor_type(s: &str) -> Result<ActorType> {
    match s.to_lowercase().as_str() {
        "agent" => Ok(ActorType::Agent),
        "human" => Ok(ActorType::Human),
        _ => anyhow::bail!("Invalid actor type: '{}'. Use 'agent' or 'human'", s),
    }
}

pub fn run_add(
    dir: &Path,
    id: &str,
    name: Option<&str>,
    role: Option<&str>,
    rate: Option<f64>,
    capacity: Option<f64>,
    capabilities: &[String],
    context_limit: Option<u64>,
    trust_level: Option<&str>,
    actor_type: Option<&str>,
    matrix_user_id: Option<&str>,
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

    // Check for Matrix ID conflicts
    if let Some(matrix_id) = matrix_user_id {
        if graph.get_actor_by_matrix_id(matrix_id).is_some() {
            anyhow::bail!("An actor with Matrix ID '{}' already exists", matrix_id);
        }
    }

    let trust = match trust_level {
        Some(s) => parse_trust_level(s)?,
        None => TrustLevel::Provisional,
    };

    let atype = match actor_type {
        Some(s) => parse_actor_type(s)?,
        None => {
            // Default to Human if --matrix is provided, otherwise Agent
            if matrix_user_id.is_some() {
                ActorType::Human
            } else {
                ActorType::Agent
            }
        }
    };

    // Validate: matrix_user_id should only be set for human actors
    if matrix_user_id.is_some() && atype == ActorType::Agent {
        anyhow::bail!("Matrix user ID can only be set for human actors. Use --type human or omit --type");
    }

    let actor = Actor {
        id: id.to_string(),
        name: name.map(String::from),
        role: role.map(String::from),
        rate,
        capacity,
        capabilities: capabilities.to_vec(),
        context_limit,
        trust_level: trust,
        last_seen: None,
        actor_type: atype.clone(),
        matrix_user_id: matrix_user_id.map(String::from),
        response_times: vec![],
    };

    // Append to file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open graph.jsonl")?;

    let json = serde_json::to_string(&Node::Actor(actor)).context("Failed to serialize actor")?;
    writeln!(file, "{}", json).context("Failed to write actor")?;

    let display_name = name.unwrap_or(id);
    let type_str = match atype {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
    };
    println!("Added {} actor: {} ({})", type_str, display_name, id);
    if let Some(matrix_id) = matrix_user_id {
        println!("  Matrix: {}", matrix_id);
    }
    if !capabilities.is_empty() {
        println!("  Capabilities: {}", capabilities.join(", "));
    }
    Ok(())
}

pub fn run_list(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let actors: Vec<_> = graph.actors().collect();

    if json {
        let output: Vec<_> = actors
            .iter()
            .map(|a| serde_json::json!({
                "id": a.id,
                "name": a.name,
                "role": a.role,
                "rate": a.rate,
                "capacity": a.capacity,
                "capabilities": a.capabilities,
                "context_limit": a.context_limit,
                "trust_level": a.trust_level,
                "last_seen": a.last_seen,
                "actor_type": a.actor_type,
                "matrix_user_id": a.matrix_user_id,
            }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if actors.is_empty() {
            println!("No actors found");
        } else {
            for actor in actors {
                let name = actor.name.as_deref().unwrap_or(&actor.id);
                let type_str = match actor.actor_type {
                    ActorType::Human => " [human]",
                    ActorType::Agent => "",
                };
                let role_str = actor.role.as_ref().map(|r| format!(" [{}]", r)).unwrap_or_default();
                let matrix_str = actor.matrix_user_id.as_ref().map(|m| format!(" <{}>", m)).unwrap_or_default();
                let caps_str = if actor.capabilities.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", actor.capabilities.join(", "))
                };
                println!("{} - {}{}{}{}{}", actor.id, name, type_str, role_str, matrix_str, caps_str);
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
    fn test_add_actor_minimal() {
        let temp_dir = setup_workgraph();

        let result = run_add(
            temp_dir.path(),
            "erik",
            None,
            None,
            None,
            None,
            &[],
            None,
            None,
            None,
            None,
        );

        assert!(result.is_ok());

        // Verify actor was added
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let actor = graph.get_actor("erik").unwrap();
        assert_eq!(actor.id, "erik");
        assert!(actor.name.is_none());
        assert_eq!(actor.actor_type, ActorType::Agent);
    }

    #[test]
    fn test_add_actor_with_all_fields() {
        let temp_dir = setup_workgraph();

        let result = run_add(
            temp_dir.path(),
            "erik",
            Some("Erik Smith"),
            Some("engineer"),
            Some(150.0),
            Some(40.0),
            &["rust".to_string(), "testing".to_string()],
            Some(100000),
            Some("verified"),
            None,
            None,
        );

        assert!(result.is_ok());

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let actor = graph.get_actor("erik").unwrap();
        assert_eq!(actor.id, "erik");
        assert_eq!(actor.name, Some("Erik Smith".to_string()));
        assert_eq!(actor.role, Some("engineer".to_string()));
        assert_eq!(actor.rate, Some(150.0));
        assert_eq!(actor.capacity, Some(40.0));
        assert_eq!(actor.capabilities, vec!["rust", "testing"]);
        assert_eq!(actor.context_limit, Some(100000));
        assert_eq!(actor.trust_level, TrustLevel::Verified);
    }

    #[test]
    fn test_add_human_actor_with_matrix() {
        let temp_dir = setup_workgraph();

        let result = run_add(
            temp_dir.path(),
            "erik",
            Some("Erik"),
            None,
            None,
            None,
            &[],
            None,
            None,
            Some("human"),
            Some("@erik:matrix.org"),
        );

        assert!(result.is_ok());

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let actor = graph.get_actor("erik").unwrap();
        assert_eq!(actor.actor_type, ActorType::Human);
        assert_eq!(actor.matrix_user_id, Some("@erik:matrix.org".to_string()));
    }

    #[test]
    fn test_add_human_actor_inferred_from_matrix() {
        let temp_dir = setup_workgraph();

        // When --matrix is provided without --type, it should default to human
        let result = run_add(
            temp_dir.path(),
            "erik",
            Some("Erik"),
            None,
            None,
            None,
            &[],
            None,
            None,
            None, // No type specified
            Some("@erik:matrix.org"),
        );

        assert!(result.is_ok());

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let actor = graph.get_actor("erik").unwrap();
        assert_eq!(actor.actor_type, ActorType::Human);
    }

    #[test]
    fn test_agent_with_matrix_fails() {
        let temp_dir = setup_workgraph();

        // Setting --matrix with --type agent should fail
        let result = run_add(
            temp_dir.path(),
            "bot",
            None,
            None,
            None,
            None,
            &[],
            None,
            None,
            Some("agent"),
            Some("@bot:matrix.org"),
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("human actors"));
    }

    #[test]
    fn test_duplicate_matrix_id_fails() {
        let temp_dir = setup_workgraph();

        // Add first actor with matrix ID
        run_add(
            temp_dir.path(),
            "erik",
            None,
            None,
            None,
            None,
            &[],
            None,
            None,
            Some("human"),
            Some("@erik:matrix.org"),
        ).unwrap();

        // Try to add another actor with the same matrix ID
        let result = run_add(
            temp_dir.path(),
            "erik2",
            None,
            None,
            None,
            None,
            &[],
            None,
            None,
            Some("human"),
            Some("@erik:matrix.org"),
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_add_actor_duplicate_id_fails() {
        let temp_dir = setup_workgraph();

        // Add first actor
        run_add(temp_dir.path(), "erik", None, None, None, None, &[], None, None, None, None).unwrap();

        // Try to add duplicate
        let result = run_add(temp_dir.path(), "erik", None, None, None, None, &[], None, None, None, None);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_add_actor_without_init_fails() {
        let temp_dir = TempDir::new().unwrap();

        let result = run_add(temp_dir.path(), "erik", None, None, None, None, &[], None, None, None, None);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_list_actors_empty() {
        let temp_dir = setup_workgraph();

        let result = run_list(temp_dir.path(), false);

        assert!(result.is_ok());
    }

    #[test]
    fn test_list_actors_multiple() {
        let temp_dir = setup_workgraph();

        run_add(temp_dir.path(), "erik", Some("Erik"), Some("engineer"), None, None, &[], None, None, None, None).unwrap();
        run_add(temp_dir.path(), "alice", Some("Alice"), Some("pm"), None, None, &[], None, None, None, None).unwrap();

        let result = run_list(temp_dir.path(), false);

        assert!(result.is_ok());
    }

    #[test]
    fn test_list_actors_json() {
        let temp_dir = setup_workgraph();

        run_add(temp_dir.path(), "erik", Some("Erik"), Some("engineer"), Some(100.0), Some(40.0), &["rust".to_string()], Some(50000), Some("verified"), None, None).unwrap();

        let result = run_list(temp_dir.path(), true);

        assert!(result.is_ok());
    }

    #[test]
    fn test_list_without_init_fails() {
        let temp_dir = TempDir::new().unwrap();

        let result = run_list(temp_dir.path(), false);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }
}

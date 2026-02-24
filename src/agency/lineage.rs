use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::store::{load_all_motivations, load_all_roles, AgencyError};
use super::types::*;

/// Build the ancestry tree for a role by walking parent_ids.
/// Returns nodes ordered from the target (first) up to its oldest ancestors.
pub fn role_ancestry(role_id: &str, roles_dir: &Path) -> Result<Vec<AncestryNode>, AgencyError> {
    let all_roles = load_all_roles(roles_dir)?;
    let role_map: HashMap<String, &Role> = all_roles.iter().map(|r| (r.id.clone(), r)).collect();
    let mut ancestry = Vec::new();
    let mut queue = vec![role_id.to_string()];
    let mut visited = HashSet::new();

    while let Some(id) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some(role) = role_map.get(&id) {
            ancestry.push(AncestryNode {
                id: role.id.clone(),
                name: role.name.clone(),
                generation: role.lineage.generation,
                created_by: role.lineage.created_by.clone(),
                created_at: role.lineage.created_at,
                parent_ids: role.lineage.parent_ids.clone(),
            });
            for parent in &role.lineage.parent_ids {
                queue.push(parent.clone());
            }
        }
    }
    Ok(ancestry)
}

/// Build the ancestry tree for a motivation by walking parent_ids.
pub fn motivation_ancestry(
    motivation_id: &str,
    motivations_dir: &Path,
) -> Result<Vec<AncestryNode>, AgencyError> {
    let all = load_all_motivations(motivations_dir)?;
    let map: HashMap<String, &Motivation> = all.iter().map(|m| (m.id.clone(), m)).collect();
    let mut ancestry = Vec::new();
    let mut queue = vec![motivation_id.to_string()];
    let mut visited = HashSet::new();

    while let Some(id) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some(m) = map.get(&id) {
            ancestry.push(AncestryNode {
                id: m.id.clone(),
                name: m.name.clone(),
                generation: m.lineage.generation,
                created_by: m.lineage.created_by.clone(),
                created_at: m.lineage.created_at,
                parent_ids: m.lineage.parent_ids.clone(),
            });
            for parent in &m.lineage.parent_ids {
                queue.push(parent.clone());
            }
        }
    }
    Ok(ancestry)
}

#[cfg(test)]
mod tests {
    use super::super::starters::build_role;
    use super::super::store::save_role;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_role_ancestry_tree() {
        let tmp = TempDir::new().unwrap();
        let roles_dir = tmp.path();

        // Create a 3-generation lineage: grandparent -> parent -> child
        let grandparent = build_role("Grandparent", "Gen 0", vec![], "Outcome GP");
        let gp_id = grandparent.id.clone();

        let mut parent = build_role("Parent", "Gen 1", vec![], "Outcome P");
        parent.lineage = Lineage::mutation(&gp_id, 0, "run-1");
        let p_id = parent.id.clone();

        let mut child = build_role("Child", "Gen 2", vec![], "Outcome C");
        child.lineage = Lineage::mutation(&p_id, 1, "run-2");
        let c_id = child.id.clone();

        save_role(&grandparent, roles_dir).unwrap();
        save_role(&parent, roles_dir).unwrap();
        save_role(&child, roles_dir).unwrap();

        let ancestry = role_ancestry(&c_id, roles_dir).unwrap();
        assert_eq!(ancestry.len(), 3);
        // First should be the child itself
        assert_eq!(ancestry[0].id, c_id);
        assert_eq!(ancestry[0].generation, 2);
        // Then parent
        assert_eq!(ancestry[1].id, p_id);
        assert_eq!(ancestry[1].generation, 1);
        // Then grandparent
        assert_eq!(ancestry[2].id, gp_id);
        assert_eq!(ancestry[2].generation, 0);
    }

    #[test]
    fn test_crossover_ancestry() {
        let tmp = TempDir::new().unwrap();
        let roles_dir = tmp.path();

        let p1 = build_role("Parent 1", "First parent", vec![], "Outcome P1");
        let p1_id = p1.id.clone();
        let p2 = build_role("Parent 2", "Second parent", vec![], "Outcome P2");
        let p2_id = p2.id.clone();

        let mut child = build_role(
            "Crossover Child",
            "Child from crossover",
            vec![],
            "Outcome XC",
        );
        child.lineage = Lineage::crossover(&[&p1_id, &p2_id], 0, "run-x");
        let child_id = child.id.clone();

        save_role(&p1, roles_dir).unwrap();
        save_role(&p2, roles_dir).unwrap();
        save_role(&child, roles_dir).unwrap();

        let ancestry = role_ancestry(&child_id, roles_dir).unwrap();
        assert_eq!(ancestry.len(), 3);
        assert_eq!(ancestry[0].id, child_id);
        // Both parents should be present (order depends on queue processing)
        let parent_ids: Vec<&str> = ancestry[1..].iter().map(|n| n.id.as_str()).collect();
        assert!(parent_ids.contains(&p1_id.as_str()));
        assert!(parent_ids.contains(&p2_id.as_str()));
    }
}

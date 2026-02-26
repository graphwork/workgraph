use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::store::{
    load_all_components, load_all_outcomes, load_all_roles,
    load_all_tradeoffs, AgencyError,
};
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

/// Build the ancestry tree for a role component by walking parent_ids.
///
/// `components_dir` should point to `agency/primitives/components/`.
pub fn component_ancestry(
    component_id: &str,
    components_dir: &Path,
) -> Result<Vec<AncestryNode>, AgencyError> {
    let all = load_all_components(components_dir)?;
    let map: HashMap<String, &RoleComponent> =
        all.iter().map(|c| (c.id.clone(), c)).collect();
    let mut ancestry = Vec::new();
    let mut queue = vec![component_id.to_string()];
    let mut visited = HashSet::new();

    while let Some(id) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some(c) = map.get(&id) {
            ancestry.push(AncestryNode {
                id: c.id.clone(),
                name: c.name.clone(),
                generation: c.lineage.generation,
                created_by: c.lineage.created_by.clone(),
                created_at: c.lineage.created_at,
                parent_ids: c.lineage.parent_ids.clone(),
            });
            for parent in &c.lineage.parent_ids {
                queue.push(parent.clone());
            }
        }
    }
    Ok(ancestry)
}

/// Build the ancestry tree for a desired outcome by walking parent_ids.
///
/// `outcomes_dir` should point to `agency/primitives/outcomes/`.
pub fn outcome_ancestry(
    outcome_id: &str,
    outcomes_dir: &Path,
) -> Result<Vec<AncestryNode>, AgencyError> {
    let all = load_all_outcomes(outcomes_dir)?;
    let map: HashMap<String, &DesiredOutcome> =
        all.iter().map(|o| (o.id.clone(), o)).collect();
    let mut ancestry = Vec::new();
    let mut queue = vec![outcome_id.to_string()];
    let mut visited = HashSet::new();

    while let Some(id) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some(o) = map.get(&id) {
            ancestry.push(AncestryNode {
                id: o.id.clone(),
                name: o.name.clone(),
                generation: o.lineage.generation,
                created_by: o.lineage.created_by.clone(),
                created_at: o.lineage.created_at,
                parent_ids: o.lineage.parent_ids.clone(),
            });
            for parent in &o.lineage.parent_ids {
                queue.push(parent.clone());
            }
        }
    }
    Ok(ancestry)
}

/// Build the ancestry tree for a trade-off configuration by walking parent_ids.
///
/// `tradeoffs_dir` should point to `agency/primitives/tradeoffs/`.
pub fn tradeoff_ancestry(
    tradeoff_id: &str,
    tradeoffs_dir: &Path,
) -> Result<Vec<AncestryNode>, AgencyError> {
    let all = load_all_tradeoffs(tradeoffs_dir)?;
    let map: HashMap<String, &TradeoffConfig> =
        all.iter().map(|t| (t.id.clone(), t)).collect();
    let mut ancestry = Vec::new();
    let mut queue = vec![tradeoff_id.to_string()];
    let mut visited = HashSet::new();

    while let Some(id) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        if let Some(t) = map.get(&id) {
            ancestry.push(AncestryNode {
                id: t.id.clone(),
                name: t.name.clone(),
                generation: t.lineage.generation,
                created_by: t.lineage.created_by.clone(),
                created_at: t.lineage.created_at,
                parent_ids: t.lineage.parent_ids.clone(),
            });
            for parent in &t.lineage.parent_ids {
                queue.push(parent.clone());
            }
        }
    }
    Ok(ancestry)
}

#[cfg(test)]
mod tests {
    use super::super::starters::{build_component, build_outcome, build_role, build_tradeoff};
    use super::super::store::{save_component, save_outcome, save_role, save_tradeoff};
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

    #[test]
    fn test_component_ancestry_tree() {
        let tmp = TempDir::new().unwrap();
        let components_dir = tmp.path().join("primitives/components");

        let grandparent = build_component(
            "GP Component",
            "Grandparent capability",
            ComponentCategory::Translated,
            ContentRef::Name("rust".into()),
        );
        let gp_id = grandparent.id.clone();

        let mut parent = build_component(
            "Parent Component",
            "Parent capability",
            ComponentCategory::Enhanced,
            ContentRef::Name("rust-enhanced".into()),
        );
        parent.lineage = Lineage::mutation(&gp_id, 0, "run-1");
        let p_id = parent.id.clone();

        let mut child = build_component(
            "Child Component",
            "Child capability",
            ComponentCategory::Novel,
            ContentRef::Inline("Novel machine capability".into()),
        );
        child.lineage = Lineage::mutation(&p_id, 1, "run-2");
        let c_id = child.id.clone();

        save_component(&grandparent, &components_dir).unwrap();
        save_component(&parent, &components_dir).unwrap();
        save_component(&child, &components_dir).unwrap();

        let ancestry = component_ancestry(&c_id, &components_dir).unwrap();
        assert_eq!(ancestry.len(), 3);
        assert_eq!(ancestry[0].id, c_id);
        assert_eq!(ancestry[0].generation, 2);
        assert_eq!(ancestry[1].id, p_id);
        assert_eq!(ancestry[1].generation, 1);
        assert_eq!(ancestry[2].id, gp_id);
        assert_eq!(ancestry[2].generation, 0);
    }

    #[test]
    fn test_outcome_ancestry_tree() {
        let tmp = TempDir::new().unwrap();
        let outcomes_dir = tmp.path().join("primitives/outcomes");

        let grandparent = build_outcome("GP Outcome", "Grandparent success definition", vec![]);
        let gp_id = grandparent.id.clone();

        let mut parent = build_outcome(
            "Parent Outcome",
            "Parent success definition",
            vec!["All tests pass".into()],
        );
        parent.lineage = Lineage::mutation(&gp_id, 0, "run-1");
        let p_id = parent.id.clone();

        let mut child = build_outcome(
            "Child Outcome",
            "Child success definition",
            vec!["All tests pass".into(), "Coverage > 80%".into()],
        );
        child.lineage = Lineage::mutation(&p_id, 1, "run-2");
        let c_id = child.id.clone();

        save_outcome(&grandparent, &outcomes_dir).unwrap();
        save_outcome(&parent, &outcomes_dir).unwrap();
        save_outcome(&child, &outcomes_dir).unwrap();

        let ancestry = outcome_ancestry(&c_id, &outcomes_dir).unwrap();
        assert_eq!(ancestry.len(), 3);
        assert_eq!(ancestry[0].id, c_id);
        assert_eq!(ancestry[0].generation, 2);
        assert_eq!(ancestry[1].id, p_id);
        assert_eq!(ancestry[1].generation, 1);
        assert_eq!(ancestry[2].id, gp_id);
        assert_eq!(ancestry[2].generation, 0);
    }

    #[test]
    fn test_tradeoff_ancestry_tree() {
        let tmp = TempDir::new().unwrap();
        let tradeoffs_dir = tmp.path().join("primitives/tradeoffs");

        let grandparent = build_tradeoff(
            "GP Tradeoff",
            "Grandparent trade-off config",
            vec!["Slow is fine".into()],
            vec!["Broken output".into()],
        );
        let gp_id = grandparent.id.clone();

        let mut parent = build_tradeoff(
            "Parent Tradeoff",
            "Parent trade-off config",
            vec!["Slow is fine".into(), "Verbose OK".into()],
            vec!["Broken output".into()],
        );
        parent.lineage = Lineage::mutation(&gp_id, 0, "run-1");
        let p_id = parent.id.clone();

        let mut child = build_tradeoff(
            "Child Tradeoff",
            "Child trade-off config",
            vec!["Slow is fine".into(), "Verbose OK".into()],
            vec!["Broken output".into(), "Skipped tests".into()],
        );
        child.lineage = Lineage::mutation(&p_id, 1, "run-2");
        let c_id = child.id.clone();

        save_tradeoff(&grandparent, &tradeoffs_dir).unwrap();
        save_tradeoff(&parent, &tradeoffs_dir).unwrap();
        save_tradeoff(&child, &tradeoffs_dir).unwrap();

        let ancestry = tradeoff_ancestry(&c_id, &tradeoffs_dir).unwrap();
        assert_eq!(ancestry.len(), 3);
        assert_eq!(ancestry[0].id, c_id);
        assert_eq!(ancestry[0].generation, 2);
        assert_eq!(ancestry[1].id, p_id);
        assert_eq!(ancestry[1].generation, 1);
        assert_eq!(ancestry[2].id, gp_id);
        assert_eq!(ancestry[2].generation, 0);
    }

    #[test]
    fn test_component_ancestry_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let components_dir = tmp.path().join("primitives/components");
        // Dir doesn't exist — should return empty vec, not error
        let ancestry = component_ancestry("nonexistent-id", &components_dir).unwrap();
        assert!(ancestry.is_empty());
    }

    #[test]
    fn test_outcome_ancestry_unknown_id() {
        let tmp = TempDir::new().unwrap();
        let outcomes_dir = tmp.path().join("primitives/outcomes");
        let outcome = build_outcome("An Outcome", "Some description", vec![]);
        save_outcome(&outcome, &outcomes_dir).unwrap();

        // Query for an ID that doesn't exist — should return empty vec
        let ancestry = outcome_ancestry("nonexistent-id", &outcomes_dir).unwrap();
        assert!(ancestry.is_empty());
    }
}

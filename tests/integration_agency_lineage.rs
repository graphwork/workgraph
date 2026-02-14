//! Integration tests for lineage tracking and ancestry queries.
//!
//! Covers:
//! 1. Role with no parents (generation 0, manual creation)
//! 2. Role created via mutation (single parent, generation 1)
//! 3. Chain of 3+ mutations to verify deep ancestry walking
//! 4. Crossover with 2 parents to verify both parents appear in ancestry
//! 5. Generation numbers increment correctly through chains
//! 6. role_ancestry and motivation_ancestry with missing intermediate parents (orphan resilience)
//! 7. AncestryNode output format

use tempfile::TempDir;

use workgraph::agency::{self, Lineage, SkillRef};

/// Helper: set up agency storage and return (tmp, agency_dir).
fn setup() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let agency_dir = tmp.path().join("agency");
    agency::init(&agency_dir).unwrap();
    (tmp, agency_dir)
}

// ---------------------------------------------------------------------------
// 1. Role with no parents (generation 0, manual creation)
// ---------------------------------------------------------------------------

#[test]
fn test_role_no_parents_generation_zero() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    let role = agency::build_role("Root Role", "A manually created role", vec![], "Outcome");
    agency::save_role(&role, &roles_dir).unwrap();

    // Default lineage: no parents, generation 0, created by "human"
    assert!(role.lineage.parent_ids.is_empty());
    assert_eq!(role.lineage.generation, 0);
    assert_eq!(role.lineage.created_by, "human");

    // Ancestry should contain only the role itself
    let ancestry = agency::role_ancestry(&role.id, &roles_dir).unwrap();
    assert_eq!(ancestry.len(), 1);
    assert_eq!(ancestry[0].id, role.id);
    assert_eq!(ancestry[0].generation, 0);
    assert!(ancestry[0].parent_ids.is_empty());
    assert_eq!(ancestry[0].created_by, "human");
}

#[test]
fn test_motivation_no_parents_generation_zero() {
    let (_tmp, agency_dir) = setup();
    let motivations_dir = agency_dir.join("motivations");

    let mot = agency::build_motivation("Root Motivation", "Manual", vec![], vec![]);
    agency::save_motivation(&mot, &motivations_dir).unwrap();

    assert!(mot.lineage.parent_ids.is_empty());
    assert_eq!(mot.lineage.generation, 0);
    assert_eq!(mot.lineage.created_by, "human");

    let ancestry = agency::motivation_ancestry(&mot.id, &motivations_dir).unwrap();
    assert_eq!(ancestry.len(), 1);
    assert_eq!(ancestry[0].id, mot.id);
    assert_eq!(ancestry[0].generation, 0);
}

// ---------------------------------------------------------------------------
// 2. Role created via mutation (single parent, generation 1)
// ---------------------------------------------------------------------------

#[test]
fn test_role_mutation_single_parent() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    // Parent role (gen 0)
    let parent = agency::build_role("Parent", "Original role", vec![], "Outcome A");
    agency::save_role(&parent, &roles_dir).unwrap();

    // Child role via mutation (gen 1)
    let mut child = agency::build_role("Child", "Mutated role", vec![], "Outcome B");
    child.lineage = Lineage::mutation(&parent.id, parent.lineage.generation, "run-1");
    agency::save_role(&child, &roles_dir).unwrap();

    assert_eq!(child.lineage.parent_ids, vec![parent.id.clone()]);
    assert_eq!(child.lineage.generation, 1);
    assert_eq!(child.lineage.created_by, "evolver-run-1");

    let ancestry = agency::role_ancestry(&child.id, &roles_dir).unwrap();
    assert_eq!(ancestry.len(), 2);
    // First node is the target itself
    assert_eq!(ancestry[0].id, child.id);
    assert_eq!(ancestry[0].generation, 1);
    assert_eq!(ancestry[0].parent_ids, vec![parent.id.clone()]);
    // Second node is the parent
    assert_eq!(ancestry[1].id, parent.id);
    assert_eq!(ancestry[1].generation, 0);
    assert!(ancestry[1].parent_ids.is_empty());
}

#[test]
fn test_motivation_mutation_single_parent() {
    let (_tmp, agency_dir) = setup();
    let motivations_dir = agency_dir.join("motivations");

    let parent = agency::build_motivation("Parent Mot", "Original", vec![], vec![]);
    agency::save_motivation(&parent, &motivations_dir).unwrap();

    let mut child = agency::build_motivation("Child Mot", "Mutated", vec![], vec![]);
    child.lineage = Lineage::mutation(&parent.id, parent.lineage.generation, "run-2");
    agency::save_motivation(&child, &motivations_dir).unwrap();

    assert_eq!(child.lineage.parent_ids, vec![parent.id.clone()]);
    assert_eq!(child.lineage.generation, 1);

    let ancestry = agency::motivation_ancestry(&child.id, &motivations_dir).unwrap();
    assert_eq!(ancestry.len(), 2);
    assert_eq!(ancestry[0].id, child.id);
    assert_eq!(ancestry[1].id, parent.id);
}

// ---------------------------------------------------------------------------
// 3. Chain of 3+ mutations: deep ancestry walking
// ---------------------------------------------------------------------------

#[test]
fn test_role_deep_ancestry_chain() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    // Create a chain: gen0 -> gen1 -> gen2 -> gen3
    let gen0 = agency::build_role("Gen0", "root", vec![], "outcome-0");
    agency::save_role(&gen0, &roles_dir).unwrap();

    let mut gen1 = agency::build_role("Gen1", "first mutation", vec![], "outcome-1");
    gen1.lineage = Lineage::mutation(&gen0.id, 0, "evo-1");
    agency::save_role(&gen1, &roles_dir).unwrap();

    let mut gen2 = agency::build_role(
        "Gen2",
        "second mutation",
        vec![SkillRef::Name("extra".to_string())],
        "outcome-2",
    );
    gen2.lineage = Lineage::mutation(&gen1.id, 1, "evo-2");
    agency::save_role(&gen2, &roles_dir).unwrap();

    let mut gen3 = agency::build_role("Gen3", "third mutation", vec![], "outcome-3");
    gen3.lineage = Lineage::mutation(&gen2.id, 2, "evo-3");
    agency::save_role(&gen3, &roles_dir).unwrap();

    let ancestry = agency::role_ancestry(&gen3.id, &roles_dir).unwrap();
    assert_eq!(ancestry.len(), 4, "Should walk entire chain of 4 roles");

    // Verify all generations present
    let ids: Vec<&str> = ancestry.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&gen0.id.as_str()));
    assert!(ids.contains(&gen1.id.as_str()));
    assert!(ids.contains(&gen2.id.as_str()));
    assert!(ids.contains(&gen3.id.as_str()));

    // Verify generation numbers
    let gens: Vec<u32> = ancestry.iter().map(|n| n.generation).collect();
    assert!(gens.contains(&0));
    assert!(gens.contains(&1));
    assert!(gens.contains(&2));
    assert!(gens.contains(&3));

    // First node should be the target (gen3)
    assert_eq!(ancestry[0].id, gen3.id);
    assert_eq!(ancestry[0].generation, 3);
}

#[test]
fn test_motivation_deep_ancestry_chain() {
    let (_tmp, agency_dir) = setup();
    let motivations_dir = agency_dir.join("motivations");

    let gen0 = agency::build_motivation("M0", "root", vec![], vec![]);
    agency::save_motivation(&gen0, &motivations_dir).unwrap();

    let mut gen1 = agency::build_motivation("M1", "mut-1", vec!["trade1".into()], vec![]);
    gen1.lineage = Lineage::mutation(&gen0.id, 0, "e1");
    agency::save_motivation(&gen1, &motivations_dir).unwrap();

    let mut gen2 = agency::build_motivation("M2", "mut-2", vec![], vec!["no-trade".into()]);
    gen2.lineage = Lineage::mutation(&gen1.id, 1, "e2");
    agency::save_motivation(&gen2, &motivations_dir).unwrap();

    let mut gen3 = agency::build_motivation("M3", "mut-3", vec![], vec![]);
    gen3.lineage = Lineage::mutation(&gen2.id, 2, "e3");
    agency::save_motivation(&gen3, &motivations_dir).unwrap();

    let ancestry = agency::motivation_ancestry(&gen3.id, &motivations_dir).unwrap();
    assert_eq!(ancestry.len(), 4);
    assert_eq!(ancestry[0].id, gen3.id);
    assert_eq!(ancestry[0].generation, 3);
}

// ---------------------------------------------------------------------------
// 4. Crossover with 2 parents: both parents appear in ancestry
// ---------------------------------------------------------------------------

#[test]
fn test_role_crossover_two_parents() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    let parent_a = agency::build_role("Parent A", "first parent", vec![], "outcome-a");
    agency::save_role(&parent_a, &roles_dir).unwrap();

    let parent_b = agency::build_role(
        "Parent B",
        "second parent",
        vec![SkillRef::Name("python".to_string())],
        "outcome-b",
    );
    agency::save_role(&parent_b, &roles_dir).unwrap();

    let mut crossover = agency::build_role("Crossover", "combined", vec![], "outcome-c");
    crossover.lineage = Lineage::crossover(
        &[&parent_a.id, &parent_b.id],
        0, // max parent generation
        "cross-run-1",
    );
    agency::save_role(&crossover, &roles_dir).unwrap();

    assert_eq!(crossover.lineage.parent_ids.len(), 2);
    assert_eq!(crossover.lineage.generation, 1);
    assert_eq!(crossover.lineage.created_by, "evolver-cross-run-1");

    let ancestry = agency::role_ancestry(&crossover.id, &roles_dir).unwrap();
    assert_eq!(ancestry.len(), 3, "Crossover + 2 parents");

    assert_eq!(ancestry[0].id, crossover.id);
    let parent_ids_in_ancestry: Vec<&str> = ancestry[1..].iter().map(|n| n.id.as_str()).collect();
    assert!(parent_ids_in_ancestry.contains(&parent_a.id.as_str()));
    assert!(parent_ids_in_ancestry.contains(&parent_b.id.as_str()));
}

#[test]
fn test_motivation_crossover_two_parents() {
    let (_tmp, agency_dir) = setup();
    let motivations_dir = agency_dir.join("motivations");

    let parent_a = agency::build_motivation("MA", "first", vec![], vec![]);
    agency::save_motivation(&parent_a, &motivations_dir).unwrap();

    let parent_b = agency::build_motivation("MB", "second", vec!["tradeoff".into()], vec![]);
    agency::save_motivation(&parent_b, &motivations_dir).unwrap();

    let mut crossover = agency::build_motivation("MC", "merged", vec![], vec![]);
    crossover.lineage = Lineage::crossover(&[&parent_a.id, &parent_b.id], 0, "cross-mot-1");
    agency::save_motivation(&crossover, &motivations_dir).unwrap();

    let ancestry = agency::motivation_ancestry(&crossover.id, &motivations_dir).unwrap();
    assert_eq!(ancestry.len(), 3);
    assert_eq!(ancestry[0].id, crossover.id);
    let parent_ids: Vec<&str> = ancestry[1..].iter().map(|n| n.id.as_str()).collect();
    assert!(parent_ids.contains(&parent_a.id.as_str()));
    assert!(parent_ids.contains(&parent_b.id.as_str()));
}

// ---------------------------------------------------------------------------
// 5. Generation numbers increment correctly through chains
// ---------------------------------------------------------------------------

#[test]
fn test_generation_increments_mutation() {
    // Mutation: child generation = parent generation + 1
    let lineage = Lineage::mutation("parent-id", 0, "run");
    assert_eq!(lineage.generation, 1);

    let lineage2 = Lineage::mutation("parent-id", 5, "run");
    assert_eq!(lineage2.generation, 6);

    let lineage3 = Lineage::mutation("parent-id", 99, "run");
    assert_eq!(lineage3.generation, 100);
}

#[test]
fn test_generation_increments_crossover() {
    // Crossover: child generation = max(parent generations) + 1
    let lineage = Lineage::crossover(&["a", "b"], 0, "run");
    assert_eq!(lineage.generation, 1);

    // If parents are at different generations, use the max
    let lineage2 = Lineage::crossover(&["a", "b"], 3, "run");
    assert_eq!(lineage2.generation, 4);
}

#[test]
fn test_generation_increments_through_deep_chain() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    let mut roles = vec![];
    let root = agency::build_role("R0", "gen-0", vec![], "o0");
    agency::save_role(&root, &roles_dir).unwrap();
    roles.push(root);

    // Create chain of 5 generations
    for i in 1..=5u32 {
        let mut role = agency::build_role(
            format!("R{}", i),
            format!("gen-{}", i),
            vec![],
            format!("o{}", i),
        );
        role.lineage = Lineage::mutation(&roles[(i - 1) as usize].id, i - 1, &format!("e{}", i));
        agency::save_role(&role, &roles_dir).unwrap();
        roles.push(role);
    }

    // The last role should be generation 5
    assert_eq!(roles[5].lineage.generation, 5);

    let ancestry = agency::role_ancestry(&roles[5].id, &roles_dir).unwrap();
    assert_eq!(ancestry.len(), 6);

    // Verify each generation is present
    for g in 0..=5u32 {
        assert!(
            ancestry.iter().any(|n| n.generation == g),
            "Generation {} should be in ancestry",
            g
        );
    }
}

#[test]
fn test_crossover_generation_from_mixed_gen_parents() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    // Parent A at gen 0
    let parent_a = agency::build_role("PA", "gen0 parent", vec![], "oa");
    agency::save_role(&parent_a, &roles_dir).unwrap();

    // Parent B at gen 2 (simulated by explicit lineage)
    let mut parent_b = agency::build_role("PB", "gen2 parent", vec![], "ob");
    parent_b.lineage = Lineage {
        parent_ids: vec!["fake-intermediate".to_string()],
        generation: 2,
        created_by: "evolver-test".to_string(),
        created_at: chrono::Utc::now(),
    };
    agency::save_role(&parent_b, &roles_dir).unwrap();

    // Crossover: max(0, 2) + 1 = 3
    let mut cross = agency::build_role("Cross", "crossover", vec![], "oc");
    cross.lineage = Lineage::crossover(&[&parent_a.id, &parent_b.id], 2, "x-run");
    agency::save_role(&cross, &roles_dir).unwrap();

    assert_eq!(cross.lineage.generation, 3);

    let ancestry = agency::role_ancestry(&cross.id, &roles_dir).unwrap();
    // cross (gen3), parent_a (gen0), parent_b (gen2)
    // parent_b references "fake-intermediate" which doesn't exist, so only 3 nodes
    assert_eq!(ancestry.len(), 3);
}

// ---------------------------------------------------------------------------
// 6. Orphan resilience: missing intermediate parents
// ---------------------------------------------------------------------------

#[test]
fn test_role_ancestry_missing_intermediate_parent() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    // Create grandparent (gen 0)
    let grandparent = agency::build_role("GP", "grandparent", vec![], "o-gp");
    agency::save_role(&grandparent, &roles_dir).unwrap();

    // Parent (gen 1) references grandparent
    let mut parent = agency::build_role("P", "parent", vec![], "o-p");
    parent.lineage = Lineage::mutation(&grandparent.id, 0, "e-p");
    agency::save_role(&parent, &roles_dir).unwrap();

    // Child (gen 2) references parent
    let mut child = agency::build_role("C", "child", vec![], "o-c");
    child.lineage = Lineage::mutation(&parent.id, 1, "e-c");
    agency::save_role(&child, &roles_dir).unwrap();

    // DELETE the parent file to simulate a missing intermediate
    let parent_path = roles_dir.join(format!("{}.yaml", parent.id));
    std::fs::remove_file(&parent_path).unwrap();

    // Ancestry should still succeed, returning only the nodes that exist
    let ancestry = agency::role_ancestry(&child.id, &roles_dir).unwrap();
    // child is found (it references parent), but parent is missing,
    // so the walk stops there. grandparent is NOT reachable.
    assert_eq!(
        ancestry.len(),
        1,
        "Only the child itself should be returned when parent is missing"
    );
    assert_eq!(ancestry[0].id, child.id);
    assert_eq!(ancestry[0].generation, 2);
    // parent_ids on the child node still show the reference
    assert_eq!(ancestry[0].parent_ids, vec![parent.id.clone()]);
}

#[test]
fn test_motivation_ancestry_missing_intermediate_parent() {
    let (_tmp, agency_dir) = setup();
    let motivations_dir = agency_dir.join("motivations");

    let grandparent = agency::build_motivation("GP", "grandparent", vec![], vec![]);
    agency::save_motivation(&grandparent, &motivations_dir).unwrap();

    let mut parent = agency::build_motivation("P", "parent", vec![], vec![]);
    parent.lineage = Lineage::mutation(&grandparent.id, 0, "e-p");
    agency::save_motivation(&parent, &motivations_dir).unwrap();

    let mut child = agency::build_motivation("C", "child", vec![], vec![]);
    child.lineage = Lineage::mutation(&parent.id, 1, "e-c");
    agency::save_motivation(&child, &motivations_dir).unwrap();

    // Delete parent
    let parent_path = motivations_dir.join(format!("{}.yaml", parent.id));
    std::fs::remove_file(&parent_path).unwrap();

    let ancestry = agency::motivation_ancestry(&child.id, &motivations_dir).unwrap();
    assert_eq!(ancestry.len(), 1);
    assert_eq!(ancestry[0].id, child.id);
}

#[test]
fn test_role_ancestry_missing_one_crossover_parent() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    let parent_a = agency::build_role("PA", "parent a", vec![], "oa");
    agency::save_role(&parent_a, &roles_dir).unwrap();

    let parent_b = agency::build_role("PB", "parent b", vec![], "ob");
    agency::save_role(&parent_b, &roles_dir).unwrap();

    let mut cross = agency::build_role("Cross", "crossover", vec![], "oc");
    cross.lineage = Lineage::crossover(&[&parent_a.id, &parent_b.id], 0, "x-run");
    agency::save_role(&cross, &roles_dir).unwrap();

    // Delete parent_b
    std::fs::remove_file(roles_dir.join(format!("{}.yaml", parent_b.id))).unwrap();

    let ancestry = agency::role_ancestry(&cross.id, &roles_dir).unwrap();
    // cross + parent_a (parent_b missing, silently skipped)
    assert_eq!(ancestry.len(), 2);
    let ids: Vec<&str> = ancestry.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&cross.id.as_str()));
    assert!(ids.contains(&parent_a.id.as_str()));
    assert!(!ids.contains(&parent_b.id.as_str()));
}

#[test]
fn test_role_ancestry_target_does_not_exist() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    // Query ancestry for an ID that doesn't exist
    let ancestry = agency::role_ancestry("nonexistent-id", &roles_dir).unwrap();
    assert!(
        ancestry.is_empty(),
        "Ancestry of nonexistent role should be empty"
    );
}

#[test]
fn test_motivation_ancestry_target_does_not_exist() {
    let (_tmp, agency_dir) = setup();
    let motivations_dir = agency_dir.join("motivations");

    let ancestry = agency::motivation_ancestry("nonexistent-id", &motivations_dir).unwrap();
    assert!(ancestry.is_empty());
}

// ---------------------------------------------------------------------------
// 7. AncestryNode output format
// ---------------------------------------------------------------------------

#[test]
fn test_ancestry_node_fields_populated() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    let parent = agency::build_role("Root Role", "The root", vec![], "Root outcome");
    agency::save_role(&parent, &roles_dir).unwrap();

    let mut child = agency::build_role(
        "Evolved Role",
        "A mutated descendant",
        vec![SkillRef::Name("rust".to_string())],
        "Better outcome",
    );
    child.lineage = Lineage::mutation(&parent.id, 0, "evo-42");
    agency::save_role(&child, &roles_dir).unwrap();

    let ancestry = agency::role_ancestry(&child.id, &roles_dir).unwrap();
    assert_eq!(ancestry.len(), 2);

    // Check child node
    let child_node = &ancestry[0];
    assert_eq!(child_node.id, child.id);
    assert_eq!(child_node.name, "Evolved Role");
    assert_eq!(child_node.generation, 1);
    assert_eq!(child_node.created_by, "evolver-evo-42");
    assert_eq!(child_node.parent_ids, vec![parent.id.clone()]);
    // created_at should be a valid timestamp (not zero/default)
    assert!(child_node.created_at.timestamp() > 0);

    // Check parent node
    let parent_node = &ancestry[1];
    assert_eq!(parent_node.id, parent.id);
    assert_eq!(parent_node.name, "Root Role");
    assert_eq!(parent_node.generation, 0);
    assert_eq!(parent_node.created_by, "human");
    assert!(parent_node.parent_ids.is_empty());
    assert!(parent_node.created_at.timestamp() > 0);
}

#[test]
fn test_ancestry_node_crossover_parent_ids() {
    let (_tmp, agency_dir) = setup();
    let motivations_dir = agency_dir.join("motivations");

    let pa = agency::build_motivation("A", "first", vec![], vec![]);
    agency::save_motivation(&pa, &motivations_dir).unwrap();

    let pb = agency::build_motivation("B", "second", vec!["trade".into()], vec![]);
    agency::save_motivation(&pb, &motivations_dir).unwrap();

    let mut cross = agency::build_motivation("AB", "combined", vec![], vec![]);
    cross.lineage = Lineage::crossover(&[&pa.id, &pb.id], 0, "cross-99");
    agency::save_motivation(&cross, &motivations_dir).unwrap();

    let ancestry = agency::motivation_ancestry(&cross.id, &motivations_dir).unwrap();
    let cross_node = &ancestry[0];

    // The crossover node should list both parent IDs
    assert_eq!(cross_node.parent_ids.len(), 2);
    assert!(cross_node.parent_ids.contains(&pa.id));
    assert!(cross_node.parent_ids.contains(&pb.id));
    assert_eq!(cross_node.created_by, "evolver-cross-99");
}

// ---------------------------------------------------------------------------
// Additional lineage edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_lineage_default_values() {
    let lineage = Lineage::default();
    assert!(lineage.parent_ids.is_empty());
    assert_eq!(lineage.generation, 0);
    assert_eq!(lineage.created_by, "human");
    assert!(lineage.created_at.timestamp() > 0);
}

#[test]
fn test_lineage_mutation_constructor() {
    let lineage = Lineage::mutation("parent-abc", 3, "run-xyz");
    assert_eq!(lineage.parent_ids, vec!["parent-abc".to_string()]);
    assert_eq!(lineage.generation, 4);
    assert_eq!(lineage.created_by, "evolver-run-xyz");
}

#[test]
fn test_lineage_crossover_constructor() {
    let lineage = Lineage::crossover(&["id-1", "id-2"], 5, "run-cross");
    assert_eq!(
        lineage.parent_ids,
        vec!["id-1".to_string(), "id-2".to_string()]
    );
    assert_eq!(lineage.generation, 6);
    assert_eq!(lineage.created_by, "evolver-run-cross");
}

#[test]
fn test_ancestry_no_duplicate_visits() {
    // If a diamond pattern exists (A -> B, A -> C, B -> D, C -> D),
    // the ancestry walker should visit D only once.
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    // D is the common ancestor (gen 0)
    let d = agency::build_role("D", "common ancestor", vec![], "od");
    agency::save_role(&d, &roles_dir).unwrap();

    // B and C both descend from D (gen 1)
    let mut b = agency::build_role("B", "branch b", vec![], "ob");
    b.lineage = Lineage::mutation(&d.id, 0, "e-b");
    agency::save_role(&b, &roles_dir).unwrap();

    let mut c = agency::build_role("C", "branch c", vec![SkillRef::Name("extra".into())], "oc");
    c.lineage = Lineage::mutation(&d.id, 0, "e-c");
    agency::save_role(&c, &roles_dir).unwrap();

    // A is a crossover of B and C (gen 2)
    let mut a = agency::build_role("A", "crossover of b and c", vec![], "oa");
    a.lineage = Lineage::crossover(&[&b.id, &c.id], 1, "e-a");
    agency::save_role(&a, &roles_dir).unwrap();

    let ancestry = agency::role_ancestry(&a.id, &roles_dir).unwrap();
    // A, B, C, D — D should appear exactly once even though both B and C reference it
    assert_eq!(
        ancestry.len(),
        4,
        "Diamond ancestry should have 4 unique nodes"
    );

    let ids: Vec<&str> = ancestry.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        ids.iter().filter(|&&id| id == d.id.as_str()).count(),
        1,
        "Common ancestor D should appear exactly once"
    );
}

#[test]
fn test_role_ancestry_empty_directory() {
    let (_tmp, agency_dir) = setup();
    let roles_dir = agency_dir.join("roles");

    // No roles saved — ancestry of any ID returns empty
    let ancestry = agency::role_ancestry("anything", &roles_dir).unwrap();
    assert!(ancestry.is_empty());
}

#[test]
fn test_motivation_ancestry_empty_directory() {
    let (_tmp, agency_dir) = setup();
    let motivations_dir = agency_dir.join("motivations");

    let ancestry = agency::motivation_ancestry("anything", &motivations_dir).unwrap();
    assert!(ancestry.is_empty());
}

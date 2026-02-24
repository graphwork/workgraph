use super::hash::*;
use super::store::{init, save_motivation, save_role, AgencyError};
use super::types::*;
use std::path::Path;

/// Helper to build a Role with its content-hash ID computed automatically.
pub fn build_role(
    name: impl Into<String>,
    description: impl Into<String>,
    skills: Vec<SkillRef>,
    desired_outcome: impl Into<String>,
) -> Role {
    let description = description.into();
    let desired_outcome = desired_outcome.into();
    let id = content_hash_role(&skills, &desired_outcome, &description);
    Role {
        id,
        name: name.into(),
        description,
        skills,
        desired_outcome,
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::default(),
        default_context_scope: None,
    }
}

/// Helper to build a Motivation with its content-hash ID computed automatically.
pub fn build_motivation(
    name: impl Into<String>,
    description: impl Into<String>,
    acceptable_tradeoffs: Vec<String>,
    unacceptable_tradeoffs: Vec<String>,
) -> Motivation {
    let description = description.into();
    let id = content_hash_motivation(&acceptable_tradeoffs, &unacceptable_tradeoffs, &description);
    Motivation {
        id,
        name: name.into(),
        description,
        acceptable_tradeoffs,
        unacceptable_tradeoffs,
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::default(),
    }
}

/// Return the set of built-in starter roles that ship with wg.
pub fn starter_roles() -> Vec<Role> {
    vec![
        build_role(
            "Programmer",
            "Writes, tests, and debugs code to implement features and fix bugs.",
            vec![
                SkillRef::Name("code-writing".into()),
                SkillRef::Name("testing".into()),
                SkillRef::Name("debugging".into()),
            ],
            "Working, tested code",
        ),
        build_role(
            "Reviewer",
            "Reviews code for correctness, security, and style.",
            vec![
                SkillRef::Name("code-review".into()),
                SkillRef::Name("security-audit".into()),
            ],
            "Review report with findings",
        ),
        build_role(
            "Documenter",
            "Produces clear, accurate technical documentation.",
            vec![SkillRef::Name("technical-writing".into())],
            "Clear documentation",
        ),
        build_role(
            "Architect",
            "Designs systems, analyzes dependencies, and makes structural decisions.",
            vec![
                SkillRef::Name("system-design".into()),
                SkillRef::Name("dependency-analysis".into()),
            ],
            "Design document with rationale",
        ),
    ]
}

/// Return the set of built-in starter motivations that ship with wg.
pub fn starter_motivations() -> Vec<Motivation> {
    vec![
        build_motivation(
            "Careful",
            "Prioritizes reliability and correctness above speed.",
            vec!["Slow".into(), "Verbose".into()],
            vec!["Unreliable".into(), "Untested".into()],
        ),
        build_motivation(
            "Fast",
            "Prioritizes speed and shipping over polish.",
            vec!["Less documentation".into(), "Simpler solutions".into()],
            vec!["Broken code".into()],
        ),
        build_motivation(
            "Thorough",
            "Prioritizes completeness and depth of analysis.",
            vec!["Expensive".into(), "Slow".into(), "Verbose".into()],
            vec!["Incomplete analysis".into()],
        ),
        build_motivation(
            "Balanced",
            "Moderate on all dimensions; balances speed, quality, and completeness.",
            vec!["Moderate trade-offs on any single dimension".into()],
            vec!["Extreme compromise on any dimension".into()],
        ),
    ]
}

/// Seed the agency directory with starter roles and motivations.
///
/// Only writes files that don't already exist, so existing customizations are preserved.
/// Deduplication is automatic: same content produces the same hash ID and filename.
/// Returns the number of roles and motivations that were created.
pub fn seed_starters(agency_dir: &Path) -> Result<(usize, usize), AgencyError> {
    init(agency_dir)?;

    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");

    let mut roles_created = 0;
    for role in starter_roles() {
        let path = roles_dir.join(format!("{}.yaml", role.id));
        if !path.exists() {
            save_role(&role, &roles_dir)?;
            roles_created += 1;
        }
    }

    let mut motivations_created = 0;
    for motivation in starter_motivations() {
        let path = motivations_dir.join(format!("{}.yaml", motivation.id));
        if !path.exists() {
            save_motivation(&motivation, &motivations_dir)?;
            motivations_created += 1;
        }
    }

    Ok((roles_created, motivations_created))
}

// ---------------------------------------------------------------------------
// Evolution utilities (test-only: used by evolve.rs tests to verify primitives)
// ---------------------------------------------------------------------------

/// Mutate a parent role to produce a child with updated fields and correct lineage.
///
/// Any `None` field inherits the parent's value. The child gets a fresh content-hash ID
/// based on its (possibly mutated) description, skills, and desired_outcome.
#[cfg(test)]
pub(crate) fn mutate_role(
    parent: &Role,
    run_id: &str,
    new_name: Option<&str>,
    new_description: Option<&str>,
    new_skills: Option<Vec<SkillRef>>,
    new_desired_outcome: Option<&str>,
) -> Role {
    let description = new_description
        .map(|s| s.to_string())
        .unwrap_or_else(|| parent.description.clone());
    let skills = new_skills.unwrap_or_else(|| parent.skills.clone());
    let desired_outcome = new_desired_outcome
        .map(|s| s.to_string())
        .unwrap_or_else(|| parent.desired_outcome.clone());

    let id = content_hash_role(&skills, &desired_outcome, &description);

    Role {
        id,
        name: new_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| parent.name.clone()),
        description,
        skills,
        desired_outcome,
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::mutation(&parent.id, parent.lineage.generation, run_id),
        default_context_scope: parent.default_context_scope.clone(),
    }
}

/// Crossover two motivations: union their accept/reject lists and set crossover lineage.
///
/// Produces a new motivation whose acceptable_tradeoffs and unacceptable_tradeoffs are
/// the deduplicated union of both parents' lists.
#[cfg(test)]
pub(crate) fn crossover_motivations(
    parent_a: &Motivation,
    parent_b: &Motivation,
    run_id: &str,
    name: &str,
    description: &str,
) -> Motivation {
    let mut acceptable: Vec<String> = parent_a.acceptable_tradeoffs.clone();
    for t in &parent_b.acceptable_tradeoffs {
        if !acceptable.contains(t) {
            acceptable.push(t.clone());
        }
    }

    let mut unacceptable: Vec<String> = parent_a.unacceptable_tradeoffs.clone();
    for t in &parent_b.unacceptable_tradeoffs {
        if !unacceptable.contains(t) {
            unacceptable.push(t.clone());
        }
    }

    let id = content_hash_motivation(&acceptable, &unacceptable, description);
    let max_gen = parent_a.lineage.generation.max(parent_b.lineage.generation);

    Motivation {
        id,
        name: name.to_string(),
        description: description.to_string(),
        acceptable_tradeoffs: acceptable,
        unacceptable_tradeoffs: unacceptable,
        performance: PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        },
        lineage: Lineage::crossover(&[&parent_a.id, &parent_b.id], max_gen, run_id),
    }
}

/// Tournament selection: pick the role with the highest average score.
///
/// Returns `None` if the slice is empty. Roles without a score (`avg_score == None`)
/// are treated as having score 0.0.
#[cfg(test)]
pub(crate) fn tournament_select_role(candidates: &[Role]) -> Option<&Role> {
    candidates.iter().max_by(|a, b| {
        let sa = a.performance.avg_score.unwrap_or(0.0);
        let sb = b.performance.avg_score.unwrap_or(0.0);
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// Identify roles whose average score falls below the given threshold.
///
/// Only roles with at least `min_evals` evaluations are considered; roles with
/// fewer evaluations are never flagged for retirement (they haven't been tested enough).
#[cfg(test)]
pub(crate) fn roles_below_threshold(roles: &[Role], threshold: f64, min_evals: u32) -> Vec<&Role> {
    roles
        .iter()
        .filter(|r| {
            r.performance.task_count >= min_evals
                && r.performance.avg_score.is_some_and(|s| s < threshold)
        })
        .collect()
}

/// Gap analysis: given a set of required skill names and the current roles,
/// return the skill names that are not covered by any existing role.
///
/// A skill is "covered" if at least one role has a `SkillRef::Name(n)` where
/// `n` matches the required skill (case-sensitive).
#[cfg(test)]
pub(crate) fn uncovered_skills(required: &[&str], roles: &[Role]) -> Vec<String> {
    let covered: std::collections::HashSet<&str> = roles
        .iter()
        .flat_map(|r| r.skills.iter())
        .filter_map(|s| match s {
            SkillRef::Name(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();

    required
        .iter()
        .filter(|&&skill| !covered.contains(skill))
        .map(|&s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::hash::content_hash_agent;
    use super::super::store::*;
    use super::*;
    use crate::graph::TrustLevel;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn sample_performance() -> PerformanceRecord {
        PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        }
    }

    fn sample_role() -> Role {
        build_role(
            "Implementer",
            "Writes code to fulfil task requirements.",
            vec![
                SkillRef::Name("rust".into()),
                SkillRef::Inline("fn main() {}".into()),
            ],
            "Working, tested code merged to main.",
        )
    }

    fn sample_motivation() -> Motivation {
        build_motivation(
            "Quality First",
            "Prioritise correctness and maintainability.",
            vec!["Slower delivery for higher quality".into()],
            vec!["Skipping tests".into()],
        )
    }

    fn sample_evaluation() -> Evaluation {
        let role = sample_role();
        let motivation = sample_motivation();
        let mut dims = HashMap::new();
        dims.insert("correctness".into(), 0.9);
        dims.insert("style".into(), 0.8);
        Evaluation {
            id: "eval-001".into(),
            task_id: "task-42".into(),
            agent_id: String::new(),
            role_id: role.id,
            motivation_id: motivation.id,
            score: 0.85,
            dimensions: dims,
            notes: "Good implementation with minor style issues.".into(),
            evaluator: "reviewer-bot".into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
        }
    }

    fn sample_agent() -> Agent {
        let role = sample_role();
        let motivation = sample_motivation();
        let id = content_hash_agent(&role.id, &motivation.id);
        Agent {
            id,
            role_id: role.id,
            motivation_id: motivation.id,
            name: "Test Agent".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec!["rust".into(), "testing".into()],
            rate: Some(50.0),
            capacity: Some(3.0),
            trust_level: TrustLevel::Verified,
            contact: Some("agent@example.com".into()),
            executor: "matrix".into(),
        }
    }

    // -- Storage tests -------------------------------------------------------

    #[test]
    fn test_init_creates_directories() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("agency");
        init(&base).unwrap();
        assert!(base.join("roles").is_dir());
        assert!(base.join("motivations").is_dir());
        assert!(base.join("evaluations").is_dir());
    }

    #[test]
    fn test_init_idempotent() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("agency");
        init(&base).unwrap();
        init(&base).unwrap(); // should not error
    }

    #[test]
    fn test_role_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let role = sample_role();
        let path = save_role(&role, dir).unwrap();
        assert!(path.exists());
        // Filename is content-hash ID + .yaml
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{}.yaml", role.id)
        );
        assert_eq!(role.id.len(), 64, "Role ID should be a SHA-256 hex hash");

        let loaded = load_role(&path).unwrap();
        assert_eq!(loaded.id, role.id);
        assert_eq!(loaded.name, role.name);
        assert_eq!(loaded.description, role.description);
        assert_eq!(loaded.desired_outcome, role.desired_outcome);
        assert_eq!(loaded.skills.len(), role.skills.len());
    }

    #[test]
    fn test_motivation_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let motivation = sample_motivation();
        let path = save_motivation(&motivation, dir).unwrap();
        assert!(path.exists());
        // Filename is content-hash ID + .yaml
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{}.yaml", motivation.id)
        );
        assert_eq!(
            motivation.id.len(),
            64,
            "Motivation ID should be a SHA-256 hex hash"
        );

        let loaded = load_motivation(&path).unwrap();
        assert_eq!(loaded.id, motivation.id);
        assert_eq!(loaded.name, motivation.name);
        assert_eq!(loaded.acceptable_tradeoffs, motivation.acceptable_tradeoffs);
        assert_eq!(
            loaded.unacceptable_tradeoffs,
            motivation.unacceptable_tradeoffs
        );
    }

    #[test]
    fn test_evaluation_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let eval = sample_evaluation();
        let path = save_evaluation(&eval, dir).unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "eval-001.json");

        let loaded = load_evaluation(&path).unwrap();
        assert_eq!(loaded.id, eval.id);
        assert_eq!(loaded.task_id, eval.task_id);
        assert_eq!(loaded.score, eval.score);
        assert_eq!(loaded.dimensions.len(), eval.dimensions.len());
        assert_eq!(loaded.dimensions["correctness"], 0.9);
    }

    #[test]
    fn test_load_all_roles() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("agency");
        init(&base).unwrap();

        let roles_dir = base.join("roles");
        // Two roles with different content produce different content-hash IDs
        let r1 = build_role("Role A", "First role", vec![], "Outcome A");
        let r2 = build_role("Role B", "Second role", vec![], "Outcome B");
        save_role(&r1, &roles_dir).unwrap();
        save_role(&r2, &roles_dir).unwrap();

        let all = load_all_roles(&roles_dir).unwrap();
        assert_eq!(all.len(), 2);
        // Results should be sorted by ID
        assert!(all[0].id < all[1].id, "Roles should be sorted by ID");
    }

    #[test]
    fn test_load_all_motivations() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("agency");
        init(&base).unwrap();

        let dir = base.join("motivations");
        let m1 = build_motivation("Mot A", "First", vec!["a".into()], vec![]);
        let m2 = build_motivation("Mot B", "Second", vec!["b".into()], vec![]);
        save_motivation(&m1, &dir).unwrap();
        save_motivation(&m2, &dir).unwrap();

        let all = load_all_motivations(&dir).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].id < all[1].id, "Motivations should be sorted by ID");
    }

    #[test]
    fn test_load_all_evaluations() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("agency");
        init(&base).unwrap();

        let dir = base.join("evaluations");
        let e1 = Evaluation {
            id: "eval-a".into(),
            ..sample_evaluation()
        };
        let e2 = Evaluation {
            id: "eval-b".into(),
            ..sample_evaluation()
        };
        save_evaluation(&e1, &dir).unwrap();
        save_evaluation(&e2, &dir).unwrap();

        let all = load_all_evaluations(&dir).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "eval-a");
        assert_eq!(all[1].id, "eval-b");
    }

    #[test]
    fn test_load_all_from_nonexistent_dir() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        assert_eq!(load_all_roles(&missing).unwrap().len(), 0);
        assert_eq!(load_all_motivations(&missing).unwrap().len(), 0);
        assert_eq!(load_all_evaluations(&missing).unwrap().len(), 0);
    }

    #[test]
    fn test_load_all_ignores_non_matching_extensions() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        // Write a .txt file - should be ignored by load_all_roles
        std::fs::write(dir.join("stray.txt"), "not yaml").unwrap();
        save_role(&sample_role(), dir).unwrap();

        let all = load_all_roles(dir).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_role_yaml_is_human_readable() {
        let tmp = TempDir::new().unwrap();
        let role = sample_role();
        let path = save_role(&role, tmp.path()).unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        // YAML should contain the field names as readable keys
        assert!(contents.contains("id:"));
        assert!(contents.contains("name:"));
        assert!(contents.contains("description:"));
        assert!(contents.contains("desired_outcome:"));
    }

    // -- Lineage tests -------------------------------------------------------

    #[test]
    fn test_lineage_default() {
        let lineage = Lineage::default();
        assert!(lineage.parent_ids.is_empty());
        assert_eq!(lineage.generation, 0);
        assert_eq!(lineage.created_by, "human");
    }

    #[test]
    fn test_lineage_mutation() {
        let lineage = Lineage::mutation("parent-role", 2, "run-42");
        assert_eq!(lineage.parent_ids, vec!["parent-role"]);
        assert_eq!(lineage.generation, 3);
        assert_eq!(lineage.created_by, "evolver-run-42");
    }

    #[test]
    fn test_lineage_crossover() {
        let lineage = Lineage::crossover(&["parent-a", "parent-b"], 5, "run-99");
        assert_eq!(lineage.parent_ids, vec!["parent-a", "parent-b"]);
        assert_eq!(lineage.generation, 6);
        assert_eq!(lineage.created_by, "evolver-run-99");
    }

    #[test]
    fn test_role_lineage_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut role = sample_role();
        role.lineage = Lineage::mutation("old-role", 1, "test-run");
        let path = save_role(&role, tmp.path()).unwrap();
        let loaded = load_role(&path).unwrap();
        assert_eq!(loaded.lineage.parent_ids, vec!["old-role"]);
        assert_eq!(loaded.lineage.generation, 2);
        assert_eq!(loaded.lineage.created_by, "evolver-test-run");
    }

    #[test]
    fn test_motivation_lineage_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut m = sample_motivation();
        m.lineage = Lineage::crossover(&["m-a", "m-b"], 3, "xover-1");
        let path = save_motivation(&m, tmp.path()).unwrap();
        let loaded = load_motivation(&path).unwrap();
        assert_eq!(loaded.lineage.parent_ids, vec!["m-a", "m-b"]);
        assert_eq!(loaded.lineage.generation, 4);
        assert_eq!(loaded.lineage.created_by, "evolver-xover-1");
    }

    #[test]
    fn test_role_without_lineage_deserializes_defaults() {
        // Simulate YAML from before lineage was added (no lineage field)
        let yaml = r#"
id: legacy-role
name: Legacy
description: A role from before lineage
skills: []
desired_outcome: Works
performance:
  task_count: 0
  avg_score: null
"#;
        let role: Role = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(role.lineage.generation, 0);
        assert_eq!(role.lineage.created_by, "human");
        assert!(role.lineage.parent_ids.is_empty());
    }

    #[test]
    fn test_role_yaml_includes_lineage() {
        let tmp = TempDir::new().unwrap();
        let mut role = sample_role();
        role.lineage = Lineage::mutation("src-role", 0, "evo-1");
        let path = save_role(&role, tmp.path()).unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        assert!(contents.contains("lineage:"));
        assert!(contents.contains("parent_ids:"));
        assert!(contents.contains("generation:"));
        assert!(contents.contains("created_by:"));
        assert!(contents.contains("created_at:"));
    }

    // -- Evolution utility tests ---------------------------------------------

    #[test]
    fn test_mutate_role_produces_valid_child_with_parent_lineage() {
        let parent = build_role(
            "Programmer",
            "Writes code to implement features.",
            vec![
                SkillRef::Name("coding".into()),
                SkillRef::Name("debugging".into()),
            ],
            "Working code",
        );

        let child = mutate_role(
            &parent,
            "evo-run-1",
            Some("Test-Focused Programmer"),
            None, // inherit description
            Some(vec![
                SkillRef::Name("coding".into()),
                SkillRef::Name("debugging".into()),
                SkillRef::Name("testing".into()),
            ]),
            Some("Working, tested code"),
        );

        // Child has a content-hash ID that differs from parent (skills/outcome changed)
        assert_ne!(child.id, parent.id);
        assert_eq!(child.id.len(), 64);
        // Name was overridden
        assert_eq!(child.name, "Test-Focused Programmer");
        // Description inherited from parent
        assert_eq!(child.description, parent.description);
        // Skills were mutated
        assert_eq!(child.skills.len(), 3);
        // Desired outcome was mutated
        assert_eq!(child.desired_outcome, "Working, tested code");
        // Lineage tracks the parent
        assert_eq!(child.lineage.parent_ids, vec![parent.id.clone()]);
        assert_eq!(child.lineage.generation, parent.lineage.generation + 1);
        assert_eq!(child.lineage.created_by, "evolver-evo-run-1");
        // Performance starts fresh
        assert_eq!(child.performance.task_count, 0);
        assert!(child.performance.avg_score.is_none());
    }

    #[test]
    fn test_mutate_role_inherits_all_when_no_overrides() {
        let parent = build_role(
            "Architect",
            "Designs systems.",
            vec![SkillRef::Name("system-design".into())],
            "Design document",
        );

        let child = mutate_role(&parent, "run-2", None, None, None, None);

        // Content is identical, so content-hash ID is the same
        assert_eq!(child.id, parent.id);
        // Name inherited
        assert_eq!(child.name, parent.name);
        // Lineage still tracks parent
        assert_eq!(child.lineage.parent_ids, vec![parent.id.clone()]);
        assert_eq!(child.lineage.generation, 1);
    }

    #[test]
    fn test_mutate_role_generation_increments_from_parent() {
        let mut parent = build_role("Gen3", "Third gen", vec![], "Outcome");
        parent.lineage = Lineage::mutation("gen2-id", 2, "old-run");
        assert_eq!(parent.lineage.generation, 3);

        let child = mutate_role(&parent, "new-run", None, Some("Fourth gen"), None, None);
        assert_eq!(child.lineage.generation, 4);
        assert_eq!(child.lineage.parent_ids, vec![parent.id]);
    }

    #[test]
    fn test_crossover_motivations_merges_accept_reject_lists() {
        let parent_a = build_motivation(
            "Careful",
            "Prioritizes reliability.",
            vec!["Slow".into(), "Verbose".into()],
            vec!["Unreliable".into(), "Untested".into()],
        );
        let parent_b = build_motivation(
            "Fast",
            "Prioritizes speed.",
            vec!["Less documentation".into(), "Verbose".into()], // "Verbose" overlaps
            vec!["Broken code".into(), "Untested".into()],       // "Untested" overlaps
        );

        let child = crossover_motivations(
            &parent_a,
            &parent_b,
            "xover-run",
            "Careful-Fast Hybrid",
            "Balances speed and reliability.",
        );

        // Acceptable: union, deduplicated — Slow, Verbose, Less documentation
        assert_eq!(child.acceptable_tradeoffs.len(), 3);
        assert!(child.acceptable_tradeoffs.contains(&"Slow".to_string()));
        assert!(child.acceptable_tradeoffs.contains(&"Verbose".to_string()));
        assert!(
            child
                .acceptable_tradeoffs
                .contains(&"Less documentation".to_string())
        );

        // Unacceptable: union, deduplicated — Unreliable, Untested, Broken code
        assert_eq!(child.unacceptable_tradeoffs.len(), 3);
        assert!(
            child
                .unacceptable_tradeoffs
                .contains(&"Unreliable".to_string())
        );
        assert!(
            child
                .unacceptable_tradeoffs
                .contains(&"Untested".to_string())
        );
        assert!(
            child
                .unacceptable_tradeoffs
                .contains(&"Broken code".to_string())
        );

        // Lineage is crossover of both parents
        assert_eq!(child.lineage.parent_ids.len(), 2);
        assert!(child.lineage.parent_ids.contains(&parent_a.id));
        assert!(child.lineage.parent_ids.contains(&parent_b.id));
        assert_eq!(child.lineage.generation, 1); // max(0,0) + 1
        assert_eq!(child.lineage.created_by, "evolver-xover-run");

        // Name and description match what was passed in
        assert_eq!(child.name, "Careful-Fast Hybrid");
        assert_eq!(child.description, "Balances speed and reliability.");

        // Content-hash ID is valid
        assert_eq!(child.id.len(), 64);
    }

    #[test]
    fn test_crossover_motivations_generation_uses_max() {
        let mut parent_a = build_motivation("A", "A", vec!["a".into()], vec![]);
        parent_a.lineage = Lineage::mutation("ancestor", 4, "r1");
        assert_eq!(parent_a.lineage.generation, 5);

        let mut parent_b = build_motivation("B", "B", vec!["b".into()], vec![]);
        parent_b.lineage = Lineage::mutation("ancestor2", 1, "r2");
        assert_eq!(parent_b.lineage.generation, 2);

        let child = crossover_motivations(&parent_a, &parent_b, "xr", "Hybrid", "Hybrid desc");
        // max(5, 2) + 1 = 6
        assert_eq!(child.lineage.generation, 6);
    }

    #[test]
    fn test_crossover_motivations_no_overlap() {
        let parent_a = build_motivation("A", "A", vec!["x".into()], vec!["p".into()]);
        let parent_b = build_motivation("B", "B", vec!["y".into()], vec!["q".into()]);

        let child = crossover_motivations(&parent_a, &parent_b, "r", "C", "C");
        assert_eq!(child.acceptable_tradeoffs, vec!["x", "y"]);
        assert_eq!(child.unacceptable_tradeoffs, vec!["p", "q"]);
    }

    #[test]
    fn test_tournament_select_role_picks_highest_scored() {
        let mut low = build_role("Low", "Low scorer", vec![], "Low outcome");
        low.performance.avg_score = Some(0.3);
        low.performance.task_count = 5;

        let mut mid = build_role("Mid", "Mid scorer", vec![], "Mid outcome");
        mid.performance.avg_score = Some(0.6);
        mid.performance.task_count = 5;

        let mut high = build_role("High", "High scorer", vec![], "High outcome");
        high.performance.avg_score = Some(0.9);
        high.performance.task_count = 5;

        let candidates = vec![low.clone(), mid.clone(), high.clone()];
        let winner = tournament_select_role(&candidates).unwrap();
        assert_eq!(winner.id, high.id);
    }

    #[test]
    fn test_tournament_select_role_none_scores_treated_as_zero() {
        let mut scored = build_role("Scored", "Has a score", vec![], "Outcome");
        scored.performance.avg_score = Some(0.1);

        let unscored = build_role("Unscored", "No score yet", vec![], "Outcome2");
        // unscored.performance.avg_score remains None (treated as 0.0)

        let candidates = vec![unscored.clone(), scored.clone()];
        let winner = tournament_select_role(&candidates).unwrap();
        assert_eq!(winner.id, scored.id);
    }

    #[test]
    fn test_tournament_select_role_empty_returns_none() {
        let result = tournament_select_role(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_tournament_select_role_single_candidate() {
        let role = build_role("Only", "Only one", vec![], "Only outcome");
        let candidates = vec![role.clone()];
        let winner = tournament_select_role(&candidates).unwrap();
        assert_eq!(winner.id, role.id);
    }

    #[test]
    fn test_roles_below_threshold_filters_low_scorers() {
        let mut good = build_role("Good", "Good role", vec![], "Good outcome");
        good.performance.avg_score = Some(0.8);
        good.performance.task_count = 10;

        let mut bad = build_role("Bad", "Bad role", vec![], "Bad outcome");
        bad.performance.avg_score = Some(0.2);
        bad.performance.task_count = 10;

        let mut mediocre = build_role("Meh", "Mediocre role", vec![], "Meh outcome");
        mediocre.performance.avg_score = Some(0.49);
        mediocre.performance.task_count = 10;

        let roles = vec![good.clone(), bad.clone(), mediocre.clone()];
        let to_retire = roles_below_threshold(&roles, 0.5, 5);

        assert_eq!(to_retire.len(), 2);
        let retired_ids: Vec<&str> = to_retire.iter().map(|r| r.id.as_str()).collect();
        assert!(retired_ids.contains(&bad.id.as_str()));
        assert!(retired_ids.contains(&mediocre.id.as_str()));
    }

    #[test]
    fn test_roles_below_threshold_respects_min_evals() {
        let mut low_but_new = build_role("New", "Barely tested", vec![], "New outcome");
        low_but_new.performance.avg_score = Some(0.1);
        low_but_new.performance.task_count = 2; // below min_evals

        let mut low_and_tested = build_role("Old", "Thoroughly tested", vec![], "Old outcome");
        low_and_tested.performance.avg_score = Some(0.1);
        low_and_tested.performance.task_count = 10; // above min_evals

        let roles = vec![low_but_new.clone(), low_and_tested.clone()];
        let to_retire = roles_below_threshold(&roles, 0.5, 5);

        // Only the well-tested low scorer should be flagged
        assert_eq!(to_retire.len(), 1);
        assert_eq!(to_retire[0].id, low_and_tested.id);
    }

    #[test]
    fn test_roles_below_threshold_skips_unscored() {
        let unscored = build_role("Unscored", "No evals", vec![], "Outcome");
        // avg_score is None, task_count is 0

        let roles = vec![unscored];
        let to_retire = roles_below_threshold(&roles, 0.5, 0);
        // None score => map_or(false, ...) => not flagged
        assert!(to_retire.is_empty());
    }

    #[test]
    fn test_uncovered_skills_identifies_missing() {
        let role_a = build_role(
            "Coder",
            "Writes code",
            vec![
                SkillRef::Name("coding".into()),
                SkillRef::Name("debugging".into()),
            ],
            "Code",
        );
        let role_b = build_role(
            "Reviewer",
            "Reviews code",
            vec![SkillRef::Name("code-review".into())],
            "Reviews",
        );

        let required = vec!["coding", "testing", "security-audit", "debugging"];
        let roles = vec![role_a, role_b];
        let missing = uncovered_skills(&required, &roles);

        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"testing".to_string()));
        assert!(missing.contains(&"security-audit".to_string()));
    }

    #[test]
    fn test_uncovered_skills_all_covered() {
        let role = build_role(
            "Full Stack",
            "Does everything",
            vec![
                SkillRef::Name("coding".into()),
                SkillRef::Name("testing".into()),
            ],
            "Everything",
        );

        let required = vec!["coding", "testing"];
        let missing = uncovered_skills(&required, &[role]);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_uncovered_skills_empty_roles() {
        let required = vec!["coding", "testing"];
        let missing = uncovered_skills(&required, &[]);
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"coding".to_string()));
        assert!(missing.contains(&"testing".to_string()));
    }

    #[test]
    fn test_uncovered_skills_ignores_non_name_refs() {
        let role = build_role(
            "Inline Role",
            "Has inline skills only",
            vec![
                SkillRef::Inline("coding instructions".into()),
                SkillRef::File(PathBuf::from("skills/coding.md")),
            ],
            "Outcome",
        );

        let required = vec!["coding"];
        let missing = uncovered_skills(&required, &[role]);
        // Inline and File refs don't match by name
        assert_eq!(missing, vec!["coding"]);
    }

    // -- Agent I/O roundtrip tests -------------------------------------------

    #[test]
    fn test_agent_roundtrip_all_fields() {
        let tmp = TempDir::new().unwrap();
        let agent = sample_agent();
        let path = save_agent(&agent, tmp.path()).unwrap();
        assert!(path.exists());
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{}.yaml", agent.id)
        );

        let loaded = load_agent(&path).unwrap();
        assert_eq!(loaded.id, agent.id);
        assert_eq!(loaded.role_id, agent.role_id);
        assert_eq!(loaded.motivation_id, agent.motivation_id);
        assert_eq!(loaded.name, agent.name);
        assert_eq!(loaded.performance.task_count, 0);
        assert!(loaded.performance.avg_score.is_none());
        assert_eq!(loaded.capabilities, vec!["rust", "testing"]);
        assert_eq!(loaded.rate, Some(50.0));
        assert_eq!(loaded.capacity, Some(3.0));
        assert_eq!(loaded.trust_level, TrustLevel::Verified);
        assert_eq!(loaded.contact, Some("agent@example.com".into()));
        assert_eq!(loaded.executor, "matrix");
    }

    #[test]
    fn test_agent_roundtrip_defaults() {
        let tmp = TempDir::new().unwrap();
        let role = sample_role();
        let motivation = sample_motivation();
        let id = content_hash_agent(&role.id, &motivation.id);
        let agent = Agent {
            id,
            role_id: role.id,
            motivation_id: motivation.id,
            name: "Default Agent".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
        };
        let path = save_agent(&agent, tmp.path()).unwrap();
        let loaded = load_agent(&path).unwrap();
        assert_eq!(loaded.capabilities, Vec::<String>::new());
        assert_eq!(loaded.rate, None);
        assert_eq!(loaded.capacity, None);
        assert_eq!(loaded.trust_level, TrustLevel::Provisional);
        assert_eq!(loaded.contact, None);
        assert_eq!(loaded.executor, "claude");
    }

    #[test]
    fn test_load_all_agents_sorted() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let r1 = build_role("R1", "Role 1", vec![], "O1");
        let r2 = build_role("R2", "Role 2", vec![], "O2");
        let m = sample_motivation();

        let a1 = Agent {
            id: content_hash_agent(&r1.id, &m.id),
            role_id: r1.id.clone(),
            motivation_id: m.id.clone(),
            name: "Agent 1".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
        };
        let a2 = Agent {
            id: content_hash_agent(&r2.id, &m.id),
            role_id: r2.id.clone(),
            motivation_id: m.id.clone(),
            name: "Agent 2".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
        };

        save_agent(&a1, dir).unwrap();
        save_agent(&a2, dir).unwrap();

        let all = load_all_agents(dir).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].id < all[1].id, "Agents should be sorted by ID");
    }

    #[test]
    fn test_load_all_agents_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let agents = load_all_agents(tmp.path()).unwrap();
        assert!(agents.is_empty());
    }

    #[test]
    fn test_load_all_agents_nonexistent_dir() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("no-such-dir");
        let agents = load_all_agents(&missing).unwrap();
        assert!(agents.is_empty());
    }

    #[test]
    fn test_save_agent_creates_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("deep").join("agents");
        let agent = sample_agent();
        let path = save_agent(&agent, &nested).unwrap();
        assert!(path.exists());
        assert!(nested.is_dir());
    }

    // -- Builder function tests (content-hash ID, field immutability) --------

    #[test]
    fn test_build_role_content_hash_deterministic() {
        let r1 = build_role(
            "Name A",
            "Desc",
            vec![SkillRef::Name("s".into())],
            "Outcome",
        );
        let r2 = build_role(
            "Name B",
            "Desc",
            vec![SkillRef::Name("s".into())],
            "Outcome",
        );
        // Same immutable content (skills, desired_outcome, description) => same ID
        assert_eq!(r1.id, r2.id);
        assert_eq!(r1.id.len(), 64);
    }

    #[test]
    fn test_build_role_different_description_different_id() {
        let r1 = build_role("R", "Description A", vec![], "Outcome");
        let r2 = build_role("R", "Description B", vec![], "Outcome");
        assert_ne!(r1.id, r2.id);
    }

    #[test]
    fn test_build_role_different_skills_different_id() {
        let r1 = build_role("R", "Desc", vec![SkillRef::Name("a".into())], "Outcome");
        let r2 = build_role("R", "Desc", vec![SkillRef::Name("b".into())], "Outcome");
        assert_ne!(r1.id, r2.id);
    }

    #[test]
    fn test_build_role_different_desired_outcome_different_id() {
        let r1 = build_role("R", "Desc", vec![], "Outcome A");
        let r2 = build_role("R", "Desc", vec![], "Outcome B");
        assert_ne!(r1.id, r2.id);
    }

    #[test]
    fn test_build_role_name_does_not_affect_id() {
        let r1 = build_role("Alpha", "Same desc", vec![], "Same outcome");
        let r2 = build_role("Beta", "Same desc", vec![], "Same outcome");
        // name is mutable — should NOT be part of hash
        assert_eq!(r1.id, r2.id);
    }

    #[test]
    fn test_build_role_fresh_performance() {
        let r = build_role("R", "D", vec![], "O");
        assert_eq!(r.performance.task_count, 0);
        assert!(r.performance.avg_score.is_none());
        assert!(r.performance.evaluations.is_empty());
    }

    #[test]
    fn test_build_role_default_lineage() {
        let r = build_role("R", "D", vec![], "O");
        assert!(r.lineage.parent_ids.is_empty());
        assert_eq!(r.lineage.generation, 0);
        assert_eq!(r.lineage.created_by, "human");
    }

    #[test]
    fn test_build_motivation_content_hash_deterministic() {
        let m1 = build_motivation("Name A", "Desc", vec!["a".into()], vec!["b".into()]);
        let m2 = build_motivation("Name B", "Desc", vec!["a".into()], vec!["b".into()]);
        // Same immutable content => same ID
        assert_eq!(m1.id, m2.id);
        assert_eq!(m1.id.len(), 64);
    }

    #[test]
    fn test_build_motivation_different_description_different_id() {
        let m1 = build_motivation("M", "Desc A", vec![], vec![]);
        let m2 = build_motivation("M", "Desc B", vec![], vec![]);
        assert_ne!(m1.id, m2.id);
    }

    #[test]
    fn test_build_motivation_different_acceptable_different_id() {
        let m1 = build_motivation("M", "D", vec!["x".into()], vec![]);
        let m2 = build_motivation("M", "D", vec!["y".into()], vec![]);
        assert_ne!(m1.id, m2.id);
    }

    #[test]
    fn test_build_motivation_different_unacceptable_different_id() {
        let m1 = build_motivation("M", "D", vec![], vec!["x".into()]);
        let m2 = build_motivation("M", "D", vec![], vec!["y".into()]);
        assert_ne!(m1.id, m2.id);
    }

    #[test]
    fn test_build_motivation_name_does_not_affect_id() {
        let m1 = build_motivation("Alpha", "Same", vec!["a".into()], vec!["b".into()]);
        let m2 = build_motivation("Beta", "Same", vec!["a".into()], vec!["b".into()]);
        assert_eq!(m1.id, m2.id);
    }

    #[test]
    fn test_build_motivation_fresh_performance() {
        let m = build_motivation("M", "D", vec![], vec![]);
        assert_eq!(m.performance.task_count, 0);
        assert!(m.performance.avg_score.is_none());
        assert!(m.performance.evaluations.is_empty());
    }

    #[test]
    fn test_build_motivation_default_lineage() {
        let m = build_motivation("M", "D", vec![], vec![]);
        assert!(m.lineage.parent_ids.is_empty());
        assert_eq!(m.lineage.generation, 0);
        assert_eq!(m.lineage.created_by, "human");
    }

    // -- find_*_by_prefix tests ----------------------------------------------

    #[test]
    fn test_find_role_by_prefix_exact_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let role = sample_role();
        save_role(&role, dir).unwrap();

        let found = find_role_by_prefix(dir, &role.id).unwrap();
        assert_eq!(found.id, role.id);
        assert_eq!(found.name, role.name);
    }

    #[test]
    fn test_find_role_by_prefix_short_prefix() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let role = sample_role();
        save_role(&role, dir).unwrap();

        // Use first 8 chars as prefix
        let prefix = &role.id[..8];
        let found = find_role_by_prefix(dir, prefix).unwrap();
        assert_eq!(found.id, role.id);
    }

    #[test]
    fn test_find_role_by_prefix_no_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let role = sample_role();
        save_role(&role, dir).unwrap();

        let result = find_role_by_prefix(dir, "zzzznotfound");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No role matching"));
    }

    #[test]
    fn test_find_role_by_prefix_ambiguous() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        // Create two roles — their SHA-256 IDs will both start with hex digits
        let r1 = build_role("R1", "First", vec![], "O1");
        let r2 = build_role("R2", "Second", vec![], "O2");
        save_role(&r1, dir).unwrap();
        save_role(&r2, dir).unwrap();

        // Single-char prefix that's a hex digit — likely matches both
        // Find a common prefix
        let common_len = r1
            .id
            .chars()
            .zip(r2.id.chars())
            .take_while(|(a, b)| a == b)
            .count();

        if common_len > 0 {
            let prefix = &r1.id[..common_len];
            let result = find_role_by_prefix(dir, prefix);
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(err.contains("matches"));
        }
        // If no common prefix, the two IDs diverge at char 0 — skip ambiguity test
    }

    #[test]
    fn test_find_role_by_prefix_single_char() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let role = sample_role();
        save_role(&role, dir).unwrap();

        // Single-char prefix from the role's ID
        let prefix = &role.id[..1];
        let found = find_role_by_prefix(dir, prefix).unwrap();
        assert_eq!(found.id, role.id);
    }

    #[test]
    fn test_find_role_by_prefix_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let result = find_role_by_prefix(tmp.path(), "abc");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No role matching"));
    }

    #[test]
    fn test_find_motivation_by_prefix_exact_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m = sample_motivation();
        save_motivation(&m, dir).unwrap();

        let found = find_motivation_by_prefix(dir, &m.id).unwrap();
        assert_eq!(found.id, m.id);
        assert_eq!(found.name, m.name);
    }

    #[test]
    fn test_find_motivation_by_prefix_short_prefix() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m = sample_motivation();
        save_motivation(&m, dir).unwrap();

        let prefix = &m.id[..8];
        let found = find_motivation_by_prefix(dir, prefix).unwrap();
        assert_eq!(found.id, m.id);
    }

    #[test]
    fn test_find_motivation_by_prefix_no_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m = sample_motivation();
        save_motivation(&m, dir).unwrap();

        let result = find_motivation_by_prefix(dir, "zzzznotfound");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No motivation matching")
        );
    }

    #[test]
    fn test_find_motivation_by_prefix_ambiguous() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m1 = build_motivation("M1", "First", vec!["a".into()], vec![]);
        let m2 = build_motivation("M2", "Second", vec!["b".into()], vec![]);
        save_motivation(&m1, dir).unwrap();
        save_motivation(&m2, dir).unwrap();

        let common_len = m1
            .id
            .chars()
            .zip(m2.id.chars())
            .take_while(|(a, b)| a == b)
            .count();

        if common_len > 0 {
            let prefix = &m1.id[..common_len];
            let result = find_motivation_by_prefix(dir, prefix);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("matches"));
        }
    }

    #[test]
    fn test_find_agent_by_prefix_exact_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let agent = sample_agent();
        save_agent(&agent, dir).unwrap();

        let found = find_agent_by_prefix(dir, &agent.id).unwrap();
        assert_eq!(found.id, agent.id);
        assert_eq!(found.name, agent.name);
        assert_eq!(found.executor, "matrix");
    }

    #[test]
    fn test_find_agent_by_prefix_short_prefix() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let agent = sample_agent();
        save_agent(&agent, dir).unwrap();

        let prefix = &agent.id[..8];
        let found = find_agent_by_prefix(dir, prefix).unwrap();
        assert_eq!(found.id, agent.id);
    }

    #[test]
    fn test_find_agent_by_prefix_no_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let agent = sample_agent();
        save_agent(&agent, dir).unwrap();

        let result = find_agent_by_prefix(dir, "zzzznotfound");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No agent matching")
        );
    }

    #[test]
    fn test_find_agent_by_prefix_ambiguous() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let r1 = build_role("R1", "D1", vec![], "O1");
        let r2 = build_role("R2", "D2", vec![], "O2");
        let m = sample_motivation();

        let a1 = Agent {
            id: content_hash_agent(&r1.id, &m.id),
            role_id: r1.id,
            motivation_id: m.id.clone(),
            name: "A1".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
        };
        let a2 = Agent {
            id: content_hash_agent(&r2.id, &m.id),
            role_id: r2.id,
            motivation_id: m.id.clone(),
            name: "A2".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
        };

        save_agent(&a1, dir).unwrap();
        save_agent(&a2, dir).unwrap();

        let common_len = a1
            .id
            .chars()
            .zip(a2.id.chars())
            .take_while(|(a, b)| a == b)
            .count();

        if common_len > 0 {
            let prefix = &a1.id[..common_len];
            let result = find_agent_by_prefix(dir, prefix);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("matches"));
        }
    }

    #[test]
    fn test_find_role_by_prefix_special_characters() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let role = sample_role();
        save_role(&role, dir).unwrap();

        // Prefix with special regex chars — should not cause panic, just no match
        let result = find_role_by_prefix(dir, ".*+?[]()");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No role matching"));
    }

    #[test]
    fn test_find_motivation_by_prefix_special_characters() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m = sample_motivation();
        save_motivation(&m, dir).unwrap();

        let result = find_motivation_by_prefix(dir, "^$\\{|}");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No motivation matching")
        );
    }

    #[test]
    fn test_find_agent_by_prefix_special_characters() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let agent = sample_agent();
        save_agent(&agent, dir).unwrap();

        let result = find_agent_by_prefix(dir, "!@#$%");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No agent matching")
        );
    }

    // -- is_human_executor / Agent.is_human tests ----------------------------

    #[test]
    fn test_is_human_executor_matrix() {
        assert!(is_human_executor("matrix"));
    }

    #[test]
    fn test_is_human_executor_email() {
        assert!(is_human_executor("email"));
    }

    #[test]
    fn test_is_human_executor_shell() {
        assert!(is_human_executor("shell"));
    }

    #[test]
    fn test_is_human_executor_claude_is_not_human() {
        assert!(!is_human_executor("claude"));
    }

    #[test]
    fn test_is_human_executor_empty_string() {
        assert!(!is_human_executor(""));
    }

    #[test]
    fn test_is_human_executor_unknown_string() {
        assert!(!is_human_executor("custom-ai-backend"));
    }

    #[test]
    fn test_agent_is_human_with_matrix_executor() {
        let mut agent = sample_agent();
        agent.executor = "matrix".into();
        assert!(agent.is_human());
    }

    #[test]
    fn test_agent_is_human_with_email_executor() {
        let mut agent = sample_agent();
        agent.executor = "email".into();
        assert!(agent.is_human());
    }

    #[test]
    fn test_agent_is_human_with_shell_executor() {
        let mut agent = sample_agent();
        agent.executor = "shell".into();
        assert!(agent.is_human());
    }

    #[test]
    fn test_agent_is_not_human_with_claude_executor() {
        let mut agent = sample_agent();
        agent.executor = "claude".into();
        assert!(!agent.is_human());
    }

    #[test]
    fn test_agent_is_not_human_with_default_executor() {
        let role = sample_role();
        let motivation = sample_motivation();
        let id = content_hash_agent(&role.id, &motivation.id);
        let agent = Agent {
            id,
            role_id: role.id,
            motivation_id: motivation.id,
            name: "Default".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
        };
        // default_executor() returns "claude" which is not human
        assert!(!agent.is_human());
    }
}

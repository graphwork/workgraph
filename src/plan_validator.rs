//! Plan validation for generative trace functions (Layer 2).
//!
//! Validates that a planner-generated task graph satisfies the structural
//! constraints declared in a generative function definition.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::graph::{CycleAnalysis, Node, Task, WorkGraph};
use crate::trace_function::{ForbiddenPattern, StructuralConstraints, TaskTemplate};

/// An error discovered during plan validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    TooFewTasks { count: usize, min: u32 },
    TooManyTasks { count: usize, max: u32 },
    MissingSkill(String),
    MissingPhase(String),
    ForbiddenPatternFound { tags: Vec<String>, reason: String },
    CyclesNotAllowed { cycle_count: usize },
    TooManyCycleIterations { total: u32, max: u32 },
    DepthExceeded { depth: u32, max: u32 },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooFewTasks { count, min } => {
                write!(f, "too few tasks: {count} < minimum {min}")
            }
            Self::TooManyTasks { count, max } => {
                write!(f, "too many tasks: {count} > maximum {max}")
            }
            Self::MissingSkill(s) => write!(f, "required skill not covered: {s}"),
            Self::MissingPhase(p) => write!(f, "required phase not present: {p}"),
            Self::ForbiddenPatternFound { tags, reason } => {
                write!(f, "forbidden pattern [{}]: {reason}", tags.join(", "))
            }
            Self::CyclesNotAllowed { cycle_count } => {
                write!(f, "cycles not allowed but {cycle_count} cycle(s) found")
            }
            Self::TooManyCycleIterations { total, max } => {
                write!(f, "total cycle iterations {total} exceeds maximum {max}")
            }
            Self::DepthExceeded { depth, max } => {
                write!(f, "dependency depth {depth} exceeds maximum {max}")
            }
        }
    }
}

/// Validate a generated task plan against structural constraints.
///
/// Returns `Ok(())` if all constraints are satisfied, or `Err(errors)` with
/// every violation found.
pub fn validate_plan(
    tasks: &[TaskTemplate],
    constraints: &StructuralConstraints,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    // --- Task count bounds ---
    let count = tasks.len();
    if let Some(min) = constraints.min_tasks
        && count < min as usize {
            errors.push(ValidationError::TooFewTasks { count, min });
        }
    if let Some(max) = constraints.max_tasks
        && count > max as usize {
            errors.push(ValidationError::TooManyTasks { count, max });
        }

    // --- Required skills coverage ---
    let all_skills: HashSet<&str> = tasks
        .iter()
        .flat_map(|t| t.skills.iter().map(|s| s.as_str()))
        .collect();
    for skill in &constraints.required_skills {
        if !all_skills.contains(skill.as_str()) {
            errors.push(ValidationError::MissingSkill(skill.clone()));
        }
    }

    // --- Required phases (matched via tags) ---
    let all_tags: HashSet<&str> = tasks
        .iter()
        .flat_map(|t| t.tags.iter().map(|s| s.as_str()))
        .collect();
    for phase in &constraints.required_phases {
        if !all_tags.contains(phase.as_str()) {
            errors.push(ValidationError::MissingPhase(phase.clone()));
        }
    }

    // --- Forbidden patterns ---
    for fp in &constraints.forbidden_patterns {
        check_forbidden_pattern(tasks, fp, &mut errors);
    }

    // --- Cycle constraints via CycleAnalysis ---
    let graph = build_temp_graph(tasks);
    let cycle_analysis = CycleAnalysis::from_graph(&graph);

    if !constraints.allow_cycles && !cycle_analysis.cycles.is_empty() {
        errors.push(ValidationError::CyclesNotAllowed {
            cycle_count: cycle_analysis.cycles.len(),
        });
    }

    if let Some(max_iter) = constraints.max_total_iterations {
        let total: u32 = tasks
            .iter()
            .flat_map(|t| &t.loops_to)
            .map(|l| l.max_iterations)
            .sum();
        if total > max_iter {
            errors.push(ValidationError::TooManyCycleIterations {
                total,
                max: max_iter,
            });
        }
    }

    // --- Dependency depth (BFS) ---
    if let Some(max_depth) = constraints.max_depth {
        let depth = compute_max_depth(tasks);
        if depth > max_depth {
            errors.push(ValidationError::DepthExceeded {
                depth,
                max: max_depth,
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Check if any task matches a forbidden pattern (has ALL the listed tags).
fn check_forbidden_pattern(
    tasks: &[TaskTemplate],
    pattern: &ForbiddenPattern,
    errors: &mut Vec<ValidationError>,
) {
    if pattern.tags.is_empty() {
        return;
    }
    let required: HashSet<&str> = pattern.tags.iter().map(|s| s.as_str()).collect();
    for task in tasks {
        let task_tags: HashSet<&str> = task.tags.iter().map(|s| s.as_str()).collect();
        if required.is_subset(&task_tags) {
            errors.push(ValidationError::ForbiddenPatternFound {
                tags: pattern.tags.clone(),
                reason: pattern.reason.clone(),
            });
            return; // one match per pattern is enough
        }
    }
}

/// Build a temporary WorkGraph from TaskTemplates for cycle analysis.
fn build_temp_graph(templates: &[TaskTemplate]) -> WorkGraph {
    let mut graph = WorkGraph::new();
    for t in templates {
        let task = Task {
            id: t.template_id.clone(),
            title: t.title.clone(),
            after: t.after.clone(),
            skills: t.skills.clone(),
            tags: t.tags.clone(),
            ..Default::default()
        };
        graph.add_node(Node::Task(task));
    }
    graph
}

/// Compute max dependency depth via BFS from root nodes (zero in-degree).
fn compute_max_depth(tasks: &[TaskTemplate]) -> u32 {
    if tasks.is_empty() {
        return 0;
    }

    let ids: HashSet<&str> = tasks.iter().map(|t| t.template_id.as_str()).collect();

    // Build adjacency: parent → children
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();

    for t in tasks {
        in_degree.entry(t.template_id.as_str()).or_insert(0);
        children.entry(t.template_id.as_str()).or_default();
        for dep in &t.after {
            if ids.contains(dep.as_str()) {
                children
                    .entry(dep.as_str())
                    .or_default()
                    .push(t.template_id.as_str());
                *in_degree.entry(t.template_id.as_str()).or_insert(0) += 1;
            }
        }
    }

    // BFS tracking depth per level
    let mut queue: VecDeque<(&str, u32)> = VecDeque::new();
    for (&id, &deg) in &in_degree {
        if deg == 0 {
            queue.push_back((id, 0));
        }
    }

    let mut max_depth: u32 = 0;
    let mut best: HashMap<&str, u32> = HashMap::new();

    while let Some((id, depth)) = queue.pop_front() {
        if let Some(&prev) = best.get(id)
            && depth <= prev {
                continue;
            }
        best.insert(id, depth);
        if depth > max_depth {
            max_depth = depth;
        }
        if let Some(kids) = children.get(id) {
            for &kid in kids {
                let new_depth = depth + 1;
                if best.get(kid).is_none_or(|&d| d < new_depth) {
                    queue.push_back((kid, new_depth));
                }
            }
        }
    }

    max_depth
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_function::LoopEdgeTemplate;

    fn template(id: &str) -> TaskTemplate {
        TaskTemplate {
            template_id: id.to_string(),
            title: id.to_string(),
            description: String::new(),
            skills: vec![],
            after: vec![],
            loops_to: vec![],
            role_hint: None,
            deliverables: vec![],
            verify: None,
            tags: vec![],
        }
    }

    fn empty_constraints() -> StructuralConstraints {
        StructuralConstraints {
            min_tasks: None,
            max_tasks: None,
            required_skills: vec![],
            max_depth: None,
            allow_cycles: false,
            max_total_iterations: None,
            required_phases: vec![],
            forbidden_patterns: vec![],
        }
    }

    #[test]
    fn valid_plan_passes() {
        let tasks = vec![template("a"), template("b")];
        assert!(validate_plan(&tasks, &empty_constraints()).is_ok());
    }

    #[test]
    fn too_few_tasks() {
        let tasks = vec![template("a")];
        let c = StructuralConstraints {
            min_tasks: Some(3),
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::TooFewTasks { count: 1, min: 3 }
        )));
    }

    #[test]
    fn too_many_tasks() {
        let tasks = vec![template("a"), template("b"), template("c")];
        let c = StructuralConstraints {
            max_tasks: Some(2),
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::TooManyTasks { count: 3, max: 2 }
        )));
    }

    #[test]
    fn missing_required_skill() {
        let mut t = template("a");
        t.skills = vec!["rust".to_string()];
        let tasks = vec![t, template("b")];
        let c = StructuralConstraints {
            required_skills: vec!["rust".to_string(), "python".to_string()],
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(matches!(&errs[0], ValidationError::MissingSkill(s) if s == "python"));
    }

    #[test]
    fn required_skills_covered() {
        let mut a = template("a");
        a.skills = vec!["rust".to_string()];
        let mut b = template("b");
        b.skills = vec!["testing".to_string()];
        let tasks = vec![a, b];
        let c = StructuralConstraints {
            required_skills: vec!["rust".to_string(), "testing".to_string()],
            ..empty_constraints()
        };
        assert!(validate_plan(&tasks, &c).is_ok());
    }

    #[test]
    fn missing_required_phase() {
        let mut a = template("a");
        a.tags = vec!["implement".to_string()];
        let tasks = vec![a];
        let c = StructuralConstraints {
            required_phases: vec!["implement".to_string(), "test".to_string()],
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(matches!(&errs[0], ValidationError::MissingPhase(p) if p == "test"));
    }

    #[test]
    fn required_phases_present() {
        let mut a = template("a");
        a.tags = vec!["implement".to_string()];
        let mut b = template("b");
        b.tags = vec!["test".to_string()];
        let tasks = vec![a, b];
        let c = StructuralConstraints {
            required_phases: vec!["implement".to_string(), "test".to_string()],
            ..empty_constraints()
        };
        assert!(validate_plan(&tasks, &c).is_ok());
    }

    #[test]
    fn forbidden_pattern_detected() {
        let mut a = template("a");
        a.tags = vec!["deploy".to_string(), "production".to_string()];
        let tasks = vec![a, template("b")];
        let c = StructuralConstraints {
            forbidden_patterns: vec![ForbiddenPattern {
                tags: vec!["deploy".to_string(), "production".to_string()],
                reason: "no direct production deploys".to_string(),
            }],
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            &errs[0],
            ValidationError::ForbiddenPatternFound { reason, .. }
            if reason == "no direct production deploys"
        ));
    }

    #[test]
    fn forbidden_pattern_partial_match_is_ok() {
        let mut a = template("a");
        a.tags = vec!["deploy".to_string()]; // missing "production"
        let tasks = vec![a];
        let c = StructuralConstraints {
            forbidden_patterns: vec![ForbiddenPattern {
                tags: vec!["deploy".to_string(), "production".to_string()],
                reason: "no direct production deploys".to_string(),
            }],
            ..empty_constraints()
        };
        assert!(validate_plan(&tasks, &c).is_ok());
    }

    #[test]
    fn cycles_not_allowed() {
        let mut a = template("a");
        a.after = vec!["b".to_string()];
        let mut b = template("b");
        b.after = vec!["a".to_string()];
        let tasks = vec![a, b];
        let c = StructuralConstraints {
            allow_cycles: false,
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::CyclesNotAllowed { .. })));
    }

    #[test]
    fn cycles_allowed() {
        let mut a = template("a");
        a.after = vec!["b".to_string()];
        let mut b = template("b");
        b.after = vec!["a".to_string()];
        let tasks = vec![a, b];
        let c = StructuralConstraints {
            allow_cycles: true,
            ..empty_constraints()
        };
        assert!(validate_plan(&tasks, &c).is_ok());
    }

    #[test]
    fn max_total_iterations_exceeded() {
        let mut a = template("a");
        a.loops_to = vec![LoopEdgeTemplate {
            target: "b".to_string(),
            max_iterations: 5,
            guard: None,
            delay: None,
        }];
        let mut b = template("b");
        b.loops_to = vec![LoopEdgeTemplate {
            target: "a".to_string(),
            max_iterations: 4,
            guard: None,
            delay: None,
        }];
        let tasks = vec![a, b];
        let c = StructuralConstraints {
            allow_cycles: true,
            max_total_iterations: Some(8),
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::TooManyCycleIterations { total: 9, max: 8 }
        )));
    }

    #[test]
    fn max_total_iterations_within_bounds() {
        let mut a = template("a");
        a.loops_to = vec![LoopEdgeTemplate {
            target: "b".to_string(),
            max_iterations: 3,
            guard: None,
            delay: None,
        }];
        let tasks = vec![a, template("b")];
        let c = StructuralConstraints {
            allow_cycles: true,
            max_total_iterations: Some(5),
            ..empty_constraints()
        };
        assert!(validate_plan(&tasks, &c).is_ok());
    }

    #[test]
    fn depth_exceeded() {
        // a → b → c → d (depth 3)
        let a = template("a");
        let mut b = template("b");
        b.after = vec!["a".to_string()];
        let mut c = template("c");
        c.after = vec!["b".to_string()];
        let mut d = template("d");
        d.after = vec!["c".to_string()];
        let tasks = vec![a, b, c, d];
        let constraints = StructuralConstraints {
            max_depth: Some(2),
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &constraints).unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e,
            ValidationError::DepthExceeded { depth: 3, max: 2 }
        )));
    }

    #[test]
    fn depth_within_bounds() {
        let a = template("a");
        let mut b = template("b");
        b.after = vec!["a".to_string()];
        let mut c = template("c");
        c.after = vec!["a".to_string()]; // parallel to b, both depth 1
        let tasks = vec![a, b, c];
        let constraints = StructuralConstraints {
            max_depth: Some(1),
            ..empty_constraints()
        };
        assert!(validate_plan(&tasks, &constraints).is_ok());
    }

    #[test]
    fn diamond_depth() {
        // a → b, a → c, b → d, c → d  (depth 2)
        let a = template("a");
        let mut b = template("b");
        b.after = vec!["a".to_string()];
        let mut c = template("c");
        c.after = vec!["a".to_string()];
        let mut d = template("d");
        d.after = vec!["b".to_string(), "c".to_string()];
        let tasks = vec![a, b, c, d];
        let constraints = StructuralConstraints {
            max_depth: Some(2),
            ..empty_constraints()
        };
        assert!(validate_plan(&tasks, &constraints).is_ok());
    }

    #[test]
    fn multiple_errors_collected() {
        let tasks = vec![template("a")];
        let c = StructuralConstraints {
            min_tasks: Some(3),
            required_skills: vec!["rust".to_string()],
            required_phases: vec!["test".to_string()],
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert_eq!(errs.len(), 3);
    }

    #[test]
    fn empty_plan_empty_constraints() {
        let tasks: Vec<TaskTemplate> = vec![];
        assert!(validate_plan(&tasks, &empty_constraints()).is_ok());
    }

    #[test]
    fn empty_plan_with_min_tasks() {
        let tasks: Vec<TaskTemplate> = vec![];
        let c = StructuralConstraints {
            min_tasks: Some(1),
            ..empty_constraints()
        };
        let errs = validate_plan(&tasks, &c).unwrap_err();
        assert!(matches!(
            &errs[0],
            ValidationError::TooFewTasks { count: 0, min: 1 }
        ));
    }

    #[test]
    fn display_impl() {
        let e = ValidationError::TooFewTasks { count: 1, min: 3 };
        assert_eq!(e.to_string(), "too few tasks: 1 < minimum 3");

        let e = ValidationError::MissingSkill("rust".to_string());
        assert_eq!(e.to_string(), "required skill not covered: rust");

        let e = ValidationError::ForbiddenPatternFound {
            tags: vec!["a".to_string(), "b".to_string()],
            reason: "bad".to_string(),
        };
        assert_eq!(e.to_string(), "forbidden pattern [a, b]: bad");
    }
}

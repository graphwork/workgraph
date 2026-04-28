use super::hash::*;
use super::store::{AgencyError, init, save_component, save_outcome, save_role, save_tradeoff};
use super::types::*;
use std::path::Path;

/// Helper to build a Role (composition cache entry) with its content-hash ID.
///
/// `component_ids` are IDs of RoleComponent primitives this role is composed from.
/// `outcome_id` is the ID (or description string) of the DesiredOutcome primitive.
pub fn build_role(
    name: impl Into<String>,
    description: impl Into<String>,
    component_ids: Vec<String>,
    outcome_id: impl Into<String>,
) -> Role {
    let description = description.into();
    let outcome_id = outcome_id.into();
    let id = content_hash_role(&component_ids, &outcome_id);
    Role {
        id,
        name: name.into(),
        description,
        component_ids,
        outcome_id,
        performance: PerformanceRecord::default(),
        lineage: Lineage::default(),
        default_context_scope: None,
        default_exec_mode: None,
    }
}

/// Helper to build a RoleComponent with its content-hash ID computed automatically.
pub fn build_component(
    name: impl Into<String>,
    description: impl Into<String>,
    category: ComponentCategory,
    content: ContentRef,
) -> RoleComponent {
    let description = description.into();
    let id = content_hash_component(&description, &category, &content);
    RoleComponent {
        id,
        name: name.into(),
        description,
        category,
        content,
        performance: PerformanceRecord::default(),
        lineage: Lineage::default(),
        access_control: AccessControl::default(),
        domain_tags: vec![],
        metadata: std::collections::HashMap::new(),
        former_agents: vec![],
        former_deployments: vec![],
    }
}

/// Helper to build a DesiredOutcome with its content-hash ID computed automatically.
pub fn build_outcome(
    name: impl Into<String>,
    description: impl Into<String>,
    success_criteria: Vec<String>,
) -> DesiredOutcome {
    let description = description.into();
    let id = content_hash_outcome(&description, &success_criteria);
    DesiredOutcome {
        id,
        name: name.into(),
        description,
        success_criteria,
        performance: PerformanceRecord::default(),
        lineage: Lineage::default(),
        access_control: AccessControl::default(),
        requires_human_oversight: true,
        domain_tags: vec![],
        metadata: std::collections::HashMap::new(),
        former_agents: vec![],
        former_deployments: vec![],
    }
}

/// Helper to build a TradeoffConfig with its content-hash ID computed automatically.
pub fn build_tradeoff(
    name: impl Into<String>,
    description: impl Into<String>,
    acceptable_tradeoffs: Vec<String>,
    unacceptable_tradeoffs: Vec<String>,
) -> TradeoffConfig {
    let description = description.into();
    let id = content_hash_tradeoff(&acceptable_tradeoffs, &unacceptable_tradeoffs, &description);
    TradeoffConfig {
        id,
        name: name.into(),
        description,
        acceptable_tradeoffs,
        unacceptable_tradeoffs,
        performance: PerformanceRecord::default(),
        lineage: Lineage::default(),
        access_control: AccessControl::default(),
        domain_tags: vec![],
        metadata: std::collections::HashMap::new(),
        former_agents: vec![],
        former_deployments: vec![],
    }
}

/// Return the set of built-in starter components (role capabilities).
pub fn starter_components() -> Vec<RoleComponent> {
    vec![
        build_component(
            "code-writing",
            "Writes production-quality code.",
            ComponentCategory::Translated,
            ContentRef::Name("code-writing".into()),
        ),
        build_component(
            "testing",
            "Writes and runs tests.",
            ComponentCategory::Translated,
            ContentRef::Name("testing".into()),
        ),
        build_component(
            "debugging",
            "Diagnoses and fixes bugs.",
            ComponentCategory::Translated,
            ContentRef::Name("debugging".into()),
        ),
        build_component(
            "code-review",
            "Reviews code for correctness and style.",
            ComponentCategory::Translated,
            ContentRef::Name("code-review".into()),
        ),
        build_component(
            "security-audit",
            "Audits code for security vulnerabilities.",
            ComponentCategory::Translated,
            ContentRef::Name("security-audit".into()),
        ),
        build_component(
            "technical-writing",
            "Produces clear technical documentation.",
            ComponentCategory::Translated,
            ContentRef::Name("technical-writing".into()),
        ),
        build_component(
            "system-design",
            "Designs system architectures.",
            ComponentCategory::Translated,
            ContentRef::Name("system-design".into()),
        ),
        build_component(
            "dependency-analysis",
            "Analyzes dependencies and structural decisions.",
            ComponentCategory::Translated,
            ContentRef::Name("dependency-analysis".into()),
        ),
    ]
}

/// Return the set of built-in starter outcomes.
pub fn starter_outcomes() -> Vec<DesiredOutcome> {
    vec![
        build_outcome("Working, tested code", "Working, tested code", vec![]),
        build_outcome(
            "Review report with findings",
            "Review report with findings",
            vec![],
        ),
        build_outcome("Clear documentation", "Clear documentation", vec![]),
        build_outcome(
            "Design document with rationale",
            "Design document with rationale",
            vec![],
        ),
    ]
}

/// Return the set of built-in starter roles that ship with wg.
///
/// References starter components and outcomes by their content-hash IDs.
pub fn starter_roles() -> Vec<Role> {
    let components = starter_components();
    let outcomes = starter_outcomes();

    // Build a lookup by name for convenience
    let comp_id = |name: &str| -> String {
        components
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.id.clone())
            .unwrap_or_default()
    };
    let out_id = |name: &str| -> String {
        outcomes
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.id.clone())
            .unwrap_or_default()
    };

    vec![
        build_role(
            "Programmer",
            "Writes, tests, and debugs code to implement features and fix bugs.",
            vec![
                comp_id("code-writing"),
                comp_id("testing"),
                comp_id("debugging"),
            ],
            out_id("Working, tested code"),
        ),
        build_role(
            "Reviewer",
            "Reviews code for correctness, security, and style.",
            vec![comp_id("code-review"), comp_id("security-audit")],
            out_id("Review report with findings"),
        ),
        build_role(
            "Documenter",
            "Produces clear, accurate technical documentation.",
            vec![comp_id("technical-writing")],
            out_id("Clear documentation"),
        ),
        build_role(
            "Architect",
            "Designs systems, analyzes dependencies, and makes structural decisions.",
            vec![comp_id("system-design"), comp_id("dependency-analysis")],
            out_id("Design document with rationale"),
        ),
    ]
}

/// Return the set of built-in starter tradeoffs (formerly motivations) that ship with wg.
pub fn starter_tradeoffs() -> Vec<TradeoffConfig> {
    vec![
        build_tradeoff(
            "Careful",
            "Prioritizes reliability and correctness above speed.",
            vec!["Slow".into(), "Verbose".into()],
            vec!["Unreliable".into(), "Untested".into()],
        ),
        build_tradeoff(
            "Fast",
            "Prioritizes speed and shipping over polish.",
            vec!["Less documentation".into(), "Simpler solutions".into()],
            vec!["Broken code".into()],
        ),
        build_tradeoff(
            "Thorough",
            "Prioritizes completeness and depth of analysis.",
            vec!["Expensive".into(), "Slow".into(), "Verbose".into()],
            vec!["Incomplete analysis".into()],
        ),
        build_tradeoff(
            "Balanced",
            "Moderate on all dimensions; balances speed, quality, and completeness.",
            vec!["Moderate trade-offs on any single dimension".into()],
            vec!["Extreme compromise on any dimension".into()],
        ),
    ]
}

// ---------------------------------------------------------------------------
// Special agent primitives (assigner, evaluator, evolver)
// ---------------------------------------------------------------------------

/// Return the role components for the assigner special agent.
pub fn assigner_components() -> Vec<RoleComponent> {
    vec![
        build_component(
            "task-to-component-matching",
            "Evaluate closeness of fit between a task description and each candidate role component.",
            ComponentCategory::Novel,
            ContentRef::Name("task-to-component-matching".into()),
        ),
        build_component(
            "task-to-outcome-matching",
            "Evaluate closeness of fit between task requirements and a desired outcome specification.",
            ComponentCategory::Novel,
            ContentRef::Name("task-to-outcome-matching".into()),
        ),
        build_component(
            "task-to-tradeoff-matching",
            "Evaluate whether a task's constraints are compatible with a candidate trade-off configuration.",
            ComponentCategory::Novel,
            ContentRef::Name("task-to-tradeoff-matching".into()),
        ),
        build_component(
            "historical-performance-weighting",
            "Use past evaluation data on agents and primitives to weight match scores. A role component with strong performance in analogous tasks receives higher match weight.",
            ComponentCategory::Novel,
            ContentRef::Name("historical-performance-weighting".into()),
        ),
        build_component(
            "composition-cache-search",
            "Query the composition cache for pre-composed agents, rank by fit score, return best match. Used in performance mode.",
            ComponentCategory::Novel,
            ContentRef::Name("composition-cache-search".into()),
        ),
        build_component(
            "primitive-first-composition",
            "Assemble novel agent configurations from the primitive store without cache bias; record the composition rationale for retrospective analysis. Used in learning mode.",
            ComponentCategory::Novel,
            ContentRef::Name("primitive-first-composition".into()),
        ),
        build_component(
            "task-clarification",
            "When a task is missing one or more well-formed criteria (what it is, how to know when done, how to evaluate quality), request clarification before assigning.",
            ComponentCategory::Novel,
            ContentRef::Name("task-clarification".into()),
        ),
    ]
}

/// Return the role components for the evaluator special agent.
pub fn evaluator_components() -> Vec<RoleComponent> {
    vec![
        build_component(
            "cardinal-scale-grading",
            "Produce a numerical score (0.0–1.0) with calibrated confidence. The primary grading modality.",
            ComponentCategory::Novel,
            ContentRef::Name("cardinal-scale-grading".into()),
        ),
        build_component(
            "ordinal-scale-grading",
            "Rank performance relative to a reference set (other agents, historical baselines) without producing absolute scores. Useful when absolute calibration is difficult.",
            ComponentCategory::Novel,
            ContentRef::Name("ordinal-scale-grading".into()),
        ),
        build_component(
            "rubric-interpretation",
            "Parse and apply an explicit rubric provided with the task. Maps to rubric specification spectrum levels 1–4.",
            ComponentCategory::Novel,
            ContentRef::Name("rubric-interpretation".into()),
        ),
        build_component(
            "domain-specific-evaluation-standards",
            "Apply evaluation norms from a particular field (e.g., software engineering, research, creative writing). Invoked when task rubric specifies a domain standard.",
            ComponentCategory::Novel,
            ContentRef::Name("domain-specific-evaluation-standards".into()),
        ),
        build_component(
            "underspecification-detection",
            "Identify when a task has no rubric (control by omission) and flag this before grading rather than making arbitrary meaningmaking decisions.",
            ComponentCategory::Novel,
            ContentRef::Name("underspecification-detection".into()),
        ),
        build_component(
            "grade-transparency",
            "Produce grades with sufficient rationale that a human reviewer or peer evaluator can assess the grading quality. Makes the evaluator evaluable.",
            ComponentCategory::Novel,
            ContentRef::Name("grade-transparency".into()),
        ),
    ]
}

/// Return the role components for the evolver special agent.
pub fn evolver_components() -> Vec<RoleComponent> {
    vec![
        build_component(
            "wording-mutation",
            "Change the wording of a role component while preserving its general meaning. Tests whether articulation precision affects performance.",
            ComponentCategory::Novel,
            ContentRef::Name("wording-mutation".into()),
        ),
        build_component(
            "component-substitution",
            "Swap one role component for a similar-but-different one. Tests whether the conceptual difference between similar components is significant.",
            ComponentCategory::Novel,
            ContentRef::Name("component-substitution".into()),
        ),
        build_component(
            "configuration-mutation",
            "Change how role components are combined into a role or agent without changing the individual components. Tests whether composition structure matters.",
            ComponentCategory::Novel,
            ContentRef::Name("configuration-mutation".into()),
        ),
        build_component(
            "randomisation",
            "Select from the existing primitive pool and recombine without attractor-area bias. Explores existing primitive space without conventional constraints.",
            ComponentCategory::Novel,
            ContentRef::Name("randomisation".into()),
        ),
        build_component(
            "bizarre-ideation",
            "Generate entirely novel primitives unconstrained by the current store. Operates outside the existing primitive space.",
            ComponentCategory::Novel,
            ContentRef::Name("bizarre-ideation".into()),
        ),
        build_component(
            "crossover-recombination",
            "Blend attributes from two parent primitives to produce offspring that combine strengths of both.",
            ComponentCategory::Novel,
            ContentRef::Name("crossover-recombination".into()),
        ),
        build_component(
            "gap-analysis",
            "Identify structural gaps in the current primitive pool where new capabilities would improve coverage.",
            ComponentCategory::Novel,
            ContentRef::Name("gap-analysis".into()),
        ),
        build_component(
            "retirement-identification",
            "Identify and retire underperforming primitives based on evaluation data and usage patterns.",
            ComponentCategory::Novel,
            ContentRef::Name("retirement-identification".into()),
        ),
    ]
}

/// Return the role components for the agent creator special agent.
pub fn creator_components() -> Vec<RoleComponent> {
    vec![
        build_component(
            "research-literature-search",
            "Search academic and practitioner literature for documented effective role structures, workflows, or task execution patterns. Target: role components and desired outcomes that have empirical grounding.",
            ComponentCategory::Novel,
            ContentRef::Name("research-literature-search".into()),
        ),
        build_component(
            "analogous-domain-identification",
            "Identify domains with structural similarities to the current work. Enables targeted distant search rather than undirected exploration.",
            ComponentCategory::Novel,
            ContentRef::Name("analogous-domain-identification".into()),
        ),
        build_component(
            "structural-similarity-recognition",
            "Given an existing primitive, recognise structurally similar capabilities in distant domains (e.g., systematic adversarial testing in software engineering maps to red team methodology in security).",
            ComponentCategory::Novel,
            ContentRef::Name("structural-similarity-recognition".into()),
        ),
        build_component(
            "absorptive-capacity-assessment",
            "Evaluate whether the current primitive store has enough related capability to usefully absorb a candidate new primitive (Cohen & Levinthal, 1990). Flag distant primitives that require prerequisite capabilities the agency does not yet have.",
            ComponentCategory::Novel,
            ContentRef::Name("absorptive-capacity-assessment".into()),
        ),
        build_component(
            "federation-import",
            "Recognise and import known-good primitives from other Agency instances. Internal proximity in the proximity continuum.",
            ComponentCategory::Novel,
            ContentRef::Name("federation-import".into()),
        ),
        build_component(
            "primitive-candidate-specification",
            "Articulate a candidate new primitive at the correct granularity: independently testable, meaningfully recombinable, single-typed. Produces specification with provenance notes.",
            ComponentCategory::Novel,
            ContentRef::Name("primitive-candidate-specification".into()),
        ),
    ]
}

/// Return the desired outcomes for special agents.
pub fn special_agent_outcomes() -> Vec<DesiredOutcome> {
    vec![
        build_outcome(
            "Optimal agent-task assignment",
            "The closest available agent configuration for the task, with a confidence score, a match rationale, and a flag if the task was underspecified.",
            vec![
                "Agent configuration matches task requirements".into(),
                "Confidence score provided".into(),
                "Match rationale documented".into(),
            ],
        ),
        build_outcome(
            "Calibrated evaluation grade",
            "A calibrated grade (0.0–1.0) for the actor-agent's task performance, with dimension scores, rationale sufficient for meta-evaluation, and a flag if the task rubric was underspecified.",
            vec![
                "Grade is calibrated and accurate".into(),
                "Dimension scores provided".into(),
                "Rationale sufficient for meta-evaluation".into(),
            ],
        ),
        build_outcome(
            "Proposed primitive modifications",
            "A proposed set of primitive modifications at the specified level and amount, with rationale and lineage tracking.",
            vec![
                "Modifications target specified level and amount".into(),
                "Rationale provided for each change".into(),
                "Lineage tracking maintained".into(),
            ],
        ),
        build_outcome(
            "New primitive candidates",
            "New primitive candidates sourced from outside the current store, specified at the correct granularity, with provenance notes and absorptive capacity flags.",
            vec![
                "Sourced from outside current primitive store".into(),
                "Independently testable and recombinable".into(),
                "Provenance notes included".into(),
                "Absorptive capacity assessment provided".into(),
            ],
        ),
    ]
}

/// Return the trade-off configurations for special agents.
pub fn special_agent_tradeoffs() -> Vec<TradeoffConfig> {
    vec![
        // Assigner tradeoffs
        build_tradeoff(
            "Assigner Balanced",
            "Balanced assigner: flags low-confidence assignments, makes short clarification requests when needed.",
            vec![
                "Flagging low-confidence assignments".into(),
                "Short clarification requests".into(),
            ],
            vec![
                "Blocking on ambiguity that does not affect match quality".into(),
                "Failing silently without flagging issues".into(),
            ],
        ),
        // Evaluator tradeoffs
        build_tradeoff(
            "Evaluator Balanced",
            "Balanced evaluator: standard rubric application with reasonable benefit of doubt. Proper scoring rule incentive structure.",
            vec![
                "Standard rubric application".into(),
                "Reasonable benefit of doubt".into(),
            ],
            vec![
                "Arbitrary grade inflation or deflation".into(),
                "Strategic grading to optimize own performance history".into(),
            ],
        ),
        // Evolver tradeoffs
        build_tradeoff(
            "Evolver Balanced",
            "Balanced evolver: moderate exploration with validation. Balances speed vs quality of proposals.",
            vec![
                "Moderate exploration intensity".into(),
                "Multi-step validation of proposals".into(),
            ],
            vec![
                "Changing desired outcomes without human gate".into(),
                "Extreme disruption to working configurations".into(),
            ],
        ),
        // Agent Creator tradeoffs — four proximity-continuum configurations
        build_tradeoff(
            "Creator Unconstrained",
            "Agent creator chooses search domain freely. Maximum exploration breadth with absorptive capacity assessment active.",
            vec![
                "Searching any domain the creator judges relevant".into(),
                "Wide exploration with uncertain payoff".into(),
            ],
            vec![
                "Importing primitives without absorptive capacity assessment".into(),
                "Ignoring provenance tracking".into(),
            ],
        ),
        build_tradeoff(
            "Creator Adjacent",
            "Search restricted to domains the human specifies as similar to current work. Moderate exploration with high absorptive capacity.",
            vec![
                "Focused search in human-specified adjacent domains".into(),
                "Higher confidence in absorbability".into(),
            ],
            vec![
                "Searching outside specified adjacent domains".into(),
                "Importing without domain relevance justification".into(),
            ],
        ),
        build_tradeoff(
            "Creator Distant",
            "Search restricted to domains the human specifies as far away. High novelty potential but absorptive capacity assessment is critical.",
            vec![
                "Distant domain search for novel primitives".into(),
                "Accepting higher uncertainty in exchange for novelty".into(),
            ],
            vec![
                "Skipping absorptive capacity assessment for distant imports".into(),
                "Importing distant primitives into a thin primitive store without warning".into(),
            ],
        ),
        build_tradeoff(
            "Creator Internal",
            "Search restricted to existing projects accessible to the human (federation). Lowest risk, highest absorbability.",
            vec![
                "Federation-only import from known-good stores".into(),
                "Prioritising proven primitives over novel ones".into(),
            ],
            vec![
                "Searching outside federated stores".into(),
                "Importing without checking existing store overlap".into(),
            ],
        ),
    ]
}

/// Return the roles for special agents, composed from their specific components and outcomes.
pub fn special_agent_roles() -> Vec<Role> {
    let a_comps = assigner_components();
    let e_comps = evaluator_components();
    let v_comps = evolver_components();
    let c_comps = creator_components();
    let outcomes = special_agent_outcomes();

    let a_comp_ids: Vec<String> = a_comps.iter().map(|c| c.id.clone()).collect();
    let e_comp_ids: Vec<String> = e_comps.iter().map(|c| c.id.clone()).collect();
    let v_comp_ids: Vec<String> = v_comps.iter().map(|c| c.id.clone()).collect();
    let c_comp_ids: Vec<String> = c_comps.iter().map(|c| c.id.clone()).collect();

    let assigner_outcome = outcomes
        .iter()
        .find(|o| o.name == "Optimal agent-task assignment")
        .unwrap();
    let evaluator_outcome = outcomes
        .iter()
        .find(|o| o.name == "Calibrated evaluation grade")
        .unwrap();
    let evolver_outcome = outcomes
        .iter()
        .find(|o| o.name == "Proposed primitive modifications")
        .unwrap();
    let creator_outcome = outcomes
        .iter()
        .find(|o| o.name == "New primitive candidates")
        .unwrap();

    vec![
        build_role(
            "Assigner",
            "Matches tasks to agent configurations by evaluating fit across role components, desired outcomes, and trade-off configurations. Uses performance history to weight matches. Can request task clarification when needed.",
            a_comp_ids,
            &assigner_outcome.id,
        ),
        build_role(
            "Evaluator",
            "Grades actor-agents that have completed tasks. Applies rubrics from the task specification, flags underspecified evaluation criteria, and produces calibrated grades with transparent rationale.",
            e_comp_ids,
            &evaluator_outcome.id,
        ),
        build_role(
            "Evolver",
            "Modifies agency primitives and their configurations. Can mutate wording, substitute components, randomise recombination, or generate entirely novel primitives. Targets a specified level of the primitive hierarchy with a specified amount of change.",
            v_comp_ids,
            &evolver_outcome.id,
        ),
        build_role(
            "Agent Creator",
            "Expands the primitive store by searching outside the agency for new role components, desired outcomes, and trade-off configurations. Searches research literature, analogous domains, and (when permitted) other Agency instances. Assesses absorptive capacity before recommending distant imports.",
            c_comp_ids,
            &creator_outcome.id,
        ),
    ]
}

/// Seed the agency directory with starter primitives and cache entries.
///
/// Only writes files that don't already exist, so existing customizations are preserved.
/// Deduplication is automatic: same content produces the same hash ID and filename.
/// Returns the number of roles and tradeoffs that were created.
pub fn seed_starters(agency_dir: &Path) -> Result<(usize, usize), AgencyError> {
    init(agency_dir)?;

    let components_dir = agency_dir.join("primitives/components");
    let outcomes_dir = agency_dir.join("primitives/outcomes");
    let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
    let roles_dir = agency_dir.join("cache/roles");

    // Seed components (actor + special agent)
    for component in starter_components()
        .into_iter()
        .chain(assigner_components())
        .chain(evaluator_components())
        .chain(evolver_components())
        .chain(creator_components())
    {
        let path = components_dir.join(format!("{}.yaml", component.id));
        if !path.exists() {
            save_component(&component, &components_dir)?;
        }
    }

    // Seed outcomes (actor + special agent)
    for outcome in starter_outcomes()
        .into_iter()
        .chain(special_agent_outcomes())
    {
        let path = outcomes_dir.join(format!("{}.yaml", outcome.id));
        if !path.exists() {
            save_outcome(&outcome, &outcomes_dir)?;
        }
    }

    let mut roles_created = 0;
    for role in starter_roles().into_iter().chain(special_agent_roles()) {
        let path = roles_dir.join(format!("{}.yaml", role.id));
        if !path.exists() {
            save_role(&role, &roles_dir)?;
            roles_created += 1;
        }
    }

    let mut tradeoffs_created = 0;
    for tradeoff in starter_tradeoffs()
        .into_iter()
        .chain(special_agent_tradeoffs())
    {
        let path = tradeoffs_dir.join(format!("{}.yaml", tradeoff.id));
        if !path.exists() {
            save_tradeoff(&tradeoff, &tradeoffs_dir)?;
            tradeoffs_created += 1;
        }
    }

    Ok((roles_created, tradeoffs_created))
}

// ---------------------------------------------------------------------------
// Agent Creator pipeline function
// ---------------------------------------------------------------------------

/// Return the creator → evolver → assigner pipeline as a TraceFunction.
///
/// This pipeline represents the spec's "agent creator → evolver → assigner" workflow:
/// 1. Creator searches outside the agency for new primitive candidates
/// 2. Evolver tests configurations using the new primitives
/// 3. Assigner deploys the tested configurations to tasks
pub fn creator_pipeline_function() -> crate::function::TraceFunction {
    use crate::function::*;

    TraceFunction {
        kind: "trace-function".to_string(),
        version: 2,
        id: "creator-pipeline".to_string(),
        name: "Agent Creator Pipeline".to_string(),
        description: "Create new primitives → evolve and test configurations → assign to tasks. \
                       The agent creator searches outside the agency for new role components, \
                       desired outcomes, or trade-off configurations. The evolver tests \
                       configurations using the new primitives. The assigner deploys tested \
                       configurations to real tasks."
            .to_string(),
        extracted_from: vec![ExtractionSource {
            task_id: "built-in".to_string(),
            run_id: None,
            timestamp: "2026-02-25T00:00:00Z".to_string(),
        }],
        extracted_by: Some("wg agency init".to_string()),
        extracted_at: Some("2026-02-25T00:00:00Z".to_string()),
        tags: vec![
            "agency".to_string(),
            "pipeline".to_string(),
            "creator".to_string(),
        ],
        inputs: vec![
            FunctionInput {
                name: "search_domain".to_string(),
                input_type: InputType::Enum,
                description: "Where to search for new primitives. Maps to creator trade-off \
                              configuration: unconstrained, adjacent, distant, internal."
                    .to_string(),
                required: false,
                default: Some(serde_yaml::Value::String("unconstrained".to_string())),
                example: None,
                min: None,
                max: None,
                values: Some(vec![
                    "unconstrained".to_string(),
                    "adjacent".to_string(),
                    "distant".to_string(),
                    "internal".to_string(),
                ]),
            },
            FunctionInput {
                name: "target_type".to_string(),
                input_type: InputType::Enum,
                description: "What type of primitive to search for.".to_string(),
                required: false,
                default: Some(serde_yaml::Value::String("component".to_string())),
                example: None,
                min: None,
                max: None,
                values: Some(vec![
                    "component".to_string(),
                    "outcome".to_string(),
                    "tradeoff".to_string(),
                ]),
            },
            FunctionInput {
                name: "evolution_level".to_string(),
                input_type: InputType::Enum,
                description: "Evolver target level for testing new primitives.".to_string(),
                required: false,
                default: Some(serde_yaml::Value::String("component".to_string())),
                example: None,
                min: None,
                max: None,
                values: Some(vec![
                    "component".to_string(),
                    "role-composition".to_string(),
                    "agent-composition".to_string(),
                ]),
            },
        ],
        tasks: vec![
            TaskTemplate {
                template_id: "create".to_string(),
                title: "Search for new primitive candidates ({{search_domain}})".to_string(),
                description: "Use the agent creator to search for new {{target_type}} primitives. \
                              Search domain: {{search_domain}}. Assess absorptive capacity. \
                              Output: primitive candidate specifications with provenance notes."
                    .to_string(),
                skills: vec![],
                after: vec![],
                loops_to: vec![],
                role_hint: Some("Agent Creator".to_string()),
                deliverables: vec!["Primitive candidate specifications".to_string()],
                verify: None,
                tags: vec!["creator".to_string()],
            },
            TaskTemplate {
                template_id: "evolve".to_string(),
                title: "Evolve and test configurations with new primitives".to_string(),
                description: "Use the evolver to test configurations incorporating the new \
                              primitives from the creation step. Target level: \
                              {{evolution_level}}. Produce modified compositions and evaluate \
                              their fitness."
                    .to_string(),
                skills: vec![],
                after: vec!["create".to_string()],
                loops_to: vec![],
                role_hint: Some("Evolver".to_string()),
                deliverables: vec!["Tested configurations".to_string()],
                verify: None,
                tags: vec!["evolver".to_string()],
            },
            TaskTemplate {
                template_id: "assign".to_string(),
                title: "Deploy tested configurations to tasks".to_string(),
                description: "Use the assigner to deploy the evolved configurations to real \
                              tasks. Configurations that passed evolution testing are now \
                              available in the composition cache for performance-mode deployment."
                    .to_string(),
                skills: vec![],
                after: vec!["evolve".to_string()],
                loops_to: vec![],
                role_hint: Some("Assigner".to_string()),
                deliverables: vec!["Deployment report".to_string()],
                verify: None,
                tags: vec!["assigner".to_string()],
            },
        ],
        outputs: vec![
            FunctionOutput {
                name: "new_primitives".to_string(),
                description: "New primitive candidates that were created and tested.".to_string(),
                from_task: "create".to_string(),
                field: "artifacts".to_string(),
            },
            FunctionOutput {
                name: "tested_configurations".to_string(),
                description: "Configurations that were evolved and tested.".to_string(),
                from_task: "evolve".to_string(),
                field: "artifacts".to_string(),
            },
        ],
        planning: None,
        constraints: None,
        memory: None,
        visibility: FunctionVisibility::Internal,
        redacted_fields: vec![],
    }
}

// ---------------------------------------------------------------------------
// Evolution utilities (test-only: used by evolve.rs tests to verify primitives)
// ---------------------------------------------------------------------------

/// Mutate a parent role to produce a child with updated fields and correct lineage.
///
/// Any `None` field inherits the parent's value. The child gets a fresh content-hash ID
/// based on its (possibly mutated) component_ids and outcome_id.
#[cfg(test)]
pub(crate) fn mutate_role(
    parent: &Role,
    run_id: &str,
    new_name: Option<&str>,
    new_description: Option<&str>,
    new_component_ids: Option<Vec<String>>,
    new_outcome_id: Option<&str>,
) -> Role {
    let description = new_description
        .map(|s| s.to_string())
        .unwrap_or_else(|| parent.description.clone());
    let component_ids = new_component_ids.unwrap_or_else(|| parent.component_ids.clone());
    let outcome_id = new_outcome_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| parent.outcome_id.clone());

    let id = content_hash_role(&component_ids, &outcome_id);

    Role {
        id,
        name: new_name
            .map(|s| s.to_string())
            .unwrap_or_else(|| parent.name.clone()),
        description,
        component_ids,
        outcome_id,
        performance: PerformanceRecord::default(),
        lineage: Lineage::mutation(&parent.id, parent.lineage.generation, run_id),
        default_context_scope: parent.default_context_scope.clone(),
        default_exec_mode: parent.default_exec_mode.clone(),
    }
}

/// Crossover two motivations: union their accept/reject lists and set crossover lineage.
///
/// Produces a new tradeoff config whose acceptable_tradeoffs and unacceptable_tradeoffs are
/// the deduplicated union of both parents' lists.
#[cfg(test)]
pub(crate) fn crossover_tradeoffs(
    parent_a: &TradeoffConfig,
    parent_b: &TradeoffConfig,
    run_id: &str,
    name: &str,
    description: &str,
) -> TradeoffConfig {
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

    let id = content_hash_tradeoff(&acceptable, &unacceptable, description);
    let max_gen = parent_a.lineage.generation.max(parent_b.lineage.generation);

    TradeoffConfig {
        id,
        name: name.to_string(),
        description: description.to_string(),
        acceptable_tradeoffs: acceptable,
        unacceptable_tradeoffs: unacceptable,
        performance: PerformanceRecord::default(),
        lineage: Lineage::crossover(&[&parent_a.id, &parent_b.id], max_gen, run_id),
        access_control: AccessControl::default(),
        domain_tags: vec![],
        metadata: std::collections::HashMap::new(),
        former_agents: vec![],
        former_deployments: vec![],
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

/// Gap analysis: given a set of required component IDs and the current roles,
/// return the component IDs that are not covered by any existing role.
///
/// A component is "covered" if at least one role includes its ID in `component_ids`.
#[cfg(test)]
pub(crate) fn uncovered_skills(required: &[&str], roles: &[Role]) -> Vec<String> {
    let covered: std::collections::HashSet<&str> = roles
        .iter()
        .flat_map(|r| r.component_ids.iter())
        .map(|s| s.as_str())
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
    use tempfile::TempDir;

    fn sample_performance() -> PerformanceRecord {
        PerformanceRecord::default()
    }

    fn sample_role() -> Role {
        build_role(
            "Implementer",
            "Writes code to fulfil task requirements.",
            vec!["rust".to_string(), "fn-main".to_string()],
            "Working, tested code merged to main.",
        )
    }

    fn sample_tradeoff() -> TradeoffConfig {
        build_tradeoff(
            "Quality First",
            "Prioritise correctness and maintainability.",
            vec!["Slower delivery for higher quality".into()],
            vec!["Skipping tests".into()],
        )
    }

    fn sample_evaluation() -> Evaluation {
        let role = sample_role();
        let motivation = sample_tradeoff();
        let mut dims = HashMap::new();
        dims.insert("correctness".into(), 0.9);
        dims.insert("style".into(), 0.8);
        Evaluation {
            id: "eval-001".into(),
            task_id: "task-42".into(),
            agent_id: String::new(),
            role_id: role.id,
            tradeoff_id: motivation.id,
            score: 0.85,
            dimensions: dims,
            notes: "Good implementation with minor style issues.".into(),
            evaluator: "reviewer-bot".into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
            model: None,
            source: "llm".to_string(),
            loop_iteration: 0,
        }
    }

    fn sample_agent() -> Agent {
        let role = sample_role();
        let motivation = sample_tradeoff();
        let id = content_hash_agent(&role.id, &motivation.id);
        Agent {
            id,
            role_id: role.id,
            tradeoff_id: motivation.id,
            name: "Test Agent".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec!["rust".into(), "testing".into()],
            rate: Some(50.0),
            capacity: Some(3.0),
            trust_level: TrustLevel::Verified,
            contact: Some("agent@example.com".into()),
            executor: "matrix".into(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
        }
    }

    // -- Storage tests -------------------------------------------------------

    #[test]
    fn test_init_creates_directories() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("agency");
        init(&base).unwrap();
        assert!(base.join("primitives/components").is_dir());
        assert!(base.join("primitives/outcomes").is_dir());
        assert!(base.join("primitives/tradeoffs").is_dir());
        assert!(base.join("cache/roles").is_dir());
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
        assert_eq!(loaded.outcome_id, role.outcome_id);
        assert_eq!(loaded.component_ids.len(), role.component_ids.len());
    }

    #[test]
    fn test_motivation_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let motivation = sample_tradeoff();
        let path = save_tradeoff(&motivation, dir).unwrap();
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

        let loaded = load_tradeoff(&path).unwrap();
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

        let roles_dir = base.join("cache/roles");
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
    fn test_load_all_tradeoffs() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("agency");
        init(&base).unwrap();

        let dir = base.join("primitives/tradeoffs");
        let m1 = build_tradeoff("Mot A", "First", vec!["a".into()], vec![]);
        let m2 = build_tradeoff("Mot B", "Second", vec!["b".into()], vec![]);
        save_tradeoff(&m1, &dir).unwrap();
        save_tradeoff(&m2, &dir).unwrap();

        let all = load_all_tradeoffs(&dir).unwrap();
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
        assert_eq!(load_all_tradeoffs(&missing).unwrap().len(), 0);
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
        assert!(contents.contains("outcome_id:"));
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
        let mut m = sample_tradeoff();
        m.lineage = Lineage::crossover(&["m-a", "m-b"], 3, "xover-1");
        let path = save_tradeoff(&m, tmp.path()).unwrap();
        let loaded = load_tradeoff(&path).unwrap();
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
            vec!["coding".to_string(), "debugging".to_string()],
            "Working code",
        );

        let child = mutate_role(
            &parent,
            "evo-run-1",
            Some("Test-Focused Programmer"),
            None, // inherit description
            Some(vec![
                "coding".to_string(),
                "debugging".to_string(),
                "testing".to_string(),
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
        assert_eq!(child.component_ids.len(), 3);
        // Desired outcome was mutated
        assert_eq!(child.outcome_id, "Working, tested code");
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
            vec!["system-design".to_string()],
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
    fn test_crossover_tradeoffs_merges_accept_reject_lists() {
        let parent_a = build_tradeoff(
            "Careful",
            "Prioritizes reliability.",
            vec!["Slow".into(), "Verbose".into()],
            vec!["Unreliable".into(), "Untested".into()],
        );
        let parent_b = build_tradeoff(
            "Fast",
            "Prioritizes speed.",
            vec!["Less documentation".into(), "Verbose".into()], // "Verbose" overlaps
            vec!["Broken code".into(), "Untested".into()],       // "Untested" overlaps
        );

        let child = crossover_tradeoffs(
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
    fn test_crossover_tradeoffs_generation_uses_max() {
        let mut parent_a = build_tradeoff("A", "A", vec!["a".into()], vec![]);
        parent_a.lineage = Lineage::mutation("ancestor", 4, "r1");
        assert_eq!(parent_a.lineage.generation, 5);

        let mut parent_b = build_tradeoff("B", "B", vec!["b".into()], vec![]);
        parent_b.lineage = Lineage::mutation("ancestor2", 1, "r2");
        assert_eq!(parent_b.lineage.generation, 2);

        let child = crossover_tradeoffs(&parent_a, &parent_b, "xr", "Hybrid", "Hybrid desc");
        // max(5, 2) + 1 = 6
        assert_eq!(child.lineage.generation, 6);
    }

    #[test]
    fn test_crossover_tradeoffs_no_overlap() {
        let parent_a = build_tradeoff("A", "A", vec!["x".into()], vec!["p".into()]);
        let parent_b = build_tradeoff("B", "B", vec!["y".into()], vec!["q".into()]);

        let child = crossover_tradeoffs(&parent_a, &parent_b, "r", "C", "C");
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
            vec!["coding".to_string(), "debugging".to_string()],
            "Code",
        );
        let role_b = build_role(
            "Reviewer",
            "Reviews code",
            vec!["code-review".to_string()],
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
            vec!["coding".to_string(), "testing".to_string()],
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
                "inline:coding instructions".to_string(),
                "file:skills/coding.md".to_string(),
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
        assert_eq!(loaded.tradeoff_id, agent.tradeoff_id);
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
        let motivation = sample_tradeoff();
        let id = content_hash_agent(&role.id, &motivation.id);
        let agent = Agent {
            id,
            role_id: role.id,
            tradeoff_id: motivation.id,
            name: "Default Agent".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
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
        let m = sample_tradeoff();

        let a1 = Agent {
            id: content_hash_agent(&r1.id, &m.id),
            role_id: r1.id.clone(),
            tradeoff_id: m.id.clone(),
            name: "Agent 1".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
        };
        let a2 = Agent {
            id: content_hash_agent(&r2.id, &m.id),
            role_id: r2.id.clone(),
            tradeoff_id: m.id.clone(),
            name: "Agent 2".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
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
        let r1 = build_role("Name A", "Desc", vec!["s".to_string()], "Outcome");
        let r2 = build_role("Name B", "Desc", vec!["s".to_string()], "Outcome");
        // Same immutable content (skills, desired_outcome, description) => same ID
        assert_eq!(r1.id, r2.id);
        assert_eq!(r1.id.len(), 64);
    }

    #[test]
    fn test_build_role_different_description_same_id() {
        // Description is mutable metadata, not part of the content hash
        let r1 = build_role("R", "Description A", vec![], "Outcome");
        let r2 = build_role("R", "Description B", vec![], "Outcome");
        assert_eq!(r1.id, r2.id);
    }

    #[test]
    fn test_build_role_different_skills_different_id() {
        let r1 = build_role("R", "Desc", vec!["a".to_string()], "Outcome");
        let r2 = build_role("R", "Desc", vec!["b".to_string()], "Outcome");
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
    fn test_build_tradeoff_content_hash_deterministic() {
        let m1 = build_tradeoff("Name A", "Desc", vec!["a".into()], vec!["b".into()]);
        let m2 = build_tradeoff("Name B", "Desc", vec!["a".into()], vec!["b".into()]);
        // Same immutable content => same ID
        assert_eq!(m1.id, m2.id);
        assert_eq!(m1.id.len(), 64);
    }

    #[test]
    fn test_build_tradeoff_different_description_different_id() {
        let m1 = build_tradeoff("M", "Desc A", vec![], vec![]);
        let m2 = build_tradeoff("M", "Desc B", vec![], vec![]);
        assert_ne!(m1.id, m2.id);
    }

    #[test]
    fn test_build_tradeoff_different_acceptable_different_id() {
        let m1 = build_tradeoff("M", "D", vec!["x".into()], vec![]);
        let m2 = build_tradeoff("M", "D", vec!["y".into()], vec![]);
        assert_ne!(m1.id, m2.id);
    }

    #[test]
    fn test_build_tradeoff_different_unacceptable_different_id() {
        let m1 = build_tradeoff("M", "D", vec![], vec!["x".into()]);
        let m2 = build_tradeoff("M", "D", vec![], vec!["y".into()]);
        assert_ne!(m1.id, m2.id);
    }

    #[test]
    fn test_build_tradeoff_name_does_not_affect_id() {
        let m1 = build_tradeoff("Alpha", "Same", vec!["a".into()], vec!["b".into()]);
        let m2 = build_tradeoff("Beta", "Same", vec!["a".into()], vec!["b".into()]);
        assert_eq!(m1.id, m2.id);
    }

    #[test]
    fn test_build_tradeoff_fresh_performance() {
        let m = build_tradeoff("M", "D", vec![], vec![]);
        assert_eq!(m.performance.task_count, 0);
        assert!(m.performance.avg_score.is_none());
        assert!(m.performance.evaluations.is_empty());
    }

    #[test]
    fn test_build_tradeoff_default_lineage() {
        let m = build_tradeoff("M", "D", vec![], vec![]);
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
    fn test_find_tradeoff_by_prefix_exact_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m = sample_tradeoff();
        save_tradeoff(&m, dir).unwrap();

        let found = find_tradeoff_by_prefix(dir, &m.id).unwrap();
        assert_eq!(found.id, m.id);
        assert_eq!(found.name, m.name);
    }

    #[test]
    fn test_find_tradeoff_by_prefix_short_prefix() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m = sample_tradeoff();
        save_tradeoff(&m, dir).unwrap();

        let prefix = &m.id[..8];
        let found = find_tradeoff_by_prefix(dir, prefix).unwrap();
        assert_eq!(found.id, m.id);
    }

    #[test]
    fn test_find_tradeoff_by_prefix_no_match() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m = sample_tradeoff();
        save_tradeoff(&m, dir).unwrap();

        let result = find_tradeoff_by_prefix(dir, "zzzznotfound");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No tradeoff matching")
        );
    }

    #[test]
    fn test_find_tradeoff_by_prefix_ambiguous() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m1 = build_tradeoff("M1", "First", vec!["a".into()], vec![]);
        let m2 = build_tradeoff("M2", "Second", vec!["b".into()], vec![]);
        save_tradeoff(&m1, dir).unwrap();
        save_tradeoff(&m2, dir).unwrap();

        let common_len = m1
            .id
            .chars()
            .zip(m2.id.chars())
            .take_while(|(a, b)| a == b)
            .count();

        if common_len > 0 {
            let prefix = &m1.id[..common_len];
            let result = find_tradeoff_by_prefix(dir, prefix);
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
        let m = sample_tradeoff();

        let a1 = Agent {
            id: content_hash_agent(&r1.id, &m.id),
            role_id: r1.id,
            tradeoff_id: m.id.clone(),
            name: "A1".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
        };
        let a2 = Agent {
            id: content_hash_agent(&r2.id, &m.id),
            role_id: r2.id,
            tradeoff_id: m.id.clone(),
            name: "A2".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
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
    fn test_find_tradeoff_by_prefix_special_characters() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let m = sample_tradeoff();
        save_tradeoff(&m, dir).unwrap();

        let result = find_tradeoff_by_prefix(dir, "^$\\{|}");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No tradeoff matching")
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
        let motivation = sample_tradeoff();
        let id = content_hash_agent(&role.id, &motivation.id);
        let agent = Agent {
            id,
            role_id: role.id,
            tradeoff_id: motivation.id,
            name: "Default".into(),
            performance: sample_performance(),
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".into(),
            preferred_model: None,
            preferred_provider: None,
            attractor_weight: 1.0,
            deployment_history: vec![],
            staleness_flags: vec![],
        };
        // default_executor() returns "claude" which is not human
        assert!(!agent.is_human());
    }
}

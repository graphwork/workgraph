use anyhow::{Result, bail};

/// Strategies the evolver can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Mutation,
    Crossover,
    GapAnalysis,
    Retirement,
    MotivationTuning,
    All,
    // New strategies
    ComponentMutation,
    Randomisation,
    BizarreIdeation,
    CoordinatorEvolution,
}

impl Strategy {
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "mutation" => Ok(Self::Mutation),
            "crossover" => Ok(Self::Crossover),
            "gap-analysis" => Ok(Self::GapAnalysis),
            "retirement" => Ok(Self::Retirement),
            "motivation-tuning" => Ok(Self::MotivationTuning),
            "all" => Ok(Self::All),
            "component-mutation" => Ok(Self::ComponentMutation),
            "randomisation" => Ok(Self::Randomisation),
            "bizarre-ideation" => Ok(Self::BizarreIdeation),
            "coordinator" | "coordinator-evolution" => Ok(Self::CoordinatorEvolution),
            other => bail!(
                "Unknown strategy '{}'. Valid: mutation, crossover, gap-analysis, retirement, \
                 motivation-tuning, component-mutation, randomisation, bizarre-ideation, coordinator, all",
                other
            ),
        }
    }

    /// Returns all individual strategies (excludes `All`).
    pub fn all_individual() -> Vec<Self> {
        vec![
            Self::Mutation,
            Self::Crossover,
            Self::GapAnalysis,
            Self::Retirement,
            Self::MotivationTuning,
            Self::ComponentMutation,
            Self::Randomisation,
            Self::BizarreIdeation,
            Self::CoordinatorEvolution,
        ]
    }

    /// Whether this strategy can produce useful output without any evaluations.
    pub fn needs_no_evals(self) -> bool {
        matches!(
            self,
            Self::GapAnalysis | Self::Randomisation | Self::BizarreIdeation | Self::CoordinatorEvolution
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mutation => "mutation",
            Self::Crossover => "crossover",
            Self::GapAnalysis => "gap-analysis",
            Self::Retirement => "retirement",
            Self::MotivationTuning => "motivation-tuning",
            Self::All => "all",
            Self::ComponentMutation => "component-mutation",
            Self::Randomisation => "randomisation",
            Self::BizarreIdeation => "bizarre-ideation",
            Self::CoordinatorEvolution => "coordinator",
        }
    }
}

// ---------------------------------------------------------------------------
// Evolver target (Level × Amount)
// ---------------------------------------------------------------------------

/// The level of the primitive hierarchy the evolver targets.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolverLevel {
    Primitives,
    Configurations,
    Agents,
    AgentConfigurations,
}

/// The perturbation magnitude for an evolver operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolverAmount {
    Minimal,
    Moderate,
    Maximal,
}

/// Entity type targeted by an evolver operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolverEntityType {
    Component,
    Outcome,
    Tradeoff,
    Role,
    Agent,
    MetaAgent,
}

/// Two-dimensional evolver targeting: Level × Amount.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvolverTarget {
    pub level: EvolverLevel,
    pub amount: EvolverAmount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<EvolverEntityType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ids: Option<Vec<String>>,
}

impl Default for EvolverTarget {
    fn default() -> Self {
        Self {
            level: EvolverLevel::Configurations,
            amount: EvolverAmount::Moderate,
            entity_type: None,
            target_ids: None,
        }
    }
}

/// A single evolution operation returned by the evolver agent.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct EvolverOperation {
    /// Operation type: create_role, modify_role, create_motivation, modify_motivation,
    /// retire_role, retire_motivation, wording_mutation, component_substitution,
    /// config_add_component, config_remove_component, config_swap_outcome,
    /// config_swap_tradeoff, random_compose_role, random_compose_agent, bizarre_ideation,
    /// meta_swap_role, meta_swap_tradeoff, meta_compose_agent, modify_coordinator_prompt
    pub op: String,

    // -- Targeting --
    /// The entity type this operation targets.
    #[serde(default)]
    pub entity_type: Option<String>,

    // -- Source references --
    /// For modify/retire/substitution: the ID of the existing entity to act on.
    #[serde(default)]
    pub target_id: Option<String>,

    // -- Composition changes --
    /// For component_substitution, config_add_component: component hash to add.
    #[serde(default)]
    pub add_component_id: Option<String>,
    /// For component_substitution, config_remove_component: component hash to remove.
    #[serde(default)]
    pub remove_component_id: Option<String>,
    /// For config_swap_outcome: new outcome hash.
    #[serde(default)]
    pub new_outcome_id: Option<String>,
    /// For config_swap_tradeoff: new tradeoff hash.
    #[serde(default)]
    pub new_tradeoff_id: Option<String>,

    // -- Content fields (for wording_mutation, bizarre_ideation) --
    /// New name for created/mutated entity.
    #[serde(default)]
    pub new_name: Option<String>,
    /// New description for created/mutated entity.
    #[serde(default)]
    pub new_description: Option<String>,
    /// New content (serialized ContentRef).
    #[serde(default)]
    pub new_content: Option<String>,
    /// New category: "translated" | "enhanced" | "novel".
    #[serde(default)]
    pub new_category: Option<String>,
    /// New success criteria (for outcome bizarre ideation).
    #[serde(default)]
    pub new_success_criteria: Option<Vec<String>>,
    /// New acceptable tradeoffs (for tradeoff bizarre ideation).
    #[serde(default)]
    pub new_acceptable_tradeoffs: Option<Vec<String>>,
    /// New unacceptable tradeoffs (for tradeoff bizarre ideation).
    #[serde(default)]
    pub new_unacceptable_tradeoffs: Option<Vec<String>>,

    // -- Randomisation --
    /// Selection method: "uniform_random" | "performance_weighted_inverse".
    #[serde(default)]
    pub selection_method: Option<String>,

    // -- Backwards compatibility (legacy role/motivation fields) --
    /// New ID for the created/modified entity.
    #[serde(default)]
    pub new_id: Option<String>,
    /// Legacy name field.
    #[serde(default)]
    pub name: Option<String>,
    /// Legacy description field.
    #[serde(default)]
    pub description: Option<String>,
    /// Component IDs (for roles or random_compose_role).
    #[serde(default, alias = "skills")]
    pub component_ids: Option<Vec<String>>,
    /// Outcome ID (for roles or random_compose_role).
    #[serde(default, alias = "desired_outcome")]
    pub outcome_id: Option<String>,
    /// Role ID (for random_compose_agent).
    #[serde(default)]
    pub role_id: Option<String>,
    /// Tradeoff ID (for random_compose_agent).
    #[serde(default)]
    pub tradeoff_id: Option<String>,
    /// Acceptable trade-offs (for motivations).
    #[serde(default)]
    pub acceptable_tradeoffs: Option<Vec<String>>,
    /// Unacceptable trade-offs (for motivations).
    #[serde(default)]
    pub unacceptable_tradeoffs: Option<Vec<String>>,

    // -- Meta-agent targeting --
    /// For meta_swap_role, meta_swap_tradeoff, meta_compose_agent:
    /// which meta-agent slot to target: "assigner" | "evaluator" | "evolver".
    #[serde(default)]
    pub meta_role: Option<String>,

    // -- Provenance --
    /// Rationale for this operation.
    #[serde(default)]
    pub rationale: Option<String>,
    /// For bizarre ideation: the prompt used to generate the new primitive.
    #[serde(default)]
    pub ideation_prompt: Option<String>,

    // -- Fan-out analyzer fields --
    /// Analyzer's confidence in this operation (0.0-1.0).
    /// Used by synthesizer for priority scoring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Expected impact description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_impact: Option<String>,
}

/// Top-level structured output from the evolver agent.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct EvolverOutput {
    /// Run ID for lineage tracking.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Level × Amount targeting for this run.
    #[serde(default)]
    pub target: Option<EvolverTarget>,
    /// List of proposed operations.
    pub operations: Vec<EvolverOperation>,
    /// Operations placed in deferred queue (not yet applied).
    #[serde(default)]
    pub deferred_operations: Vec<EvolverOperation>,
    /// Optional summary from the evolver.
    #[serde(default)]
    pub summary: Option<String>,
}

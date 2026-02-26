use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::graph::TrustLevel;

// ---------------------------------------------------------------------------
// Content reference (replaces SkillRef)
// ---------------------------------------------------------------------------

/// Reference to content, which can come from various sources.
/// Renamed from `SkillRef` to reflect that role components are broader than "skills".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentRef {
    Name(String),
    File(PathBuf),
    Url(String),
    Inline(String),
}

/// A resolved skill/content with its name and content loaded into memory.
#[derive(Debug, Clone)]
pub struct ResolvedSkill {
    pub name: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Primitive metadata types
// ---------------------------------------------------------------------------

/// Category of a role component, describing its origin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentCategory {
    /// Directly translated from a human skill
    Translated,
    /// Enhanced version of a human skill
    Enhanced,
    /// Novel machine-only capability
    Novel,
}

/// Access control policy for federation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessPolicy {
    Private,
    Shared,
    Open,
}

/// Access control metadata for a primitive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessControl {
    pub owner: String,
    pub policy: AccessPolicy,
}

impl Default for AccessControl {
    fn default() -> Self {
        Self {
            owner: "local".to_string(),
            policy: AccessPolicy::Open,
        }
    }
}

/// Reference from a primitive to a deployment (task assignment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRef {
    pub agent_id: String,
    pub task_id: String,
    pub timestamp: String,
    pub score: Option<f64>,
}

/// Staleness reason for a composition cache entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StalenessReason {
    Superseded,
    Retired,
}

/// A staleness flag on a composition cache entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StalenessFlag {
    pub primitive_id: String,
    pub reason: StalenessReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_id: Option<String>,
    pub flagged_at: String,
}

// ---------------------------------------------------------------------------
// Shared performance / lineage types
// ---------------------------------------------------------------------------

/// Reference to an evaluation, stored inline in a PerformanceRecord.
///
/// `context_id` provides cross-reference: for components it holds role_id,
/// for roles it holds tradeoff_id, for agents it holds role_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRef {
    pub score: f64,
    pub task_id: String,
    pub timestamp: String,
    pub context_id: String,
}

/// Aggregated performance data for any entity (primitive or cache).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceRecord {
    pub task_count: u32,
    pub avg_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluations: Vec<EvaluationRef>,
    /// Organisational-level performance (populated after downstream tasks complete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_performance: Option<OrgPerformanceRecord>,
}

/// Organisational-level performance record, parallel to task-level PerformanceRecord.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrgPerformanceRecord {
    pub task_count: u32,
    pub avg_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluations: Vec<OrgEvalRef>,
}

/// Reference to an organisational evaluation, stored inline in an OrgPerformanceRecord.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgEvalRef {
    pub score: f64,
    pub task_id: String,
    pub timestamp: String,
    pub downstream_task_count: u32,
}

/// The time range over which an organisational evaluation was collected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationWindow {
    /// ID of the graph epoch or run this covers (if available)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch_id: Option<String>,
    /// Earliest task timestamp included
    pub from: String,
    /// Latest task timestamp included
    pub to: String,
}

/// An organisational-level evaluation of how an agent's output served the downstream graph.
///
/// Captures three dimensions:
/// - `downstream_usability` (0.50 weight): downstream task scores, discounted by graph distance
/// - `coordination_overhead` (0.30 weight): penalty for clarification requests and re-spawns
/// - `blocking_behaviour` (0.20 weight): duration relative to p50 for similar tasks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgEvaluation {
    #[serde(default)]
    pub id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    #[serde(default)]
    pub role_id: String,
    #[serde(default, alias = "motivation_id")]
    pub tradeoff_id: String,

    /// Composite organisational score [0, 1]
    pub score: f64,

    /// Dimension scores. Keys: "downstream_usability", "coordination_overhead", "blocking_behaviour"
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub dimensions: HashMap<String, f64>,

    /// Observation window: the time range this covers
    pub observation_window: ObservationWindow,

    /// How many downstream tasks contributed to the downstream_usability score
    pub downstream_task_count: u32,

    pub notes: String,
    pub timestamp: String,

    /// Source convention: "org:composite", "org:downstream-usability", etc.
    #[serde(default = "default_org_eval_source")]
    pub source: String,
}

fn default_org_eval_source() -> String {
    "org:composite".to_string()
}

/// Lineage metadata for tracking evolutionary history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lineage {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_ids: Vec<String>,
    #[serde(default)]
    pub generation: u32,
    #[serde(default = "default_created_by")]
    pub created_by: String,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

fn default_created_by() -> String {
    "human".to_string()
}

impl Default for Lineage {
    fn default() -> Self {
        Lineage {
            parent_ids: Vec::new(),
            generation: 0,
            created_by: "human".to_string(),
            created_at: Utc::now(),
        }
    }
}

impl Lineage {
    /// Create lineage for a mutation (single parent).
    pub fn mutation(parent_id: &str, parent_generation: u32, run_id: &str) -> Self {
        Lineage {
            parent_ids: vec![parent_id.to_string()],
            generation: parent_generation.saturating_add(1),
            created_by: format!("evolver-{}", run_id),
            created_at: Utc::now(),
        }
    }

    /// Create lineage for a crossover (two parents).
    pub fn crossover(parent_ids: &[&str], max_parent_generation: u32, run_id: &str) -> Self {
        Lineage {
            parent_ids: parent_ids
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            generation: max_parent_generation.saturating_add(1),
            created_by: format!("evolver-{}", run_id),
            created_at: Utc::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// A role component — a single capability, stored as a first-class primitive.
///
/// Stored in `primitives/components/{hash}.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleComponent {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: ComponentCategory,
    pub content: ContentRef,
    pub performance: PerformanceRecord,
    #[serde(default)]
    pub lineage: Lineage,
    #[serde(default)]
    pub access_control: AccessControl,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_agents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_deployments: Vec<DeploymentRef>,
}

/// A desired outcome — what success looks like, stored as a first-class primitive.
///
/// Stored in `primitives/outcomes/{hash}.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredOutcome {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub success_criteria: Vec<String>,
    pub performance: PerformanceRecord,
    #[serde(default)]
    pub lineage: Lineage,
    #[serde(default)]
    pub access_control: AccessControl,
    #[serde(default = "default_true")]
    pub requires_human_oversight: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_agents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_deployments: Vec<DeploymentRef>,
}

fn default_true() -> bool {
    true
}

/// A trade-off configuration — how an agent navigates competing considerations.
///
/// Replaces the old `Motivation` struct. Stored in `primitives/tradeoffs/{hash}.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeoffConfig {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptable_tradeoffs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unacceptable_tradeoffs: Vec<String>,
    pub performance: PerformanceRecord,
    #[serde(default)]
    pub lineage: Lineage,
    #[serde(default)]
    pub access_control: AccessControl,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_agents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_deployments: Vec<DeploymentRef>,
}

// ---------------------------------------------------------------------------
// Composition cache
// ---------------------------------------------------------------------------

/// A role — a composition of component IDs + an outcome ID.
///
/// Stored in `cache/roles/{hash}.yaml`. No longer bundles skills inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Sorted component IDs for deterministic hashing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub component_ids: Vec<String>,
    /// ID of the DesiredOutcome primitive.
    #[serde(default)]
    pub outcome_id: String,
    pub performance: PerformanceRecord,
    #[serde(default)]
    pub lineage: Lineage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context_scope: Option<String>,
}

fn default_executor() -> String {
    "claude".to_string()
}

/// A first-class agent entity — a role paired with a trade-off configuration.
///
/// Stored in `cache/agents/{hash}.yaml`.
/// Agent ID = SHA-256(role_id + tradeoff_id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub role_id: String,
    #[serde(alias = "motivation_id")]
    pub tradeoff_id: String,
    pub name: String,
    pub performance: PerformanceRecord,
    #[serde(default)]
    pub lineage: Lineage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<f64>,
    #[serde(default, skip_serializing_if = "is_default_trust")]
    pub trust_level: TrustLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
    #[serde(
        default = "default_executor",
        skip_serializing_if = "is_default_executor"
    )]
    pub executor: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployment_history: Vec<DeploymentRef>,
    #[serde(default = "default_attractor_weight")]
    pub attractor_weight: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staleness_flags: Vec<StalenessFlag>,
}

fn default_attractor_weight() -> f64 {
    0.5
}

/// Executor types that represent human operators (not AI agents).
const HUMAN_EXECUTORS: &[&str] = &["matrix", "email", "shell"];

/// Returns true if the given executor string represents a human operator.
pub fn is_human_executor(executor: &str) -> bool {
    HUMAN_EXECUTORS.contains(&executor)
}

impl Agent {
    /// Returns true if this agent uses a human executor (matrix, email, shell).
    pub fn is_human(&self) -> bool {
        is_human_executor(&self.executor)
    }
}

fn is_default_trust(level: &TrustLevel) -> bool {
    *level == TrustLevel::Provisional
}

fn is_default_executor(executor: &str) -> bool {
    executor == "claude"
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// An evaluation of agent performance on a specific task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluation {
    #[serde(default)]
    pub id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    #[serde(default)]
    pub role_id: String,
    #[serde(default, alias = "motivation_id")]
    pub tradeoff_id: String,
    #[serde(alias = "value")]
    pub score: f64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub dimensions: HashMap<String, f64>,
    #[serde(alias = "reasoning")]
    pub notes: String,
    #[serde(alias = "evaluated_by")]
    pub evaluator: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "default_eval_source")]
    pub source: String,
}

fn default_eval_source() -> String {
    "llm".to_string()
}

// ---------------------------------------------------------------------------
// Evaluation source type conventions
// ---------------------------------------------------------------------------

/// Standard evaluation source types.
pub mod eval_source {
    /// Auto-evaluator (LLM judge).
    pub const LLM: &str = "llm";
    /// Human evaluation.
    pub const MANUAL: &str = "manual";
    /// Human reviewing evaluator output (meta-evaluation).
    pub const META_HUMAN_REVIEW: &str = "meta:human-review";

    /// Build a peer-evaluation source string: `meta:peer-eval:{evaluator_id}`.
    pub fn meta_peer_eval(evaluator_id: &str) -> String {
        format!("meta:peer-eval:{}", evaluator_id)
    }

    /// Build an outcome-correlation source string: `meta:outcome-correlation:{metric}`.
    pub fn meta_outcome_correlation(metric: &str) -> String {
        format!("meta:outcome-correlation:{}", metric)
    }

    /// Returns true if the source is a meta-evaluation type.
    pub fn is_meta(source: &str) -> bool {
        source.starts_with("meta:")
    }
}

// ---------------------------------------------------------------------------
// Metadata / display types
// ---------------------------------------------------------------------------

/// Summary counts of entities in a store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoreCounts {
    pub components: usize,
    pub outcomes: usize,
    pub tradeoffs: usize,
    pub roles: usize,
    pub agents: usize,
    pub evaluations: usize,
}

/// A node in a lineage ancestry tree.
#[derive(Debug, Clone)]
pub struct AncestryNode {
    pub id: String,
    pub name: String,
    pub generation: u32,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub parent_ids: Vec<String>,
}

/// An entry in the artifact manifest written to artifacts.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub path: String,
    pub size: Option<u64>,
}

// ---------------------------------------------------------------------------
// Run mode continuum types
// ---------------------------------------------------------------------------

/// What dimension was varied in a learning experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExperimentDimension {
    /// A single role component was swapped.
    RoleComponent {
        /// None if this is a new addition.
        replaced: Option<String>,
        introduced: String,
    },
    /// The trade-off configuration was swapped.
    TradeoffConfig {
        replaced: Option<String>,
        introduced: String,
    },
    /// Everything composed fresh (no controlled variable).
    NovelComposition,
}

/// Metadata recorded when an assignment is made in learning mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentExperiment {
    /// The base composition used as the control (None for NovelComposition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_composition: Option<String>,
    /// What was varied.
    pub dimension: ExperimentDimension,
    /// Whether this was triggered by the bizarre ideation schedule.
    #[serde(default)]
    pub bizarre_ideation: bool,
    /// UCB scores of alternatives considered (for post-hoc analysis).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub ucb_scores: HashMap<String, f64>,
}

/// How a task assignment was routed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssignmentMode {
    /// Deployed from composition cache.
    CacheHit { cache_score: f64 },
    /// Cache miss; basic composition, no structured experiment.
    CacheMiss,
    /// Deliberate learning experiment (run_mode > 0).
    Learning(AssignmentExperiment),
    /// Forced learning episode (exploration_interval trigger).
    ForcedExploration(AssignmentExperiment),
}

/// Persisted alongside each task assignment.
///
/// Stored in `.workgraph/agency/assignments/<task_id>.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAssignmentRecord {
    pub task_id: String,
    pub agent_id: String,
    pub composition_id: String,
    pub timestamp: String,
    /// Snapshot of run_mode at time of assignment.
    pub run_mode_value: f64,
    pub mode: AssignmentMode,
}

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::graph::TrustLevel;

/// A resolved skill with its name and content loaded into memory.
#[derive(Debug, Clone)]
pub struct ResolvedSkill {
    pub name: String,
    pub content: String,
}

/// Reference to a skill definition, which can come from various sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillRef {
    Name(String),
    File(PathBuf),
    Url(String),
    Inline(String),
}

/// Reference to an evaluation, stored inline in a PerformanceRecord.
///
/// For roles, `context_id` holds the motivation_id used during the task.
/// For motivations, `context_id` holds the role_id used during the task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRef {
    pub score: f64,
    pub task_id: String,
    pub timestamp: String,
    /// motivation_id (when stored on a role) or role_id (when stored on a motivation)
    pub context_id: String,
}

/// Aggregated performance data for a role or motivation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerformanceRecord {
    pub task_count: u32,
    pub avg_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluations: Vec<EvaluationRef>,
}

/// Lineage metadata for tracking evolutionary history of roles and motivations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lineage {
    /// Parent ID(s). None for manually created items. Single parent for mutation,
    /// multiple parents for crossover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_ids: Vec<String>,
    /// Generation number: 0 for manually created, incrementing for evolved.
    #[serde(default)]
    pub generation: u32,
    /// Who created this: "human" or "evolver-{run_id}".
    #[serde(default = "default_created_by")]
    pub created_by: String,
    /// Timestamp when this was created.
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

/// A role defines what an agent does: its capabilities, purpose, and track record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillRef>,
    pub desired_outcome: String,
    pub performance: PerformanceRecord,
    #[serde(default)]
    pub lineage: Lineage,
    /// Default context scope for agents in this role (clean, task, graph, full)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context_scope: Option<String>,
}

/// A motivation defines why an agent acts: its goals and ethical boundaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Motivation {
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
}

fn default_executor() -> String {
    "claude".to_string()
}

/// A first-class agent entity: a persistent, reusable, named pairing of a role and a motivation.
///
/// Agent ID = SHA-256(role_id + motivation_id). Performance is tracked at the agent level
/// (distinct from its constituent role and motivation individually). Stored as YAML in
/// `.workgraph/agency/agents/{hash}.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub role_id: String,
    pub motivation_id: String,
    pub name: String,
    pub performance: PerformanceRecord,
    #[serde(default)]
    pub lineage: Lineage,
    /// Skills/capabilities this agent has (for task matching)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Hourly rate for cost tracking
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
    /// Maximum concurrent task capacity
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<f64>,
    /// Trust level for this agent
    #[serde(default, skip_serializing_if = "is_default_trust")]
    pub trust_level: TrustLevel,
    /// Contact info (email, matrix ID, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
    /// Executor backend to use (default: "claude")
    #[serde(
        default = "default_executor",
        skip_serializing_if = "is_default_executor"
    )]
    pub executor: String,
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
    #[serde(default)]
    pub motivation_id: String,
    #[serde(alias = "value")]
    pub score: f64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub dimensions: HashMap<String, f64>,
    #[serde(alias = "reasoning")]
    pub notes: String,
    #[serde(alias = "evaluated_by")]
    pub evaluator: String,
    pub timestamp: String,
    /// Model used by the agent for this task (e.g., "anthropic/claude-opus-4-6")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Source of this evaluation. Convention: "llm" (auto-evaluator), "manual",
    /// "outcome:<metric>" (e.g. "outcome:sharpe"), "vx:<peer-id>".
    /// Defaults to "llm" for backward compatibility with existing evaluation files.
    #[serde(default = "default_eval_source")]
    pub source: String,
}

fn default_eval_source() -> String {
    "llm".to_string()
}

/// Summary counts of entities in a store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoreCounts {
    pub roles: usize,
    pub motivations: usize,
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
    /// File size in bytes, or None if the file doesn't exist.
    pub size: Option<u64>,
}

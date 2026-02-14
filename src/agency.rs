use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
            generation: parent_generation + 1,
            created_by: format!("evolver-{}", run_id),
            created_at: Utc::now(),
        }
    }

    /// Create lineage for a crossover (two parents).
    pub fn crossover(parent_ids: &[&str], max_parent_generation: u32, run_id: &str) -> Self {
        Lineage {
            parent_ids: parent_ids.iter().map(|s| s.to_string()).collect(),
            generation: max_parent_generation + 1,
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

/// An agent's identity, composed of a role (what) and a motivation (why).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub role_id: String,
    pub motivation_id: String,
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
    pub id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    pub role_id: String,
    pub motivation_id: String,
    pub score: f64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub dimensions: HashMap<String, f64>,
    pub notes: String,
    pub evaluator: String,
    pub timestamp: String,
}

/// Expand `~` at the start of a path to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

/// Resolve a single skill reference to its content.
///
/// - `Name`: returns the name string as-is (tag only).
/// - `File`: reads the file, expanding `~` and resolving relative paths from `workgraph_root`.
/// - `Url`: fetches the URL content (requires `matrix-lite` feature for reqwest).
/// - `Inline`: returns the content directly.
///
/// `workgraph_root` is the project root directory (parent of `.workgraph/`).
pub fn resolve_skill(skill: &SkillRef, workgraph_root: &Path) -> Result<ResolvedSkill, String> {
    match skill {
        SkillRef::Name(name) => Ok(ResolvedSkill {
            name: name.clone(),
            content: name.clone(),
        }),
        SkillRef::File(path) => {
            let expanded = expand_tilde(path);
            let resolved = if expanded.is_absolute() {
                expanded
            } else {
                workgraph_root.join(&expanded)
            };
            let content = fs::read_to_string(&resolved)
                .map_err(|e| format!("Failed to read skill file {}: {}", resolved.display(), e))?;
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            Ok(ResolvedSkill { name, content })
        }
        SkillRef::Url(url) => resolve_url(url),
        SkillRef::Inline(content) => Ok(ResolvedSkill {
            name: "inline".to_string(),
            content: content.clone(),
        }),
    }
}

#[cfg(feature = "matrix-lite")]
fn resolve_url(url: &str) -> Result<ResolvedSkill, String> {
    // Use a blocking reqwest call since skill resolution happens at setup time.
    let body = reqwest::blocking::get(url)
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| format!("Failed to fetch skill URL {}: {}", url, e))?;
    Ok(ResolvedSkill {
        name: url.to_string(),
        content: body,
    })
}

#[cfg(not(feature = "matrix-lite"))]
fn resolve_url(url: &str) -> Result<ResolvedSkill, String> {
    Err(format!(
        "Cannot fetch skill URL {} (built without HTTP support; enable matrix-lite feature)",
        url
    ))
}

/// Resolve all skills in a role, returning successfully resolved skills.
///
/// Skills that fail to resolve produce a warning on stderr but do not abort.
pub fn resolve_all_skills(role: &Role, workgraph_root: &Path) -> Vec<ResolvedSkill> {
    role.skills
        .iter()
        .filter_map(|skill_ref| match resolve_skill(skill_ref, workgraph_root) {
            Ok(resolved) => Some(resolved),
            Err(warning) => {
                eprintln!("Warning: {}", warning);
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------------

/// Render the identity section to inject into agent prompts.
///
/// The output is placed between system context and task description in the prompt.
pub fn render_identity_prompt(
    role: &Role,
    motivation: &Motivation,
    resolved_skills: &[ResolvedSkill],
) -> String {
    let mut out = String::new();

    out.push_str("## Agent Identity\n\n");
    out.push_str(&format!("### Role: {}\n", role.name));
    out.push_str(&format!("{}\n\n", role.description));

    if !resolved_skills.is_empty() {
        out.push_str("#### Skills\n");
        for skill in resolved_skills {
            out.push_str(&format!("### {}\n{}\n\n", skill.name, skill.content));
        }
    }

    out.push_str("#### Desired Outcome\n");
    out.push_str(&format!("{}\n\n", role.desired_outcome));

    out.push_str("### Operational Parameters\n");

    out.push_str("#### Acceptable Trade-offs\n");
    for tradeoff in &motivation.acceptable_tradeoffs {
        out.push_str(&format!("- {}\n", tradeoff));
    }
    out.push('\n');

    out.push_str("#### Non-negotiable Constraints\n");
    for constraint in &motivation.unacceptable_tradeoffs {
        out.push_str(&format!("- {}\n", constraint));
    }
    out.push('\n');

    out.push_str("---");

    out
}

/// Input data for the evaluator prompt renderer.
pub struct EvaluatorInput<'a> {
    /// Task title
    pub task_title: &'a str,
    /// Task description (may be None)
    pub task_description: Option<&'a str>,
    /// Task skills required
    pub task_skills: &'a [String],
    /// Verification criteria (if any)
    pub verify: Option<&'a str>,
    /// Agent that worked on the task (if assigned)
    pub agent: Option<&'a Agent>,
    /// Role used by the agent (if identity was assigned)
    pub role: Option<&'a Role>,
    /// Motivation used by the agent (if identity was assigned)
    pub motivation: Option<&'a Motivation>,
    /// Produced artifacts (file paths / references)
    pub artifacts: &'a [String],
    /// Progress log entries
    pub log_entries: &'a [crate::graph::LogEntry],
    /// Time the task started (ISO 8601, if available)
    pub started_at: Option<&'a str>,
    /// Time the task completed (ISO 8601, if available)
    pub completed_at: Option<&'a str>,
}

/// Render the evaluator prompt that an LLM evaluator will receive.
///
/// The output is a self-contained prompt instructing the evaluator to assess
/// the agent's work and return structured JSON.
pub fn render_evaluator_prompt(input: &EvaluatorInput) -> String {
    let mut out = String::new();

    // -- System instructions --
    out.push_str("# Evaluator Instructions\n\n");
    out.push_str(
        "You are an evaluator assessing the quality of work performed by an AI agent.\n\
         Review the task definition, the agent identity that was used, the produced artifacts,\n\
         and the task log. Then produce a JSON evaluation.\n\n",
    );

    // -- Task definition --
    out.push_str("## Task Definition\n\n");
    out.push_str(&format!("**Title:** {}\n\n", input.task_title));
    if let Some(desc) = input.task_description {
        out.push_str(&format!("**Description:**\n{}\n\n", desc));
    }
    if !input.task_skills.is_empty() {
        out.push_str("**Required Skills:**\n");
        for skill in input.task_skills {
            out.push_str(&format!("- {}\n", skill));
        }
        out.push('\n');
    }
    if let Some(verify) = input.verify {
        out.push_str(&format!("**Verification Criteria:**\n{}\n\n", verify));
    }

    // -- Agent identity --
    out.push_str("## Agent Identity\n\n");
    if let Some(agent) = input.agent {
        out.push_str(&format!(
            "**Agent:** {} ({})\n\n",
            agent.name,
            short_hash(&agent.id)
        ));
    }
    if let Some(role) = input.role {
        out.push_str(&format!("**Role:** {} ({})\n", role.name, role.id));
        out.push_str(&format!("{}\n\n", role.description));
        out.push_str(&format!(
            "**Desired Outcome:** {}\n\n",
            role.desired_outcome
        ));
    } else {
        out.push_str("*No role was assigned.*\n\n");
    }
    if let Some(motivation) = input.motivation {
        out.push_str(&format!(
            "**Motivation:** {} ({})\n",
            motivation.name, motivation.id
        ));
        out.push_str(&format!("{}\n\n", motivation.description));
        if !motivation.acceptable_tradeoffs.is_empty() {
            out.push_str("**Acceptable Trade-offs:**\n");
            for t in &motivation.acceptable_tradeoffs {
                out.push_str(&format!("- {}\n", t));
            }
            out.push('\n');
        }
        if !motivation.unacceptable_tradeoffs.is_empty() {
            out.push_str("**Non-negotiable Constraints:**\n");
            for c in &motivation.unacceptable_tradeoffs {
                out.push_str(&format!("- {}\n", c));
            }
            out.push('\n');
        }
    } else {
        out.push_str("*No motivation was assigned.*\n\n");
    }

    // -- Artifacts --
    out.push_str("## Task Artifacts\n\n");
    if input.artifacts.is_empty() {
        out.push_str("*No artifacts were recorded.*\n\n");
    } else {
        for artifact in input.artifacts {
            out.push_str(&format!("- `{}`\n", artifact));
        }
        out.push('\n');
    }

    // -- Log --
    out.push_str("## Task Log\n\n");
    if input.log_entries.is_empty() {
        out.push_str("*No log entries.*\n\n");
    } else {
        for entry in input.log_entries {
            let actor = entry.actor.as_deref().unwrap_or("system");
            out.push_str(&format!(
                "- [{}] ({}): {}\n",
                entry.timestamp, actor, entry.message
            ));
        }
        out.push('\n');
    }

    // -- Timing --
    if input.started_at.is_some() || input.completed_at.is_some() {
        out.push_str("## Timing\n\n");
        if let Some(started) = input.started_at {
            out.push_str(&format!("- Started: {}\n", started));
        }
        if let Some(completed) = input.completed_at {
            out.push_str(&format!("- Completed: {}\n", completed));
        }
        out.push('\n');
    }

    // -- Evaluation rubric & output format --
    out.push_str("## Evaluation Criteria\n\n");
    out.push_str(
        "Assess the agent's work on these dimensions (each scored 0.0 to 1.0):\n\n\
         1. **correctness** — Does the output match the desired outcome? Are verification\n\
            criteria satisfied? Is the implementation functionally correct?\n\
         2. **completeness** — Were all aspects of the task addressed? Are there missing\n\
            pieces, unhandled edge cases, or incomplete deliverables?\n\
         3. **efficiency** — Was the work done efficiently within the allowed parameters?\n\
            Minimal unnecessary steps, no wasted effort, appropriate scope.\n\
         4. **style_adherence** — Does the output follow project conventions, coding\n\
            standards, and the constraints set by the motivation (trade-offs respected,\n\
            non-negotiable constraints honoured)?\n\n",
    );

    out.push_str(
        "Compute an overall **score** as a weighted average:\n\
         - correctness: 40%\n\
         - completeness: 30%\n\
         - efficiency: 15%\n\
         - style_adherence: 15%\n\n",
    );

    out.push_str("## Required Output\n\n");
    out.push_str(
        "Respond with **only** a JSON object (no markdown fences, no commentary):\n\n\
         ```\n\
         {\n  \
           \"score\": <0.0-1.0>,\n  \
           \"dimensions\": {\n    \
             \"correctness\": <0.0-1.0>,\n    \
             \"completeness\": <0.0-1.0>,\n    \
             \"efficiency\": <0.0-1.0>,\n    \
             \"style_adherence\": <0.0-1.0>\n  \
           },\n  \
           \"notes\": \"<brief explanation of strengths, weaknesses, and suggestions>\"\n\
         }\n\
         ```\n",
    );

    out
}

// ---------------------------------------------------------------------------
// Content-Hash Identifiers
// ---------------------------------------------------------------------------

/// Default number of hex characters for short display of content hashes.
pub const SHORT_HASH_LEN: usize = 8;

/// Return the first `SHORT_HASH_LEN` hex characters of a full hash for display.
pub fn short_hash(full_hash: &str) -> &str {
    &full_hash[..full_hash.len().min(SHORT_HASH_LEN)]
}

/// Compute the SHA-256 content hash for a role based on its immutable fields:
/// skills + desired_outcome + description (canonical YAML).
///
/// Performance, lineage, name, and id are excluded because they are mutable.
pub fn content_hash_role(skills: &[SkillRef], desired_outcome: &str, description: &str) -> String {
    #[derive(Serialize)]
    struct RoleHashInput<'a> {
        skills: &'a [SkillRef],
        desired_outcome: &'a str,
        description: &'a str,
    }
    let input = RoleHashInput {
        skills,
        desired_outcome,
        description,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Compute the SHA-256 content hash for a motivation based on its immutable fields:
/// acceptable_tradeoffs + unacceptable_tradeoffs + description (canonical YAML).
///
/// Performance, lineage, name, and id are excluded because they are mutable.
pub fn content_hash_motivation(
    acceptable_tradeoffs: &[String],
    unacceptable_tradeoffs: &[String],
    description: &str,
) -> String {
    #[derive(Serialize)]
    struct MotivationHashInput<'a> {
        acceptable_tradeoffs: &'a [String],
        unacceptable_tradeoffs: &'a [String],
        description: &'a str,
    }
    let input = MotivationHashInput {
        acceptable_tradeoffs,
        unacceptable_tradeoffs,
        description,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Compute the SHA-256 content hash for an agent based on its constituent IDs:
/// role_id + motivation_id.
///
/// This is deterministic: the same (role_id, motivation_id) pair always produces the same agent ID.
pub fn content_hash_agent(role_id: &str, motivation_id: &str) -> String {
    #[derive(Serialize)]
    struct AgentHashInput<'a> {
        role_id: &'a str,
        motivation_id: &'a str,
    }
    let input = AgentHashInput {
        role_id,
        motivation_id,
    };
    let yaml = serde_yaml::to_string(&input).expect("serialization of hash input cannot fail");
    let digest = Sha256::digest(yaml.as_bytes());
    format!("{:x}", digest)
}

/// Find a role in a directory by full ID or unique prefix match.
///
/// Returns the loaded role, or an error if no match or ambiguous match.
pub fn find_role_by_prefix(roles_dir: &Path, prefix: &str) -> Result<Role, AgencyError> {
    let all = load_all_roles(roles_dir)?;
    let matches: Vec<&Role> = all.iter().filter(|r| r.id.starts_with(prefix)).collect();
    match matches.len() {
        0 => Err(AgencyError::NotFound(format!(
            "No role matching '{}'",
            prefix
        ))),
        1 => Ok(matches[0].clone()),
        n => {
            let ids: Vec<&str> = matches.iter().map(|r| r.id.as_str()).collect();
            Err(AgencyError::Ambiguous(format!(
                "Prefix '{}' matches {} roles: {}",
                prefix,
                n,
                ids.join(", ")
            )))
        }
    }
}

/// Find a motivation in a directory by full ID or unique prefix match.
///
/// Returns the loaded motivation, or an error if no match or ambiguous match.
pub fn find_motivation_by_prefix(
    motivations_dir: &Path,
    prefix: &str,
) -> Result<Motivation, AgencyError> {
    let all = load_all_motivations(motivations_dir)?;
    let matches: Vec<&Motivation> = all.iter().filter(|m| m.id.starts_with(prefix)).collect();
    match matches.len() {
        0 => Err(AgencyError::NotFound(format!(
            "No motivation matching '{}'",
            prefix
        ))),
        1 => Ok(matches[0].clone()),
        n => {
            let ids: Vec<&str> = matches.iter().map(|m| m.id.as_str()).collect();
            Err(AgencyError::Ambiguous(format!(
                "Prefix '{}' matches {} motivations: {}",
                prefix,
                n,
                ids.join(", ")
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum AgencyError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Ambiguous(String),
}

/// Initialise the agency directory structure under `base`.
///
/// Creates:
///   base/roles/
///   base/motivations/
///   base/evaluations/
///   base/agents/
pub fn init(base: &Path) -> Result<(), AgencyError> {
    fs::create_dir_all(base.join("roles"))?;
    fs::create_dir_all(base.join("motivations"))?;
    fs::create_dir_all(base.join("evaluations"))?;
    fs::create_dir_all(base.join("agents"))?;
    Ok(())
}

// -- Roles (YAML) -----------------------------------------------------------

/// Load a single role from a YAML file.
pub fn load_role(path: &Path) -> Result<Role, AgencyError> {
    let contents = fs::read_to_string(path)?;
    let role: Role = serde_yaml::from_str(&contents)?;
    Ok(role)
}

/// Save a role as `<role.id>.yaml` inside `dir`.
pub fn save_role(role: &Role, dir: &Path) -> Result<PathBuf, AgencyError> {
    let path = dir.join(format!("{}.yaml", role.id));
    let yaml = serde_yaml::to_string(role)?;
    fs::write(&path, yaml)?;
    Ok(path)
}

/// Load all roles from YAML files in `dir`.
pub fn load_all_roles(dir: &Path) -> Result<Vec<Role>, AgencyError> {
    let mut roles = Vec::new();
    if !dir.exists() {
        return Ok(roles);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            roles.push(load_role(&path)?);
        }
    }
    roles.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(roles)
}

// -- Motivations (YAML) -----------------------------------------------------

/// Load a single motivation from a YAML file.
pub fn load_motivation(path: &Path) -> Result<Motivation, AgencyError> {
    let contents = fs::read_to_string(path)?;
    let motivation: Motivation = serde_yaml::from_str(&contents)?;
    Ok(motivation)
}

/// Save a motivation as `<motivation.id>.yaml` inside `dir`.
pub fn save_motivation(motivation: &Motivation, dir: &Path) -> Result<PathBuf, AgencyError> {
    let path = dir.join(format!("{}.yaml", motivation.id));
    let yaml = serde_yaml::to_string(motivation)?;
    fs::write(&path, yaml)?;
    Ok(path)
}

/// Load all motivations from YAML files in `dir`.
pub fn load_all_motivations(dir: &Path) -> Result<Vec<Motivation>, AgencyError> {
    let mut motivations = Vec::new();
    if !dir.exists() {
        return Ok(motivations);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            motivations.push(load_motivation(&path)?);
        }
    }
    motivations.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(motivations)
}

// -- Evaluations (JSON) ------------------------------------------------------

/// Load a single evaluation from a JSON file.
pub fn load_evaluation(path: &Path) -> Result<Evaluation, AgencyError> {
    let contents = fs::read_to_string(path)?;
    let eval: Evaluation = serde_json::from_str(&contents)?;
    Ok(eval)
}

/// Save an evaluation as `<evaluation.id>.json` inside `dir`.
pub fn save_evaluation(evaluation: &Evaluation, dir: &Path) -> Result<PathBuf, AgencyError> {
    let path = dir.join(format!("{}.json", evaluation.id));
    let json = serde_json::to_string_pretty(evaluation)?;
    fs::write(&path, json)?;
    Ok(path)
}

/// Load all evaluations from JSON files in `dir`.
pub fn load_all_evaluations(dir: &Path) -> Result<Vec<Evaluation>, AgencyError> {
    let mut evals = Vec::new();
    if !dir.exists() {
        return Ok(evals);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            evals.push(load_evaluation(&path)?);
        }
    }
    evals.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(evals)
}

// -- Agents (YAML) -----------------------------------------------------------

/// Load a single agent from a YAML file.
pub fn load_agent(path: &Path) -> Result<Agent, AgencyError> {
    let contents = fs::read_to_string(path)?;
    let agent: Agent = serde_yaml::from_str(&contents)?;
    Ok(agent)
}

/// Save an agent as `<agent.id>.yaml` inside `dir`.
pub fn save_agent(agent: &Agent, dir: &Path) -> Result<PathBuf, AgencyError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.yaml", agent.id));
    let yaml = serde_yaml::to_string(agent)?;
    fs::write(&path, yaml)?;
    Ok(path)
}

/// Load all agents from YAML files in `dir`.
pub fn load_all_agents(dir: &Path) -> Result<Vec<Agent>, AgencyError> {
    let mut agents = Vec::new();
    if !dir.exists() {
        return Ok(agents);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            agents.push(load_agent(&path)?);
        }
    }
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(agents)
}

/// Find an agent in a directory by full ID or unique prefix match.
pub fn find_agent_by_prefix(agents_dir: &Path, prefix: &str) -> Result<Agent, AgencyError> {
    let all = load_all_agents(agents_dir)?;
    let matches: Vec<&Agent> = all.iter().filter(|a| a.id.starts_with(prefix)).collect();
    match matches.len() {
        0 => Err(AgencyError::NotFound(format!(
            "No agent matching '{}'",
            prefix
        ))),
        1 => Ok(matches[0].clone()),
        n => {
            let ids: Vec<&str> = matches.iter().map(|a| a.id.as_str()).collect();
            Err(AgencyError::Ambiguous(format!(
                "Prefix '{}' matches {} agents: {}",
                prefix,
                n,
                ids.join(", ")
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Lineage queries
// ---------------------------------------------------------------------------

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

/// Build the ancestry tree for a role by walking parent_ids.
/// Returns nodes ordered from the target (first) up to its oldest ancestors.
pub fn role_ancestry(role_id: &str, roles_dir: &Path) -> Result<Vec<AncestryNode>, AgencyError> {
    let all_roles = load_all_roles(roles_dir)?;
    let role_map: HashMap<String, &Role> = all_roles.iter().map(|r| (r.id.clone(), r)).collect();
    let mut ancestry = Vec::new();
    let mut queue = vec![role_id.to_string()];
    let mut visited = std::collections::HashSet::new();

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
    let mut visited = std::collections::HashSet::new();

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

// ---------------------------------------------------------------------------
// Evaluation Recording
// ---------------------------------------------------------------------------

/// Recalculate the average score from a list of EvaluationRefs.
///
/// Returns `None` if the list is empty.
pub fn recalculate_avg_score(evaluations: &[EvaluationRef]) -> Option<f64> {
    if evaluations.is_empty() {
        return None;
    }
    let sum: f64 = evaluations.iter().map(|e| e.score).sum();
    Some(sum / evaluations.len() as f64)
}

/// Update a PerformanceRecord with a new evaluation reference.
///
/// Increments task_count, appends the EvaluationRef, and recalculates avg_score.
pub fn update_performance(record: &mut PerformanceRecord, eval_ref: EvaluationRef) {
    record.task_count += 1;
    record.evaluations.push(eval_ref);
    record.avg_score = recalculate_avg_score(&record.evaluations);
}

/// Record an evaluation: persist the eval JSON, and update agent, role, and motivation performance.
///
/// Steps:
/// 1. Save the `Evaluation` as JSON in `agency_dir/evaluations/eval-{task_id}-{timestamp}.json`.
/// 2. Load the agent (if agent_id is set), add an `EvaluationRef`, recalculate scores, save.
/// 3. Load the role, add an `EvaluationRef` (with motivation_id as context), recalculate scores, save.
/// 4. Load the motivation, add an `EvaluationRef` (with role_id as context), recalculate scores, save.
///
/// Returns the path to the saved evaluation JSON.
pub fn record_evaluation(
    evaluation: &Evaluation,
    agency_dir: &Path,
) -> Result<PathBuf, AgencyError> {
    init(agency_dir)?;

    let evals_dir = agency_dir.join("evaluations");
    let roles_dir = agency_dir.join("roles");
    let motivations_dir = agency_dir.join("motivations");
    let agents_dir = agency_dir.join("agents");

    // 1. Save the full Evaluation JSON with task_id-timestamp naming
    let safe_ts = evaluation.timestamp.replace(':', "-");
    let eval_filename = format!("eval-{}-{}.json", evaluation.task_id, safe_ts);
    let eval_path = evals_dir.join(&eval_filename);
    let json = serde_json::to_string_pretty(evaluation)?;
    fs::write(&eval_path, json)?;

    // 2. Update agent performance (if agent_id is present)
    if !evaluation.agent_id.is_empty() {
        if let Ok(mut agent) = find_agent_by_prefix(&agents_dir, &evaluation.agent_id) {
            let agent_eval_ref = EvaluationRef {
                score: evaluation.score,
                task_id: evaluation.task_id.clone(),
                timestamp: evaluation.timestamp.clone(),
                context_id: evaluation.task_id.clone(),
            };
            update_performance(&mut agent.performance, agent_eval_ref);
            save_agent(&agent, &agents_dir)?;
        }
    }

    // 3. Update role performance (look up by prefix to support both full and short IDs)
    if let Ok(mut role) = find_role_by_prefix(&roles_dir, &evaluation.role_id) {
        let role_eval_ref = EvaluationRef {
            score: evaluation.score,
            task_id: evaluation.task_id.clone(),
            timestamp: evaluation.timestamp.clone(),
            context_id: evaluation.motivation_id.clone(),
        };
        update_performance(&mut role.performance, role_eval_ref);
        save_role(&role, &roles_dir)?;
    }

    // 4. Update motivation performance
    if let Ok(mut motivation) =
        find_motivation_by_prefix(&motivations_dir, &evaluation.motivation_id)
    {
        let motivation_eval_ref = EvaluationRef {
            score: evaluation.score,
            task_id: evaluation.task_id.clone(),
            timestamp: evaluation.timestamp.clone(),
            context_id: evaluation.role_id.clone(),
        };
        update_performance(&mut motivation.performance, motivation_eval_ref);
        save_motivation(&motivation, &motivations_dir)?;
    }

    Ok(eval_path)
}

// ---------------------------------------------------------------------------
// Starter Roles & Motivations
// ---------------------------------------------------------------------------

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
// Evolution utilities
// ---------------------------------------------------------------------------

/// Mutate a parent role to produce a child with updated fields and correct lineage.
///
/// Any `None` field inherits the parent's value. The child gets a fresh content-hash ID
/// based on its (possibly mutated) description, skills, and desired_outcome.
pub fn mutate_role(
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
    }
}

/// Crossover two motivations: union their accept/reject lists and set crossover lineage.
///
/// Produces a new motivation whose acceptable_tradeoffs and unacceptable_tradeoffs are
/// the deduplicated union of both parents' lists.
pub fn crossover_motivations(
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
pub fn tournament_select_role(candidates: &[Role]) -> Option<&Role> {
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
pub fn roles_below_threshold(roles: &[Role], threshold: f64, min_evals: u32) -> Vec<&Role> {
    roles
        .iter()
        .filter(|r| {
            r.performance.task_count >= min_evals
                && r.performance.avg_score.map_or(false, |s| s < threshold)
        })
        .collect()
}

/// Gap analysis: given a set of required skill names and the current roles,
/// return the skill names that are not covered by any existing role.
///
/// A skill is "covered" if at least one role has a `SkillRef::Name(n)` where
/// `n` matches the required skill (case-sensitive).
pub fn uncovered_skills(required: &[&str], roles: &[Role]) -> Vec<String> {
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

// ---------------------------------------------------------------------------
// Task output capture
// ---------------------------------------------------------------------------

/// An entry in the artifact manifest written to artifacts.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub path: String,
    /// File size in bytes, or None if the file doesn't exist.
    pub size: Option<u64>,
}

/// Capture a snapshot of what an agent produced for a completed task.
///
/// Writes three files into `{wg_dir}/output/{task_id}/`:
///   - `changes.patch` — git diff from claim time (`started_at`) to now
///   - `artifacts.json` — JSON list of registered artifacts with file sizes
///   - `log.json` — full progress log entries
///
/// This is a mechanical operation — no LLM is involved. The coordinator calls
/// this after marking a task done but before creating any evaluation task.
/// The evaluator reads from `{wg_dir}/output/{task_id}/` to assess work.
///
/// Errors are non-fatal: individual capture steps log warnings and continue.
pub fn capture_task_output(
    wg_dir: &Path,
    task: &crate::graph::Task,
) -> Result<PathBuf, AgencyError> {
    let output_dir = wg_dir.join("output").join(&task.id);
    fs::create_dir_all(&output_dir)?;

    // 1. Git diff capture
    capture_git_diff(&output_dir, task);

    // 2. Artifact manifest
    capture_artifact_manifest(&output_dir, task);

    // 3. Log snapshot
    capture_log_snapshot(&output_dir, task);

    Ok(output_dir)
}

/// Capture git diff from task claim time to now, saved as changes.patch.
///
/// Uses `started_at` as the since-timestamp for `git diff`. If the project
/// is not a git repo or the diff fails, writes an empty patch with a comment.
fn capture_git_diff(output_dir: &Path, task: &crate::graph::Task) {
    let patch_path = output_dir.join("changes.patch");

    // Find the project root by walking up from the .workgraph dir
    let project_root = output_dir.ancestors().find(|p| p.join(".git").exists());

    let project_root = match project_root {
        Some(root) => root.to_path_buf(),
        None => {
            // Not a git repo — write an empty patch with explanation
            let _ = fs::write(&patch_path, "# Not a git repository — no diff captured\n");
            return;
        }
    };

    // Build the git diff command.
    // If we have a started_at timestamp, find the commit closest to that time
    // and diff from there to the current working tree (including uncommitted).
    let output = if let Some(ref started_at) = task.started_at {
        // Find the last commit before the task was claimed
        let rev_result = std::process::Command::new("git")
            .args([
                "rev-list",
                "-1",
                &format!("--before={}", started_at),
                "HEAD",
            ])
            .current_dir(&project_root)
            .output();

        match rev_result {
            Ok(rev_output) if rev_output.status.success() => {
                let base_rev = String::from_utf8_lossy(&rev_output.stdout)
                    .trim()
                    .to_string();

                if base_rev.is_empty() {
                    // No commit before started_at — diff entire working tree
                    std::process::Command::new("git")
                        .args(["diff", "HEAD"])
                        .current_dir(&project_root)
                        .output()
                } else {
                    // Diff from base revision to current working tree
                    std::process::Command::new("git")
                        .args(["diff", &base_rev])
                        .current_dir(&project_root)
                        .output()
                }
            }
            _ => {
                // rev-list failed — fall back to uncommitted changes only
                std::process::Command::new("git")
                    .args(["diff", "HEAD"])
                    .current_dir(&project_root)
                    .output()
            }
        }
    } else {
        // No started_at — just capture uncommitted changes
        std::process::Command::new("git")
            .args(["diff", "HEAD"])
            .current_dir(&project_root)
            .output()
    };

    match output {
        Ok(out) if out.status.success() => {
            let diff = String::from_utf8_lossy(&out.stdout);
            if diff.is_empty() {
                let _ = fs::write(&patch_path, "# No changes detected in git diff\n");
            } else {
                let _ = fs::write(&patch_path, diff.as_bytes());
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let _ = fs::write(
                &patch_path,
                format!("# git diff failed: {}\n", stderr.trim()),
            );
        }
        Err(e) => {
            let _ = fs::write(&patch_path, format!("# git diff failed: {}\n", e));
        }
    }
}

/// Capture artifact manifest as artifacts.json — a JSON list of registered
/// artifacts with their file paths and sizes.
fn capture_artifact_manifest(output_dir: &Path, task: &crate::graph::Task) {
    let manifest_path = output_dir.join("artifacts.json");

    // Find project root for resolving relative artifact paths
    let project_root = output_dir
        .ancestors()
        .find(|p| p.join(".git").exists())
        .map(|p| p.to_path_buf());

    let entries: Vec<ArtifactEntry> = task
        .artifacts
        .iter()
        .map(|artifact_path| {
            // Try to get file size — resolve relative paths from project root
            let full_path = if Path::new(artifact_path).is_absolute() {
                PathBuf::from(artifact_path)
            } else if let Some(ref root) = project_root {
                root.join(artifact_path)
            } else {
                PathBuf::from(artifact_path)
            };

            let size = fs::metadata(&full_path).ok().map(|m| m.len());

            ArtifactEntry {
                path: artifact_path.clone(),
                size,
            }
        })
        .collect();

    match serde_json::to_string_pretty(&entries) {
        Ok(json) => {
            let _ = fs::write(&manifest_path, json);
        }
        Err(e) => {
            eprintln!("Warning: failed to serialize artifact manifest: {}", e);
        }
    }
}

/// Capture the full progress log as log.json.
fn capture_log_snapshot(output_dir: &Path, task: &crate::graph::Task) {
    let log_path = output_dir.join("log.json");

    match serde_json::to_string_pretty(&task.log) {
        Ok(json) => {
            let _ = fs::write(&log_path, json);
        }
        Err(e) => {
            eprintln!("Warning: failed to serialize log snapshot: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn test_role(skills: Vec<SkillRef>) -> Role {
        build_role("Test Role", "A test role", skills, "Testing")
    }

    #[test]
    fn resolve_name_returns_name_as_content() {
        let skill = SkillRef::Name("my-skill".to_string());
        let resolved = resolve_skill(&skill, Path::new("/tmp")).unwrap();
        assert_eq!(resolved.name, "my-skill");
        assert_eq!(resolved.content, "my-skill");
    }

    #[test]
    fn resolve_inline_returns_content_directly() {
        let skill = SkillRef::Inline("do the thing well".to_string());
        let resolved = resolve_skill(&skill, Path::new("/tmp")).unwrap();
        assert_eq!(resolved.name, "inline");
        assert_eq!(resolved.content, "do the thing well");
    }

    #[test]
    fn resolve_file_absolute_path() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("skill.md");
        let mut f = fs::File::create(&file_path).unwrap();
        write!(f, "# Skill\nDo stuff").unwrap();

        let skill = SkillRef::File(file_path.clone());
        let resolved = resolve_skill(&skill, Path::new("/nonexistent")).unwrap();
        assert_eq!(resolved.name, "skill");
        assert_eq!(resolved.content, "# Skill\nDo stuff");
    }

    #[test]
    fn resolve_file_relative_path() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("skills").join("coding.txt");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, "Write good code").unwrap();

        let skill = SkillRef::File(PathBuf::from("skills/coding.txt"));
        let resolved = resolve_skill(&skill, dir.path()).unwrap();
        assert_eq!(resolved.name, "coding");
        assert_eq!(resolved.content, "Write good code");
    }

    #[test]
    fn resolve_file_missing_returns_error() {
        let skill = SkillRef::File(PathBuf::from("/no/such/file.md"));
        let result = resolve_skill(&skill, Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read skill file"));
    }

    #[test]
    fn resolve_all_skips_failures_gracefully() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("good.md");
        fs::write(&file_path, "good content").unwrap();

        let role = test_role(vec![
            SkillRef::Name("tag-only".to_string()),
            SkillRef::File(PathBuf::from("/no/such/file.md")),
            SkillRef::File(file_path),
            SkillRef::Inline("inline content".to_string()),
        ]);

        let resolved = resolve_all_skills(&role, dir.path());
        // The missing file should be skipped, leaving 3 resolved skills
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].name, "tag-only");
        assert_eq!(resolved[1].name, "good");
        assert_eq!(resolved[1].content, "good content");
        assert_eq!(resolved[2].name, "inline");
    }

    #[test]
    fn expand_tilde_with_home() {
        let path = Path::new("~/some/file.txt");
        let expanded = expand_tilde(path);
        // Should not start with ~ anymore
        assert!(!expanded.starts_with("~"));
        assert!(expanded.ends_with("some/file.txt"));
    }

    #[test]
    fn expand_tilde_without_tilde() {
        let path = Path::new("/absolute/path.txt");
        let expanded = expand_tilde(path);
        assert_eq!(expanded, PathBuf::from("/absolute/path.txt"));
    }

    // -- Storage tests -------------------------------------------------------

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
        }
    }

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
        fs::write(dir.join("stray.txt"), "not yaml").unwrap();
        save_role(&sample_role(), dir).unwrap();

        let all = load_all_roles(dir).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_role_yaml_is_human_readable() {
        let tmp = TempDir::new().unwrap();
        let role = sample_role();
        let path = save_role(&role, tmp.path()).unwrap();
        let contents = fs::read_to_string(path).unwrap();
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
    fn test_role_ancestry_tree() {
        let tmp = TempDir::new().unwrap();
        let roles_dir = tmp.path();

        // Create a 3-generation lineage: grandparent -> parent -> child
        // Use build_role for content-hash IDs, then set lineage
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
    fn test_role_yaml_includes_lineage() {
        let tmp = TempDir::new().unwrap();
        let mut role = sample_role();
        role.lineage = Lineage::mutation("src-role", 0, "evo-1");
        let path = save_role(&role, tmp.path()).unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("lineage:"));
        assert!(contents.contains("parent_ids:"));
        assert!(contents.contains("generation:"));
        assert!(contents.contains("created_by:"));
        assert!(contents.contains("created_at:"));
    }

    // -- Identity prompt rendering tests ------------------------------------

    #[test]
    fn test_render_identity_prompt_full() {
        let role = build_role(
            "Implementer",
            "Writes code to fulfil task requirements.",
            vec![],
            "Working, tested code merged to main.",
        );
        let motivation = build_motivation(
            "Quality First",
            "Prioritise correctness and maintainability.",
            vec![
                "Slower delivery for higher quality".into(),
                "More verbose code for clarity".into(),
            ],
            vec!["Skipping tests".into(), "Ignoring error handling".into()],
        );
        let skills = vec![
            ResolvedSkill {
                name: "Rust".into(),
                content: "Write idiomatic Rust code.".into(),
            },
            ResolvedSkill {
                name: "Testing".into(),
                content: "Write comprehensive tests.".into(),
            },
        ];

        let output = render_identity_prompt(&role, &motivation, &skills);

        // Verify structure
        assert!(output.starts_with("## Agent Identity\n"));
        assert!(output.contains("### Role: Implementer\n"));
        assert!(output.contains("Writes code to fulfil task requirements.\n"));
        assert!(output.contains("#### Skills\n"));
        assert!(output.contains("### Rust\nWrite idiomatic Rust code.\n"));
        assert!(output.contains("### Testing\nWrite comprehensive tests.\n"));
        assert!(output.contains("#### Desired Outcome\n"));
        assert!(output.contains("Working, tested code merged to main.\n"));
        assert!(output.contains("### Operational Parameters\n"));
        assert!(output.contains("#### Acceptable Trade-offs\n"));
        assert!(output.contains("- Slower delivery for higher quality\n"));
        assert!(output.contains("- More verbose code for clarity\n"));
        assert!(output.contains("#### Non-negotiable Constraints\n"));
        assert!(output.contains("- Skipping tests\n"));
        assert!(output.contains("- Ignoring error handling\n"));
        assert!(output.ends_with("---"));
    }

    #[test]
    fn test_render_identity_prompt_no_skills() {
        let role = build_role(
            "Reviewer",
            "Reviews code for quality.",
            vec![],
            "All code reviewed.",
        );
        let motivation = build_motivation(
            "Fast",
            "Be fast.",
            vec!["Less thorough reviews".into()],
            vec!["Missing security issues".into()],
        );

        let output = render_identity_prompt(&role, &motivation, &[]);

        // No Skills header when empty
        assert!(!output.contains("#### Skills\n"));
        // But everything else is present
        assert!(output.contains("### Role: Reviewer\n"));
        assert!(output.contains("#### Desired Outcome\n"));
        assert!(output.contains("#### Acceptable Trade-offs\n"));
        assert!(output.contains("- Less thorough reviews\n"));
        assert!(output.contains("#### Non-negotiable Constraints\n"));
        assert!(output.contains("- Missing security issues\n"));
    }

    #[test]
    fn test_render_identity_prompt_empty_tradeoffs() {
        let role = build_role("Minimal", "A minimal role.", vec![], "Done.");
        let motivation = build_motivation("Minimal Motivation", "Minimal.", vec![], vec![]);

        let output = render_identity_prompt(&role, &motivation, &[]);

        // Headers should still be present even with no items
        assert!(output.contains("#### Acceptable Trade-offs\n"));
        assert!(output.contains("#### Non-negotiable Constraints\n"));
        assert!(output.ends_with("---"));
    }

    #[test]
    fn test_render_identity_prompt_section_order() {
        let role = sample_role();
        let motivation = sample_motivation();
        let skills = vec![ResolvedSkill {
            name: "Coding".into(),
            content: "Write code.".into(),
        }];

        let output = render_identity_prompt(&role, &motivation, &skills);

        // Verify sections appear in the correct order
        let agent_identity_pos = output.find("## Agent Identity").unwrap();
        let role_pos = output.find("### Role:").unwrap();
        let skills_pos = output.find("#### Skills").unwrap();
        let desired_outcome_pos = output.find("#### Desired Outcome").unwrap();
        let operational_pos = output.find("### Operational Parameters").unwrap();
        let acceptable_pos = output.find("#### Acceptable Trade-offs").unwrap();
        let constraints_pos = output.find("#### Non-negotiable Constraints").unwrap();
        let separator_pos = output.find("---").unwrap();

        assert!(agent_identity_pos < role_pos);
        assert!(role_pos < skills_pos);
        assert!(skills_pos < desired_outcome_pos);
        assert!(desired_outcome_pos < operational_pos);
        assert!(operational_pos < acceptable_pos);
        assert!(acceptable_pos < constraints_pos);
        assert!(constraints_pos < separator_pos);
    }

    // -- Evaluator prompt rendering tests -----------------------------------

    fn sample_log_entries() -> Vec<crate::graph::LogEntry> {
        vec![
            crate::graph::LogEntry {
                timestamp: "2025-05-01T10:00:00Z".into(),
                actor: Some("agent-1".into()),
                message: "Starting implementation".into(),
            },
            crate::graph::LogEntry {
                timestamp: "2025-05-01T10:30:00Z".into(),
                actor: None,
                message: "Completed core logic".into(),
            },
        ]
    }

    #[test]
    fn test_render_evaluator_prompt_full() {
        let role = sample_role();
        let motivation = sample_motivation();
        let artifacts = vec!["src/main.rs".to_string(), "tests/test_main.rs".to_string()];
        let log = sample_log_entries();

        let input = EvaluatorInput {
            task_title: "Implement feature X",
            task_description: Some("Build feature X with full test coverage."),
            task_skills: &["rust".to_string(), "testing".to_string()],
            verify: Some("All tests pass and code compiles without warnings."),
            agent: None,
            role: Some(&role),
            motivation: Some(&motivation),
            artifacts: &artifacts,
            log_entries: &log,
            started_at: Some("2025-05-01T10:00:00Z"),
            completed_at: Some("2025-05-01T11:00:00Z"),
        };

        let output = render_evaluator_prompt(&input);

        // System instructions
        assert!(output.starts_with("# Evaluator Instructions\n"));
        assert!(output.contains("You are an evaluator"));

        // Task definition
        assert!(output.contains("## Task Definition"));
        assert!(output.contains("**Title:** Implement feature X"));
        assert!(output.contains("Build feature X with full test coverage."));
        assert!(output.contains("- rust\n"));
        assert!(output.contains("- testing\n"));
        assert!(output.contains("**Verification Criteria:**"));
        assert!(output.contains("All tests pass and code compiles without warnings."));

        // Agent identity — IDs are content hashes
        assert!(output.contains("## Agent Identity"));
        assert!(output.contains(&format!("**Role:** Implementer ({})", role.id)));
        assert!(output.contains("**Desired Outcome:** Working, tested code merged to main."));
        assert!(output.contains(&format!(
            "**Motivation:** Quality First ({})",
            motivation.id
        )));
        assert!(output.contains("**Acceptable Trade-offs:**"));
        assert!(output.contains("- Slower delivery for higher quality"));
        assert!(output.contains("**Non-negotiable Constraints:**"));
        assert!(output.contains("- Skipping tests"));

        // Artifacts
        assert!(output.contains("## Task Artifacts"));
        assert!(output.contains("- `src/main.rs`"));
        assert!(output.contains("- `tests/test_main.rs`"));

        // Log
        assert!(output.contains("## Task Log"));
        assert!(output.contains("(agent-1): Starting implementation"));
        assert!(output.contains("(system): Completed core logic"));

        // Timing
        assert!(output.contains("## Timing"));
        assert!(output.contains("- Started: 2025-05-01T10:00:00Z"));
        assert!(output.contains("- Completed: 2025-05-01T11:00:00Z"));

        // Evaluation criteria
        assert!(output.contains("## Evaluation Criteria"));
        assert!(output.contains("**correctness**"));
        assert!(output.contains("**completeness**"));
        assert!(output.contains("**efficiency**"));
        assert!(output.contains("**style_adherence**"));

        // Weights
        assert!(output.contains("correctness: 40%"));
        assert!(output.contains("completeness: 30%"));
        assert!(output.contains("efficiency: 15%"));

        // Output format
        assert!(output.contains("## Required Output"));
        assert!(output.contains("\"score\""));
        assert!(output.contains("\"dimensions\""));
        assert!(output.contains("\"notes\""));
    }

    #[test]
    fn test_render_evaluator_prompt_minimal() {
        let input = EvaluatorInput {
            task_title: "Simple task",
            task_description: None,
            task_skills: &[],
            verify: None,
            agent: None,
            role: None,
            motivation: None,
            artifacts: &[],
            log_entries: &[],
            started_at: None,
            completed_at: None,
        };

        let output = render_evaluator_prompt(&input);

        assert!(output.contains("**Title:** Simple task"));
        assert!(!output.contains("**Description:**"));
        assert!(!output.contains("**Required Skills:**"));
        assert!(!output.contains("**Verification Criteria:**"));
        assert!(output.contains("*No role was assigned.*"));
        assert!(output.contains("*No motivation was assigned.*"));
        assert!(output.contains("*No artifacts were recorded.*"));
        assert!(output.contains("*No log entries.*"));
        assert!(!output.contains("## Timing"));
        // Evaluation sections should always be present
        assert!(output.contains("## Evaluation Criteria"));
        assert!(output.contains("## Required Output"));
    }

    #[test]
    fn test_render_evaluator_prompt_section_order() {
        let role = sample_role();
        let motivation = sample_motivation();
        let log = sample_log_entries();

        let input = EvaluatorInput {
            task_title: "Test order",
            task_description: Some("desc"),
            task_skills: &["rust".to_string()],
            verify: Some("verify"),
            agent: None,
            role: Some(&role),
            motivation: Some(&motivation),
            artifacts: &["file.rs".to_string()],
            log_entries: &log,
            started_at: Some("2025-01-01T00:00:00Z"),
            completed_at: Some("2025-01-01T01:00:00Z"),
        };

        let output = render_evaluator_prompt(&input);

        let instructions_pos = output.find("# Evaluator Instructions").unwrap();
        let task_def_pos = output.find("## Task Definition").unwrap();
        let identity_pos = output.find("## Agent Identity").unwrap();
        let artifacts_pos = output.find("## Task Artifacts").unwrap();
        let log_pos = output.find("## Task Log").unwrap();
        let timing_pos = output.find("## Timing").unwrap();
        let criteria_pos = output.find("## Evaluation Criteria").unwrap();
        let required_pos = output.find("## Required Output").unwrap();

        assert!(instructions_pos < task_def_pos);
        assert!(task_def_pos < identity_pos);
        assert!(identity_pos < artifacts_pos);
        assert!(artifacts_pos < log_pos);
        assert!(log_pos < timing_pos);
        assert!(timing_pos < criteria_pos);
        assert!(criteria_pos < required_pos);
    }

    // -- Evaluation recording tests ------------------------------------------

    fn make_eval_ref(score: f64, task_id: &str, context_id: &str) -> EvaluationRef {
        EvaluationRef {
            score,
            task_id: task_id.into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
            context_id: context_id.into(),
        }
    }

    #[test]
    fn test_recalculate_avg_score_empty() {
        assert_eq!(recalculate_avg_score(&[]), None);
    }

    #[test]
    fn test_recalculate_avg_score_single() {
        let refs = vec![make_eval_ref(0.8, "t1", "m1")];
        let avg = recalculate_avg_score(&refs).unwrap();
        assert!((avg - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_recalculate_avg_score_multiple() {
        let refs = vec![
            make_eval_ref(0.6, "t1", "m1"),
            make_eval_ref(0.8, "t2", "m1"),
            make_eval_ref(1.0, "t3", "m1"),
        ];
        let avg = recalculate_avg_score(&refs).unwrap();
        assert!((avg - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_recalculate_avg_score_uneven() {
        let refs = vec![
            make_eval_ref(0.0, "t1", "m1"),
            make_eval_ref(1.0, "t2", "m1"),
        ];
        let avg = recalculate_avg_score(&refs).unwrap();
        assert!((avg - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_update_performance_increments_and_recalculates() {
        let mut record = PerformanceRecord {
            task_count: 0,
            avg_score: None,
            evaluations: vec![],
        };

        update_performance(&mut record, make_eval_ref(0.8, "t1", "m1"));
        assert_eq!(record.task_count, 1);
        assert!((record.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
        assert_eq!(record.evaluations.len(), 1);

        update_performance(&mut record, make_eval_ref(0.6, "t2", "m1"));
        assert_eq!(record.task_count, 2);
        assert!((record.avg_score.unwrap() - 0.7).abs() < f64::EPSILON);
        assert_eq!(record.evaluations.len(), 2);

        update_performance(&mut record, make_eval_ref(1.0, "t3", "m1"));
        assert_eq!(record.task_count, 3);
        assert!((record.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
        assert_eq!(record.evaluations.len(), 3);
    }

    #[test]
    fn test_update_performance_from_existing() {
        let mut record = PerformanceRecord {
            task_count: 2,
            avg_score: Some(0.7),
            evaluations: vec![
                make_eval_ref(0.6, "t1", "m1"),
                make_eval_ref(0.8, "t2", "m1"),
            ],
        };

        update_performance(&mut record, make_eval_ref(0.9, "t3", "m1"));
        assert_eq!(record.task_count, 3);
        let expected = (0.6 + 0.8 + 0.9) / 3.0;
        assert!((record.avg_score.unwrap() - expected).abs() < 1e-10);
    }

    #[test]
    fn test_record_evaluation_saves_all_artifacts() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let role = sample_role();
        let role_id = role.id.clone();
        save_role(&role, &agency_dir.join("roles")).unwrap();
        let motivation = sample_motivation();
        let motivation_id = motivation.id.clone();
        save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

        let eval = Evaluation {
            id: "eval-test-1".into(),
            task_id: "task-42".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            motivation_id: motivation_id.clone(),
            score: 0.85,
            dimensions: HashMap::new(),
            notes: "Good work".into(),
            evaluator: "test".into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
        };

        let eval_path = record_evaluation(&eval, &agency_dir).unwrap();

        // 1. Evaluation JSON was saved
        assert!(eval_path.exists());
        let saved_eval = load_evaluation(&eval_path).unwrap();
        assert_eq!(saved_eval.score, 0.85);
        assert_eq!(saved_eval.task_id, "task-42");

        // 2. Role performance was updated
        let role_path = agency_dir.join("roles").join(format!("{}.yaml", role_id));
        let updated_role = load_role(&role_path).unwrap();
        assert_eq!(updated_role.performance.task_count, 1);
        assert!((updated_role.performance.avg_score.unwrap() - 0.85).abs() < f64::EPSILON);
        assert_eq!(updated_role.performance.evaluations.len(), 1);
        assert_eq!(updated_role.performance.evaluations[0].task_id, "task-42");
        assert_eq!(
            updated_role.performance.evaluations[0].context_id,
            motivation_id
        );

        // 3. Motivation performance was updated
        let motivation_path = agency_dir
            .join("motivations")
            .join(format!("{}.yaml", motivation_id));
        let updated_motivation = load_motivation(&motivation_path).unwrap();
        assert_eq!(updated_motivation.performance.task_count, 1);
        assert!((updated_motivation.performance.avg_score.unwrap() - 0.85).abs() < f64::EPSILON);
        assert_eq!(updated_motivation.performance.evaluations.len(), 1);
        assert_eq!(
            updated_motivation.performance.evaluations[0].context_id,
            role_id
        );
    }

    #[test]
    fn test_record_evaluation_multiple_accumulates() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let role = sample_role();
        let role_id = role.id.clone();
        save_role(&role, &agency_dir.join("roles")).unwrap();
        let motivation = sample_motivation();
        let motivation_id = motivation.id.clone();
        save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

        let eval1 = Evaluation {
            id: "eval-1".into(),
            task_id: "task-1".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            motivation_id: motivation_id.clone(),
            score: 0.6,
            dimensions: HashMap::new(),
            notes: "".into(),
            evaluator: "test".into(),
            timestamp: "2025-05-01T10:00:00Z".into(),
        };

        let eval2 = Evaluation {
            id: "eval-2".into(),
            task_id: "task-2".into(),
            agent_id: String::new(),
            role_id: role_id.clone(),
            motivation_id: motivation_id.clone(),
            score: 1.0,
            dimensions: HashMap::new(),
            notes: "".into(),
            evaluator: "test".into(),
            timestamp: "2025-05-01T11:00:00Z".into(),
        };

        record_evaluation(&eval1, &agency_dir).unwrap();
        record_evaluation(&eval2, &agency_dir).unwrap();

        let role_path = agency_dir.join("roles").join(format!("{}.yaml", role_id));
        let updated_role = load_role(&role_path).unwrap();
        assert_eq!(updated_role.performance.task_count, 2);
        assert!((updated_role.performance.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
        assert_eq!(updated_role.performance.evaluations.len(), 2);

        let motivation_path = agency_dir
            .join("motivations")
            .join(format!("{}.yaml", motivation_id));
        let updated_motivation = load_motivation(&motivation_path).unwrap();
        assert_eq!(updated_motivation.performance.task_count, 2);
        assert!((updated_motivation.performance.avg_score.unwrap() - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_record_evaluation_missing_role_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        init(&agency_dir).unwrap();

        let motivation = sample_motivation();
        let motivation_id = motivation.id.clone();
        save_motivation(&motivation, &agency_dir.join("motivations")).unwrap();

        let eval = Evaluation {
            id: "eval-orphan".into(),
            task_id: "task-99".into(),
            agent_id: String::new(),
            role_id: "nonexistent-role".into(),
            motivation_id: motivation_id.clone(),
            score: 0.5,
            dimensions: HashMap::new(),
            notes: "".into(),
            evaluator: "test".into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
        };

        let result = record_evaluation(&eval, &agency_dir);
        assert!(result.is_ok());

        let motivation_path = agency_dir
            .join("motivations")
            .join(format!("{}.yaml", motivation_id));
        let updated = load_motivation(&motivation_path).unwrap();
        assert_eq!(updated.performance.task_count, 1);
    }

    #[test]
    fn test_evaluation_ref_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut role = sample_role();
        role.performance.evaluations.push(EvaluationRef {
            score: 0.75,
            task_id: "task-abc".into(),
            timestamp: "2025-05-01T12:00:00Z".into(),
            context_id: "motivation-xyz".into(),
        });
        role.performance.task_count = 1;
        role.performance.avg_score = Some(0.75);

        let path = save_role(&role, tmp.path()).unwrap();
        let loaded = load_role(&path).unwrap();

        assert_eq!(loaded.performance.evaluations.len(), 1);
        let ref0 = &loaded.performance.evaluations[0];
        assert!((ref0.score - 0.75).abs() < f64::EPSILON);
        assert_eq!(ref0.task_id, "task-abc");
        assert_eq!(ref0.timestamp, "2025-05-01T12:00:00Z");
        assert_eq!(ref0.context_id, "motivation-xyz");
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
            executor: default_executor(),
        };
        // default_executor() returns "claude" which is not human
        assert!(!agent.is_human());
    }
}

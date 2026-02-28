use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

use super::types::*;

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
pub fn init(base: &Path) -> Result<(), AgencyError> {
    fs::create_dir_all(base.join("primitives/components"))?;
    fs::create_dir_all(base.join("primitives/outcomes"))?;
    fs::create_dir_all(base.join("primitives/tradeoffs"))?;
    fs::create_dir_all(base.join("cache/roles"))?;
    fs::create_dir_all(base.join("cache/agents"))?;
    fs::create_dir_all(base.join("evaluations"))?;
    fs::create_dir_all(base.join("deferred"))?;
    fs::create_dir_all(base.join("assignments"))?;
    Ok(())
}

// Generic helpers
fn load_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, AgencyError> {
    let contents = fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&contents)?)
}

fn save_yaml<T: serde::Serialize>(val: &T, dir: &Path, id: &str) -> Result<PathBuf, AgencyError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.yaml", id));
    fs::write(&path, serde_yaml::to_string(val)?)?;
    Ok(path)
}

fn load_all_yaml<T: serde::de::DeserializeOwned + HasId>(dir: &Path) -> Result<Vec<T>, AgencyError> {
    let mut items = Vec::new();
    if !dir.exists() { return Ok(items); }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            items.push(load_yaml(&path)?);
        }
    }
    items.sort_by(|a, b| a.entity_id().cmp(b.entity_id()));
    Ok(items)
}

fn find_by_prefix<T: serde::de::DeserializeOwned + HasId + Clone>(
    dir: &Path, prefix: &str, entity_name: &str,
) -> Result<T, AgencyError> {
    let all: Vec<T> = load_all_yaml(dir)?;
    let matches: Vec<&T> = all.iter().filter(|e| e.entity_id().starts_with(prefix)).collect();
    match matches.len() {
        0 => Err(AgencyError::NotFound(format!("No {} matching '{}'", entity_name, prefix))),
        1 => Ok(matches[0].clone()),
        n => {
            let ids: Vec<&str> = matches.iter().map(|e| e.entity_id()).collect();
            Err(AgencyError::Ambiguous(format!(
                "Prefix '{}' matches {} {}s: {}", prefix, n, entity_name, ids.join(", ")
            )))
        }
    }
}

pub trait HasId { fn entity_id(&self) -> &str; }
impl HasId for RoleComponent { fn entity_id(&self) -> &str { &self.id } }
impl HasId for DesiredOutcome { fn entity_id(&self) -> &str { &self.id } }
impl HasId for TradeoffConfig { fn entity_id(&self) -> &str { &self.id } }
impl HasId for Role { fn entity_id(&self) -> &str { &self.id } }
impl HasId for Agent { fn entity_id(&self) -> &str { &self.id } }
impl HasId for Evaluation { fn entity_id(&self) -> &str { &self.id } }
impl HasId for TaskAssignmentRecord { fn entity_id(&self) -> &str { &self.task_id } }

// -- Primitives: RoleComponent
pub fn load_component(path: &Path) -> Result<RoleComponent, AgencyError> { load_yaml(path) }
pub fn save_component(c: &RoleComponent, dir: &Path) -> Result<PathBuf, AgencyError> { save_yaml(c, dir, &c.id) }
pub fn load_all_components(dir: &Path) -> Result<Vec<RoleComponent>, AgencyError> { load_all_yaml(dir) }
pub fn find_component_by_prefix(dir: &Path, prefix: &str) -> Result<RoleComponent, AgencyError> { find_by_prefix(dir, prefix, "component") }

// -- Primitives: DesiredOutcome
pub fn load_outcome(path: &Path) -> Result<DesiredOutcome, AgencyError> { load_yaml(path) }
pub fn save_outcome(o: &DesiredOutcome, dir: &Path) -> Result<PathBuf, AgencyError> { save_yaml(o, dir, &o.id) }
pub fn load_all_outcomes(dir: &Path) -> Result<Vec<DesiredOutcome>, AgencyError> { load_all_yaml(dir) }
pub fn find_outcome_by_prefix(dir: &Path, prefix: &str) -> Result<DesiredOutcome, AgencyError> { find_by_prefix(dir, prefix, "outcome") }

// -- Primitives: TradeoffConfig (formerly Motivation)
pub fn load_tradeoff(path: &Path) -> Result<TradeoffConfig, AgencyError> { load_yaml(path) }
pub fn save_tradeoff(t: &TradeoffConfig, dir: &Path) -> Result<PathBuf, AgencyError> { save_yaml(t, dir, &t.id) }
pub fn load_all_tradeoffs(dir: &Path) -> Result<Vec<TradeoffConfig>, AgencyError> { load_all_yaml(dir) }
pub fn find_tradeoff_by_prefix(dir: &Path, prefix: &str) -> Result<TradeoffConfig, AgencyError> { find_by_prefix(dir, prefix, "tradeoff") }

// -- Cache: Roles
pub fn load_role(path: &Path) -> Result<Role, AgencyError> { load_yaml(path) }
pub fn save_role(role: &Role, dir: &Path) -> Result<PathBuf, AgencyError> { save_yaml(role, dir, &role.id) }
pub fn load_all_roles(dir: &Path) -> Result<Vec<Role>, AgencyError> { load_all_yaml(dir) }
pub fn find_role_by_prefix(dir: &Path, prefix: &str) -> Result<Role, AgencyError> { find_by_prefix(dir, prefix, "role") }

// -- Cache: Agents
pub fn load_agent(path: &Path) -> Result<Agent, AgencyError> { load_yaml(path) }
pub fn save_agent(agent: &Agent, dir: &Path) -> Result<PathBuf, AgencyError> { save_yaml(agent, dir, &agent.id) }
pub fn load_all_agents(dir: &Path) -> Result<Vec<Agent>, AgencyError> { load_all_yaml(dir) }
pub fn find_agent_by_prefix(dir: &Path, prefix: &str) -> Result<Agent, AgencyError> { find_by_prefix(dir, prefix, "agent") }
pub fn load_all_agents_or_warn(dir: &Path) -> Vec<Agent> {
    match load_all_agents(dir) {
        Ok(agents) => agents,
        Err(e) => { eprintln!("Warning: failed to load agents from {}: {}", dir.display(), e); Vec::new() }
    }
}

// -- Evaluations (JSON)
pub fn load_evaluation(path: &Path) -> Result<Evaluation, AgencyError> {
    let contents = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&contents)?)
}
pub fn save_evaluation(eval: &Evaluation, dir: &Path) -> Result<PathBuf, AgencyError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", eval.id));
    fs::write(&path, serde_json::to_string_pretty(eval)?)?;
    Ok(path)
}
pub fn load_all_evaluations(dir: &Path) -> Result<Vec<Evaluation>, AgencyError> {
    let mut evals = Vec::new();
    if !dir.exists() { return Ok(evals); }
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
pub fn load_all_evaluations_or_warn(dir: &Path) -> Vec<Evaluation> {
    match load_all_evaluations(dir) {
        Ok(evals) => evals,
        Err(e) => { eprintln!("Warning: failed to load evaluations from {}: {}", dir.display(), e); Vec::new() }
    }
}


// -- TaskAssignmentRecord (YAML)
pub fn save_assignment_record(record: &TaskAssignmentRecord, dir: &Path) -> Result<PathBuf, AgencyError> {
    save_yaml(record, dir, &record.task_id)
}
pub fn load_assignment_record(path: &Path) -> Result<TaskAssignmentRecord, AgencyError> {
    load_yaml(path)
}
pub fn load_assignment_record_by_task(dir: &Path, task_id: &str) -> Result<TaskAssignmentRecord, AgencyError> {
    let path = dir.join(format!("{}.yaml", task_id));
    if !path.exists() {
        return Err(AgencyError::NotFound(format!("No assignment record for task '{}'", task_id)));
    }
    load_yaml(&path)
}
pub fn load_all_assignment_records(dir: &Path) -> Result<Vec<TaskAssignmentRecord>, AgencyError> {
    load_all_yaml(dir)
}
pub fn count_assignment_records(dir: &Path) -> usize {
    if !dir.is_dir() { return 0; }
    fs::read_dir(dir)
        .map(|e| e.filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("yaml"))
            .count())
        .unwrap_or(0)
}

// -- Store trait & LocalStore
pub trait AgencyStore {
    fn store_path(&self) -> &Path;
    fn load_components(&self) -> Result<Vec<RoleComponent>, AgencyError>;
    fn save_component(&self, c: &RoleComponent) -> Result<PathBuf, AgencyError>;
    fn exists_component(&self, id: &str) -> bool;
    fn load_outcomes(&self) -> Result<Vec<DesiredOutcome>, AgencyError>;
    fn save_outcome(&self, o: &DesiredOutcome) -> Result<PathBuf, AgencyError>;
    fn exists_outcome(&self, id: &str) -> bool;
    fn load_tradeoffs(&self) -> Result<Vec<TradeoffConfig>, AgencyError>;
    fn save_tradeoff(&self, t: &TradeoffConfig) -> Result<PathBuf, AgencyError>;
    fn exists_tradeoff(&self, id: &str) -> bool;
    fn load_roles(&self) -> Result<Vec<Role>, AgencyError>;
    fn save_role(&self, role: &Role) -> Result<PathBuf, AgencyError>;
    fn exists_role(&self, id: &str) -> bool;
    fn load_agents(&self) -> Result<Vec<Agent>, AgencyError>;
    fn save_agent(&self, agent: &Agent) -> Result<PathBuf, AgencyError>;
    fn exists_agent(&self, id: &str) -> bool;
    fn load_evaluations(&self) -> Result<Vec<Evaluation>, AgencyError>;
    fn save_evaluation(&self, eval: &Evaluation) -> Result<PathBuf, AgencyError>;
}

#[derive(Debug, Clone)]
pub struct LocalStore { path: PathBuf }

impl LocalStore {
    pub fn new(path: impl Into<PathBuf>) -> Self { Self { path: path.into() } }
    pub fn components_dir(&self) -> PathBuf { self.path.join("primitives/components") }
    pub fn outcomes_dir(&self) -> PathBuf { self.path.join("primitives/outcomes") }
    pub fn tradeoffs_dir(&self) -> PathBuf { self.path.join("primitives/tradeoffs") }
    pub fn roles_dir(&self) -> PathBuf { self.path.join("cache/roles") }
    pub fn agents_dir(&self) -> PathBuf { self.path.join("cache/agents") }
    pub fn evaluations_dir(&self) -> PathBuf { self.path.join("evaluations") }
    pub fn assignments_dir(&self) -> PathBuf { self.path.join("assignments") }
    pub fn is_valid(&self) -> bool { self.components_dir().is_dir() || self.roles_dir().is_dir() }

    fn count_yaml(dir: &Path) -> usize {
        if !dir.is_dir() { return 0; }
        fs::read_dir(dir).map(|e| e.filter_map(|e| e.ok()).filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("yaml")).count()).unwrap_or(0)
    }
    fn count_json(dir: &Path) -> usize {
        if !dir.is_dir() { return 0; }
        fs::read_dir(dir).map(|e| e.filter_map(|e| e.ok()).filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json")).count()).unwrap_or(0)
    }
    pub fn entity_counts(&self) -> StoreCounts {
        StoreCounts {
            components: Self::count_yaml(&self.components_dir()),
            outcomes: Self::count_yaml(&self.outcomes_dir()),
            tradeoffs: Self::count_yaml(&self.tradeoffs_dir()),
            roles: Self::count_yaml(&self.roles_dir()),
            agents: Self::count_yaml(&self.agents_dir()),
            evaluations: Self::count_json(&self.evaluations_dir()),
        }
    }
}

impl AgencyStore for LocalStore {
    fn store_path(&self) -> &Path { &self.path }
    fn load_components(&self) -> Result<Vec<RoleComponent>, AgencyError> { load_all_components(&self.components_dir()) }
    fn save_component(&self, c: &RoleComponent) -> Result<PathBuf, AgencyError> { save_component(c, &self.components_dir()) }
    fn exists_component(&self, id: &str) -> bool { self.components_dir().join(format!("{}.yaml", id)).exists() }
    fn load_outcomes(&self) -> Result<Vec<DesiredOutcome>, AgencyError> { load_all_outcomes(&self.outcomes_dir()) }
    fn save_outcome(&self, o: &DesiredOutcome) -> Result<PathBuf, AgencyError> { save_outcome(o, &self.outcomes_dir()) }
    fn exists_outcome(&self, id: &str) -> bool { self.outcomes_dir().join(format!("{}.yaml", id)).exists() }
    fn load_tradeoffs(&self) -> Result<Vec<TradeoffConfig>, AgencyError> { load_all_tradeoffs(&self.tradeoffs_dir()) }
    fn save_tradeoff(&self, t: &TradeoffConfig) -> Result<PathBuf, AgencyError> { save_tradeoff(t, &self.tradeoffs_dir()) }
    fn exists_tradeoff(&self, id: &str) -> bool { self.tradeoffs_dir().join(format!("{}.yaml", id)).exists() }
    fn load_roles(&self) -> Result<Vec<Role>, AgencyError> { load_all_roles(&self.roles_dir()) }
    fn save_role(&self, role: &Role) -> Result<PathBuf, AgencyError> { save_role(role, &self.roles_dir()) }
    fn exists_role(&self, id: &str) -> bool { self.roles_dir().join(format!("{}.yaml", id)).exists() }
    fn load_agents(&self) -> Result<Vec<Agent>, AgencyError> { load_all_agents(&self.agents_dir()) }
    fn save_agent(&self, agent: &Agent) -> Result<PathBuf, AgencyError> { save_agent(agent, &self.agents_dir()) }
    fn exists_agent(&self, id: &str) -> bool { self.agents_dir().join(format!("{}.yaml", id)).exists() }
    fn load_evaluations(&self) -> Result<Vec<Evaluation>, AgencyError> { load_all_evaluations(&self.evaluations_dir()) }
    fn save_evaluation(&self, eval: &Evaluation) -> Result<PathBuf, AgencyError> { save_evaluation(eval, &self.evaluations_dir()) }
}

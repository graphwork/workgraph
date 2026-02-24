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
    fs::create_dir_all(dir)?;
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
    fs::create_dir_all(dir)?;
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
    fs::create_dir_all(dir)?;
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

/// Load all evaluations, falling back to empty with a warning on errors.
///
/// Unlike `.load_all_evaluations().unwrap_or_default()`, this emits a stderr
/// warning when the evaluations directory exists but contains corrupt data.
pub fn load_all_evaluations_or_warn(dir: &Path) -> Vec<Evaluation> {
    match load_all_evaluations(dir) {
        Ok(evals) => evals,
        Err(e) => {
            eprintln!(
                "Warning: failed to load evaluations from {}: {}",
                dir.display(),
                e
            );
            Vec::new()
        }
    }
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

/// Load all agents, falling back to empty with a warning on errors.
///
/// Unlike `.load_all_agents().unwrap_or_default()`, this emits a stderr
/// warning when the agents directory exists but contains corrupt data.
pub fn load_all_agents_or_warn(dir: &Path) -> Vec<Agent> {
    match load_all_agents(dir) {
        Ok(agents) => agents,
        Err(e) => {
            eprintln!(
                "Warning: failed to load agents from {}: {}",
                dir.display(),
                e
            );
            Vec::new()
        }
    }
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
// Agency Store trait
// ---------------------------------------------------------------------------

/// An agency store that can load and save entities.
///
/// Abstracts over different storage backends (local filesystem, git, HTTP).
/// The initial implementation is `LocalStore` which reads/writes YAML/JSON files.
pub trait AgencyStore {
    /// The root path of this store (e.g. `.workgraph/agency/` or a bare `agency/` dir).
    fn store_path(&self) -> &Path;

    fn load_roles(&self) -> Result<Vec<Role>, AgencyError>;
    fn load_motivations(&self) -> Result<Vec<Motivation>, AgencyError>;
    fn load_agents(&self) -> Result<Vec<Agent>, AgencyError>;
    fn load_evaluations(&self) -> Result<Vec<Evaluation>, AgencyError>;

    fn save_role(&self, role: &Role) -> Result<PathBuf, AgencyError>;
    fn save_motivation(&self, motivation: &Motivation) -> Result<PathBuf, AgencyError>;
    fn save_agent(&self, agent: &Agent) -> Result<PathBuf, AgencyError>;
    fn save_evaluation(&self, eval: &Evaluation) -> Result<PathBuf, AgencyError>;

    fn exists_role(&self, id: &str) -> bool;
    fn exists_motivation(&self, id: &str) -> bool;
    fn exists_agent(&self, id: &str) -> bool;
}

/// A local filesystem-backed agency store.
///
/// Wraps an agency directory path and delegates to the existing free functions.
#[derive(Debug, Clone)]
pub struct LocalStore {
    /// Root of the agency store (the directory containing roles/, motivations/, agents/, evaluations/).
    path: PathBuf,
}

impl LocalStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn roles_dir(&self) -> PathBuf {
        self.path.join("roles")
    }

    pub fn motivations_dir(&self) -> PathBuf {
        self.path.join("motivations")
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.path.join("agents")
    }

    pub fn evaluations_dir(&self) -> PathBuf {
        self.path.join("evaluations")
    }

    /// Returns true if this looks like a valid agency store
    /// (has at least a roles/ subdirectory).
    pub fn is_valid(&self) -> bool {
        self.roles_dir().is_dir()
    }

    /// Count YAML files in a subdirectory.
    fn count_yaml(dir: &Path) -> usize {
        if !dir.is_dir() {
            return 0;
        }
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .and_then(|ext| ext.to_str())
                            == Some("yaml")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// Count JSON files in a subdirectory.
    fn count_json(dir: &Path) -> usize {
        if !dir.is_dir() {
            return 0;
        }
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .and_then(|ext| ext.to_str())
                            == Some("json")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// Quick entity counts without fully parsing files.
    pub fn entity_counts(&self) -> StoreCounts {
        StoreCounts {
            roles: Self::count_yaml(&self.roles_dir()),
            motivations: Self::count_yaml(&self.motivations_dir()),
            agents: Self::count_yaml(&self.agents_dir()),
            evaluations: Self::count_json(&self.evaluations_dir()),
        }
    }
}

impl AgencyStore for LocalStore {
    fn store_path(&self) -> &Path {
        &self.path
    }

    fn load_roles(&self) -> Result<Vec<Role>, AgencyError> {
        load_all_roles(&self.roles_dir())
    }

    fn load_motivations(&self) -> Result<Vec<Motivation>, AgencyError> {
        load_all_motivations(&self.motivations_dir())
    }

    fn load_agents(&self) -> Result<Vec<Agent>, AgencyError> {
        load_all_agents(&self.agents_dir())
    }

    fn load_evaluations(&self) -> Result<Vec<Evaluation>, AgencyError> {
        load_all_evaluations(&self.evaluations_dir())
    }

    fn save_role(&self, role: &Role) -> Result<PathBuf, AgencyError> {
        save_role(role, &self.roles_dir())
    }

    fn save_motivation(&self, motivation: &Motivation) -> Result<PathBuf, AgencyError> {
        save_motivation(motivation, &self.motivations_dir())
    }

    fn save_agent(&self, agent: &Agent) -> Result<PathBuf, AgencyError> {
        save_agent(agent, &self.agents_dir())
    }

    fn save_evaluation(&self, eval: &Evaluation) -> Result<PathBuf, AgencyError> {
        save_evaluation(eval, &self.evaluations_dir())
    }

    fn exists_role(&self, id: &str) -> bool {
        self.roles_dir().join(format!("{}.yaml", id)).exists()
    }

    fn exists_motivation(&self, id: &str) -> bool {
        self.motivations_dir()
            .join(format!("{}.yaml", id))
            .exists()
    }

    fn exists_agent(&self, id: &str) -> bool {
        self.agents_dir().join(format!("{}.yaml", id)).exists()
    }
}

//! Federation: shared logic for transferring agency entities between stores.
//!
//! Both `wg agency pull` and `wg agency push` are thin wrappers around `transfer()`,
//! differing only in which store is source vs. target.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::agency::{
    Agent, AgencyStore, EvaluationRef, Lineage, LocalStore, Motivation, PerformanceRecord, Role,
};

// ---------------------------------------------------------------------------
// Federation config: named remotes stored in .workgraph/federation.yaml
// ---------------------------------------------------------------------------

/// A named remote agency store reference.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Remote {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<String>,
}

/// A peer workgraph instance (another repo with its own .workgraph/).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerConfig {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Top-level federation.yaml structure.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FederationConfig {
    #[serde(default)]
    pub remotes: BTreeMap<String, Remote>,
    /// Peer workgraph instances for cross-repo communication.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peers: BTreeMap<String, PeerConfig>,
}

/// Load federation config from .workgraph/federation.yaml.
/// Returns default (empty) if the file doesn't exist.
pub fn load_federation_config(workgraph_dir: &Path) -> Result<FederationConfig, anyhow::Error> {
    let path = workgraph_dir.join("federation.yaml");
    if !path.exists() {
        return Ok(FederationConfig::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let config: FederationConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

/// Save federation config to .workgraph/federation.yaml.
pub fn save_federation_config(
    workgraph_dir: &Path,
    config: &FederationConfig,
) -> Result<(), anyhow::Error> {
    let path = workgraph_dir.join("federation.yaml");
    let content = serde_yaml::to_string(config)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Resolve a store reference, checking named remotes in federation.yaml first,
/// then falling back to filesystem path resolution.
pub fn resolve_store_with_remotes(
    reference: &str,
    workgraph_dir: &Path,
) -> Result<LocalStore, anyhow::Error> {
    let config = load_federation_config(workgraph_dir)?;
    if let Some(remote) = config.remotes.get(reference) {
        return resolve_store(&remote.path);
    }
    resolve_store(reference)
}

/// Update the last_sync timestamp for a named remote (if it exists).
pub fn touch_remote_sync(workgraph_dir: &Path, name: &str) -> Result<(), anyhow::Error> {
    let mut config = load_federation_config(workgraph_dir)?;
    if let Some(remote) = config.remotes.get_mut(name) {
        remote.last_sync = Some(chrono::Utc::now().to_rfc3339());
        save_federation_config(workgraph_dir, &config)?;
    }
    Ok(())
}

/// Which entity types to transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityFilter {
    All,
    Roles,
    Motivations,
    Agents,
}

/// Options controlling a transfer operation.
#[derive(Debug, Clone)]
pub struct TransferOptions {
    /// Only preview, don't write.
    pub dry_run: bool,
    /// Skip merging performance data.
    pub no_performance: bool,
    /// Skip copying evaluation JSON files.
    pub no_evaluations: bool,
    /// Overwrite target metadata instead of merging.
    pub force: bool,
    /// Only transfer specific entity IDs.
    pub entity_ids: Vec<String>,
    /// Filter by entity type.
    pub entity_filter: EntityFilter,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            no_performance: false,
            no_evaluations: false,
            force: false,
            entity_ids: Vec::new(),
            entity_filter: EntityFilter::All,
        }
    }
}

/// Summary of what was transferred.
#[derive(Debug, Clone, Default)]
pub struct TransferSummary {
    pub roles_added: usize,
    pub roles_updated: usize,
    pub roles_skipped: usize,
    pub motivations_added: usize,
    pub motivations_updated: usize,
    pub motivations_skipped: usize,
    pub agents_added: usize,
    pub agents_updated: usize,
    pub agents_skipped: usize,
    pub evaluations_added: usize,
    pub evaluations_skipped: usize,
}

impl std::fmt::Display for TransferSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "  Roles:        +{} new, {} updated, {} skipped",
            self.roles_added, self.roles_updated, self.roles_skipped
        )?;
        writeln!(
            f,
            "  Motivations:  +{} new, {} updated, {} skipped",
            self.motivations_added, self.motivations_updated, self.motivations_skipped
        )?;
        writeln!(
            f,
            "  Agents:       +{} new, {} updated, {} skipped",
            self.agents_added, self.agents_updated, self.agents_skipped
        )?;
        write!(
            f,
            "  Evaluations:  +{} new, {} skipped",
            self.evaluations_added, self.evaluations_skipped
        )
    }
}

/// Resolve a store reference string to a `LocalStore`.
///
/// Resolution order (per §3.1 of the design doc):
/// 1. Absolute path or `~/` → filesystem path, look for `agency/` or `.workgraph/agency/`
/// 2. Relative path → resolve from CWD
///
/// Named remotes (from `.workgraph/federation.yaml`) are a future extension.
pub fn resolve_store(reference: &str) -> Result<LocalStore, anyhow::Error> {
    let expanded = if let Some(suffix) = reference.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
        home.join(suffix)
    } else {
        PathBuf::from(reference)
    };

    let path = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };

    // Canonicalize if it exists
    let path = path.canonicalize().unwrap_or(path);

    // Check for agency store in several locations:
    // 1. path itself has roles/ (it IS the agency dir)
    // 2. path/agency/ has roles/ (bare store)
    // 3. path/.workgraph/agency/ has roles/ (project store)
    if path.join("roles").is_dir() {
        return Ok(LocalStore::new(path));
    }
    let agency_sub = path.join("agency");
    if agency_sub.join("roles").is_dir() {
        return Ok(LocalStore::new(agency_sub));
    }
    let wg_agency = path.join(".workgraph").join("agency");
    if wg_agency.join("roles").is_dir() {
        return Ok(LocalStore::new(wg_agency));
    }

    // Target doesn't exist yet — return the best-guess path.
    // For push, we create it. For pull, the caller can error.
    // Prefer .workgraph/agency if parent looks like a project dir.
    if path.join(".workgraph").is_dir() {
        Ok(LocalStore::new(wg_agency))
    } else if path.join("agency").is_dir() || path.file_name().map(|n| n != "agency").unwrap_or(true) {
        // If path/agency exists but has no roles, or if path is not named "agency",
        // assume we want path/agency/ (bare store convention).
        // But if the path itself ends in "agency", use it directly.
        if path.file_name().map(|n| n == "agency").unwrap_or(false) {
            Ok(LocalStore::new(path))
        } else {
            Ok(LocalStore::new(agency_sub))
        }
    } else {
        Ok(LocalStore::new(path))
    }
}

// ---------------------------------------------------------------------------
// Peer resolution: name → path → .workgraph dir → socket discovery
// ---------------------------------------------------------------------------

/// Result of resolving a peer reference to a concrete path.
#[derive(Debug, Clone)]
pub struct ResolvedPeer {
    /// The project root (parent of .workgraph/).
    pub project_path: PathBuf,
    /// The .workgraph directory.
    pub workgraph_dir: PathBuf,
}

/// Resolve a peer reference string to a concrete path.
///
/// Resolution order (per §2.3 of cross-repo design doc):
/// 1. Named peer in federation.yaml → look up `path`
/// 2. Absolute path or `~/` → filesystem path
/// 3. Relative path → resolve from CWD
pub fn resolve_peer(
    reference: &str,
    workgraph_dir: &Path,
) -> Result<ResolvedPeer, anyhow::Error> {
    let config = load_federation_config(workgraph_dir)?;

    // Check named peers first
    let raw_path = if let Some(peer) = config.peers.get(reference) {
        peer.path.clone()
    } else {
        reference.to_string()
    };

    // Expand ~/
    let expanded = if let Some(suffix) = raw_path.strip_prefix("~/") {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
        home.join(suffix)
    } else {
        PathBuf::from(&raw_path)
    };

    // Make absolute
    let project_path = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };

    // Canonicalize if possible
    let project_path = project_path.canonicalize().unwrap_or(project_path);

    let wg_dir = project_path.join(".workgraph");
    if !wg_dir.is_dir() {
        anyhow::bail!(
            "No .workgraph directory found at '{}'. Is this a workgraph project?",
            project_path.display()
        );
    }

    Ok(ResolvedPeer {
        project_path,
        workgraph_dir: wg_dir,
    })
}

/// Peer service status information.
#[derive(Debug, Clone)]
pub struct PeerServiceStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub socket_path: Option<String>,
    pub started_at: Option<String>,
}

/// Check whether a peer's workgraph service is running.
///
/// Reads `<workgraph_dir>/service/state.json` and checks if the PID is alive.
pub fn check_peer_service(workgraph_dir: &Path) -> PeerServiceStatus {
    let state_path = workgraph_dir.join("service").join("state.json");
    if !state_path.exists() {
        return PeerServiceStatus {
            running: false,
            pid: None,
            socket_path: None,
            started_at: None,
        };
    }

    let content = match std::fs::read_to_string(&state_path) {
        Ok(c) => c,
        Err(_) => {
            return PeerServiceStatus {
                running: false,
                pid: None,
                socket_path: None,
                started_at: None,
            }
        }
    };

    #[derive(serde::Deserialize)]
    struct StateJson {
        pid: u32,
        socket_path: String,
        #[serde(default)]
        started_at: Option<String>,
    }

    let state: StateJson = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => {
            return PeerServiceStatus {
                running: false,
                pid: None,
                socket_path: None,
                started_at: None,
            }
        }
    };

    let alive = is_pid_alive(state.pid);

    PeerServiceStatus {
        running: alive,
        pid: Some(state.pid),
        socket_path: Some(state.socket_path),
        started_at: state.started_at,
    }
}

/// Check if a process with the given PID is alive.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    true
}

// ---------------------------------------------------------------------------
// Cross-repo dependency resolution
// ---------------------------------------------------------------------------

/// Parse a remote reference string like "peer:task-id" into (peer_name, task_id).
///
/// Returns `None` if the string doesn't contain a colon or looks like
/// a local reference. Local task IDs are slug-based (lowercase alphanumeric +
/// dashes, no colons), so the colon delimiter is unambiguous.
pub fn parse_remote_ref(dep: &str) -> Option<(&str, &str)> {
    // Split on the first colon only
    let (peer, task_id) = dep.split_once(':')?;
    // Both parts must be non-empty
    if peer.is_empty() || task_id.is_empty() {
        return None;
    }
    Some((peer, task_id))
}

/// The status of a remote task, resolved via IPC or direct file access.
#[derive(Debug, Clone)]
pub struct RemoteTaskStatus {
    pub task_id: String,
    pub status: crate::graph::Status,
    pub title: Option<String>,
    pub assigned: Option<String>,
    /// How the status was resolved
    pub resolution: RemoteResolution,
}

/// How the remote task status was resolved.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteResolution {
    /// Resolved via IPC to a running peer service
    Ipc,
    /// Resolved by directly reading the peer's graph.jsonl
    DirectFileAccess,
    /// Could not resolve — peer not found or inaccessible
    Unreachable(String),
}

/// Resolve the status of a task in a remote peer workgraph.
///
/// Resolution order (per §4.4 of cross-repo design doc):
/// 1. Look up peer path from federation config or parse as path
/// 2. Check if peer service is running
/// 3. If running: query via IPC
/// 4. If not running: load peer's graph.jsonl directly
/// 5. If peer not found: return error
pub fn resolve_remote_task_status(
    peer_name: &str,
    task_id: &str,
    local_workgraph_dir: &Path,
) -> RemoteTaskStatus {
    // Try to resolve the peer
    let resolved = match resolve_peer(peer_name, local_workgraph_dir) {
        Ok(r) => r,
        Err(e) => {
            return RemoteTaskStatus {
                task_id: task_id.to_string(),
                status: crate::graph::Status::Open, // treat as blocking
                title: None,
                assigned: None,
                resolution: RemoteResolution::Unreachable(format!(
                    "Cannot resolve peer '{}': {}",
                    peer_name, e
                )),
            };
        }
    };

    // Check if the peer's service is running
    let service_status = check_peer_service(&resolved.workgraph_dir);

    if service_status.running
        && let Some(socket_path) = &service_status.socket_path {
            // Try IPC first
            match query_task_via_ipc(socket_path, task_id) {
                Ok(status) => return status,
                Err(_) => {
                    // Fall through to direct file access
                }
            }
        }

    // Fall back to direct graph file read
    let graph_path = resolved.workgraph_dir.join("graph.jsonl");
    if !graph_path.exists() {
        return RemoteTaskStatus {
            task_id: task_id.to_string(),
            status: crate::graph::Status::Open,
            title: None,
            assigned: None,
            resolution: RemoteResolution::Unreachable(format!(
                "No graph.jsonl at peer '{}'",
                peer_name
            )),
        };
    }

    match crate::parser::load_graph(&graph_path) {
        Ok(graph) => match graph.get_task(task_id) {
            Some(task) => RemoteTaskStatus {
                task_id: task.id.clone(),
                status: task.status,
                title: Some(task.title.clone()),
                assigned: task.assigned.clone(),
                resolution: RemoteResolution::DirectFileAccess,
            },
            None => RemoteTaskStatus {
                task_id: task_id.to_string(),
                status: crate::graph::Status::Open,
                title: None,
                assigned: None,
                resolution: RemoteResolution::Unreachable(format!(
                    "Task '{}' not found in peer '{}'",
                    task_id, peer_name
                )),
            },
        },
        Err(e) => RemoteTaskStatus {
            task_id: task_id.to_string(),
            status: crate::graph::Status::Open,
            title: None,
            assigned: None,
            resolution: RemoteResolution::Unreachable(format!(
                "Failed to load peer '{}' graph: {}",
                peer_name, e
            )),
        },
    }
}

/// Query a task's status via IPC to a running peer service.
#[cfg(unix)]
fn query_task_via_ipc(
    socket_path: &str,
    task_id: &str,
) -> Result<RemoteTaskStatus, anyhow::Error> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let request = serde_json::json!({
        "QueryTask": { "task_id": task_id }
    });
    writeln!(stream, "{}", request)?;
    stream.flush()?;

    let reader = BufReader::new(&stream);
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let resp: serde_json::Value = serde_json::from_str(&line)?;
        if resp.get("ok") == Some(&serde_json::Value::Bool(true)) {
            let status_str = resp
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("Open");
            let status = parse_status_string(status_str);
            return Ok(RemoteTaskStatus {
                task_id: task_id.to_string(),
                status,
                title: resp.get("title").and_then(|v| v.as_str()).map(String::from),
                assigned: resp
                    .get("assigned")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                resolution: RemoteResolution::Ipc,
            });
        } else {
            let err_msg = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown error");
            anyhow::bail!("IPC error: {}", err_msg);
        }
    }
    anyhow::bail!("No response from peer service")
}

#[cfg(not(unix))]
fn query_task_via_ipc(
    _socket_path: &str,
    _task_id: &str,
) -> Result<RemoteTaskStatus, anyhow::Error> {
    anyhow::bail!("IPC is only supported on Unix systems")
}

/// Parse a status string (from IPC response) into a Status enum.
fn parse_status_string(s: &str) -> crate::graph::Status {
    match s.to_lowercase().as_str() {
        "done" => crate::graph::Status::Done,
        "open" => crate::graph::Status::Open,
        "inprogress" | "in-progress" => crate::graph::Status::InProgress,
        "failed" => crate::graph::Status::Failed,
        "abandoned" => crate::graph::Status::Abandoned,
        "blocked" => crate::graph::Status::Blocked,
        _ => crate::graph::Status::Open,
    }
}

/// Ensure the target store directory structure exists.
pub fn ensure_store_dirs(store: &LocalStore) -> Result<(), anyhow::Error> {
    crate::agency::init(store.store_path())?;
    Ok(())
}

/// Transfer entities from `source` to `target`.
///
/// This is the core operation used by both pull (remote→local) and push (local→remote).
pub fn transfer(
    source: &LocalStore,
    target: &LocalStore,
    opts: &TransferOptions,
) -> Result<TransferSummary, anyhow::Error> {
    let mut summary = TransferSummary::default();

    if !opts.dry_run {
        ensure_store_dirs(target)?;
    }

    let has_filter = !opts.entity_ids.is_empty();
    let matches_filter = |id: &str| -> bool {
        opts.entity_ids.iter().any(|prefix| id.starts_with(prefix.as_str()))
    };

    // Load source entities as needed
    let source_roles = if matches!(opts.entity_filter, EntityFilter::All | EntityFilter::Roles | EntityFilter::Agents) {
        source.load_roles().unwrap_or_default()
    } else {
        Vec::new()
    };
    let source_motivations = if matches!(opts.entity_filter, EntityFilter::All | EntityFilter::Motivations | EntityFilter::Agents) {
        source.load_motivations().unwrap_or_default()
    } else {
        Vec::new()
    };
    let source_agents = if matches!(opts.entity_filter, EntityFilter::All | EntityFilter::Agents) {
        source.load_agents().unwrap_or_default()
    } else {
        Vec::new()
    };

    // Build lookup maps
    let role_map: HashMap<String, &Role> = source_roles.iter().map(|r| (r.id.clone(), r)).collect();
    let motivation_map: HashMap<String, &Motivation> =
        source_motivations.iter().map(|m| (m.id.clone(), m)).collect();

    // Determine which entities to transfer (with referential integrity for agents)
    let mut roles_to_transfer: Vec<&Role> = Vec::new();
    let mut motivations_to_transfer: Vec<&Motivation> = Vec::new();
    let mut agents_to_transfer: Vec<&Agent> = Vec::new();

    // Collect agents
    if matches!(opts.entity_filter, EntityFilter::All | EntityFilter::Agents) {
        for agent in &source_agents {
            if has_filter && !matches_filter(&agent.id) {
                continue;
            }
            agents_to_transfer.push(agent);
        }
    }

    // Collect directly-requested roles and motivations
    if matches!(opts.entity_filter, EntityFilter::All | EntityFilter::Roles) {
        for role in &source_roles {
            if has_filter && !matches_filter(&role.id) {
                continue;
            }
            roles_to_transfer.push(role);
        }
    }
    if matches!(opts.entity_filter, EntityFilter::All | EntityFilter::Motivations) {
        for motivation in &source_motivations {
            if has_filter && !matches_filter(&motivation.id) {
                continue;
            }
            motivations_to_transfer.push(motivation);
        }
    }

    // Load target entities ONCE for O(1) lookups during both referential
    // integrity checks and the transfer phase (avoids repeated filesystem reads).
    // Errors are propagated — if target YAML is corrupt, we must not silently
    // treat it as empty (which would cause overwrites instead of merges).
    let target_role_map: HashMap<String, Role> = target
        .load_roles()?
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
    let target_motivation_map: HashMap<String, Motivation> = target
        .load_motivations()?
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();
    let target_agent_map: HashMap<String, Agent> = target
        .load_agents()?
        .into_iter()
        .map(|a| (a.id.clone(), a))
        .collect();

    // Referential integrity: when transferring agents, also transfer their
    // referenced roles and motivations if not already in the target.
    let mut dep_role_ids: HashSet<String> = HashSet::new();
    let mut dep_motivation_ids: HashSet<String> = HashSet::new();
    for agent in &agents_to_transfer {
        if !target_role_map.contains_key(&agent.role_id) {
            dep_role_ids.insert(agent.role_id.clone());
        }
        if !target_motivation_map.contains_key(&agent.motivation_id) {
            dep_motivation_ids.insert(agent.motivation_id.clone());
        }
    }
    // Add dependency roles not already in the transfer set.
    // Per §7.1: if a dependency doesn't exist in source, that's a broken agent — error out.
    let existing_role_ids: HashSet<String> = roles_to_transfer.iter().map(|r| r.id.clone()).collect();
    for dep_id in &dep_role_ids {
        if !existing_role_ids.contains(dep_id) {
            if let Some(role) = role_map.get(dep_id) {
                roles_to_transfer.push(role);
            } else {
                return Err(anyhow::anyhow!(
                    "Agent references role '{}' which does not exist in source store \
                     (broken referential integrity, see design doc §7.1)",
                    dep_id
                ));
            }
        }
    }
    let existing_motivation_ids: HashSet<String> =
        motivations_to_transfer.iter().map(|m| m.id.clone()).collect();
    for dep_id in &dep_motivation_ids {
        if !existing_motivation_ids.contains(dep_id) {
            if let Some(motivation) = motivation_map.get(dep_id) {
                motivations_to_transfer.push(motivation);
            } else {
                return Err(anyhow::anyhow!(
                    "Agent references motivation '{}' which does not exist in source store \
                     (broken referential integrity, see design doc §7.1)",
                    dep_id
                ));
            }
        }
    }

    // Transfer roles
    for role in &roles_to_transfer {
        if let Some(existing) = target_role_map.get(&role.id) {
            // Entity exists — check if metadata differs and merge
            if opts.force || opts.no_performance {
                let mut merged = (*role).clone();
                if opts.no_performance {
                    merged.performance = existing.performance.clone();
                }
                if !opts.dry_run {
                    target.save_role(&merged)?;
                }
                summary.roles_updated += 1;
            } else {
                // Merge metadata
                let merged = merge_role(existing, role);
                if merged_role_differs(existing, &merged) {
                    if !opts.dry_run {
                        target.save_role(&merged)?;
                    }
                    summary.roles_updated += 1;
                } else {
                    summary.roles_skipped += 1;
                }
            }
        } else {
            let mut to_save = (*role).clone();
            if opts.no_performance {
                to_save.performance = PerformanceRecord::default();
            }
            if !opts.dry_run {
                target.save_role(&to_save)?;
            }
            summary.roles_added += 1;
        }
    }

    // Transfer motivations
    for motivation in &motivations_to_transfer {
        if let Some(existing) = target_motivation_map.get(&motivation.id) {
            if opts.force || opts.no_performance {
                let mut merged = (*motivation).clone();
                if opts.no_performance {
                    merged.performance = existing.performance.clone();
                }
                if !opts.dry_run {
                    target.save_motivation(&merged)?;
                }
                summary.motivations_updated += 1;
            } else {
                let merged = merge_motivation(existing, motivation);
                if merged_motivation_differs(existing, &merged) {
                    if !opts.dry_run {
                        target.save_motivation(&merged)?;
                    }
                    summary.motivations_updated += 1;
                } else {
                    summary.motivations_skipped += 1;
                }
            }
        } else {
            let mut to_save = (*motivation).clone();
            if opts.no_performance {
                to_save.performance = PerformanceRecord::default();
            }
            if !opts.dry_run {
                target.save_motivation(&to_save)?;
            }
            summary.motivations_added += 1;
        }
    }

    // Transfer agents
    for agent in &agents_to_transfer {
        if let Some(existing) = target_agent_map.get(&agent.id) {
            if opts.force || opts.no_performance {
                let mut merged = (*agent).clone();
                if opts.no_performance {
                    merged.performance = existing.performance.clone();
                }
                if !opts.dry_run {
                    target.save_agent(&merged)?;
                }
                summary.agents_updated += 1;
            } else {
                let merged = merge_agent(existing, agent);
                if merged_agent_differs(existing, &merged) {
                    if !opts.dry_run {
                        target.save_agent(&merged)?;
                    }
                    summary.agents_updated += 1;
                } else {
                    summary.agents_skipped += 1;
                }
            }
        } else {
            let mut to_save = (*agent).clone();
            if opts.no_performance {
                to_save.performance = PerformanceRecord::default();
            }
            if !opts.dry_run {
                target.save_agent(&to_save)?;
            }
            summary.agents_added += 1;
        }
    }

    // Transfer evaluations
    if !opts.no_evaluations && matches!(opts.entity_filter, EntityFilter::All | EntityFilter::Agents) {
        let source_evals = source.load_evaluations().unwrap_or_default();
        let target_evals: HashSet<String> = target
            .load_evaluations()
            .unwrap_or_default()
            .iter()
            .map(|e| e.id.clone())
            .collect();

        // Build filter sets ONCE outside the eval loop
        let eval_agent_ids: HashSet<&String> = agents_to_transfer.iter().map(|a| &a.id).collect();
        let eval_role_ids: HashSet<&String> = roles_to_transfer.iter().map(|r| &r.id).collect();
        let eval_motivation_ids: HashSet<&String> =
            motivations_to_transfer.iter().map(|m| &m.id).collect();

        for eval in &source_evals {
            // If filtering by entity, only transfer evals for transferred agents/roles/motivations
            if has_filter {
                let relevant = eval_agent_ids.contains(&eval.agent_id)
                    || eval_role_ids.contains(&eval.role_id)
                    || eval_motivation_ids.contains(&eval.motivation_id);
                if !relevant {
                    continue;
                }
            }

            if target_evals.contains(&eval.id) {
                summary.evaluations_skipped += 1;
            } else {
                if !opts.dry_run {
                    target.save_evaluation(eval)?;
                }
                summary.evaluations_added += 1;
            }
        }
    }

    Ok(summary)
}

// ---------------------------------------------------------------------------
// Metadata merge helpers (§6 of design doc)
// ---------------------------------------------------------------------------

/// Merge performance records: union evaluation refs by (task_id, timestamp), recalculate stats.
fn merge_performance(target: &PerformanceRecord, source: &PerformanceRecord) -> PerformanceRecord {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut merged_evals: Vec<EvaluationRef> = Vec::new();

    for eval in target.evaluations.iter().chain(source.evaluations.iter()) {
        let key = (eval.task_id.clone(), eval.timestamp.clone());
        if seen.insert(key) {
            merged_evals.push(eval.clone());
        }
    }

    let task_count = merged_evals.len() as u32;
    let avg_score = if merged_evals.is_empty() {
        None
    } else {
        let sum: f64 = merged_evals.iter().map(|e| e.score).sum();
        Some(sum / merged_evals.len() as f64)
    };

    PerformanceRecord {
        task_count,
        avg_score,
        evaluations: merged_evals,
    }
}

/// Merge lineage: prefer richer lineage (more parent_ids, higher generation).
fn merge_lineage(target: &Lineage, source: &Lineage) -> Lineage {
    if source.parent_ids.len() > target.parent_ids.len() {
        source.clone()
    } else if target.parent_ids.len() > source.parent_ids.len() {
        target.clone()
    } else if source.generation > target.generation {
        source.clone()
    } else {
        // Equal or target wins (keep local/target)
        target.clone()
    }
}

/// Merge a role: target name wins, performance is unioned, lineage prefers richer.
fn merge_role(target: &Role, source: &Role) -> Role {
    Role {
        id: target.id.clone(),
        name: target.name.clone(), // keep target name
        description: target.description.clone(),
        skills: target.skills.clone(),
        desired_outcome: target.desired_outcome.clone(),
        performance: merge_performance(&target.performance, &source.performance),
        lineage: merge_lineage(&target.lineage, &source.lineage),
    }
}

/// Merge a motivation: target name wins, performance is unioned, lineage prefers richer.
fn merge_motivation(target: &Motivation, source: &Motivation) -> Motivation {
    Motivation {
        id: target.id.clone(),
        name: target.name.clone(),
        description: target.description.clone(),
        acceptable_tradeoffs: target.acceptable_tradeoffs.clone(),
        unacceptable_tradeoffs: target.unacceptable_tradeoffs.clone(),
        performance: merge_performance(&target.performance, &source.performance),
        lineage: merge_lineage(&target.lineage, &source.lineage),
    }
}

/// Merge an agent: target name wins, performance is unioned, lineage prefers richer.
fn merge_agent(target: &Agent, source: &Agent) -> Agent {
    Agent {
        id: target.id.clone(),
        role_id: target.role_id.clone(),
        motivation_id: target.motivation_id.clone(),
        name: target.name.clone(),
        performance: merge_performance(&target.performance, &source.performance),
        lineage: merge_lineage(&target.lineage, &source.lineage),
        capabilities: target.capabilities.clone(),
        rate: target.rate,
        capacity: target.capacity,
        trust_level: target.trust_level.clone(),
        contact: target.contact.clone(),
        executor: target.executor.clone(),
    }
}

/// Check if merged role has different metadata from original.
fn merged_role_differs(original: &Role, merged: &Role) -> bool {
    original.performance.task_count != merged.performance.task_count
        || original.performance.evaluations.len() != merged.performance.evaluations.len()
        || original.lineage.generation != merged.lineage.generation
        || original.lineage.parent_ids.len() != merged.lineage.parent_ids.len()
}

/// Check if merged motivation has different metadata from original.
fn merged_motivation_differs(original: &Motivation, merged: &Motivation) -> bool {
    original.performance.task_count != merged.performance.task_count
        || original.performance.evaluations.len() != merged.performance.evaluations.len()
        || original.lineage.generation != merged.lineage.generation
        || original.lineage.parent_ids.len() != merged.lineage.parent_ids.len()
}

/// Check if merged agent has different metadata from original.
fn merged_agent_differs(original: &Agent, merged: &Agent) -> bool {
    original.performance.task_count != merged.performance.task_count
        || original.performance.evaluations.len() != merged.performance.evaluations.len()
        || original.lineage.generation != merged.lineage.generation
        || original.lineage.parent_ids.len() != merged.lineage.parent_ids.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agency::{self, EvaluationRef, Lineage, PerformanceRecord};
    use tempfile::TempDir;

    fn setup_store(tmp: &TempDir, name: &str) -> LocalStore {
        let path = tmp.path().join(name).join("agency");
        agency::init(&path).unwrap();
        LocalStore::new(path)
    }

    fn make_role(id: &str, name: &str) -> Role {
        Role {
            id: id.to_string(),
            name: name.to_string(),
            description: "test role".to_string(),
            skills: Vec::new(),
            desired_outcome: "test outcome".to_string(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
        }
    }

    fn make_motivation(id: &str, name: &str) -> Motivation {
        Motivation {
            id: id.to_string(),
            name: name.to_string(),
            description: "test motivation".to_string(),
            acceptable_tradeoffs: Vec::new(),
            unacceptable_tradeoffs: Vec::new(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
        }
    }

    fn make_agent(id: &str, name: &str, role_id: &str, motivation_id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            role_id: role_id.to_string(),
            motivation_id: motivation_id.to_string(),
            name: name.to_string(),
            performance: PerformanceRecord::default(),
            lineage: Lineage::default(),
            capabilities: Vec::new(),
            rate: None,
            capacity: None,
            trust_level: crate::graph::TrustLevel::Provisional,
            contact: None,
            executor: "claude".to_string(),
        }
    }

    #[test]
    fn transfer_new_roles() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        source.save_role(&make_role("r1", "role1")).unwrap();
        source.save_role(&make_role("r2", "role2")).unwrap();

        let summary = transfer(&source, &target, &TransferOptions::default()).unwrap();
        assert_eq!(summary.roles_added, 2);
        assert_eq!(summary.roles_skipped, 0);
        assert!(target.exists_role("r1"));
        assert!(target.exists_role("r2"));
    }

    #[test]
    fn transfer_skips_identical() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        let role = make_role("r1", "role1");
        source.save_role(&role).unwrap();
        target.save_role(&role).unwrap();

        let summary = transfer(&source, &target, &TransferOptions::default()).unwrap();
        assert_eq!(summary.roles_added, 0);
        assert_eq!(summary.roles_skipped, 1);
    }

    #[test]
    fn transfer_merges_performance() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        let mut role_source = make_role("r1", "role1");
        role_source.performance.evaluations.push(EvaluationRef {
            score: 0.9,
            task_id: "task-a".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            context_id: String::new(),
        });
        role_source.performance.task_count = 1;
        role_source.performance.avg_score = Some(0.9);

        let mut role_target = make_role("r1", "role1-local");
        role_target.performance.evaluations.push(EvaluationRef {
            score: 0.8,
            task_id: "task-b".to_string(),
            timestamp: "2026-01-02T00:00:00Z".to_string(),
            context_id: String::new(),
        });
        role_target.performance.task_count = 1;
        role_target.performance.avg_score = Some(0.8);

        source.save_role(&role_source).unwrap();
        target.save_role(&role_target).unwrap();

        let summary = transfer(&source, &target, &TransferOptions::default()).unwrap();
        assert_eq!(summary.roles_updated, 1);

        // Verify merged performance
        let roles = target.load_roles().unwrap();
        let merged = roles.iter().find(|r| r.id == "r1").unwrap();
        assert_eq!(merged.performance.task_count, 2);
        assert_eq!(merged.name, "role1-local"); // target name preserved
    }

    #[test]
    fn transfer_agent_pulls_dependencies() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        let role = make_role("r1", "role1");
        let motivation = make_motivation("m1", "motivation1");
        let agent = make_agent("a1", "agent1", "r1", "m1");

        source.save_role(&role).unwrap();
        source.save_motivation(&motivation).unwrap();
        source.save_agent(&agent).unwrap();

        // Transfer only agents — should auto-include role and motivation
        let opts = TransferOptions {
            entity_filter: EntityFilter::Agents,
            ..Default::default()
        };
        let summary = transfer(&source, &target, &opts).unwrap();

        assert_eq!(summary.agents_added, 1);
        assert_eq!(summary.roles_added, 1);
        assert_eq!(summary.motivations_added, 1);
        assert!(target.exists_role("r1"));
        assert!(target.exists_motivation("m1"));
    }

    #[test]
    fn transfer_agent_skips_existing_deps() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        let role = make_role("r1", "role1");
        let motivation = make_motivation("m1", "motivation1");
        let agent = make_agent("a1", "agent1", "r1", "m1");

        source.save_role(&role).unwrap();
        source.save_motivation(&motivation).unwrap();
        source.save_agent(&agent).unwrap();

        // Pre-populate target with deps
        target.save_role(&role).unwrap();
        target.save_motivation(&motivation).unwrap();

        let opts = TransferOptions {
            entity_filter: EntityFilter::Agents,
            ..Default::default()
        };
        let summary = transfer(&source, &target, &opts).unwrap();

        assert_eq!(summary.agents_added, 1);
        // Deps already exist in target — referential integrity check skips them
        // (they were never added to the transfer set, so no count increment)
        assert_eq!(summary.roles_added, 0);
        assert_eq!(summary.motivations_added, 0);
    }

    #[test]
    fn transfer_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        source.save_role(&make_role("r1", "role1")).unwrap();

        let opts = TransferOptions {
            dry_run: true,
            ..Default::default()
        };
        let summary = transfer(&source, &target, &opts).unwrap();
        assert_eq!(summary.roles_added, 1);
        assert!(!target.exists_role("r1")); // not actually written
    }

    #[test]
    fn transfer_no_performance_strips_perf() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        let mut role = make_role("r1", "role1");
        role.performance.task_count = 5;
        role.performance.avg_score = Some(0.85);
        source.save_role(&role).unwrap();

        let opts = TransferOptions {
            no_performance: true,
            ..Default::default()
        };
        transfer(&source, &target, &opts).unwrap();

        let roles = target.load_roles().unwrap();
        let saved = roles.iter().find(|r| r.id == "r1").unwrap();
        assert_eq!(saved.performance.task_count, 0);
        assert!(saved.performance.avg_score.is_none());
    }

    #[test]
    fn transfer_entity_filter_by_id() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        source.save_role(&make_role("r1", "role1")).unwrap();
        source.save_role(&make_role("r2", "role2")).unwrap();

        let opts = TransferOptions {
            entity_ids: vec!["r1".to_string()],
            ..Default::default()
        };
        let summary = transfer(&source, &target, &opts).unwrap();
        assert_eq!(summary.roles_added, 1);
        assert!(target.exists_role("r1"));
        assert!(!target.exists_role("r2"));
    }

    #[test]
    fn merge_performance_deduplicates() {
        let a = PerformanceRecord {
            task_count: 1,
            avg_score: Some(0.9),
            evaluations: vec![EvaluationRef {
                score: 0.9,
                task_id: "t1".to_string(),
                timestamp: "2026-01-01".to_string(),
                context_id: String::new(),
            }],
        };
        let b = PerformanceRecord {
            task_count: 2,
            avg_score: Some(0.85),
            evaluations: vec![
                EvaluationRef {
                    score: 0.9,
                    task_id: "t1".to_string(),
                    timestamp: "2026-01-01".to_string(),
                    context_id: String::new(),
                },
                EvaluationRef {
                    score: 0.8,
                    task_id: "t2".to_string(),
                    timestamp: "2026-01-02".to_string(),
                    context_id: String::new(),
                },
            ],
        };
        let merged = merge_performance(&a, &b);
        assert_eq!(merged.task_count, 2); // deduped
        assert_eq!(merged.evaluations.len(), 2);
    }

    #[test]
    fn merge_lineage_prefers_richer() {
        let sparse = Lineage {
            parent_ids: Vec::new(),
            generation: 0,
            created_by: "human".to_string(),
            created_at: chrono::Utc::now(),
        };
        let rich = Lineage {
            parent_ids: vec!["p1".to_string()],
            generation: 1,
            created_by: "evolver-1".to_string(),
            created_at: chrono::Utc::now(),
        };
        let merged = merge_lineage(&sparse, &rich);
        assert_eq!(merged.parent_ids.len(), 1);
        assert_eq!(merged.generation, 1);
    }

    #[test]
    fn resolve_store_finds_project_store() {
        let tmp = TempDir::new().unwrap();
        let wg = tmp.path().join(".workgraph").join("agency");
        agency::init(&wg).unwrap();

        let store = resolve_store(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(store.store_path(), wg);
    }

    #[test]
    fn resolve_store_finds_bare_store() {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("agency");
        agency::init(&bare).unwrap();

        let store = resolve_store(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(store.store_path(), bare);
    }

    #[test]
    fn resolve_store_finds_direct_agency_dir() {
        let tmp = TempDir::new().unwrap();
        let direct = tmp.path().join("myagency");
        agency::init(&direct).unwrap();

        let store = resolve_store(direct.to_str().unwrap()).unwrap();
        assert_eq!(store.store_path(), direct);
    }

    #[test]
    fn transfer_errors_on_corrupt_target_yaml() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        // Put a valid role in source
        source.save_role(&make_role("r1", "role1")).unwrap();
        // Also put it in target so the merge path is exercised
        target.save_role(&make_role("r1", "role1")).unwrap();

        // Corrupt a role YAML in the target store
        let corrupt_path = target.roles_dir().join("corrupt.yaml");
        std::fs::write(&corrupt_path, "{{{{not valid yaml!!!!").unwrap();

        let result = transfer(&source, &target, &TransferOptions::default());
        assert!(result.is_err(), "transfer should fail on corrupt target YAML");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("yaml") || err_msg.contains("YAML") || err_msg.contains("parse") || err_msg.contains("scan"),
            "error should mention YAML parsing: {}",
            err_msg
        );
    }

    #[test]
    fn transfer_errors_on_missing_agent_role_dependency() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        // Create an agent that references a role not in source
        let motivation = make_motivation("m1", "motivation1");
        source.save_motivation(&motivation).unwrap();

        let agent = make_agent("a1", "agent1", "nonexistent-role", "m1");
        source.save_agent(&agent).unwrap();

        let opts = TransferOptions {
            entity_filter: EntityFilter::Agents,
            ..Default::default()
        };
        let result = transfer(&source, &target, &opts);
        assert!(result.is_err(), "transfer should fail on missing role dependency");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("nonexistent-role") && err_msg.contains("referential integrity"),
            "error should mention the missing role and referential integrity: {}",
            err_msg
        );
    }

    #[test]
    fn transfer_errors_on_missing_agent_motivation_dependency() {
        let tmp = TempDir::new().unwrap();
        let source = setup_store(&tmp, "source");
        let target = setup_store(&tmp, "target");

        // Create an agent that references a motivation not in source
        let role = make_role("r1", "role1");
        source.save_role(&role).unwrap();

        let agent = make_agent("a1", "agent1", "r1", "nonexistent-motivation");
        source.save_agent(&agent).unwrap();

        let opts = TransferOptions {
            entity_filter: EntityFilter::Agents,
            ..Default::default()
        };
        let result = transfer(&source, &target, &opts);
        assert!(result.is_err(), "transfer should fail on missing motivation dependency");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("nonexistent-motivation") && err_msg.contains("referential integrity"),
            "error should mention the missing motivation and referential integrity: {}",
            err_msg
        );
    }

    // -----------------------------------------------------------------------
    // Cross-repo dependency tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_remote_ref_valid() {
        let result = parse_remote_ref("workgraph:implement-trace");
        assert_eq!(result, Some(("workgraph", "implement-trace")));
    }

    #[test]
    fn parse_remote_ref_multiple_colons() {
        // Only splits on first colon
        let result = parse_remote_ref("peer:task:with:colons");
        assert_eq!(result, Some(("peer", "task:with:colons")));
    }

    #[test]
    fn parse_remote_ref_no_colon() {
        assert_eq!(parse_remote_ref("local-task-id"), None);
    }

    #[test]
    fn parse_remote_ref_empty_peer() {
        assert_eq!(parse_remote_ref(":task-id"), None);
    }

    #[test]
    fn parse_remote_ref_empty_task() {
        assert_eq!(parse_remote_ref("peer:"), None);
    }

    #[test]
    fn parse_remote_ref_empty_string() {
        assert_eq!(parse_remote_ref(""), None);
    }

    #[test]
    fn parse_status_string_all_variants() {
        assert_eq!(parse_status_string("Done"), crate::graph::Status::Done);
        assert_eq!(parse_status_string("done"), crate::graph::Status::Done);
        assert_eq!(parse_status_string("Open"), crate::graph::Status::Open);
        assert_eq!(
            parse_status_string("InProgress"),
            crate::graph::Status::InProgress
        );
        assert_eq!(
            parse_status_string("in-progress"),
            crate::graph::Status::InProgress
        );
        assert_eq!(parse_status_string("Failed"), crate::graph::Status::Failed);
        assert_eq!(
            parse_status_string("Abandoned"),
            crate::graph::Status::Abandoned
        );
        assert_eq!(
            parse_status_string("Blocked"),
            crate::graph::Status::Blocked
        );
        // Unknown defaults to Open
        assert_eq!(parse_status_string("bogus"), crate::graph::Status::Open);
    }

    #[test]
    fn resolve_remote_task_status_via_direct_file_access() {
        let tmp = TempDir::new().unwrap();

        // Set up local workgraph with federation config pointing to a peer
        let local_wg = tmp.path().join("local").join(".workgraph");
        std::fs::create_dir_all(&local_wg).unwrap();

        // Set up peer workgraph with a task
        let peer_project = tmp.path().join("peer-project");
        let peer_wg = peer_project.join(".workgraph");
        std::fs::create_dir_all(&peer_wg).unwrap();

        // Create a task in the peer's graph
        let mut peer_graph = crate::graph::WorkGraph::new();
        let mut task = crate::graph::Task::default();
        task.id = "remote-task".to_string();
        task.title = "A remote task".to_string();
        task.status = crate::graph::Status::Done;
        peer_graph.add_node(crate::graph::Node::Task(task));
        crate::parser::save_graph(&peer_graph, &peer_wg.join("graph.jsonl")).unwrap();

        // Configure federation with the peer
        let config = FederationConfig {
            remotes: BTreeMap::new(),
            peers: {
                let mut m = BTreeMap::new();
                m.insert(
                    "mypeer".to_string(),
                    PeerConfig {
                        path: peer_project.to_str().unwrap().to_string(),
                        description: None,
                    },
                );
                m
            },
        };
        save_federation_config(&local_wg, &config).unwrap();

        // Resolve the remote task
        let result = resolve_remote_task_status("mypeer", "remote-task", &local_wg);
        assert_eq!(result.status, crate::graph::Status::Done);
        assert_eq!(result.title.as_deref(), Some("A remote task"));
        assert_eq!(result.resolution, RemoteResolution::DirectFileAccess);
    }

    #[test]
    fn resolve_remote_task_status_not_found() {
        let tmp = TempDir::new().unwrap();
        let local_wg = tmp.path().join("local").join(".workgraph");
        std::fs::create_dir_all(&local_wg).unwrap();

        // Set up peer with empty graph
        let peer_project = tmp.path().join("peer-project");
        let peer_wg = peer_project.join(".workgraph");
        std::fs::create_dir_all(&peer_wg).unwrap();
        let peer_graph = crate::graph::WorkGraph::new();
        crate::parser::save_graph(&peer_graph, &peer_wg.join("graph.jsonl")).unwrap();

        let config = FederationConfig {
            remotes: BTreeMap::new(),
            peers: {
                let mut m = BTreeMap::new();
                m.insert(
                    "mypeer".to_string(),
                    PeerConfig {
                        path: peer_project.to_str().unwrap().to_string(),
                        description: None,
                    },
                );
                m
            },
        };
        save_federation_config(&local_wg, &config).unwrap();

        let result = resolve_remote_task_status("mypeer", "nonexistent", &local_wg);
        // Task not found → treated as open (blocking)
        assert_eq!(result.status, crate::graph::Status::Open);
        assert!(
            matches!(result.resolution, RemoteResolution::Unreachable(_)),
            "expected Unreachable, got {:?}",
            result.resolution
        );
    }

    #[test]
    fn resolve_remote_task_status_unknown_peer() {
        let tmp = TempDir::new().unwrap();
        let local_wg = tmp.path().join("local").join(".workgraph");
        std::fs::create_dir_all(&local_wg).unwrap();

        // No federation config → empty peers
        let result = resolve_remote_task_status("unknown-peer", "some-task", &local_wg);
        assert_eq!(result.status, crate::graph::Status::Open);
        assert!(matches!(result.resolution, RemoteResolution::Unreachable(_)));
    }
}

use chrono::{Duration, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};

/// Configuration for structural cycle iteration.
/// Only present on the cycle header task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CycleConfig {
    /// Hard cap on cycle iterations
    pub max_iterations: u32,
    /// Condition that must be true to iterate (None = always, up to max_iterations)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<LoopGuard>,
    /// Time delay before re-activation (e.g., "30s", "5m", "1h")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay: Option<String>,
}

/// Guard condition for a loop edge
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LoopGuard {
    /// Loop if a specific task has this status
    TaskStatus { task: String, status: Status },
    /// Loop if iteration count < N (redundant with max_iterations but explicit)
    IterationLessThan(u32),
    /// Always loop (up to max_iterations)
    Always,
}

/// Parse a human-readable duration string like "30s", "5m", "1h", "24h" into seconds.
/// Returns None if the string is not a valid duration.
pub fn parse_delay(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Use char boundary to avoid panic on multi-byte UTF-8
    let last_char = s.chars().last()?;
    let split_pos = s.len() - last_char.len_utf8();
    let num_part = &s[..split_pos];
    let num: u64 = num_part.parse().ok()?;
    let unit = last_char;
    match unit {
        's' => Some(num),
        'm' => num.checked_mul(60),
        'h' => num.checked_mul(3600),
        'd' => num.checked_mul(86400),
        _ => None,
    }
}

/// A log entry for tracking progress/notes on a task
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub message: String,
}

/// Cost/time estimate for a task
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Estimate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// Task status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Open,
    InProgress,
    Done,
    Blocked,
    Failed,
    Abandoned,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Open => write!(f, "open"),
            Status::InProgress => write!(f, "in-progress"),
            Status::Done => write!(f, "done"),
            Status::Blocked => write!(f, "blocked"),
            Status::Failed => write!(f, "failed"),
            Status::Abandoned => write!(f, "abandoned"),
        }
    }
}

/// Custom deserializer that maps legacy "pending-review" status to Done.
impl<'de> serde::Deserialize<'de> for Status {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "open" => Ok(Status::Open),
            "in-progress" => Ok(Status::InProgress),
            "done" => Ok(Status::Done),
            "blocked" => Ok(Status::Blocked),
            "failed" => Ok(Status::Failed),
            "abandoned" => Ok(Status::Abandoned),
            // Migration: pending-review is treated as done
            "pending-review" => Ok(Status::Done),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &[
                    "open",
                    "in-progress",
                    "done",
                    "blocked",
                    "failed",
                    "abandoned",
                ],
            )),
        }
    }
}

impl Status {
    /// Whether this status is terminal — the task will not progress further
    /// without explicit intervention (retry, reopen, etc.).
    /// Terminal statuses should not block dependent tasks.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Status::Done | Status::Failed | Status::Abandoned)
    }
}

/// A task node.
///
/// A task in the workgraph with dependencies, status, and execution metadata.
///
/// Custom `Deserialize` handles migration from the old `identity` field
/// (`{"role_id": "...", "motivation_id": "..."}`) to the new `agent` field
/// (content-hash string).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Task {
    pub id: String,
    pub title: String,
    /// Detailed description of the task (body, acceptance criteria, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<Estimate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocks")]
    pub before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "blocked_by")]
    pub after: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Required skills/capabilities for this task
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Input files/context paths needed for this task
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// Expected output paths/artifacts
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliverables: Vec<String>,
    /// Actual produced artifacts (paths/references)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    /// Shell command to execute for this task (optional, for wg exec)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    /// Task is not ready until this timestamp (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_before: Option<String>,
    /// Timestamp when the task was created (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Timestamp when the task status changed to InProgress (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Timestamp when the task status changed to Done (ISO 8601 / RFC 3339)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Progress log entries
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<LogEntry>,
    /// Number of times this task has been retried after failure
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retry_count: u32,
    /// Maximum number of retries allowed (None = unlimited)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// Reason for failure or abandonment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// Preferred model for this task (haiku, sonnet, opus)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Verification criteria - if set, task requires review before done
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    /// Agent assigned to this task (content-hash of an Agent in the agency)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Current cycle iteration (0 = first run, incremented on each re-activation)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub loop_iteration: u32,
    /// Configuration for structural cycle iteration (only on cycle header tasks)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_config: Option<CycleConfig>,
    /// Task is not ready until this timestamp (ISO 8601 / RFC 3339).
    /// Set by loop edges with a delay — prevents immediate dispatch after re-activation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_after: Option<String>,
    /// When true, the task is paused and will not be dispatched by the coordinator.
    /// The task retains its status and loop state; `wg resume` clears this flag.
    #[serde(default, skip_serializing_if = "is_bool_false")]
    pub paused: bool,
    /// Visibility zone for trace exports. Controls what crosses organizational boundaries.
    /// Values: "internal" (default, org-only), "public" (sanitized sharing),
    /// "peer" (richer view for credentialed peers).
    #[serde(default = "default_visibility", skip_serializing_if = "is_default_visibility")]
    pub visibility: String,
}

fn default_visibility() -> String {
    "internal".to_string()
}

/// Deserialize loops_to accepting both old string format and array format.
fn deserialize_loops_to<'de, D>(deserializer: D) -> Result<Vec<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct LoopsToVisitor;

    impl<'de> de::Visitor<'de> for LoopsToVisitor {
        type Value = Vec<serde_json::Value>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string or array for loops_to")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![serde_json::Value::String(v.to_string())])
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(vec![serde_json::Value::String(v)])
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(vec![])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(vec![])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(val) = seq.next_element()? {
                values.push(val);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(LoopsToVisitor)
}

fn is_default_visibility(val: &str) -> bool {
    val == "internal"
}

/// Legacy identity format: `{"role_id": "...", "motivation_id": "..."}`.
/// Used for migrating old JSONL data that stored identity inline on tasks.
#[derive(Deserialize)]
struct LegacyIdentity {
    role_id: String,
    motivation_id: String,
}

/// Helper struct for deserializing Task with migration from old `identity` field.
#[derive(Deserialize)]
struct TaskHelper {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: Status,
    #[serde(default)]
    assigned: Option<String>,
    #[serde(default)]
    estimate: Option<Estimate>,
    #[serde(default, alias = "blocks")]
    before: Vec<String>,
    #[serde(default, alias = "blocked_by")]
    after: Vec<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    deliverables: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    exec: Option<String>,
    #[serde(default)]
    not_before: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
    #[serde(default)]
    log: Vec<LogEntry>,
    #[serde(default)]
    retry_count: u32,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    failure_reason: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    verify: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    /// Deprecated: silently ignored on deserialization for backward compatibility.
    /// Accepts both old string format ("loops_to": "b") and array format ("loops_to": ["b"]).
    #[serde(default, deserialize_with = "deserialize_loops_to")]
    #[allow(dead_code)]
    loops_to: Vec<serde_json::Value>,
    #[serde(default)]
    loop_iteration: u32,
    #[serde(default)]
    cycle_config: Option<CycleConfig>,
    #[serde(default)]
    ready_after: Option<String>,
    #[serde(default)]
    paused: bool,
    #[serde(default = "default_visibility")]
    visibility: String,
    /// Old format: inline identity object. Migrated to `agent` hash on read.
    #[serde(default)]
    identity: Option<LegacyIdentity>,
}

impl<'de> Deserialize<'de> for Task {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let helper = TaskHelper::deserialize(deserializer)?;

        // Migrate: if old `identity` field present and no `agent`, compute hash
        let agent = match (helper.agent, helper.identity) {
            (Some(a), _) => Some(a),
            (None, Some(legacy)) => Some(crate::agency::content_hash_agent(
                &legacy.role_id,
                &legacy.motivation_id,
            )),
            (None, None) => None,
        };

        Ok(Task {
            id: helper.id,
            title: helper.title,
            description: helper.description,
            status: helper.status,
            assigned: helper.assigned,
            estimate: helper.estimate,
            before: helper.before,
            after: helper.after,
            requires: helper.requires,
            tags: helper.tags,
            skills: helper.skills,
            inputs: helper.inputs,
            deliverables: helper.deliverables,
            artifacts: helper.artifacts,
            exec: helper.exec,
            not_before: helper.not_before,
            created_at: helper.created_at,
            started_at: helper.started_at,
            completed_at: helper.completed_at,
            log: helper.log,
            retry_count: helper.retry_count,
            max_retries: helper.max_retries,
            failure_reason: helper.failure_reason,
            model: helper.model,
            verify: helper.verify,
            agent,
            loop_iteration: helper.loop_iteration,
            cycle_config: helper.cycle_config,
            ready_after: helper.ready_after,
            paused: helper.paused,
            visibility: helper.visibility,
        })
    }
}

fn is_zero(val: &u32) -> bool {
    *val == 0
}

fn is_bool_false(val: &bool) -> bool {
    !*val
}

/// Trust level for an agent
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TrustLevel {
    /// Fully verified (human admin, proven agent)
    Verified,
    /// Provisionally trusted (new agent, limited permissions)
    #[default]
    Provisional,
    /// Unknown trust (external agent, needs verification)
    Unknown,
}

/// A resource (budget, compute, etc.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// A node in the work graph (task or resource)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[allow(clippy::large_enum_variant)]
pub enum Node {
    Task(Task),
    Resource(Resource),
}

impl Node {
    pub fn id(&self) -> &str {
        match self {
            Node::Task(t) => &t.id,
            Node::Resource(r) => &r.id,
        }
    }
}

/// A detected cycle (strongly connected component) in the task graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedCycle {
    /// All task IDs in this cycle's SCC.
    pub members: Vec<String>,
    /// The entry point / loop header task ID.
    pub header: String,
    /// Is this a reducible cycle (single entry point)?
    pub reducible: bool,
}

/// Cached cycle analysis derived from the graph's after edges.
/// Never serialized — recomputed lazily on access.
#[derive(Debug, Clone, Default)]
pub struct CycleAnalysis {
    /// Non-trivial SCCs (cycles).
    pub cycles: Vec<DetectedCycle>,
    /// Which cycle each task belongs to (task_id → index into cycles).
    pub task_to_cycle: HashMap<String, usize>,
    /// Back-edges: (predecessor_id, header_id) pairs within cycles.
    pub back_edges: HashSet<(String, String)>,
}

impl CycleAnalysis {
    /// Compute cycle analysis from a WorkGraph's after edges.
    pub fn from_graph(graph: &WorkGraph) -> Self {
        use crate::cycle::NamedGraph;

        let mut named = NamedGraph::new();
        for task in graph.tasks() {
            named.add_node(&task.id);
        }
        for task in graph.tasks() {
            for dep_id in &task.after {
                if graph.get_task(dep_id).is_some() {
                    named.add_edge(dep_id, &task.id);
                }
            }
        }

        let metadata = named.analyze_cycles();
        let mut cycles = Vec::new();
        let mut task_to_cycle = HashMap::new();
        let mut back_edges = HashSet::new();

        for (idx, meta) in metadata.iter().enumerate() {
            let members: Vec<String> = meta
                .members
                .iter()
                .map(|&nid| named.get_name(nid).to_string())
                .collect();
            let header = named.get_name(meta.header).to_string();

            for member in &members {
                task_to_cycle.insert(member.clone(), idx);
            }
            for &(src, tgt) in &meta.back_edges {
                back_edges.insert((
                    named.get_name(src).to_string(),
                    named.get_name(tgt).to_string(),
                ));
            }
            cycles.push(DetectedCycle {
                members,
                header,
                reducible: meta.reducible,
            });
        }

        CycleAnalysis {
            cycles,
            task_to_cycle,
            back_edges,
        }
    }
}

/// The work graph: a directed task graph with dependency edges and optional loop edges.
///
/// Tasks depend on other tasks via `after`/`blocks` edges. Resources are
/// consumed by tasks via `requires` edges. The graph is persisted as JSONL
/// (one node per line) and supports concurrent readers via atomic writes.
#[derive(Debug, Clone, Default)]
pub struct WorkGraph {
    nodes: HashMap<String, Node>,
    /// Cached cycle analysis. Lazily computed; invalidated on structural mutations.
    cycle_analysis: Option<CycleAnalysis>,
}

impl WorkGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            cycle_analysis: None,
        }
    }

    /// Insert a node (task or resource) into the graph.
    pub fn add_node(&mut self, node: Node) {
        self.cycle_analysis = None;
        self.nodes.insert(node.id().to_string(), node);
    }

    /// Look up a node by ID.
    pub fn get_node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Look up a task by ID, returning `None` if the node is a resource.
    pub fn get_task(&self, id: &str) -> Option<&Task> {
        match self.nodes.get(id) {
            Some(Node::Task(t)) => Some(t),
            _ => None,
        }
    }

    /// Look up a task by ID (mutable), returning `None` if the node is a resource.
    pub fn get_task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.cycle_analysis = None;
        match self.nodes.get_mut(id) {
            Some(Node::Task(t)) => Some(t),
            _ => None,
        }
    }

    /// Look up a task by ID, returning an error with did-you-mean suggestions if not found.
    pub fn get_task_or_err(&self, id: &str) -> anyhow::Result<&Task> {
        self.get_task(id)
            .ok_or_else(|| self.task_not_found_error(id))
    }

    /// Look up a task by ID (mutable), returning an error with did-you-mean suggestions if not found.
    pub fn get_task_mut_or_err(&mut self, id: &str) -> anyhow::Result<&mut Task> {
        self.cycle_analysis = None;
        let err = self.task_not_found_error(id);
        self.nodes
            .get_mut(id)
            .and_then(|n| match n {
                Node::Task(t) => Some(t),
                _ => None,
            })
            .ok_or(err)
    }

    /// Build a "Task not found" error, suggesting similar task IDs if any exist.
    fn task_not_found_error(&self, id: &str) -> anyhow::Error {
        let suggestion = self
            .tasks()
            .map(|t| t.id.as_str())
            .filter(|candidate| is_similar(id, candidate))
            .min_by_key(|candidate| levenshtein(id, candidate))
            .map(|s| s.to_string());

        match suggestion {
            Some(s) => anyhow::anyhow!("Task '{}' not found. Did you mean '{}'?", id, s),
            None => anyhow::anyhow!("Task '{}' not found", id),
        }
    }

    /// Look up a resource by ID, returning `None` if the node is a task.
    pub fn get_resource(&self, id: &str) -> Option<&Resource> {
        match self.nodes.get(id) {
            Some(Node::Resource(r)) => Some(r),
            _ => None,
        }
    }

    /// Iterate over all nodes (tasks and resources) in the graph.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// Iterate over all tasks in the graph, skipping resource nodes.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.nodes.values().filter_map(|n| match n {
            Node::Task(t) => Some(t),
            _ => None,
        })
    }

    /// Iterate over all resources in the graph, skipping task nodes.
    pub fn resources(&self) -> impl Iterator<Item = &Resource> {
        self.nodes.values().filter_map(|n| match n {
            Node::Resource(r) => Some(r),
            _ => None,
        })
    }

    /// Remove a node by ID, returning the removed node if it existed.
    ///
    /// Also cleans up all references to the removed node from other tasks
    /// (`after`, `blocks`, `requires`).
    pub fn remove_node(&mut self, id: &str) -> Option<Node> {
        self.cycle_analysis = None;
        let removed = self.nodes.remove(id);
        if removed.is_some() {
            for node in self.nodes.values_mut() {
                if let Node::Task(task) = node {
                    task.after.retain(|dep| dep != id);
                    task.before.retain(|dep| dep != id);
                    task.requires.retain(|dep| dep != id);
                }
            }
        }
        removed
    }

    /// Return the total number of nodes (tasks + resources) in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Return true if the graph contains no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Invalidate cached cycle analysis. Called by structural mutations.
    pub fn invalidate_cycle_cache(&mut self) {
        self.cycle_analysis = None;
    }

    /// Compute cycle analysis without caching (for use with immutable references).
    pub fn compute_cycle_analysis(&self) -> CycleAnalysis {
        CycleAnalysis::from_graph(self)
    }

    /// Get or compute cached cycle analysis.
    pub fn get_cycle_analysis(&mut self) -> &CycleAnalysis {
        if self.cycle_analysis.is_none() {
            self.cycle_analysis = Some(CycleAnalysis::from_graph(self));
        }
        self.cycle_analysis.as_ref().unwrap()
    }
}

/// Evaluate a guard condition against the current graph state.
fn evaluate_guard(guard: &Option<LoopGuard>, graph: &WorkGraph) -> bool {
    match guard {
        None | Some(LoopGuard::Always) => true,
        // IterationLessThan is checked by callers where iteration count is available.
        Some(LoopGuard::IterationLessThan(_)) => true,
        Some(LoopGuard::TaskStatus { task, status }) => graph
            .get_task(task)
            .map(|t| t.status == *status)
            .unwrap_or(false),
    }
}

/// Evaluate structural cycle iteration after a task transitions to Done.
///
/// Checks if the completed task is part of a structural cycle (detected via
/// `CycleAnalysis`). If ALL cycle members are Done, and the cycle header has
/// a `CycleConfig`, evaluates whether to iterate:
/// 1. Check convergence tag on header (any member can set it via --converged)
/// 2. Check `max_iterations` on header's `CycleConfig`
/// 3. Check guard condition
/// 4. If iterating: re-open all cycle members, increment `loop_iteration`,
///    optionally set `ready_after` if delay is configured.
///
/// Returns the list of task IDs that were re-activated.
pub fn evaluate_cycle_iteration(
    graph: &mut WorkGraph,
    completed_task_id: &str,
    cycle_analysis: &CycleAnalysis,
) -> Vec<String> {
    // 1. Check if the completed task is in a cycle
    let cycle_idx = match cycle_analysis.task_to_cycle.get(completed_task_id) {
        Some(&idx) => idx,
        None => return vec![],
    };

    let cycle = &cycle_analysis.cycles[cycle_idx];

    // 2. Find the cycle member with CycleConfig (may differ from SCC header).
    //    The spec requires exactly one member to have it; wg check enforces this.
    let (config_owner_id, cycle_config) = {
        let mut found = None;
        for member_id in &cycle.members {
            if let Some(task) = graph.get_task(member_id) {
                if let Some(ref config) = task.cycle_config {
                    found = Some((member_id.clone(), config.clone()));
                    break;
                }
            }
        }
        match found {
            Some(pair) => pair,
            None => return vec![], // No config = no cycle iteration
        }
    };

    // 3. Check if ALL cycle members are Done
    for member_id in &cycle.members {
        match graph.get_task(member_id) {
            Some(t) if t.status == Status::Done => {}
            _ => return vec![], // Not all done yet
        }
    }

    // 4. Check convergence tag on config owner (or any member)
    if let Some(owner) = graph.get_task(&config_owner_id) {
        if owner.tags.contains(&"converged".to_string()) {
            return vec![];
        }
    }

    // 5. Check max_iterations (using config owner's loop_iteration)
    let current_iter = graph
        .get_task(&config_owner_id)
        .map(|t| t.loop_iteration)
        .unwrap_or(0);
    if current_iter >= cycle_config.max_iterations {
        return vec![];
    }

    // 6. Check guard condition
    if !evaluate_guard(&cycle_config.guard, graph) {
        return vec![];
    }
    if let Some(LoopGuard::IterationLessThan(n)) = &cycle_config.guard {
        if current_iter >= *n {
            return vec![];
        }
    }

    // 7. All checks passed — re-open all cycle members
    let new_iteration = current_iter + 1;
    let ready_after = cycle_config
        .delay
        .as_ref()
        .and_then(|d| match parse_delay(d) {
            Some(secs) if secs <= i64::MAX as u64 => {
                Some((Utc::now() + Duration::seconds(secs as i64)).to_rfc3339())
            }
            _ => None,
        });

    let mut reactivated = Vec::new();

    for member_id in &cycle.members {
        if let Some(task) = graph.get_task_mut(member_id) {
            task.status = Status::Open;
            task.assigned = None;
            task.started_at = None;
            task.completed_at = None;
            task.loop_iteration = new_iteration;
            if *member_id == config_owner_id {
                task.ready_after = ready_after.clone();
            }

            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                message: format!(
                    "Re-activated by cycle iteration (iteration {}/{})",
                    new_iteration, cycle_config.max_iterations
                ),
            });

            reactivated.push(member_id.clone());
        }
    }

    reactivated
}

/// Compute Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Check if two task IDs are similar enough to suggest.
/// Returns true if one is a prefix of the other, or Levenshtein distance <= 2.
fn is_similar(query: &str, candidate: &str) -> bool {
    if candidate.starts_with(query) || query.starts_with(candidate) {
        return true;
    }
    levenshtein(query, candidate) <= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn test_status_is_terminal() {
        assert!(!Status::Open.is_terminal());
        assert!(!Status::InProgress.is_terminal());
        assert!(!Status::Blocked.is_terminal());
        assert!(Status::Done.is_terminal());
        assert!(Status::Failed.is_terminal());
        assert!(Status::Abandoned.is_terminal());
    }

    #[test]
    fn test_workgraph_new_is_empty() {
        let graph = WorkGraph::new();
        assert!(graph.is_empty());
        assert_eq!(graph.len(), 0);
    }

    #[test]
    fn test_add_and_get_task() {
        let mut graph = WorkGraph::new();
        let task = make_task("api-design", "Design API");
        graph.add_node(Node::Task(task));

        assert_eq!(graph.len(), 1);
        let retrieved = graph.get_task("api-design").unwrap();
        assert_eq!(retrieved.title, "Design API");
    }

    #[test]
    fn test_get_nonexistent_returns_none() {
        let graph = WorkGraph::new();
        assert!(graph.get_node("nonexistent").is_none());
        assert!(graph.get_task("nonexistent").is_none());
    }

    #[test]
    fn test_remove_node() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        assert_eq!(graph.len(), 1);

        let removed = graph.remove_node("t1");
        assert!(removed.is_some());
        assert!(graph.is_empty());
    }

    #[test]
    fn test_remove_node_cleans_up_references() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));

        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        t2.before = vec!["t1".to_string()];
        t2.requires = vec!["t1".to_string()];
        graph.add_node(Node::Task(t2));

        graph.remove_node("t1");

        let t2 = graph.get_task("t2").unwrap();
        assert!(t2.after.is_empty(), "after should be cleaned");
        assert!(t2.before.is_empty(), "blocks should be cleaned");
        assert!(t2.requires.is_empty(), "requires should be cleaned");
    }

    #[test]
    fn test_tasks_iterator() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        graph.add_node(Node::Task(make_task("t2", "Task 2")));

        let tasks: Vec<_> = graph.tasks().collect();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn test_task_with_blocks() {
        let mut graph = WorkGraph::new();
        let mut task1 = make_task("api-design", "Design API");
        task1.before = vec!["api-impl".to_string()];

        let mut task2 = make_task("api-impl", "Implement API");
        task2.after = vec!["api-design".to_string()];

        graph.add_node(Node::Task(task1));
        graph.add_node(Node::Task(task2));

        let design = graph.get_task("api-design").unwrap();
        assert_eq!(design.before, vec!["api-impl"]);

        let impl_task = graph.get_task("api-impl").unwrap();
        assert_eq!(impl_task.after, vec!["api-design"]);
    }

    #[test]
    fn test_task_serialization() {
        let task = make_task("t1", "Test task");
        let json = serde_json::to_string(&Node::Task(task)).unwrap();
        assert!(json.contains("\"kind\":\"task\""));
        assert!(json.contains("\"id\":\"t1\""));
    }

    #[test]
    fn test_task_deserialization() {
        let json = r#"{"id":"t1","kind":"task","title":"Test","status":"open"}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.id, "t1");
                assert_eq!(t.title, "Test");
                assert_eq!(t.status, Status::Open);
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_status_serialization() {
        assert_eq!(
            serde_json::to_string(&Status::InProgress).unwrap(),
            "\"in-progress\""
        );
    }

    #[test]
    fn test_timestamp_fields_serialization() {
        let mut task = make_task("t1", "Test task");
        task.created_at = Some("2024-01-15T10:30:00Z".to_string());
        task.started_at = Some("2024-01-15T11:00:00Z".to_string());
        task.completed_at = Some("2024-01-15T12:00:00Z".to_string());

        let json = serde_json::to_string(&Node::Task(task)).unwrap();
        assert!(json.contains("\"created_at\":\"2024-01-15T10:30:00Z\""));
        assert!(json.contains("\"started_at\":\"2024-01-15T11:00:00Z\""));
        assert!(json.contains("\"completed_at\":\"2024-01-15T12:00:00Z\""));

        // Verify deserialization
        let node: Node = serde_json::from_str(&json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.created_at, Some("2024-01-15T10:30:00Z".to_string()));
                assert_eq!(t.started_at, Some("2024-01-15T11:00:00Z".to_string()));
                assert_eq!(t.completed_at, Some("2024-01-15T12:00:00Z".to_string()));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_timestamp_fields_omitted_when_none() {
        let task = make_task("t1", "Test task");
        let json = serde_json::to_string(&Node::Task(task)).unwrap();

        // Verify timestamps are not included when None
        assert!(!json.contains("created_at"));
        assert!(!json.contains("started_at"));
        assert!(!json.contains("completed_at"));
    }

    #[test]
    fn test_deliverables_serialization() {
        let mut task = make_task("t1", "Build feature");
        task.deliverables = vec!["src/feature.rs".to_string(), "docs/feature.md".to_string()];

        let json = serde_json::to_string(&Node::Task(task)).unwrap();
        assert!(json.contains("\"deliverables\""));
        assert!(json.contains("src/feature.rs"));
        assert!(json.contains("docs/feature.md"));

        // Verify deserialization
        let node: Node = serde_json::from_str(&json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.deliverables.len(), 2);
                assert!(t.deliverables.contains(&"src/feature.rs".to_string()));
                assert!(t.deliverables.contains(&"docs/feature.md".to_string()));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_deliverables_omitted_when_empty() {
        let task = make_task("t1", "Test task");
        let json = serde_json::to_string(&Node::Task(task)).unwrap();

        // Verify deliverables not included when empty
        assert!(!json.contains("deliverables"));
    }

    #[test]
    fn test_deserialize_with_agent_field() {
        let json = r#"{"id":"t1","kind":"task","title":"Test","status":"open","agent":"abc123"}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.agent, Some("abc123".to_string()));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_deserialize_legacy_identity_migrates_to_agent() {
        // Old format had identity: {role_id, motivation_id} inline on the task
        let json = r#"{"id":"t1","kind":"task","title":"Test","status":"open","identity":{"role_id":"role-abc","motivation_id":"mot-xyz"}}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        match node {
            Node::Task(t) => {
                // Should be migrated to agent hash
                let expected = crate::agency::content_hash_agent("role-abc", "mot-xyz");
                assert_eq!(t.agent, Some(expected));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_deserialize_agent_field_takes_precedence_over_legacy_identity() {
        // If both agent and identity are present, agent wins
        let json = r#"{"id":"t1","kind":"task","title":"Test","status":"open","agent":"explicit-hash","identity":{"role_id":"role-abc","motivation_id":"mot-xyz"}}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        match node {
            Node::Task(t) => {
                assert_eq!(t.agent, Some("explicit-hash".to_string()));
            }
            _ => panic!("Expected Task"),
        }
    }

    #[test]
    fn test_serialize_does_not_emit_identity_field() {
        let mut task = make_task("t1", "Test task");
        task.agent = Some("abc123".to_string());
        let json = serde_json::to_string(&Node::Task(task)).unwrap();
        // New format only has "agent", never "identity"
        assert!(json.contains("\"agent\":\"abc123\""));
        assert!(!json.contains("\"identity\""));
    }

    // ── parse_delay tests ──────────────────────────────────────────

    #[test]
    fn test_parse_delay_seconds() {
        assert_eq!(parse_delay("30s"), Some(30));
        assert_eq!(parse_delay("1s"), Some(1));
    }

    #[test]
    fn test_parse_delay_minutes() {
        assert_eq!(parse_delay("5m"), Some(300));
        assert_eq!(parse_delay("1m"), Some(60));
    }

    #[test]
    fn test_parse_delay_hours() {
        assert_eq!(parse_delay("2h"), Some(7200));
        assert_eq!(parse_delay("1h"), Some(3600));
    }

    #[test]
    fn test_parse_delay_days() {
        assert_eq!(parse_delay("1d"), Some(86400));
        assert_eq!(parse_delay("7d"), Some(604800));
    }

    #[test]
    fn test_parse_delay_empty_string() {
        assert_eq!(parse_delay(""), None);
    }

    #[test]
    fn test_parse_delay_whitespace_only() {
        assert_eq!(parse_delay("   "), None);
    }

    #[test]
    fn test_parse_delay_whitespace_around_value() {
        assert_eq!(parse_delay("  10s  "), Some(10));
        assert_eq!(parse_delay("\t5m\t"), Some(300));
    }

    #[test]
    fn test_parse_delay_invalid_unit() {
        assert_eq!(parse_delay("10x"), None);
        assert_eq!(parse_delay("5w"), None);
        assert_eq!(parse_delay("3y"), None);
    }

    #[test]
    fn test_parse_delay_missing_numeric_prefix() {
        assert_eq!(parse_delay("s"), None);
        assert_eq!(parse_delay("m"), None);
        assert_eq!(parse_delay("h"), None);
        assert_eq!(parse_delay("d"), None);
    }

    #[test]
    fn test_parse_delay_zero_duration() {
        assert_eq!(parse_delay("0s"), Some(0));
        assert_eq!(parse_delay("0m"), Some(0));
        assert_eq!(parse_delay("0h"), Some(0));
        assert_eq!(parse_delay("0d"), Some(0));
    }

    #[test]
    fn test_parse_delay_large_values() {
        assert_eq!(parse_delay("999999s"), Some(999999));
        assert_eq!(parse_delay("100000m"), Some(6_000_000));
    }

    #[test]
    fn test_parse_delay_overflow_returns_none() {
        // u64::MAX / 86400 < 213_503_982_334_601, so this day value overflows
        // The function returns None on overflow instead of panicking
        assert_eq!(parse_delay("213503982334602d"), None);
        assert_eq!(parse_delay("999999999999999999h"), None);
        assert_eq!(parse_delay("999999999999999999m"), None);
    }

    #[test]
    fn test_parse_delay_fractional_number() {
        // parse::<u64> fails on fractional input
        assert_eq!(parse_delay("1.5s"), None);
        assert_eq!(parse_delay("2.0m"), None);
    }

    #[test]
    fn test_parse_delay_negative_number() {
        assert_eq!(parse_delay("-5s"), None);
    }

    #[test]
    fn test_parse_delay_no_unit_just_number() {
        // Last char is a digit, not a valid unit
        assert_eq!(parse_delay("10"), None);
    }

    #[test]
    fn test_parse_delay_multibyte_utf8_no_panic() {
        // Multi-byte UTF-8 unit should return None, not panic
        assert_eq!(parse_delay("30🎯"), None);
        assert_eq!(parse_delay("5ñ"), None);
        assert_eq!(parse_delay("10日"), None);
    }

    #[test]
    fn test_levenshtein() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("food", "foo"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn test_is_similar() {
        // Prefix matches
        assert!(is_similar("api", "api-design"));
        assert!(is_similar("api-design", "api"));

        // Edit distance <= 2
        assert!(is_similar("foo", "food"));
        assert!(is_similar("foo", "boo"));
        assert!(is_similar("abc", "axc"));

        // Too far apart
        assert!(!is_similar("abc", "xyz"));
        assert!(!is_similar("hello", "world"));
    }

    #[test]
    fn test_get_task_or_err_found() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("api-design", "Design API")));

        let task = graph.get_task_or_err("api-design").unwrap();
        assert_eq!(task.title, "Design API");
    }

    #[test]
    fn test_get_task_or_err_not_found_with_suggestion() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("api-design", "Design API")));

        let err = graph.get_task_or_err("api-desgin").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "should say not found: {}", msg);
        assert!(
            msg.contains("Did you mean 'api-design'?"),
            "should suggest api-design: {}",
            msg
        );
    }

    #[test]
    fn test_get_task_or_err_not_found_no_suggestion() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("api-design", "Design API")));

        let err = graph.get_task_or_err("zzz-totally-different").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "should say not found: {}", msg);
        assert!(
            !msg.contains("Did you mean"),
            "should not suggest anything: {}",
            msg
        );
    }

    #[test]
    fn test_get_task_mut_or_err_found() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("build-ui", "Build UI")));

        let task = graph.get_task_mut_or_err("build-ui").unwrap();
        task.title = "Build UI v2".to_string();

        assert_eq!(graph.get_task("build-ui").unwrap().title, "Build UI v2");
    }

    #[test]
    fn test_get_task_or_err_prefix_suggestion() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("api-design", "Design API")));
        graph.add_node(Node::Task(make_task("build-ui", "Build UI")));

        let err = graph.get_task_or_err("api").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Did you mean 'api-design'?"),
            "should suggest prefix match: {}",
            msg
        );
    }
}

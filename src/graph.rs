use chrono::{Duration, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

/// A loop edge: a conditional back-edge that can re-activate upstream tasks on completion.
/// Loop edges are NOT blocking edges — they are separate from `blocked_by` and don't affect
/// `ready_tasks()` or scheduling. They only fire when the source task completes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopEdge {
    /// Task ID to re-activate when this task completes
    pub target: String,
    /// Condition that must be true to loop (None = always loop, up to max_iterations)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<LoopGuard>,
    /// Hard cap on iterations (required — no unbounded loops)
    pub max_iterations: u32,
    /// How long to wait before re-activating the target (e.g. "30s", "5m", "1h", "24h").
    /// When set, loop firing sets target.ready_after = now + delay instead of making it immediately ready.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
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
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
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
    /// Back-edges that can re-activate upstream tasks on completion
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops_to: Vec<LoopEdge>,
    /// Current loop iteration (0 = first run, incremented on each re-activation)
    #[serde(default, skip_serializing_if = "is_zero")]
    pub loop_iteration: u32,
    /// Task is not ready until this timestamp (ISO 8601 / RFC 3339).
    /// Set by loop edges with a delay — prevents immediate dispatch after re-activation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_after: Option<String>,
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
    #[serde(default)]
    blocks: Vec<String>,
    #[serde(default)]
    blocked_by: Vec<String>,
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
    #[serde(default)]
    loops_to: Vec<LoopEdge>,
    #[serde(default)]
    loop_iteration: u32,
    #[serde(default)]
    ready_after: Option<String>,
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
            blocks: helper.blocks,
            blocked_by: helper.blocked_by,
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
            loops_to: helper.loops_to,
            loop_iteration: helper.loop_iteration,
            ready_after: helper.ready_after,
        })
    }
}

fn is_zero(val: &u32) -> bool {
    *val == 0
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

/// The work graph: a DAG of tasks and resources with embedded dependency edges.
///
/// Tasks depend on other tasks via `blocked_by`/`blocks` edges. Resources are
/// consumed by tasks via `requires` edges. The graph is persisted as JSONL
/// (one node per line) and supports concurrent readers via atomic writes.
#[derive(Debug, Clone, Default)]
pub struct WorkGraph {
    nodes: HashMap<String, Node>,
}

impl WorkGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    /// Insert a node (task or resource) into the graph.
    pub fn add_node(&mut self, node: Node) {
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
        match self.nodes.get_mut(id) {
            Some(Node::Task(t)) => Some(t),
            _ => None,
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
    /// (`blocked_by`, `blocks`, `requires`, and `loops_to` targets).
    pub fn remove_node(&mut self, id: &str) -> Option<Node> {
        let removed = self.nodes.remove(id);
        if removed.is_some() {
            for node in self.nodes.values_mut() {
                if let Node::Task(task) = node {
                    task.blocked_by.retain(|dep| dep != id);
                    task.blocks.retain(|dep| dep != id);
                    task.requires.retain(|dep| dep != id);
                    task.loops_to.retain(|edge| edge.target != id);
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
}

/// Evaluate a loop guard condition against the current graph state.
fn evaluate_guard(guard: &Option<LoopGuard>, graph: &WorkGraph) -> bool {
    match guard {
        None | Some(LoopGuard::Always) => true,
        // IterationLessThan is checked in evaluate_loop_edges() where the
        // target task's loop_iteration is available.
        Some(LoopGuard::IterationLessThan(_)) => true,
        Some(LoopGuard::TaskStatus { task, status }) => graph
            .get_task(task)
            .map(|t| t.status == *status)
            .unwrap_or(false),
    }
}

/// Evaluate loop edges after a task transitions to Done.
///
/// For each `LoopEdge` on the completed task:
/// 1. Check guard condition against current graph state.
/// 2. Check `target.loop_iteration < max_iterations`.
/// 3. If both true: re-open the target task (set status to Open, clear
///    assigned/started_at/completed_at, increment loop_iteration, optionally
///    set ready_after if the edge has a delay).
/// 4. Also re-open any intermediate tasks between source and target whose
///    blockers are no longer all Done (since the target was just re-opened).
///
/// Returns the list of task IDs that were re-activated.
pub fn evaluate_loop_edges(graph: &mut WorkGraph, source_id: &str) -> Vec<String> {
    // Collect loop edges from the source task (clone to avoid borrow issues)
    let loop_edges: Vec<LoopEdge> = match graph.get_task(source_id) {
        Some(task) => task.loops_to.clone(),
        None => return vec![],
    };

    let mut reactivated = Vec::new();

    for edge in &loop_edges {
        // 1. Check guard condition
        if !evaluate_guard(&edge.guard, graph) {
            continue;
        }

        // Also check IterationLessThan guard specifically
        if let Some(LoopGuard::IterationLessThan(n)) = &edge.guard {
            let current_iter = graph
                .get_task(&edge.target)
                .map(|t| t.loop_iteration)
                .unwrap_or(0);
            if current_iter >= *n {
                continue;
            }
        }

        // 2. Check iteration limit
        let current_iter = match graph.get_task(&edge.target) {
            Some(target) => target.loop_iteration,
            None => {
                eprintln!(
                    "Warning: loop target '{}' referenced by '{}' does not exist, skipping",
                    edge.target, source_id
                );
                continue;
            }
        };
        if current_iter >= edge.max_iterations {
            continue;
        }

        // 3. Re-activate the target task
        let new_iteration = current_iter + 1;
        let ready_after = edge.delay.as_ref().and_then(|d| match parse_delay(d) {
            Some(secs) if secs <= i64::MAX as u64 => {
                Some((Utc::now() + Duration::seconds(secs as i64)).to_rfc3339())
            }
            Some(secs) => {
                eprintln!(
                    "Warning: delay value {}s on loop edge {} → {} exceeds maximum, ignoring delay",
                    secs, source_id, edge.target
                );
                None
            }
            None => {
                eprintln!(
                    "Warning: invalid delay '{}' on loop edge {} → {}, ignoring delay",
                    d, source_id, edge.target
                );
                None
            }
        });

        let target_reactivated = if let Some(target) = graph.get_task_mut(&edge.target) {
            target.status = Status::Open;
            target.assigned = None;
            target.started_at = None;
            target.completed_at = None;
            target.loop_iteration = new_iteration;
            target.ready_after = ready_after;

            target.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                message: format!(
                    "Re-activated by loop from {} (iteration {}/{})",
                    source_id, new_iteration, edge.max_iterations
                ),
            });

            reactivated.push(edge.target.clone());
            true
        } else {
            false
        };

        // Only re-open intermediates and source if target was successfully reactivated
        if !target_reactivated {
            continue;
        }

        // 4. Re-open intermediate tasks between target and source whose
        //    blockers are no longer all Done.
        //
        //    We find tasks that transitively depend on the target (via blocked_by)
        //    and are between the target and source in the dependency chain.
        //    Per the design doc (section 2): intermediate tasks don't strictly
        //    need explicit re-opening because ready_tasks() won't mark them
        //    ready when their blocker is Open. However, if they were marked Done
        //    from a previous iteration, we should re-open them so they run again.
        let intermediates = find_intermediate_tasks(graph, &edge.target, source_id);
        for mid_id in &intermediates {
            if mid_id == source_id {
                continue; // Source is re-opened separately below
            }
            if let Some(mid_task) = graph.get_task_mut(mid_id)
                && mid_task.status == Status::Done
            {
                mid_task.status = Status::Open;
                mid_task.assigned = None;
                mid_task.started_at = None;
                mid_task.completed_at = None;
                mid_task.loop_iteration = new_iteration;

                mid_task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    message: format!(
                        "Re-opened: blocker '{}' was re-activated by loop from {}",
                        edge.target, source_id
                    ),
                });

                reactivated.push(mid_id.clone());
            }
        }
    }

    // 5. Re-open the source task itself so it runs again in the next
    //    iteration of the cycle.  The source was just marked Done by the
    //    caller, but it is part of the loop and must execute again.
    //    Only do this once even if multiple loop edges fired.
    if !reactivated.is_empty() {
        // Use the max iteration across all reactivated targets
        let max_iteration = reactivated
            .iter()
            .filter_map(|id| graph.get_task(id).map(|t| t.loop_iteration))
            .max()
            .unwrap_or(1);

        if let Some(src_task) = graph.get_task_mut(source_id) {
            src_task.status = Status::Open;
            src_task.assigned = None;
            src_task.started_at = None;
            src_task.completed_at = None;
            src_task.loop_iteration = max_iteration;

            src_task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                message: format!("Re-opened by own loop (iteration {})", max_iteration),
            });

            reactivated.push(source_id.to_string());
        }
    }

    reactivated
}

/// Find tasks that are on the dependency path between `from` (the loop target)
/// and `to` (the loop source). These are tasks that have `from` as a transitive
/// blocker and are themselves transitive blockers of `to`.
///
///
/// Uses two passes:
///   1. Forward BFS from `from` along dependents to find all tasks reachable from `from`.
///   2. Backward BFS from `to` along blocked_by to find all tasks that can reach `to`.
///
/// The intersection (excluding `from` and `to` themselves) gives the true intermediates.
fn find_intermediate_tasks(graph: &WorkGraph, from: &str, to: &str) -> Vec<String> {
    use std::collections::{HashSet, VecDeque};

    // Build reverse index: task_id -> list of tasks that are blocked_by it
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    for task in graph.tasks() {
        for blocker_id in &task.blocked_by {
            dependents
                .entry(blocker_id.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    // Pass 1: Forward BFS from `from` — find all tasks reachable via dependents
    let mut forward_reachable = HashSet::new();
    let mut queue = VecDeque::new();
    if let Some(deps) = dependents.get(from) {
        for dep in deps {
            if forward_reachable.insert(dep.clone()) {
                queue.push_back(dep.clone());
            }
        }
    }
    while let Some(current) = queue.pop_front() {
        if current == to {
            continue; // Don't traverse past the source
        }
        if let Some(deps) = dependents.get(&current) {
            for dep in deps {
                if forward_reachable.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    // Pass 2: Backward BFS from `to` — find all tasks that transitively block `to`
    let mut backward_reachable = HashSet::new();
    if let Some(task) = graph.get_task(to) {
        for blocker in &task.blocked_by {
            if backward_reachable.insert(blocker.clone()) {
                queue.push_back(blocker.clone());
            }
        }
    }
    while let Some(current) = queue.pop_front() {
        if current == from {
            continue; // Don't traverse past the target
        }
        if let Some(task) = graph.get_task(&current) {
            for blocker in &task.blocked_by {
                if backward_reachable.insert(blocker.clone()) {
                    queue.push_back(blocker.clone());
                }
            }
        }
    }

    // Intersection: tasks reachable from `from` AND that can reach `to`
    forward_reachable
        .into_iter()
        .filter(|id| id != from && id != to && backward_reachable.contains(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
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
        t2.blocked_by = vec!["t1".to_string()];
        t2.blocks = vec!["t1".to_string()];
        t2.requires = vec!["t1".to_string()];
        t2.loops_to = vec![LoopEdge {
            target: "t1".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];
        graph.add_node(Node::Task(t2));

        graph.remove_node("t1");

        let t2 = graph.get_task("t2").unwrap();
        assert!(t2.blocked_by.is_empty(), "blocked_by should be cleaned");
        assert!(t2.blocks.is_empty(), "blocks should be cleaned");
        assert!(t2.requires.is_empty(), "requires should be cleaned");
        assert!(t2.loops_to.is_empty(), "loops_to should be cleaned");
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
        task1.blocks = vec!["api-impl".to_string()];

        let mut task2 = make_task("api-impl", "Implement API");
        task2.blocked_by = vec!["api-design".to_string()];

        graph.add_node(Node::Task(task1));
        graph.add_node(Node::Task(task2));

        let design = graph.get_task("api-design").unwrap();
        assert_eq!(design.blocks, vec!["api-impl"]);

        let impl_task = graph.get_task("api-impl").unwrap();
        assert_eq!(impl_task.blocked_by, vec!["api-design"]);
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

    // ── find_intermediate_tasks tests ────────────────────────────────

    #[test]
    fn test_find_intermediate_empty_graph() {
        let graph = WorkGraph::new();
        let result = find_intermediate_tasks(&graph, "a", "b");
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_intermediate_linear_chain() {
        // A -> B -> C (B blocked_by A, C blocked_by B)
        // from=A, to=C: intermediate should be [B]
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.blocks = vec!["b".to_string()];
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        b.blocks = vec!["c".to_string()];
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["b".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let result = find_intermediate_tasks(&graph, "a", "c");
        assert_eq!(result, vec!["b"]);
    }

    #[test]
    fn test_find_intermediate_branching_dependencies() {
        // A -> B -> D, A -> C -> D
        // from=A, to=D: intermediates should contain B and C
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.blocks = vec!["b".to_string(), "c".to_string()];
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        b.blocks = vec!["d".to_string()];
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["a".to_string()];
        c.blocks = vec!["d".to_string()];
        let mut d = make_task("d", "D");
        d.blocked_by = vec!["b".to_string(), "c".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let mut result = find_intermediate_tasks(&graph, "a", "d");
        result.sort();
        assert_eq!(result, vec!["b", "c"]);
    }

    #[test]
    fn test_find_intermediate_no_path() {
        // A and B are independent tasks with no dependency path
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "A")));
        graph.add_node(Node::Task(make_task("b", "B")));

        let result = find_intermediate_tasks(&graph, "a", "b");
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_intermediate_nonexistent_nodes() {
        let graph = WorkGraph::new();
        let result = find_intermediate_tasks(&graph, "nonexistent-from", "nonexistent-to");
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_intermediate_missing_from_node() {
        // Only 'to' exists in the graph
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("b", "B")));

        let result = find_intermediate_tasks(&graph, "missing", "b");
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_intermediate_missing_to_node() {
        // Only 'from' exists, and it has a dependent that isn't 'to'
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.blocks = vec!["b".to_string()];
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));

        // Looking for path from a to "missing" — b is reachable from a but not on a
        // path to "missing" (which doesn't exist), so it should NOT be returned.
        let result = find_intermediate_tasks(&graph, "a", "missing");
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_intermediate_self_referential() {
        // from == to: should return nothing since the source is skipped
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.blocked_by = vec!["a".to_string()]; // self-dependency
        graph.add_node(Node::Task(a));

        let result = find_intermediate_tasks(&graph, "a", "a");
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_intermediate_diamond() {
        // Diamond: A -> B, A -> C, B -> D, C -> D
        // from=A, to=D: intermediates should be B and C
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.blocks = vec!["b".to_string(), "c".to_string()];
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        b.blocks = vec!["d".to_string()];
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["a".to_string()];
        c.blocks = vec!["d".to_string()];
        let mut d = make_task("d", "D");
        d.blocked_by = vec!["b".to_string(), "c".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let mut result = find_intermediate_tasks(&graph, "a", "d");
        result.sort();
        assert_eq!(result, vec!["b", "c"]);
    }

    #[test]
    fn test_find_intermediate_excludes_source_and_target() {
        // A -> B -> C: from=A, to=C
        // Result should be [B], not include A or C
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.blocks = vec!["b".to_string()];
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        b.blocks = vec!["c".to_string()];
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["b".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let result = find_intermediate_tasks(&graph, "a", "c");
        assert!(!result.contains(&"a".to_string()));
        assert!(!result.contains(&"c".to_string()));
        assert_eq!(result, vec!["b"]);
    }

    #[test]
    fn test_find_intermediate_direct_edge_no_intermediates() {
        // A -> B directly: from=A, to=B — no intermediates
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.blocks = vec!["b".to_string()];
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));

        let result = find_intermediate_tasks(&graph, "a", "b");
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_intermediate_longer_chain() {
        // A -> B -> C -> D -> E: from=A, to=E
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.blocks = vec!["b".to_string()];
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        b.blocks = vec!["c".to_string()];
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["b".to_string()];
        c.blocks = vec!["d".to_string()];
        let mut d = make_task("d", "D");
        d.blocked_by = vec!["c".to_string()];
        d.blocks = vec!["e".to_string()];
        let mut e = make_task("e", "E");
        e.blocked_by = vec!["d".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));
        graph.add_node(Node::Task(e));

        let result = find_intermediate_tasks(&graph, "a", "e");
        assert_eq!(result.len(), 3);
        assert!(result.contains(&"b".to_string()));
        assert!(result.contains(&"c".to_string()));
        assert!(result.contains(&"d".to_string()));
    }

    #[test]
    fn test_find_intermediate_excludes_unrelated_branch() {
        // Graph: a -> b -> c (loop from c back to a)
        //        a -> x -> y (branch NOT on path to c)
        // Only b should be returned, not x or y
        let mut graph = WorkGraph::new();

        let a = make_task("a", "A");
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["b".to_string()];
        let mut x = make_task("x", "X");
        x.blocked_by = vec!["a".to_string()];
        let mut y = make_task("y", "Y");
        y.blocked_by = vec!["x".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(x));
        graph.add_node(Node::Task(y));

        // Loop from c back to a — intermediates should only be b
        let result = find_intermediate_tasks(&graph, "a", "c");
        assert_eq!(result, vec!["b".to_string()]);
    }

    #[test]
    fn test_find_intermediate_loop_iteration_updated() {
        // Chain: tgt -> mid -> src (blocked_by direction)
        // Loop: src loops_to tgt
        // When src completes, tgt gets re-opened, mid (between tgt and src)
        // should also get re-opened with the correct loop_iteration.
        let mut graph = WorkGraph::new();

        let mut tgt = make_task("tgt", "Target");
        tgt.status = Status::Done;

        let mut mid = make_task("mid", "Middle");
        mid.blocked_by = vec!["tgt".to_string()];
        mid.status = Status::Done;

        let mut src = make_task("src", "Source");
        src.blocked_by = vec!["mid".to_string()];
        src.status = Status::Done;
        src.loops_to.push(LoopEdge {
            target: "tgt".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        });

        graph.add_node(Node::Task(tgt));
        graph.add_node(Node::Task(mid));
        graph.add_node(Node::Task(src));

        let reactivated = evaluate_loop_edges(&mut graph, "src");

        // All three should be re-opened
        assert!(reactivated.contains(&"tgt".to_string()));
        assert!(reactivated.contains(&"mid".to_string()));
        assert!(reactivated.contains(&"src".to_string()));

        // Mid should have the same loop_iteration as src and tgt (iteration 1)
        let mid = graph.get_task("mid").unwrap();
        assert_eq!(
            mid.loop_iteration, 1,
            "intermediate task should have updated loop_iteration"
        );
        assert_eq!(mid.status, Status::Open);

        let src = graph.get_task("src").unwrap();
        assert_eq!(src.loop_iteration, 1);

        let tgt = graph.get_task("tgt").unwrap();
        assert_eq!(tgt.loop_iteration, 1);
    }

    #[test]
    fn test_evaluate_loop_edges_multi_target_no_duplicate_source() {
        // Source task has two loop edges to different targets.
        // When both fire, the source should appear exactly once in reactivated.
        let mut graph = WorkGraph::new();

        let mut tgt_a = make_task("tgt-a", "Target A");
        tgt_a.status = Status::Done;

        let mut tgt_b = make_task("tgt-b", "Target B");
        tgt_b.status = Status::Done;

        let mut src = make_task("src", "Source");
        src.status = Status::Done;
        src.loops_to.push(LoopEdge {
            target: "tgt-a".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        });
        src.loops_to.push(LoopEdge {
            target: "tgt-b".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        });

        graph.add_node(Node::Task(tgt_a));
        graph.add_node(Node::Task(tgt_b));
        graph.add_node(Node::Task(src));

        let reactivated = evaluate_loop_edges(&mut graph, "src");

        // Source should appear exactly once, not twice
        let source_count = reactivated.iter().filter(|id| *id == "src").count();
        assert_eq!(
            source_count, 1,
            "source should appear exactly once in reactivated list, got {}",
            source_count
        );

        // Both targets should be reactivated
        assert!(reactivated.contains(&"tgt-a".to_string()));
        assert!(reactivated.contains(&"tgt-b".to_string()));

        // Source should be at iteration 1 (max of both targets)
        let src = graph.get_task("src").unwrap();
        assert_eq!(src.loop_iteration, 1);
        assert_eq!(src.status, Status::Open);
    }
}

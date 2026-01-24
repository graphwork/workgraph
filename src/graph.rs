use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// A task node
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

fn is_zero(val: &u32) -> bool {
    *val == 0
}

/// Trust level for an actor
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TrustLevel {
    /// Fully verified actor (human admin, proven agent)
    Verified,
    /// Provisionally trusted (new agent, limited permissions)
    #[default]
    Provisional,
    /// Unknown trust (external agent, needs verification)
    Unknown,
}

/// An actor (human or agent)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<f64>,
    /// Skills/capabilities this actor has (for task matching)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Maximum context size this actor can handle (in tokens)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
    /// Trust level for this actor
    #[serde(default, skip_serializing_if = "is_default_trust")]
    pub trust_level: TrustLevel,
    /// Last heartbeat timestamp (ISO 8601)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

fn is_default_trust(level: &TrustLevel) -> bool {
    *level == TrustLevel::Provisional
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

/// Node kind discriminator
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Task,
    Actor,
    Resource,
}

/// A node in the work graph (task, actor, or resource)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Node {
    Task(Task),
    Actor(Actor),
    Resource(Resource),
}

impl Node {
    pub fn id(&self) -> &str {
        match self {
            Node::Task(t) => &t.id,
            Node::Actor(a) => &a.id,
            Node::Resource(r) => &r.id,
        }
    }

    pub fn kind(&self) -> NodeKind {
        match self {
            Node::Task(_) => NodeKind::Task,
            Node::Actor(_) => NodeKind::Actor,
            Node::Resource(_) => NodeKind::Resource,
        }
    }
}

/// The work graph: a collection of nodes with embedded edges
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

    pub fn add_node(&mut self, node: Node) {
        self.nodes.insert(node.id().to_string(), node);
    }

    pub fn get_node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn get_task(&self, id: &str) -> Option<&Task> {
        match self.nodes.get(id) {
            Some(Node::Task(t)) => Some(t),
            _ => None,
        }
    }

    pub fn get_task_mut(&mut self, id: &str) -> Option<&mut Task> {
        match self.nodes.get_mut(id) {
            Some(Node::Task(t)) => Some(t),
            _ => None,
        }
    }

    pub fn get_actor(&self, id: &str) -> Option<&Actor> {
        match self.nodes.get(id) {
            Some(Node::Actor(a)) => Some(a),
            _ => None,
        }
    }

    pub fn get_actor_mut(&mut self, id: &str) -> Option<&mut Actor> {
        match self.nodes.get_mut(id) {
            Some(Node::Actor(a)) => Some(a),
            _ => None,
        }
    }

    pub fn get_resource(&self, id: &str) -> Option<&Resource> {
        match self.nodes.get(id) {
            Some(Node::Resource(r)) => Some(r),
            _ => None,
        }
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.nodes.values().filter_map(|n| match n {
            Node::Task(t) => Some(t),
            _ => None,
        })
    }

    pub fn actors(&self) -> impl Iterator<Item = &Actor> {
        self.nodes.values().filter_map(|n| match n {
            Node::Actor(a) => Some(a),
            _ => None,
        })
    }

    pub fn resources(&self) -> impl Iterator<Item = &Resource> {
        self.nodes.values().filter_map(|n| match n {
            Node::Resource(r) => Some(r),
            _ => None,
        })
    }

    pub fn remove_node(&mut self, id: &str) -> Option<Node> {
        self.nodes.remove(id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
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
        }
    }

    fn make_actor(id: &str, name: &str) -> Actor {
        Actor {
            id: id.to_string(),
            name: Some(name.to_string()),
            role: None,
            rate: None,
            capacity: None,
            capabilities: vec![],
            context_limit: None,
            trust_level: TrustLevel::Provisional,
            last_seen: None,
        }
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
    fn test_add_and_get_actor() {
        let mut graph = WorkGraph::new();
        let actor = make_actor("erik", "Erik");
        graph.add_node(Node::Actor(actor));

        let retrieved = graph.get_actor("erik").unwrap();
        assert_eq!(retrieved.name, Some("Erik".to_string()));
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
    fn test_tasks_iterator() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        graph.add_node(Node::Task(make_task("t2", "Task 2")));
        graph.add_node(Node::Actor(make_actor("a1", "Actor 1")));

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
        task.deliverables = vec![
            "src/feature.rs".to_string(),
            "docs/feature.md".to_string(),
        ];

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
}

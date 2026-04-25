//! Agent Registry
//!
//! Manages the registry of running agents.
//! Lives at `.workgraph/service/registry.json`
//!
//! Features:
//! - Store agent info: id, pid, task_id, executor type, started_at, last_heartbeat, status, output_file
//! - Atomic file operations via write-to-temp-then-rename
//! - File locking via `load_locked()` for all write paths
//! - Auto-increment agent IDs (agent-1, agent-2, etc.)
//!
//! # Lock hierarchy
//!
//! When multiple locks must be held, acquire them in this order to prevent deadlocks:
//!
//! 1. **Graph lock** (`graph.lock`) — acquired per-call by `load_graph()`/`save_graph()`
//! 2. **Registry lock** (`.workgraph/service/.registry.lock`) — held via `LockedRegistry`
//!
//! The graph lock is acquired and released within each `load_graph()`/`save_graph()` call,
//! so it is safe to hold the registry lock while calling graph operations. Never acquire
//! the registry lock from within a graph lock callback.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Agent status in the registry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum AgentStatus {
    /// Agent is starting up
    Starting,
    /// Agent is actively working
    Working,
    /// Agent is idle, waiting for work
    Idle,
    /// Agent is stopping gracefully
    Stopping,
    /// Agent voluntarily parked via `wg wait` (exited cleanly, task is Waiting)
    Parked,
    /// Agent is frozen via SIGSTOP (process stopped but in memory)
    Frozen,
    /// Agent has completed its task
    Done,
    /// Agent failed
    Failed,
    /// Agent is dead (no heartbeat)
    Dead,
}

/// Entry for a single agent in the registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    /// Unique agent ID (e.g., "agent-7")
    pub id: String,
    /// Process ID of the agent
    pub pid: u32,
    /// Task the agent is working on
    pub task_id: String,
    /// Executor type used (e.g., "claude", "shell")
    pub executor: String,
    /// When the agent was started (ISO 8601)
    pub started_at: String,
    /// Last heartbeat timestamp (ISO 8601)
    pub last_heartbeat: String,
    /// Current status
    pub status: AgentStatus,
    /// Path to the agent's output log file
    pub output_file: String,
    /// Model used for this agent (e.g., "anthropic/claude-opus-4-latest")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// When the agent finished (ISO 8601), set on transition to Done/Failed/Dead
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

impl AgentEntry {
    /// Check if the agent is considered alive (can still work)
    pub fn is_alive(&self) -> bool {
        matches!(
            self.status,
            AgentStatus::Starting | AgentStatus::Working | AgentStatus::Idle
        )
    }

    /// Strict liveness check — the agent is considered *live* if and
    /// only if ALL of the following hold:
    ///
    /// 1. The status is alive (`Starting | Working | Idle`), AND
    /// 2. The underlying process exists (PID is valid and running), AND
    /// 3. The last heartbeat is fresh (within `heartbeat_timeout_secs`).
    ///
    /// This is the invariant used by worktree cleanup paths: a worktree
    /// is safe to remove only when its owning agent is **not** live by
    /// this definition. The status-only `is_alive()` check is too loose
    /// because an agent can crash with the registry still reporting
    /// `Working` — leaving its worktree incorrectly protected forever.
    /// Conversely, just-process-alive is also too loose because a
    /// zombie/orphan process can exist with stale heartbeat long after
    /// real work stopped.
    pub fn is_live(&self, heartbeat_timeout_secs: u64) -> bool {
        if !self.is_alive() {
            return false;
        }
        if !super::is_process_alive(self.pid) {
            return false;
        }
        match self.seconds_since_heartbeat() {
            Some(secs) if secs >= 0 && (secs as u64) <= heartbeat_timeout_secs => true,
            // Invalid timestamp, future-dated heartbeat, or stale: not live.
            _ => false,
        }
    }

    /// Calculate uptime in seconds from started_at to now
    pub fn uptime_secs(&self) -> Option<i64> {
        let started = DateTime::parse_from_rfc3339(&self.started_at).ok()?;
        let now = Utc::now();
        Some((now - started.with_timezone(&Utc)).num_seconds())
    }

    /// Format uptime as human-readable string (e.g., "5m", "2h", "1d")
    pub fn uptime_human(&self) -> String {
        match self.uptime_secs() {
            Some(secs) if secs < 0 => "0s".to_string(),
            Some(secs) => crate::format_duration(secs, true),
            None => "unknown".to_string(),
        }
    }

    /// Seconds since last heartbeat
    pub fn seconds_since_heartbeat(&self) -> Option<i64> {
        let last = DateTime::parse_from_rfc3339(&self.last_heartbeat).ok()?;
        let now = Utc::now();
        Some((now - last.with_timezone(&Utc)).num_seconds())
    }
}

/// The agent registry - tracks all running agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegistry {
    /// Map of agent ID to agent entry
    pub agents: HashMap<String, AgentEntry>,
    /// Next agent ID to assign
    pub next_agent_id: u32,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self {
            agents: HashMap::new(),
            next_agent_id: 1,
        }
    }
}

impl AgentRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the path to the registry file
    pub fn registry_path(workgraph_dir: &Path) -> PathBuf {
        workgraph_dir.join("service").join("registry.json")
    }

    /// Load registry from disk, creating a new one if it doesn't exist
    pub fn load(workgraph_dir: &Path) -> Result<Self> {
        let path = Self::registry_path(workgraph_dir);

        if !path.exists() {
            return Ok(Self::new());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read registry from {:?}", path))?;

        let registry: AgentRegistry = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse registry from {:?}", path))?;

        Ok(registry)
    }

    /// Load registry from disk, falling back to empty with a warning on errors.
    ///
    /// Unlike `.load().unwrap_or_default()`, this emits a stderr warning
    /// when the registry file exists but is corrupt.
    pub fn load_or_warn(workgraph_dir: &Path) -> Self {
        match Self::load(workgraph_dir) {
            Ok(registry) => registry,
            Err(e) => {
                eprintln!("Warning: {}, using empty registry", e);
                Self::new()
            }
        }
    }

    /// Save registry to disk atomically
    ///
    /// Uses a write-to-temp-then-rename strategy for atomic updates.
    /// This ensures the registry file is never left in a corrupted state.
    pub fn save(&self, workgraph_dir: &Path) -> Result<()> {
        let path = Self::registry_path(workgraph_dir);
        let service_dir = workgraph_dir.join("service");

        // Create service directory if it doesn't exist
        if !service_dir.exists() {
            fs::create_dir_all(&service_dir).with_context(|| {
                format!("Failed to create service directory at {:?}", service_dir)
            })?;
        }

        let content = serde_json::to_string_pretty(self).context("Failed to serialize registry")?;

        // Write to temporary file first
        let temp_path = service_dir.join(".registry.json.tmp");
        {
            let mut file = File::create(&temp_path)
                .with_context(|| format!("Failed to create temp file at {:?}", temp_path))?;
            file.write_all(content.as_bytes())
                .context("Failed to write to temp file")?;
            file.sync_all().context("Failed to sync temp file")?;
        }

        // Atomic rename
        fs::rename(&temp_path, &path)
            .with_context(|| format!("Failed to rename temp file to {:?}", path))?;

        Ok(())
    }

    /// Load the registry with a file lock for concurrent access
    ///
    /// This acquires an exclusive lock before reading. The lock is released
    /// when the returned LockedRegistry is dropped or saved.
    #[cfg(unix)]
    pub fn load_locked(workgraph_dir: &Path) -> Result<LockedRegistry> {
        use std::fs::OpenOptions;
        use std::os::unix::io::AsRawFd;

        let service_dir = workgraph_dir.join("service");

        // Ensure service directory exists
        if !service_dir.exists() {
            fs::create_dir_all(&service_dir).with_context(|| {
                format!("Failed to create service directory at {:?}", service_dir)
            })?;
        }

        let lock_path = service_dir.join(".registry.lock");

        // Open/create lock file
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .open(&lock_path)
            .with_context(|| format!("Failed to open lock file at {:?}", lock_path))?;

        // Acquire exclusive lock
        let fd = lock_file.as_raw_fd();
        let result = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if result != 0 {
            anyhow::bail!(
                "Failed to acquire lock: {}",
                std::io::Error::last_os_error()
            );
        }

        let registry = Self::load(workgraph_dir)?;

        Ok(LockedRegistry {
            registry,
            workgraph_dir: workgraph_dir.to_path_buf(),
            _lock_file: lock_file,
        })
    }

    /// Load the registry with a file lock (non-Unix fallback - no actual locking)
    #[cfg(not(unix))]
    pub fn load_locked(workgraph_dir: &Path) -> Result<LockedRegistry> {
        use std::fs::OpenOptions;

        let service_dir = workgraph_dir.join("service");

        if !service_dir.exists() {
            fs::create_dir_all(&service_dir)?;
        }

        let lock_path = service_dir.join(".registry.lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        let registry = Self::load(workgraph_dir)?;

        Ok(LockedRegistry {
            registry,
            workgraph_dir: workgraph_dir.to_path_buf(),
            _lock_file: lock_file,
        })
    }

    /// Register a new agent, returning the assigned agent ID
    ///
    /// The agent ID is auto-incremented (agent-1, agent-2, etc.)
    pub fn register_agent(
        &mut self,
        pid: u32,
        task_id: &str,
        executor: &str,
        output_file: &str,
    ) -> String {
        self.register_agent_with_model(pid, task_id, executor, output_file, None)
    }

    /// Register a new agent with an optional model, returning the assigned agent ID
    pub fn register_agent_with_model(
        &mut self,
        pid: u32,
        task_id: &str,
        executor: &str,
        output_file: &str,
        model: Option<&str>,
    ) -> String {
        let agent_id = format!("agent-{}", self.next_agent_id);
        self.next_agent_id = self.next_agent_id.saturating_add(1);

        let now = chrono::Utc::now().to_rfc3339();

        let entry = AgentEntry {
            id: agent_id.clone(),
            pid,
            task_id: task_id.to_string(),
            executor: executor.to_string(),
            started_at: now.clone(),
            last_heartbeat: now,
            status: AgentStatus::Working,
            output_file: output_file.to_string(),
            model: model.map(std::string::ToString::to_string),
            completed_at: None,
        };

        self.agents.insert(agent_id.clone(), entry);
        agent_id
    }

    /// Get an agent by ID
    pub fn get_agent(&self, agent_id: &str) -> Option<&AgentEntry> {
        self.agents.get(agent_id)
    }

    /// Get a mutable reference to an agent by ID
    pub fn get_agent_mut(&mut self, agent_id: &str) -> Option<&mut AgentEntry> {
        self.agents.get_mut(agent_id)
    }

    /// Unregister an agent (remove from registry)
    ///
    /// Returns the removed agent entry, or None if not found.
    pub fn unregister_agent(&mut self, agent_id: &str) -> Option<AgentEntry> {
        self.agents.remove(agent_id)
    }

    /// Get all agents
    pub fn all(&self) -> impl Iterator<Item = &AgentEntry> {
        self.agents.values()
    }

    /// List all agents as a Vec
    pub fn list_agents(&self) -> Vec<&AgentEntry> {
        self.agents.values().collect()
    }

    /// List all alive agents (starting, working, or idle)
    pub fn list_alive_agents(&self) -> Vec<&AgentEntry> {
        self.agents.values().filter(|a| a.is_alive()).collect()
    }

    /// Get agents working on a specific task
    #[cfg(test)]
    pub fn agents_for_task(&self, task_id: &str) -> Vec<&AgentEntry> {
        self.agents
            .values()
            .filter(|a| a.task_id == task_id)
            .collect()
    }

    /// Get agent by task ID (returns first match)
    pub fn get_agent_by_task(&self, task_id: &str) -> Option<&AgentEntry> {
        self.agents.values().find(|a| a.task_id == task_id)
    }

    /// Get mutable agent by task ID (returns first match)
    pub fn get_agent_by_task_mut(&mut self, task_id: &str) -> Option<&mut AgentEntry> {
        self.agents.values_mut().find(|a| a.task_id == task_id)
    }

    /// Update heartbeat for an agent
    pub fn heartbeat(&mut self, agent_id: &str) -> bool {
        if let Some(agent) = self.agents.get_mut(agent_id) {
            agent.last_heartbeat = chrono::Utc::now().to_rfc3339();
            true
        } else {
            false
        }
    }

    /// Update heartbeat for an agent (returns Result for consistency)
    pub fn update_heartbeat(&mut self, agent_id: &str) -> Result<()> {
        if self.heartbeat(agent_id) {
            Ok(())
        } else {
            anyhow::bail!("Agent not found: {}", agent_id)
        }
    }

    /// Update agent status
    pub fn set_status(&mut self, agent_id: &str, status: AgentStatus) -> bool {
        if let Some(agent) = self.agents.get_mut(agent_id) {
            agent.status = status;
            // Record completion time on terminal transitions
            if matches!(
                status,
                AgentStatus::Parked | AgentStatus::Done | AgentStatus::Failed | AgentStatus::Dead
            ) && agent.completed_at.is_none()
            {
                agent.completed_at = Some(chrono::Utc::now().to_rfc3339());
            }
            true
        } else {
            false
        }
    }

    /// Update agent status (returns Result for consistency)
    pub fn update_status(&mut self, agent_id: &str, status: AgentStatus) -> Result<()> {
        if self.set_status(agent_id, status) {
            Ok(())
        } else {
            anyhow::bail!("Agent not found: {}", agent_id)
        }
    }

    /// Find agents that have exceeded the heartbeat timeout
    pub fn find_dead_agents(&self, timeout_secs: i64) -> Vec<&AgentEntry> {
        self.agents
            .values()
            .filter(|a| {
                a.is_alive()
                    && a.seconds_since_heartbeat()
                        .map(|s| s > timeout_secs)
                        .unwrap_or(true)
            })
            .collect()
    }

    /// Mark agents as dead if they've exceeded the heartbeat timeout
    ///
    /// Returns the IDs of agents that were marked as dead.
    pub fn mark_dead_agents(&mut self, timeout_secs: i64) -> Vec<String> {
        let dead_ids: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| {
                a.is_alive()
                    && a.seconds_since_heartbeat()
                        .map(|s| s > timeout_secs)
                        .unwrap_or(true)
            })
            .map(|(id, _)| id.clone())
            .collect();

        let now = chrono::Utc::now().to_rfc3339();
        for id in &dead_ids {
            if let Some(agent) = self.agents.get_mut(id) {
                agent.status = AgentStatus::Dead;
                if agent.completed_at.is_none() {
                    agent.completed_at = Some(now.clone());
                }
            }
        }

        dead_ids
    }

    /// Count agents by status
    #[cfg(test)]
    pub fn count_by_status(&self) -> HashMap<AgentStatus, usize> {
        let mut counts = HashMap::new();
        for agent in self.agents.values() {
            *counts.entry(agent.status).or_insert(0) += 1;
        }
        counts
    }

    /// Get count of active (alive) agents
    pub fn active_count(&self) -> usize {
        self.agents.values().filter(|a| a.is_alive()).count()
    }

    /// Get count of idle agents
    pub fn idle_count(&self) -> usize {
        self.agents
            .values()
            .filter(|a| a.status == AgentStatus::Idle)
            .count()
    }
}

/// A registry with an active file lock
///
/// The lock is released when this struct is dropped.
pub struct LockedRegistry {
    pub registry: AgentRegistry,
    workgraph_dir: PathBuf,
    _lock_file: File,
}

impl LockedRegistry {
    /// Save the registry and release the lock
    pub fn save(self) -> Result<()> {
        self.registry.save(&self.workgraph_dir)
    }

    /// Save the registry without consuming self (lock remains held)
    pub fn save_ref(&self) -> Result<()> {
        self.registry.save(&self.workgraph_dir)
    }
}

impl std::ops::Deref for LockedRegistry {
    type Target = AgentRegistry;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl std::ops::DerefMut for LockedRegistry {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_new_registry() {
        let registry = AgentRegistry::new();
        assert!(registry.agents.is_empty());
        assert_eq!(registry.next_agent_id, 1);
    }

    #[test]
    fn test_is_live_requires_all_three_invariants() {
        // Unix: use a PID that cannot exist (the reaper's PID-0 trick:
        // on Linux pid 0 is invalid for `kill(pid, 0)`, returning ESRCH.
        // We use a more portable approach — pick a PID very unlikely to
        // be alive, then verify the check fails.

        // Case 1: status Done + fresh heartbeat + any PID → NOT live
        // (is_alive() fails first)
        let entry = AgentEntry {
            id: "agent-test".to_string(),
            pid: std::process::id(), // our own PID — definitely alive
            task_id: "t".to_string(),
            executor: "claude".to_string(),
            started_at: Utc::now().to_rfc3339(),
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Done,
            output_file: "/tmp/out".to_string(),
            model: None,
            completed_at: None,
        };
        assert!(!entry.is_live(300), "Done status should not be live");

        // Case 2: status Working + fresh heartbeat + own PID → LIVE
        let entry = AgentEntry {
            id: "agent-test".to_string(),
            pid: std::process::id(),
            task_id: "t".to_string(),
            executor: "claude".to_string(),
            started_at: Utc::now().to_rfc3339(),
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: "/tmp/out".to_string(),
            model: None,
            completed_at: None,
        };
        assert!(
            entry.is_live(300),
            "Working status + own PID + fresh heartbeat should be live"
        );

        // Case 3: status Working + own PID + STALE heartbeat → NOT live
        // (3600 seconds ago, timeout 300)
        let stale_heartbeat = (Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339();
        let entry = AgentEntry {
            id: "agent-test".to_string(),
            pid: std::process::id(),
            task_id: "t".to_string(),
            executor: "claude".to_string(),
            started_at: Utc::now().to_rfc3339(),
            last_heartbeat: stale_heartbeat,
            status: AgentStatus::Working,
            output_file: "/tmp/out".to_string(),
            model: None,
            completed_at: None,
        };
        assert!(
            !entry.is_live(300),
            "stale heartbeat should fail liveness even if status+process ok"
        );

        // Case 4: status Working + fresh heartbeat + DEFINITELY DEAD PID → NOT live
        // PID 0x7FFF_FFFE is extremely unlikely to be in use (near PID_MAX)
        let entry = AgentEntry {
            id: "agent-test".to_string(),
            pid: 0x7FFF_FFFE,
            task_id: "t".to_string(),
            executor: "claude".to_string(),
            started_at: Utc::now().to_rfc3339(),
            last_heartbeat: Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: "/tmp/out".to_string(),
            model: None,
            completed_at: None,
        };
        assert!(
            !entry.is_live(300),
            "dead PID should fail liveness even if status+heartbeat ok"
        );

        // Case 5: timeout of 0 → even a just-written heartbeat may fail.
        // Use a heartbeat 1 second ago with timeout 0 → not live.
        let past = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        let entry = AgentEntry {
            id: "agent-test".to_string(),
            pid: std::process::id(),
            task_id: "t".to_string(),
            executor: "claude".to_string(),
            started_at: Utc::now().to_rfc3339(),
            last_heartbeat: past,
            status: AgentStatus::Working,
            output_file: "/tmp/out".to_string(),
            model: None,
            completed_at: None,
        };
        assert!(
            !entry.is_live(0),
            "timeout=0 should reject even 1-second-old heartbeat"
        );
    }

    #[test]
    fn test_register_agent() {
        let mut registry = AgentRegistry::new();

        let agent_id = registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");
        assert_eq!(agent_id, "agent-1");
        assert_eq!(registry.next_agent_id, 2);

        let agent = registry.get_agent(&agent_id).unwrap();
        assert_eq!(agent.pid, 12345);
        assert_eq!(agent.task_id, "task-1");
        assert_eq!(agent.executor, "claude");
        assert_eq!(agent.status, AgentStatus::Working);
    }

    #[test]
    fn test_register_multiple_agents() {
        let mut registry = AgentRegistry::new();

        let id1 = registry.register_agent(111, "task-1", "claude", "/tmp/1.log");
        let id2 = registry.register_agent(222, "task-2", "shell", "/tmp/2.log");
        let id3 = registry.register_agent(333, "task-3", "claude", "/tmp/3.log");

        assert_eq!(id1, "agent-1");
        assert_eq!(id2, "agent-2");
        assert_eq!(id3, "agent-3");
        assert_eq!(registry.agents.len(), 3);
    }

    #[test]
    fn test_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let mut registry = AgentRegistry::new();

        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");
        registry.save(temp_dir.path()).unwrap();

        // Verify file exists
        let path = AgentRegistry::registry_path(temp_dir.path());
        assert!(path.exists());

        let loaded = AgentRegistry::load(temp_dir.path()).unwrap();
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.next_agent_id, 2);

        let agent = loaded.get_agent("agent-1").unwrap();
        assert_eq!(agent.task_id, "task-1");
    }

    #[test]
    fn test_atomic_save() {
        let temp_dir = TempDir::new().unwrap();
        let mut registry = AgentRegistry::new();

        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");
        registry.save(temp_dir.path()).unwrap();

        // Temp file should not exist after save
        let temp_path = temp_dir.path().join("service").join(".registry.json.tmp");
        assert!(!temp_path.exists());

        // Registry file should exist
        let path = AgentRegistry::registry_path(temp_dir.path());
        assert!(path.exists());
    }

    #[test]
    fn test_load_missing_registry() {
        let temp_dir = TempDir::new().unwrap();
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        assert!(registry.agents.is_empty());
    }

    #[test]
    fn test_unregister_agent() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");

        let removed = registry.unregister_agent("agent-1");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().pid, 12345);
        assert!(registry.agents.is_empty());

        // Unregistering again should return None
        assert!(registry.unregister_agent("agent-1").is_none());
    }

    #[test]
    fn test_list_agents() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-1", "claude", "/tmp/1.log");
        registry.register_agent(222, "task-2", "shell", "/tmp/2.log");

        let agents = registry.list_agents();
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn test_agents_for_task() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-1", "claude", "/tmp/1.log");
        registry.register_agent(222, "task-2", "shell", "/tmp/2.log");
        registry.register_agent(333, "task-1", "claude", "/tmp/3.log");

        let agents = registry.agents_for_task("task-1");
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn test_get_agent_by_task() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-a", "claude", "/tmp/1.log");
        registry.register_agent(222, "task-b", "shell", "/tmp/2.log");

        let agent = registry.get_agent_by_task("task-b").unwrap();
        assert_eq!(agent.id, "agent-2");
        assert_eq!(agent.pid, 222);

        assert!(registry.get_agent_by_task("task-c").is_none());
    }

    #[test]
    fn test_update_heartbeat() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");

        let original_hb = registry
            .get_agent("agent-1")
            .unwrap()
            .last_heartbeat
            .clone();
        std::thread::sleep(std::time::Duration::from_millis(10));

        registry.update_heartbeat("agent-1").unwrap();

        let new_hb = registry
            .get_agent("agent-1")
            .unwrap()
            .last_heartbeat
            .clone();
        assert_ne!(original_hb, new_hb);
    }

    #[test]
    fn test_update_heartbeat_missing() {
        let mut registry = AgentRegistry::new();
        let result = registry.update_heartbeat("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_update_status() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");

        registry
            .update_status("agent-1", AgentStatus::Working)
            .unwrap();

        let agent = registry.get_agent("agent-1").unwrap();
        assert_eq!(agent.status, AgentStatus::Working);
    }

    #[test]
    fn test_update_status_missing() {
        let mut registry = AgentRegistry::new();
        let result = registry.update_status("nonexistent", AgentStatus::Dead);
        assert!(result.is_err());
    }

    #[test]
    fn test_agent_is_alive() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");

        // Starting is alive
        assert!(registry.get_agent("agent-1").unwrap().is_alive());

        // Working is alive
        registry.set_status("agent-1", AgentStatus::Working);
        assert!(registry.get_agent("agent-1").unwrap().is_alive());

        // Idle is alive
        registry.set_status("agent-1", AgentStatus::Idle);
        assert!(registry.get_agent("agent-1").unwrap().is_alive());

        // Dead is not alive
        registry.set_status("agent-1", AgentStatus::Dead);
        assert!(!registry.get_agent("agent-1").unwrap().is_alive());

        // Done is not alive
        registry.set_status("agent-1", AgentStatus::Done);
        assert!(!registry.get_agent("agent-1").unwrap().is_alive());
    }

    #[test]
    fn test_list_alive_agents() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-1", "claude", "/tmp/1.log");
        registry.register_agent(222, "task-2", "shell", "/tmp/2.log");
        registry.register_agent(333, "task-3", "custom", "/tmp/3.log");

        // Mark one as dead
        registry.set_status("agent-2", AgentStatus::Dead);

        let alive = registry.list_alive_agents();
        assert_eq!(alive.len(), 2);
    }

    #[test]
    fn test_find_dead_agents() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-1", "claude", "/tmp/1.log");

        // With a very large timeout, no agents should be dead
        let dead = registry.find_dead_agents(3600);
        assert!(dead.is_empty());

        // Manually set an old heartbeat timestamp to simulate a dead agent
        if let Some(agent) = registry.get_agent_mut("agent-1") {
            agent.last_heartbeat = "2020-01-01T00:00:00Z".to_string();
        }

        // Now with a 60-second timeout, the agent should be detected as dead
        let dead = registry.find_dead_agents(60);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].id, "agent-1");
    }

    #[test]
    fn test_mark_dead_agents() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-1", "claude", "/tmp/1.log");
        registry.register_agent(222, "task-2", "shell", "/tmp/2.log");

        // Manually set old heartbeat timestamps to simulate dead agents
        if let Some(agent) = registry.get_agent_mut("agent-1") {
            agent.last_heartbeat = "2020-01-01T00:00:00Z".to_string();
        }
        if let Some(agent) = registry.get_agent_mut("agent-2") {
            agent.last_heartbeat = "2020-01-01T00:00:00Z".to_string();
        }

        let dead_ids = registry.mark_dead_agents(60);
        assert_eq!(dead_ids.len(), 2);

        // Both should now be marked as dead
        assert_eq!(
            registry.get_agent("agent-1").unwrap().status,
            AgentStatus::Dead
        );
        assert_eq!(
            registry.get_agent("agent-2").unwrap().status,
            AgentStatus::Dead
        );
    }

    #[test]
    fn test_mark_dead_agents_excludes_already_dead() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-1", "claude", "/tmp/1.log");

        // Set old heartbeat
        if let Some(agent) = registry.get_agent_mut("agent-1") {
            agent.last_heartbeat = "2020-01-01T00:00:00Z".to_string();
        }

        // Mark as dead
        let dead_ids = registry.mark_dead_agents(60);
        assert_eq!(dead_ids.len(), 1);

        // Calling again should not find any new dead agents
        let dead_ids = registry.mark_dead_agents(60);
        assert!(dead_ids.is_empty());
    }

    #[test]
    fn test_count_by_status() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-1", "claude", "/tmp/1.log");
        registry.register_agent(222, "task-2", "shell", "/tmp/2.log");
        registry.register_agent(333, "task-3", "custom", "/tmp/3.log");

        registry.set_status("agent-1", AgentStatus::Working);
        registry.set_status("agent-2", AgentStatus::Working);
        registry.set_status("agent-3", AgentStatus::Idle);

        let counts = registry.count_by_status();
        assert_eq!(counts.get(&AgentStatus::Working), Some(&2));
        assert_eq!(counts.get(&AgentStatus::Idle), Some(&1));
    }

    #[test]
    fn test_active_and_idle_count() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(111, "task-1", "claude", "/tmp/1.log");
        registry.register_agent(222, "task-2", "shell", "/tmp/2.log");
        registry.register_agent(333, "task-3", "custom", "/tmp/3.log");

        registry.set_status("agent-1", AgentStatus::Working);
        registry.set_status("agent-2", AgentStatus::Idle);
        registry.set_status("agent-3", AgentStatus::Dead);

        assert_eq!(registry.active_count(), 2); // Working and Idle are alive
        assert_eq!(registry.idle_count(), 1);
    }

    #[test]
    fn test_agent_uptime() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");

        let agent = registry.get_agent("agent-1").unwrap();
        let uptime = agent.uptime_secs().unwrap();
        assert!((0..5).contains(&uptime)); // Should be nearly instant

        let human = agent.uptime_human();
        assert!(human.ends_with('s')); // Less than a minute
    }

    #[test]
    fn test_seconds_since_heartbeat() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");

        let agent = registry.get_agent("agent-1").unwrap();
        let secs = agent.seconds_since_heartbeat().unwrap();
        assert!((0..5).contains(&secs));
    }

    #[test]
    fn test_locked_registry() {
        let temp_dir = TempDir::new().unwrap();

        {
            let mut locked = AgentRegistry::load_locked(temp_dir.path()).unwrap();
            locked.register_agent(12345, "task-1", "claude", "/tmp/output.log");
            locked.save().unwrap();
        } // Lock released here

        // Verify changes persisted
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        assert_eq!(registry.agents.len(), 1);
    }

    #[test]
    fn test_locked_registry_save_ref() {
        let temp_dir = TempDir::new().unwrap();

        let mut locked = AgentRegistry::load_locked(temp_dir.path()).unwrap();
        locked.register_agent(12345, "task-1", "claude", "/tmp/output.log");
        locked.save_ref().unwrap();

        // Can still access registry
        locked.register_agent(12346, "task-2", "shell", "/tmp/output2.log");
        locked.save().unwrap();

        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        assert_eq!(registry.agents.len(), 2);
    }

    #[test]
    fn test_registry_serialization() {
        let mut registry = AgentRegistry::new();
        registry.register_agent(
            12345,
            "implement-feature",
            "claude",
            ".workgraph/agents/agent-1/output.log",
        );

        let json = serde_json::to_string_pretty(&registry).unwrap();

        // Verify expected structure
        assert!(json.contains("\"agents\""));
        assert!(json.contains("\"next_agent_id\": 2"));
        assert!(json.contains("\"id\": \"agent-1\""));
        assert!(json.contains("\"pid\": 12345"));
        assert!(json.contains("\"task_id\": \"implement-feature\""));
        assert!(json.contains("\"executor\": \"claude\""));
        assert!(json.contains("\"status\": \"working\""));
    }

    #[test]
    fn test_registry_deserialization() {
        let json = r#"{
            "agents": {
                "agent-1": {
                    "id": "agent-1",
                    "pid": 54321,
                    "task_id": "implement-feature",
                    "executor": "claude",
                    "started_at": "2026-01-27T10:00:00Z",
                    "last_heartbeat": "2026-01-27T10:12:00Z",
                    "status": "working",
                    "output_file": ".workgraph/agents/agent-1/output.log"
                }
            },
            "next_agent_id": 8
        }"#;

        let registry: AgentRegistry = serde_json::from_str(json).unwrap();
        assert_eq!(registry.next_agent_id, 8);
        assert_eq!(registry.agents.len(), 1);

        let agent = registry.get_agent("agent-1").unwrap();
        assert_eq!(agent.pid, 54321);
        assert_eq!(agent.task_id, "implement-feature");
        assert_eq!(agent.status, AgentStatus::Working);
    }

    #[test]
    fn test_agent_id_overflow_saturates() {
        let mut registry = AgentRegistry::new();
        registry.next_agent_id = u32::MAX;

        let id = registry.register_agent(111, "task-1", "claude", "/tmp/1.log");
        assert_eq!(id, format!("agent-{}", u32::MAX));

        // Should saturate at MAX, not wrap to 0
        assert_eq!(registry.next_agent_id, u32::MAX);
    }

    #[test]
    fn test_locked_registry_blocks_concurrent_access() {
        use std::sync::{Arc, Barrier};
        use std::time::{Duration, Instant};

        let temp_dir = TempDir::new().unwrap();

        // Pre-create the service directory
        std::fs::create_dir_all(temp_dir.path().join("service")).unwrap();

        let path = Arc::new(temp_dir.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(2));

        let path2 = Arc::clone(&path);
        let barrier2 = Arc::clone(&barrier);

        // Thread 1: acquire lock, hold it for 200ms
        let t1 = std::thread::spawn(move || {
            let locked = AgentRegistry::load_locked(&path2).unwrap();
            barrier2.wait(); // signal that lock is held
            std::thread::sleep(Duration::from_millis(200));
            drop(locked); // release lock
        });

        // Thread 2: wait for thread 1 to hold lock, then try to acquire
        let t2 = std::thread::spawn(move || {
            barrier.wait(); // wait for thread 1 to hold lock
            let start = Instant::now();
            let _locked = AgentRegistry::load_locked(&path).unwrap();
            let waited = start.elapsed();
            // Should have blocked for at least ~150ms (thread 1 held lock for 200ms)
            assert!(
                waited >= Duration::from_millis(100),
                "Second lock acquisition should have blocked, but only waited {:?}",
                waited
            );
        });

        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn test_concurrent_agent_registration() {
        use std::sync::{Arc, Barrier};

        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(temp_dir.path().join("service")).unwrap();

        let path = Arc::new(temp_dir.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(2));

        let path1 = Arc::clone(&path);
        let barrier1 = Arc::clone(&barrier);
        let path2 = Arc::clone(&path);
        let barrier2 = Arc::clone(&barrier);

        // Two threads concurrently register agents
        let t1 = std::thread::spawn(move || {
            barrier1.wait();
            let mut locked = AgentRegistry::load_locked(&path1).unwrap();
            let id = locked.register_agent(1001, "task-a", "claude", "/tmp/a.log");
            locked.save().unwrap();
            id
        });

        let t2 = std::thread::spawn(move || {
            barrier2.wait();
            let mut locked = AgentRegistry::load_locked(&path2).unwrap();
            let id = locked.register_agent(1002, "task-b", "claude", "/tmp/b.log");
            locked.save().unwrap();
            id
        });

        let id1 = t1.join().unwrap();
        let id2 = t2.join().unwrap();

        // Both agents should be registered with distinct IDs
        assert_ne!(id1, id2, "Agent IDs must be unique");

        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        assert_eq!(registry.agents.len(), 2, "Both agents should be registered");
        assert!(registry.agents.contains_key(&id1));
        assert!(registry.agents.contains_key(&id2));
    }

    /// Verify active_count counts agents consistently across all executor types
    /// (claude, eval/assign, shell) — no executor-based filtering.
    /// This is the same logic the TUI and `wg service status` use.
    #[test]
    fn test_active_count_mixed_executor_types() {
        let mut registry = AgentRegistry::new();

        // Regular task agents (claude executor)
        registry.register_agent(100, "implement-feature", "claude", "/tmp/1.log");
        registry.register_agent(101, "fix-bug", "claude", "/tmp/2.log");

        // Dot-task agents (inline eval/assign/flip)
        registry.register_agent(200, ".assign-implement-feature", "assign", "/tmp/3.log");
        registry.register_agent(201, ".evaluate-fix-bug", "eval", "/tmp/4.log");
        registry.register_agent(202, ".flip-fix-bug", "eval", "/tmp/5.log");

        // Shell executor agent
        registry.register_agent(300, "run-tests", "shell", "/tmp/6.log");

        // All 6 agents are Working (default status after register)
        assert_eq!(registry.active_count(), 6, "all executors counted equally");

        // Mark some dot-task agents as done (they're short-lived)
        registry.set_status("agent-3", AgentStatus::Done);
        registry.set_status("agent-5", AgentStatus::Done);

        // Now 4 alive: 2 claude + 1 eval + 1 shell
        assert_eq!(registry.active_count(), 4);

        // Mark a claude agent as dead
        registry.set_status("agent-2", AgentStatus::Dead);
        assert_eq!(registry.active_count(), 3);

        // Idle agents count as alive too
        registry.set_status("agent-6", AgentStatus::Idle);
        assert_eq!(
            registry.active_count(),
            3,
            "idle agents are counted as alive"
        );
    }
}

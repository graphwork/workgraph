# Verify Timeout Implementation Plan

## Overview

This document provides detailed technical implementation guidance for each component of the verify timeout triage system. It specifies which files to modify, what changes to make, and how components interact.

## Phase 1: Configuration Foundation

### 1.1 Task Schema Changes

**File**: `src/graph.rs`

**Changes**:
```rust
// Add to Task struct after the existing verify field (around line 278)
/// Verification timeout override for this specific task (e.g., "15m", "900s")
/// Takes priority over global WG_VERIFY_TIMEOUT and coordinator defaults
#[serde(skip_serializing_if = "Option::is_none")]
pub verify_timeout: Option<String>,
```

**Migration**: This is backward compatible as it's an Optional field.

### 1.2 Command Line Interface

**File**: `src/commands/add.rs`

**Changes**:
```rust
// Add to AddCommand struct in the clap arguments (around line 30)
#[clap(long, help = "Verification timeout (e.g., '15m', '900s'). Overrides global WG_VERIFY_TIMEOUT")]
pub verify_timeout: Option<String>,

// In the execute function, add to task creation:
if let Some(timeout) = &self.verify_timeout {
    task.verify_timeout = Some(timeout.clone());
}
```

**Validation**: Add timeout string parsing validation using the existing `parse_delay` function.

### 1.3 Timeout Resolution Logic

**File**: `src/commands/done.rs`

**Changes**: Replace the existing timeout resolution (lines 71-74) with:

```rust
// Enhanced timeout resolution with priority order
fn resolve_verify_timeout(task: &Task, coordinator_config: &CoordinatorConfig) -> Duration {
    // 1. Task-specific timeout (highest priority)
    if let Some(task_timeout) = &task.verify_timeout {
        if let Some(secs) = crate::graph::parse_delay(task_timeout) {
            return Duration::from_secs(secs);
        }
    }
    
    // 2. Global environment variable
    if let Ok(env_timeout) = std::env::var("WG_VERIFY_TIMEOUT") {
        if let Ok(secs) = env_timeout.parse::<u64>() {
            return Duration::from_secs(secs);
        }
    }
    
    // 3. Coordinator configuration default
    coordinator_config.verify_default_timeout
        .as_ref()
        .and_then(|s| crate::graph::parse_delay(s))
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(900)) // New default: 900s instead of 300s
}
```

### 1.4 Configuration Schema

**File**: `src/config.rs`

**Changes**: Add to CoordinatorConfig struct (around line 2300):
```rust
/// Default verify timeout for tasks without specific override
#[serde(default = "default_verify_timeout")]
pub verify_default_timeout: Option<String>,

/// Maximum number of concurrent verify processes to prevent cascade failures
#[serde(default = "default_max_concurrent_verifies")]  
pub max_concurrent_verifies: u32,

/// Enable intelligent triage instead of hard timeout failure
#[serde(default = "default_verify_triage_enabled")]
pub verify_triage_enabled: bool,

/// Time without output before considering process potentially stuck
#[serde(default = "default_verify_progress_timeout")]
pub verify_progress_timeout: Option<String>,

// Default value functions
fn default_verify_timeout() -> Option<String> {
    Some("900s".to_string())
}

fn default_max_concurrent_verifies() -> u32 {
    2
}

fn default_verify_triage_enabled() -> bool {
    false  // Start disabled, enable gradually
}

fn default_verify_progress_timeout() -> Option<String> {
    Some("300s".to_string())
}
```

---

## Phase 2: Build Directory Isolation

### 2.1 Worktree Setup Enhancement

**File**: `src/commands/spawn/worktree.rs`

**Changes**: Enhance the `setup_worktree` function to configure cargo isolation:

```rust
// Add after existing worktree creation logic (around line 150)
fn configure_cargo_isolation(worktree_path: &Path) -> Result<()> {
    let target_dir = worktree_path.join("target");
    std::fs::create_dir_all(&target_dir)?;
    
    let env_file = worktree_path.join(".env");
    let cargo_target_dir = format!("CARGO_TARGET_DIR={}\n", target_dir.display());
    
    // Append to .env file or create it
    std::fs::write(&env_file, cargo_target_dir)?;
    
    // Also set in bashrc for interactive use
    let bashrc_file = worktree_path.join(".bashrc");
    let bashrc_content = format!("export CARGO_TARGET_DIR={}\n", target_dir.display());
    std::fs::write(&bashrc_file, bashrc_content)?;
    
    Ok(())
}

// Call this function after successful worktree creation
if coordinator_config.worktree_isolation {
    configure_cargo_isolation(&worktree_path)?;
}
```

### 2.2 Agent Environment Setup

**File**: `src/commands/spawn/execution.rs`

**Changes**: Ensure `CARGO_TARGET_DIR` is passed to agent environment:

```rust
// Add to environment variable setup (around line 200)
if let Some(worktree_path) = agent_context.worktree_path {
    let target_dir = worktree_path.join("target");
    env.insert("CARGO_TARGET_DIR".to_string(), target_dir.display().to_string());
}
```

### 2.3 Auto-Generated Setup Script

**File**: `src/commands/spawn/worktree.rs`

**Changes**: Generate `.workgraph/worktree-setup.sh` if missing:

```rust
fn ensure_worktree_setup_script(project_root: &Path) -> Result<()> {
    let script_path = project_root.join(".workgraph/worktree-setup.sh");
    
    if !script_path.exists() {
        let script_content = r#"#!/bin/bash
# Auto-generated worktree setup script
WORKTREE_PATH="$1"
export CARGO_TARGET_DIR="$WORKTREE_PATH/target"
mkdir -p "$CARGO_TARGET_DIR"

# Persist for shell sessions
echo "export CARGO_TARGET_DIR=\"$CARGO_TARGET_DIR\"" >> "$WORKTREE_PATH/.bashrc"
echo "export CARGO_TARGET_DIR=\"$CARGO_TARGET_DIR\"" >> "$WORKTREE_PATH/.env"
"#;
        std::fs::write(&script_path, script_content)?;
        
        // Make executable
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms)?;
    }
    
    Ok(())
}
```

---

## Phase 3: Verify Process Throttling

### 3.1 Coordinator State Enhancement

**File**: `src/commands/service/coordinator.rs`

**Changes**: Add verify process tracking to coordinator state:

```rust
// Add to coordinator state structure
#[derive(Debug, Clone)]
pub struct VerifyState {
    /// Currently running verify processes
    pub active_verifies: HashMap<String, VerifyProcess>, // task_id -> process info
    /// Queue of tasks waiting to start verify  
    pub verify_queue: VecDeque<String>, // task_ids in FIFO order
    /// Configuration limits
    pub max_concurrent: u32,
}

#[derive(Debug, Clone)]
pub struct VerifyProcess {
    pub task_id: String,
    pub started_at: Instant,
    pub process_id: Option<u32>,
}

impl VerifyState {
    fn new(max_concurrent: u32) -> Self {
        Self {
            active_verifies: HashMap::new(),
            verify_queue: VecDeque::new(),
            max_concurrent,
        }
    }
    
    fn can_start_verify(&self) -> bool {
        self.active_verifies.len() < self.max_concurrent as usize
    }
    
    fn enqueue_verify(&mut self, task_id: String) {
        if !self.verify_queue.contains(&task_id) {
            self.verify_queue.push_back(task_id);
        }
    }
    
    fn start_verify(&mut self, task_id: String) -> bool {
        if self.can_start_verify() {
            self.active_verifies.insert(task_id.clone(), VerifyProcess {
                task_id: task_id.clone(),
                started_at: Instant::now(),
                process_id: None,
            });
            true
        } else {
            self.enqueue_verify(task_id);
            false
        }
    }
    
    fn complete_verify(&mut self, task_id: &str) {
        self.active_verifies.remove(task_id);
        
        // Start next queued verify if any
        if let Some(next_task) = self.verify_queue.pop_front() {
            self.start_verify(next_task);
        }
    }
}
```

### 3.2 Verify Queue Persistence

**File**: `src/commands/service/mod.rs`

**Changes**: Add queue persistence to service state:

```rust
// Add to ServiceState serialization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedVerifyState {
    pub verify_queue: Vec<String>,
    pub max_concurrent: u32,
}

// Save/load functions
impl ServiceState {
    fn save_verify_queue(&self, verify_state: &VerifyState) -> Result<()> {
        let persisted = PersistedVerifyState {
            verify_queue: verify_state.verify_queue.iter().cloned().collect(),
            max_concurrent: verify_state.max_concurrent,
        };
        
        let path = self.service_dir.join("verify-queue.json");
        let json = serde_json::to_string_pretty(&persisted)?;
        std::fs::write(path, json)?;
        Ok(())
    }
    
    fn load_verify_queue(&self) -> Result<VerifyState> {
        let path = self.service_dir.join("verify-queue.json");
        
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let persisted: PersistedVerifyState = serde_json::from_str(&content)?;
            let mut state = VerifyState::new(persisted.max_concurrent);
            state.verify_queue = persisted.verify_queue.into();
            Ok(state)
        } else {
            Ok(VerifyState::new(2)) // Default
        }
    }
}
```

### 3.3 Verify Command Integration

**File**: `src/commands/done.rs`

**Changes**: Check queue before starting verify:

```rust
// Add to verify execution function
fn execute_verify_with_throttling(
    task_id: &str,
    verify_cmd: &str, 
    coordinator_state: &mut CoordinatorState
) -> Result<VerifyOutput> {
    
    // Check if we can start immediately or need to queue
    if !coordinator_state.verify_state.start_verify(task_id.to_string()) {
        // Task was queued, return special result
        return Ok(VerifyOutput {
            stdout: String::new(),
            stderr: format!("Verify queued - {} tasks ahead", 
                          coordinator_state.verify_state.verify_queue.len()),
            exit_code: "queued".to_string(),
        });
    }
    
    // Proceed with actual verify execution
    let result = execute_verify_command(verify_cmd, task_id)?;
    
    // Mark verify as complete, start next in queue
    coordinator_state.verify_state.complete_verify(task_id);
    
    Ok(result)
}
```

---

## Phase 4: Progress Monitoring & Triage

### 4.1 Progress Monitoring Infrastructure

**File**: `src/commands/done.rs`

**Changes**: Replace simple timeout loop with progress-aware monitoring:

```rust
#[derive(Debug)]
struct ProgressMonitor {
    last_stdout_activity: Instant,
    last_stderr_activity: Instant,
    total_stdout_bytes: usize,
    total_stderr_bytes: usize,
    process_start: Instant,
}

impl ProgressMonitor {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_stdout_activity: now,
            last_stderr_activity: now,
            total_stdout_bytes: 0,
            total_stderr_bytes: 0,
            process_start: now,
        }
    }
    
    fn update_stdout(&mut self, new_bytes: usize) {
        if new_bytes > 0 {
            self.last_stdout_activity = Instant::now();
            self.total_stdout_bytes += new_bytes;
        }
    }
    
    fn update_stderr(&mut self, new_bytes: usize) {
        if new_bytes > 0 {
            self.last_stderr_activity = Instant::now();
            self.total_stderr_bytes += new_bytes;
        }
    }
    
    fn last_activity(&self) -> Instant {
        self.last_stdout_activity.max(self.last_stderr_activity)
    }
    
    fn has_recent_activity(&self, threshold: Duration) -> bool {
        self.last_activity().elapsed() < threshold
    }
}
```

### 4.2 Triage Decision Engine

**File**: `src/commands/done.rs`

**Changes**: Implement triage algorithm:

```rust
#[derive(Debug, PartialEq)]
enum TriageResult {
    GenuineHang { reason: String },
    WaitingOnLocks { detected_locks: Vec<String> },
    HighSystemLoad { load_avg: f64 },
    UnknownButActive { activity_type: String },
    ResourcePressure { details: String },
}

fn triage_timeout_process(
    child: &mut std::process::Child,
    monitor: &ProgressMonitor,
    task_id: &str,
    progress_timeout: Duration,
) -> Result<TriageResult> {
    
    // 1. Check for recent output activity
    if monitor.has_recent_activity(Duration::from_secs(60)) {
        return Ok(TriageResult::UnknownButActive {
            activity_type: "recent_output".to_string(),
        });
    }
    
    // 2. Check for cargo lock files (common contention point)
    let lock_files = detect_cargo_locks()?;
    if !lock_files.is_empty() {
        return Ok(TriageResult::WaitingOnLocks {
            detected_locks: lock_files,
        });
    }
    
    // 3. Check process CPU usage  
    if let Some(pid) = child.id() {
        let cpu_usage = get_process_cpu_usage(pid)?;
        if cpu_usage > 1.0 { // Process using >1% CPU
            return Ok(TriageResult::UnknownButActive {
                activity_type: format!("cpu_usage_{:.1}%", cpu_usage),
            });
        }
    }
    
    // 4. Check system load
    let load_avg = get_system_load_average()?;
    if load_avg > 4.0 {
        return Ok(TriageResult::HighSystemLoad { load_avg });
    }
    
    // 5. Check for I/O wait patterns
    let io_wait = get_io_wait_percentage()?;
    if io_wait > 50.0 {
        return Ok(TriageResult::ResourcePressure {
            details: format!("high_io_wait_{:.1}%", io_wait),
        });
    }
    
    // 6. Default to genuine hang if no other indicators
    Ok(TriageResult::GenuineHang {
        reason: format!("no_activity_{}s_no_locks_low_cpu", 
                       monitor.last_activity().elapsed().as_secs()),
    })
}

// Helper functions for system monitoring
fn detect_cargo_locks() -> Result<Vec<String>> {
    let mut locks = Vec::new();
    
    // Common cargo lock files
    let lock_patterns = [
        "target/.rustc_info.json.lock",
        "target/debug/.cargo-lock",
        "Cargo.lock",
    ];
    
    for pattern in &lock_patterns {
        if std::path::Path::new(pattern).exists() {
            locks.push(pattern.to_string());
        }
    }
    
    Ok(locks)
}

fn get_process_cpu_usage(pid: u32) -> Result<f64> {
    // Use /proc/[pid]/stat to get CPU usage
    let stat_path = format!("/proc/{}/stat", pid);
    if let Ok(content) = std::fs::read_to_string(stat_path) {
        // Parse CPU usage from stat file (simplified)
        // In real implementation, would need proper parsing and time-based calculation
        return Ok(1.5); // Placeholder
    }
    Ok(0.0)
}

fn get_system_load_average() -> Result<f64> {
    if let Ok(content) = std::fs::read_to_string("/proc/loadavg") {
        let parts: Vec<&str> = content.split_whitespace().collect();
        if let Ok(load) = parts[0].parse::<f64>() {
            return Ok(load);
        }
    }
    Ok(0.0)
}

fn get_io_wait_percentage() -> Result<f64> {
    // Parse /proc/stat for I/O wait percentage
    // Simplified implementation
    Ok(10.0) // Placeholder
}
```

### 4.3 Triage Action Handler

**File**: `src/commands/done.rs`

**Changes**: Handle triage results with appropriate actions:

```rust
fn handle_triage_result(
    triage: TriageResult,
    task_id: &str,
    retry_count: &mut u32,
    config: &CoordinatorConfig,
) -> Result<VerifyAction> {
    
    match triage {
        TriageResult::GenuineHang { reason } => {
            log::warn!("Task {} verify genuinely hung: {}", task_id, reason);
            Ok(VerifyAction::Fail(reason))
        },
        
        TriageResult::WaitingOnLocks { detected_locks } => {
            if *retry_count < 2 {
                *retry_count += 1;
                log::info!("Task {} waiting on locks {:?}, retrying with longer timeout", 
                          task_id, detected_locks);
                Ok(VerifyAction::RetryWithTimeout(Duration::from_secs(1200)))
            } else {
                Ok(VerifyAction::Fail(format!("Persistent lock contention: {:?}", detected_locks)))
            }
        },
        
        TriageResult::HighSystemLoad { load_avg } => {
            if *retry_count < 1 {
                *retry_count += 1;
                log::info!("Task {} delayed due to high system load {:.2}, retrying in 5min", 
                          task_id, load_avg);
                Ok(VerifyAction::RetryAfterDelay(Duration::from_secs(300)))
            } else {
                Ok(VerifyAction::Fail(format!("Persistent high load: {:.2}", load_avg)))
            }
        },
        
        TriageResult::UnknownButActive { activity_type } => {
            log::info!("Task {} still active ({}), extending timeout", task_id, activity_type);
            Ok(VerifyAction::ExtendTimeout(Duration::from_secs(300)))
        },
        
        TriageResult::ResourcePressure { details } => {
            log::info!("Task {} under resource pressure ({}), retrying later", task_id, details);
            Ok(VerifyAction::RetryAfterDelay(Duration::from_secs(180)))
        },
    }
}

enum VerifyAction {
    Fail(String),
    RetryWithTimeout(Duration),
    RetryAfterDelay(Duration), 
    ExtendTimeout(Duration),
}
```

---

## Phase 5: Integration & Testing

### 5.1 End-to-End Integration

**File**: `src/commands/done.rs`

**Changes**: Integrate all components in the main verify function:

```rust
pub fn execute_verify_with_triage(
    task: &Task,
    verify_cmd: &str,
    coordinator_config: &CoordinatorConfig,
) -> Result<VerifyOutput> {
    
    // Phase 1: Resolve timeout configuration
    let base_timeout = resolve_verify_timeout(task, coordinator_config);
    let progress_timeout = coordinator_config.verify_progress_timeout
        .as_ref()
        .and_then(|s| crate::graph::parse_delay(s))
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(300));
    
    // Phase 3: Check verify throttling (if enabled)
    if coordinator_config.max_concurrent_verifies > 0 {
        // Would integrate with coordinator state here
        // For now, proceed directly
    }
    
    // Start the verify process
    let mut child = std::process::Command::new("bash")
        .arg("-c")
        .arg(verify_cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    
    // Phase 2: Set up isolated environment (already handled in spawn)
    
    // Phase 4: Execute with progress monitoring
    let mut monitor = ProgressMonitor::new();
    let mut current_timeout = base_timeout;
    let mut retry_count = 0;
    
    loop {
        match execute_with_monitoring(&mut child, current_timeout, &mut monitor) {
            Ok(output) => return Ok(output),
            Err(TimeoutError { stdout, stderr }) => {
                
                // Triage the timeout if enabled
                if coordinator_config.verify_triage_enabled {
                    let triage_result = triage_timeout_process(
                        &mut child, &monitor, &task.id, progress_timeout
                    )?;
                    
                    match handle_triage_result(triage_result, &task.id, &mut retry_count, coordinator_config)? {
                        VerifyAction::Fail(reason) => {
                            let _ = child.kill();
                            return Err(VerifyOutput {
                                stdout, stderr,
                                exit_code: format!("timeout_triage_failed: {}", reason),
                            });
                        },
                        VerifyAction::ExtendTimeout(extension) => {
                            current_timeout = extension;
                            continue; // Continue with extended timeout
                        },
                        VerifyAction::RetryWithTimeout(new_timeout) => {
                            let _ = child.kill();
                            // Restart process with new timeout (simplified)
                            current_timeout = new_timeout;
                            monitor = ProgressMonitor::new();
                            child = std::process::Command::new("bash")
                                .arg("-c").arg(verify_cmd)
                                .stdout(std::process::Stdio::piped())
                                .stderr(std::process::Stdio::piped())
                                .spawn()?;
                            continue;
                        },
                        VerifyAction::RetryAfterDelay(delay) => {
                            let _ = child.kill();
                            std::thread::sleep(delay);
                            // Restart process after delay
                            monitor = ProgressMonitor::new();
                            child = std::process::Command::new("bash")
                                .arg("-c").arg(verify_cmd)
                                .stdout(std::process::Stdio::piped())
                                .stderr(std::process::Stdio::piped())
                                .spawn()?;
                            continue;
                        },
                    }
                } else {
                    // Legacy behavior: hard timeout failure
                    let _ = child.kill();
                    return Err(VerifyOutput {
                        stdout, stderr,
                        exit_code: format!("timeout_{}s", current_timeout.as_secs()),
                    });
                }
            }
        }
    }
}
```

### 5.2 Configuration Validation

**File**: `src/config.rs`

**Changes**: Add validation for new configuration options:

```rust
impl CoordinatorConfig {
    pub fn validate_verify_settings(&self) -> Result<(), String> {
        // Validate timeout format
        if let Some(timeout_str) = &self.verify_default_timeout {
            if crate::graph::parse_delay(timeout_str).is_none() {
                return Err(format!("Invalid verify_default_timeout format: {}", timeout_str));
            }
        }
        
        if let Some(progress_str) = &self.verify_progress_timeout {
            if crate::graph::parse_delay(progress_str).is_none() {
                return Err(format!("Invalid verify_progress_timeout format: {}", progress_str));
            }
        }
        
        // Validate concurrency limits
        if self.max_concurrent_verifies == 0 {
            return Err("max_concurrent_verifies must be > 0".to_string());
        }
        
        Ok(())
    }
}
```

### 5.3 Testing Integration

**File**: `tests/integration_verify_timeout.rs` (new file)

**Changes**: Add comprehensive integration tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_per_task_timeout_configuration() {
        // Test task-specific timeout overrides global setting
    }
    
    #[test] 
    fn test_triage_detects_lock_contention() {
        // Test triage correctly identifies cargo lock contention
    }
    
    #[test]
    fn test_verify_queue_throttling() {
        // Test that concurrent verify commands are properly queued
    }
    
    #[test]
    fn test_isolated_build_directories() {
        // Test that agents get separate CARGO_TARGET_DIR
    }
    
    #[test]
    fn test_triage_genuine_hang_detection() {
        // Test triage correctly identifies stuck processes
    }
    
    #[test]
    fn test_backward_compatibility() {
        // Test existing verify strings continue to work
    }
}
```

---

## File Summary

### Files to Modify
- `src/graph.rs` - Add `verify_timeout` field to Task
- `src/commands/add.rs` - Add `--verify-timeout` CLI flag  
- `src/commands/done.rs` - Main verify execution with triage
- `src/commands/spawn/worktree.rs` - Cargo isolation setup
- `src/commands/spawn/execution.rs` - Agent environment setup
- `src/commands/service/coordinator.rs` - Verify queue management
- `src/config.rs` - New coordinator configuration options

### Files to Create
- `tests/integration_verify_timeout.rs` - Integration tests
- `.workgraph/worktree-setup.sh` - Auto-generated setup script

### Estimated Implementation Effort
- **Phase 1**: 2-3 days (configuration foundation)
- **Phase 2**: 2-3 days (build isolation) 
- **Phase 3**: 4-5 days (verify throttling + queue)
- **Phase 4**: 5-6 days (progress monitoring + triage)
- **Phase 5**: 2-3 days (integration + testing)

**Total**: 15-20 days for complete implementation

### Dependencies Between Changes
1. Configuration (Phase 1) must be completed first
2. Build isolation (Phase 2) can be developed in parallel with throttling (Phase 3)
3. Triage (Phase 4) depends on progress monitoring infrastructure from Phase 1
4. Integration (Phase 5) requires all previous phases

### Risk Mitigation
- Each phase can be feature-flagged for gradual rollout
- Comprehensive tests prevent regressions
- Backward compatibility ensures existing tasks continue to work
- Clear rollback procedures for each component
//! Background job management for the native executor.
//!
//! Provides `Job`, `JobStatus`, and `JobStore` for running and managing
//! detached background tasks that persist across agent restarts.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;

/// Maximum concurrent background jobs.
const DEFAULT_MAX_CONCURRENT: usize = 10;

/// Grace period (seconds) before SIGKILL after SIGTERM.
const KILL_GRACE_PERIOD_SECS: u64 = 5;

/// Job file name pattern.
const JOB_FILE_PREFIX: &str = "job-";

/// Lock file extension.
const LOCK_EXT: &str = ".lock";

/// PID file extension.
const PID_EXT: &str = ".pid";

/// Log file extension.
const LOG_EXT: &str = ".log";

/// Represents a background task that persists across agent restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Unique identifier (generated UUID).
    pub id: String,
    /// Human-readable name for display.
    pub name: String,
    /// The shell command that was executed.
    pub command: String,
    /// Current status of the job.
    pub status: JobStatus,
    /// Process ID (if running).
    pub pid: Option<u32>,
    /// Exit code (if completed or failed).
    pub exit_code: Option<i32>,
    /// Timestamp when the job was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the job last updated (status change).
    pub updated_at: DateTime<Utc>,
    /// When the job finished (if terminal state).
    pub finished_at: Option<DateTime<Utc>>,
    /// Path to the log file containing stdout/stderr.
    pub log_path: PathBuf,
    /// Working directory for the command.
    pub working_dir: PathBuf,
}

/// Possible states for a background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Job is running in the background.
    Running,
    /// Job completed successfully (exit code 0).
    Completed,
    /// Job failed (non-zero exit code).
    Failed,
    /// Job was cancelled by user request.
    Cancelled,
    /// Job is in an unknown state (orphan detection).
    Orphaned,
}

impl JobStatus {
    /// Returns true if this is a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled | JobStatus::Orphaned
        )
    }
}

/// Manages persistence and discovery of background jobs.
#[derive(Debug, Clone)]
pub struct JobStore {
    /// Cache of loaded jobs (refreshed on demand).
    jobs: HashMap<String, Job>,
    /// Maximum concurrent jobs allowed.
    max_concurrent: usize,
    /// Jobs directory path.
    jobs_dir: PathBuf,
}

impl JobStore {
    /// Create a new JobStore at the given path.
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        let jobs_dir = base_dir.join("jobs");
        fs::create_dir_all(&jobs_dir).context("Failed to create jobs directory")?;

        let mut store = Self {
            jobs: HashMap::new(),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            jobs_dir,
        };

        // Load existing jobs from disk
        store.load_all()?;

        Ok(store)
    }

    /// Set the maximum concurrent jobs allowed.
    pub fn set_max_concurrent(&mut self, max: usize) {
        self.max_concurrent = max;
    }

    /// Get the jobs directory path.
    pub fn jobs_dir(&self) -> &Path {
        &self.jobs_dir
    }

    /// Get a job by ID or name.
    pub fn get(&self, id_or_name: &str) -> Option<&Job> {
        // First try exact ID match
        if let Some(job) = self.jobs.get(id_or_name) {
            return Some(job);
        }
        // Then try name match
        self.jobs.values().find(|j| j.name == id_or_name)
    }

    /// Get all jobs (sorted by created_at).
    pub fn list(&self) -> Vec<&Job> {
        let mut jobs: Vec<&Job> = self.jobs.values().collect();
        jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        jobs
    }

    /// Check if a named job exists (prevents duplicate launches).
    pub fn exists(&self, name: &str) -> bool {
        self.jobs.values().any(|j| j.name == name)
    }

    /// Get the current count of running jobs.
    pub fn running_count(&self) -> usize {
        self.jobs
            .values()
            .filter(|j| j.status == JobStatus::Running)
            .count()
    }

    /// Refresh job states from disk (check PIDs, exit codes).
    pub fn refresh(&mut self) -> Result<()> {
        for job in self.jobs.values_mut() {
            if job.status == JobStatus::Running
                && let Some(pid) = job.pid
            {
                // Use kill(pid, 0) to check if process exists
                if !process_exists(pid) {
                    job.status = JobStatus::Orphaned;
                    job.updated_at = Utc::now();
                }
            }
        }
        Ok(())
    }

    /// Load all jobs from disk.
    fn load_all(&mut self) -> Result<()> {
        if !self.jobs_dir.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&self.jobs_dir)? {
            let entry = entry?;
            let path = entry.path();

            // Only process .json files
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }

            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read job file: {:?}", path))?;

            let job: Job = serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse job file: {:?}", path))?;

            self.jobs.insert(job.id.clone(), job);
        }

        // Refresh to detect orphaned jobs
        self.refresh()?;

        Ok(())
    }

    /// Save a job to disk.
    fn save_job(&self, job: &Job) -> Result<()> {
        let path = self
            .jobs_dir
            .join(format!("{}{}.json", JOB_FILE_PREFIX, job.id));
        let content = serde_json::to_string_pretty(job).context("Failed to serialize job")?;
        fs::write(&path, content).context("Failed to write job file")?;
        Ok(())
    }

    /// Create a new lock file for a job.
    fn create_lock(&self, job_id: &str, pid: u32) -> Result<PathBuf> {
        let _lock_path =
            self.jobs_dir
                .join(format!("{}{}{}", job_id, ".lock", PID_EXT.replace('.', "")));
        // Actually the lock file should be like: job-{id}.lock
        let lock_path = self.jobs_dir.join(format!("{}{}", job_id, LOCK_EXT));
        let content = format!("{}\n", pid);
        fs::write(&lock_path, content).context("Failed to write lock file")?;
        Ok(lock_path)
    }

    /// Create a PID file for a job.
    fn create_pid_file(&self, job_id: &str, pid: u32) -> Result<PathBuf> {
        let pid_path = self.jobs_dir.join(format!("{}{}", job_id, PID_EXT));
        fs::write(&pid_path, format!("{}\n", pid)).context("Failed to write PID file")?;
        Ok(pid_path)
    }

    /// Get the log file path for a job.
    fn log_path(&self, job_id: &str) -> PathBuf {
        self.jobs_dir.join(format!("{}{}", job_id, LOG_EXT))
    }

    /// Run a new background job.
    pub async fn run(&mut self, name: &str, command: &str, working_dir: &Path) -> Result<Job> {
        // Check max concurrent
        if self.running_count() >= self.max_concurrent {
            return Err(anyhow!(
                "Maximum concurrent jobs ({}) reached. Wait for a job to complete.",
                self.max_concurrent
            ));
        }

        // Check for duplicate name
        if self.exists(name) {
            return Err(anyhow!("Job with name '{}' already exists", name));
        }

        // Generate job ID
        let id = format!("{}{}", JOB_FILE_PREFIX, uuid_simple());
        let log_path = self.log_path(&id);

        let now = Utc::now();
        let job = Job {
            id: id.clone(),
            name: name.to_string(),
            command: command.to_string(),
            status: JobStatus::Running,
            pid: None,
            exit_code: None,
            created_at: now,
            updated_at: now,
            finished_at: None,
            log_path: log_path.clone(),
            working_dir: working_dir.to_path_buf(),
        };

        // Spawn the process
        let pid = spawn_detached(command, working_dir, &log_path)?;

        // Update job with PID
        let mut job = job;
        job.pid = Some(pid);

        // Save job to disk
        self.save_job(&job)?;

        // Create PID file
        self.create_pid_file(&job.id, pid)?;

        // Create lock file
        self.create_lock(&job.id, pid)?;

        // Store in memory
        self.jobs.insert(job.id.clone(), job.clone());

        Ok(job)
    }

    /// Kill a job by sending SIGTERM, then SIGKILL if needed.
    pub async fn kill(&mut self, id_or_name: &str) -> Result<()> {
        // Get job ID first (clone it to avoid borrow issues)
        let job_id = {
            let job = self
                .get(id_or_name)
                .ok_or_else(|| anyhow!("Job not found: {}", id_or_name))?;
            job.pid.context("Job has no PID")?;
            job.id.clone()
        };

        let pid = self.jobs.get(&job_id).unwrap().pid.unwrap();

        // Send SIGTERM
        if let Err(e) = kill_process(pid, false) {
            return Err(anyhow!("Failed to send SIGTERM: {}", e));
        }

        // Wait for graceful shutdown with timeout
        let grace_period = Duration::from_secs(KILL_GRACE_PERIOD_SECS);
        let check_interval = Duration::from_millis(100);

        let mut elapsed = Duration::from_secs(0);
        while elapsed < grace_period {
            tokio::time::sleep(check_interval).await;
            elapsed += check_interval;

            if !process_exists(pid) {
                // Process exited gracefully
                break;
            }
        }

        // If still running, send SIGKILL
        if process_exists(pid) {
            if let Err(e) = kill_process(pid, true) {
                return Err(anyhow!("Failed to send SIGKILL: {}", e));
            }

            // Wait a bit for SIGKILL to take effect
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Update job status
        {
            let job = self.jobs.get_mut(&job_id).unwrap();
            job.status = JobStatus::Cancelled;
            job.updated_at = Utc::now();
            job.finished_at = Some(Utc::now());
        }
        // Now save outside the borrow
        if let Some(job) = self.jobs.get(&job_id) {
            self.save_job(job)?;
        }

        Ok(())
    }

    /// Delete a job and its associated files.
    pub async fn delete(&mut self, id_or_name: &str) -> Result<()> {
        // Get job info first
        let (job_id, is_running) = {
            let job = self
                .get(id_or_name)
                .ok_or_else(|| anyhow!("Job not found: {}", id_or_name))?;
            (job.id.clone(), job.status == JobStatus::Running)
        };

        // Kill if running (we can use job_id directly now since we don't need the borrow)
        if is_running {
            // Re-acquire mutable access for kill
            self.kill(&job_id).await.ok();
        }

        // Remove files
        let json_path = self
            .jobs_dir
            .join(format!("{}{}.json", JOB_FILE_PREFIX, job_id));
        let lock_path = self.jobs_dir.join(format!("{}{}", job_id, LOCK_EXT));
        let pid_path = self.jobs_dir.join(format!("{}{}", job_id, PID_EXT));
        let log_path = self.log_path(&job_id);

        for path in &[json_path, lock_path, pid_path, log_path] {
            if path.exists() {
                fs::remove_file(path).ok();
            }
        }

        // Remove from memory
        self.jobs.remove(&job_id);

        Ok(())
    }

    /// Get output from a job's log file.
    pub fn output(&self, id_or_name: &str, lines: Option<usize>) -> Result<String> {
        let job = self
            .get(id_or_name)
            .ok_or_else(|| anyhow!("Job not found: {}", id_or_name))?;

        if !job.log_path.exists() {
            return Ok(String::new());
        }

        let file = File::open(&job.log_path)?;
        let reader = BufReader::new(file);

        let all_lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
        let count = lines.unwrap_or(all_lines.len());

        if count >= all_lines.len() {
            Ok(all_lines.join("\n"))
        } else {
            Ok(all_lines[all_lines.len() - count..].join("\n"))
        }
    }

    /// Update job status based on process exit.
    pub fn check_and_update_status(&mut self, job_id: &str) -> Result<()> {
        let job = self
            .get(job_id)
            .ok_or_else(|| anyhow!("Job not found: {}", job_id))?;

        if job.status != JobStatus::Running {
            return Ok(());
        }

        let pid = match job.pid {
            Some(p) => p,
            None => return Ok(()),
        };

        if !process_exists(pid) {
            // Process exited, get exit code
            let exit_code = get_exit_code(pid);

            let new_status = match exit_code {
                Some(0) => JobStatus::Completed,
                Some(_) => JobStatus::Failed,
                None => JobStatus::Orphaned,
            };

            // Update job status and save
            {
                let job = self.jobs.get_mut(job_id).unwrap();
                job.status = new_status;
                job.exit_code = exit_code;
                job.updated_at = Utc::now();
                job.finished_at = Some(Utc::now());
            }
            // Save outside the borrow
            if let Some(job) = self.jobs.get(job_id) {
                self.save_job(job)?;
            }
        }

        Ok(())
    }
}

// ── Helper functions ────────────────────────────────────────────────────────

/// Generate a simple UUID-like string.
fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let random: u32 = rand_simple();
    format!("{:x}-{:x}", now, random)
}

/// Simple deterministic-ish random for unique IDs.
fn rand_simple() -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut hasher = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    (hasher.finish() as u32)
        .wrapping_mul(1103515245)
        .wrapping_add(12345)
}

/// Check if a process with the given PID exists.
fn process_exists(pid: u32) -> bool {
    // Use kill(pid, 0) to check process existence
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        // On non-Unix, assume process exists (conservative)
        true
    }
}

/// Kill a process, optionally with SIGKILL.
fn kill_process(pid: u32, force: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        let result = unsafe { libc::kill(pid as libc::pid_t, sig) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        let mut cmd = Command::new("taskkill");
        if force {
            cmd.args(&["/F", "/PID", &pid.to_string()]);
        } else {
            cmd.args(&["/PID", &pid.to_string()]);
        }
        cmd.output()?;
        Ok(())
    }
}

/// Get the exit code of a dead process.
fn get_exit_code(pid: u32) -> Option<i32> {
    #[cfg(unix)]
    {
        // Use waitpid with WNOHANG on a non-blocking call

        let mut status: libc::c_int = 0;
        let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };

        if result == 0 {
            // Process still running
            None
        } else if result == -1 {
            None
        } else if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status) as i32)
        } else {
            Some(-1)
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Spawn a detached child process that writes output to a log file.
fn spawn_detached(command: &str, working_dir: &Path, log_path: &Path) -> Result<u32> {
    #[cfg(unix)]
    {
        // On Unix, we use setsid to create a new session and detach from terminal
        // The setsid command handles the detachment - the spawned process becomes session leader
        let child = TokioCommand::new("bash")
            .arg("-c")
            .arg(format!(
                "setsid {} > {} 2>&1 < /dev/null",
                command,
                log_path.display()
            ))
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn process: {}", e))?;

        Ok(child.id().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        use std::process::Command;
        // On Windows, use start /B to run in background
        let output = Command::new("cmd")
            .args(&["/C", "start", "/B", command])
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(File::create(log_path)?)
            .stderr(File::create(log_path)?)
            .output()
            .context("Failed to spawn process")?;

        // Parse the PID from output (Windows doesn't give us a clean PID)
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_job_store_creation() {
        let tmp = TempDir::new().unwrap();
        let store = JobStore::new(tmp.path().to_path_buf()).unwrap();
        assert_eq!(store.running_count(), 0);
    }

    #[tokio::test]
    async fn test_job_store_run_and_list() {
        let tmp = TempDir::new().unwrap();
        let mut store = JobStore::new(tmp.path().to_path_buf()).unwrap();

        // Run a simple background job
        let job = store
            .run("test-job", "sleep 0.1", tmp.path())
            .await
            .unwrap();

        assert_eq!(job.name, "test-job");
        assert_eq!(job.command, "sleep 0.1");
        assert_eq!(job.status, JobStatus::Running);
        assert!(job.pid.is_some());

        // List jobs
        let jobs = store.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "test-job");

        // Cleanup
        store.delete("test-job").await.ok();
    }

    #[tokio::test]
    async fn test_job_store_exists() {
        let tmp = TempDir::new().unwrap();
        let mut store = JobStore::new(tmp.path().to_path_buf()).unwrap();

        assert!(!store.exists("my-job"));

        store.run("my-job", "sleep 1", tmp.path()).await.unwrap();

        assert!(store.exists("my-job"));
        assert!(!store.exists("other-job"));

        store.delete("my-job").await.ok();
    }

    #[tokio::test]
    async fn test_job_store_kill() {
        let tmp = TempDir::new().unwrap();
        let mut store = JobStore::new(tmp.path().to_path_buf()).unwrap();

        let _job = store
            .run("kill-test", "sleep 60", tmp.path())
            .await
            .unwrap();

        // Kill should succeed
        store.kill("kill-test").await.unwrap();

        let killed_job = store.get("kill-test").unwrap();
        assert_eq!(killed_job.status, JobStatus::Cancelled);

        store.delete("kill-test").await.ok();
    }

    #[tokio::test]
    async fn test_duplicate_name_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut store = JobStore::new(tmp.path().to_path_buf()).unwrap();

        store.run("dup", "sleep 1", tmp.path()).await.unwrap();

        let result = store.run("dup", "sleep 2", tmp.path()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        store.delete("dup").await.ok();
    }
}

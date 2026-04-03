//! Agent service layer
//!
//! Provides agent registry and management for the workgraph agent service.
//!
//! This module includes:
//! - Executor configuration for spawning agents
//! - Agent registry for tracking running agents

pub mod chat_compactor;
pub mod compactor;
pub mod executor;
pub mod llm;
pub mod registry;

pub use executor::{
    ExecutorConfig, ExecutorRegistry, ExecutorSettings, PromptTemplate, TemplateVars,
};
pub use registry::{AgentEntry, AgentRegistry, AgentStatus, LockedRegistry};

/// Check if a process with the given PID is alive.
///
/// Uses `kill(pid, 0)` on Unix to probe without sending a signal.
/// On non-Unix platforms, conservatively assumes the process is alive.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    true
}

/// Send SIGTERM, wait up to `wait_secs` seconds, then SIGKILL if still alive.
#[cfg(unix)]
pub fn kill_process_graceful(pid: u32, wait_secs: u64) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::thread;
    use std::time::Duration;

    let pid_i32 = pid as i32;

    if !is_process_alive(pid) {
        return Ok(());
    }

    // Send SIGTERM
    if unsafe { libc::kill(pid_i32, libc::SIGTERM) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGTERM to PID {}", pid));
    }

    // Wait for process to exit
    for _ in 0..wait_secs {
        thread::sleep(Duration::from_secs(1));
        if !is_process_alive(pid) {
            return Ok(());
        }
    }

    // Still alive, send SIGKILL
    if unsafe { libc::kill(pid_i32, libc::SIGKILL) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGKILL to PID {}", pid));
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn kill_process_graceful(_pid: u32, _wait_secs: u64) -> anyhow::Result<()> {
    anyhow::bail!("Process killing is only supported on Unix systems")
}

/// Send SIGKILL immediately.
#[cfg(unix)]
pub fn kill_process_force(pid: u32) -> anyhow::Result<()> {
    use anyhow::Context;

    let pid_i32 = pid as i32;

    if !is_process_alive(pid) {
        return Ok(());
    }

    if unsafe { libc::kill(pid_i32, libc::SIGKILL) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGKILL to PID {}", pid));
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn kill_process_force(_pid: u32) -> anyhow::Result<()> {
    anyhow::bail!("Process killing is only supported on Unix systems")
}

/// Read a process's start time from `/proc/<pid>/stat` as seconds since epoch.
///
/// Returns `None` if the process doesn't exist, `/proc` is unavailable, or
/// the stat file can't be parsed. Used to detect PID reuse: if the process
/// at a given PID started much later than expected, the PID was recycled.
#[cfg(target_os = "linux")]
pub fn read_proc_start_time_secs(pid: u32) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // Field 2 (comm) can contain spaces and parentheses; find the last ')'.
    let comm_end = stat.rfind(')')?;
    let fields: Vec<&str> = stat[comm_end + 2..].split_whitespace().collect();
    // starttime is field 22 overall; after stripping pid + comm (fields 1-2),
    // the remaining fields start at field 3, so starttime is at index 19.
    let starttime_ticks: u64 = fields.get(19)?.parse().ok()?;

    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    if clk_tck == 0 {
        return None;
    }

    let boot_time = read_boot_time()?;
    Some(boot_time + (starttime_ticks / clk_tck) as i64)
}

#[cfg(not(target_os = "linux"))]
pub fn read_proc_start_time_secs(_pid: u32) -> Option<i64> {
    None
}

/// Read system boot time from `/proc/stat` (btime line).
#[cfg(target_os = "linux")]
fn read_boot_time() -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    for line in stat.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Check whether a process has any active child processes.
///
/// On Linux, reads `/proc/<pid>/task/<tid>/children` for each thread of the
/// process. Falls back to scanning `/proc/*/stat` for ppid matches if the
/// `children` file is unavailable (requires CONFIG_PROC_CHILDREN).
///
/// Returns `true` if at least one live child process is found.
/// Returns `false` on non-Linux platforms or if `/proc` is unavailable.
#[cfg(target_os = "linux")]
pub fn has_active_children(pid: u32) -> bool {
    // Fast path: try /proc/<pid>/task/<pid>/children (available with CONFIG_PROC_CHILDREN)
    let children_path = format!("/proc/{}/task/{}/children", pid, pid);
    if let Ok(content) = std::fs::read_to_string(&children_path) {
        let has_children = content.split_whitespace().any(|tok| {
            tok.parse::<u32>()
                .map(|child_pid| is_process_alive(child_pid))
                .unwrap_or(false)
        });
        if has_children {
            return true;
        }
        // File existed but was empty — no children via this thread. Check other
        // threads below only if there are multiple, otherwise we're done.
    }

    // Check all threads of this process for children
    let task_dir = format!("/proc/{}/task", pid);
    if let Ok(entries) = std::fs::read_dir(&task_dir) {
        for entry in entries.flatten() {
            let tid = entry.file_name();
            let tid_str = tid.to_string_lossy();
            // Skip the main thread we already checked
            if tid_str == pid.to_string() {
                continue;
            }
            let thread_children = format!("/proc/{}/task/{}/children", pid, tid_str);
            if let Ok(content) = std::fs::read_to_string(&thread_children) {
                if content.split_whitespace().any(|tok| {
                    tok.parse::<u32>()
                        .map(|child_pid| is_process_alive(child_pid))
                        .unwrap_or(false)
                }) {
                    return true;
                }
            }
        }
    }

    // Slow fallback: scan /proc/*/stat for ppid == our pid.
    // This works even without CONFIG_PROC_CHILDREN.
    if let Ok(entries) = std::fs::read_dir("/proc") {
        let pid_str = pid.to_string();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Only look at numeric directories (PIDs)
            if !name_str.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
            // Skip self
            if name_str == pid_str {
                continue;
            }
            let stat_path = format!("/proc/{}/stat", name_str);
            if let Ok(stat) = std::fs::read_to_string(&stat_path) {
                // ppid is field 4; field 2 (comm) can contain ')' so find the last one
                if let Some(comm_end) = stat.rfind(')') {
                    let after_comm = &stat[comm_end + 2..];
                    let fields: Vec<&str> = after_comm.split_whitespace().collect();
                    // fields[0] = state, fields[1] = ppid
                    if let Some(ppid_str) = fields.get(1)
                        && *ppid_str == pid_str
                    {
                        return true;
                    }
                }
            }
        }
    }

    false
}

#[cfg(not(target_os = "linux"))]
pub fn has_active_children(_pid: u32) -> bool {
    false
}

/// Check whether the process at `pid` is the same one that was started at
/// `expected_start_epoch` (Unix timestamp). Returns `true` if we can confirm
/// the process identity matches, or if the check is inconclusive (non-Linux,
/// missing `/proc`, etc.). Returns `false` only when we can positively
/// determine that the PID has been reused by a different process.
pub fn verify_process_identity(pid: u32, expected_start_epoch: i64) -> bool {
    match read_proc_start_time_secs(pid) {
        Some(actual_start) => {
            // Allow 120 seconds of slack: the wrapper script may take a
            // moment to start after the spawn timestamp is recorded, and
            // clock granularity in /proc/stat is 1 second.
            actual_start <= expected_start_epoch + 120
        }
        None => {
            // Can't read /proc — process might be gone, or we're not on
            // Linux. Fall back to conservative "assume same process".
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_active_children_with_child_process() {
        // Spawn a child process that sleeps, then verify has_active_children returns true
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("failed to spawn sleep");
        let our_pid = std::process::id();

        // Our process should now have at least one child
        assert!(
            has_active_children(our_pid),
            "expected has_active_children to return true when a child process is running"
        );

        // Clean up
        let mut child = child;
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn has_active_children_without_child_process() {
        // PID 1 (init/systemd) always exists but its children aren't ours.
        // Use a nonsense PID that doesn't exist.
        assert!(
            !has_active_children(u32::MAX - 1),
            "expected has_active_children to return false for a nonexistent PID"
        );
    }

    #[test]
    fn has_active_children_after_child_exits() {
        // Spawn a child that exits immediately, then verify no active children
        let child = std::process::Command::new("true")
            .spawn()
            .expect("failed to spawn true");
        let mut child = child;
        child.wait().ok(); // Wait for it to exit

        // After the child exits, check with a fresh spawn to avoid flakiness
        // from other test children. Use a dedicated subprocess as the parent.
        let parent = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("failed to spawn sleep");
        let parent_pid = parent.id();

        // The sleep process has no children of its own
        assert!(
            !has_active_children(parent_pid),
            "expected has_active_children to return false for a process with no children"
        );

        // Clean up
        let mut parent = parent;
        parent.kill().ok();
        parent.wait().ok();
    }
}

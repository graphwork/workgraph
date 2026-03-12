//! Agent service layer
//!
//! Provides agent registry and management for the workgraph agent service.
//!
//! This module includes:
//! - Executor configuration for spawning agents
//! - Agent registry for tracking running agents

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

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

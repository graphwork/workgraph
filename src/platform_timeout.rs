//! Cross-platform replacement for `timeout(1)`.
//!
//! Unix has `timeout(1)`, which wraps a command and kills it (exit code 124)
//! if it runs past a deadline. Windows has no equivalent — `TIMEOUT.EXE` is an
//! interactive "press any key to continue, else wait N seconds" utility, and
//! fails with "Default option is not allowed more than '1' time(s)" when
//! invoked with `timeout(1)`-style arguments. Any workgraph code that shells
//! out to `timeout` therefore breaks on native Windows.
//!
//! [`spawn_with_timeout`] hides the platform difference behind one API:
//!
//! - On Unix: prefixes the command with `timeout <secs>s`, preserving the
//!   exit-code-124-on-timeout semantics callers may be relying on.
//! - On Windows: spawns the program directly and arms a background thread
//!   that runs `taskkill /F /T /PID <pid>` at the deadline. The thread is
//!   disarmed (via an atomic flag) when the returned [`TimeoutGuard`] drops,
//!   so children that exit in time don't risk a PID-reuse race.
//!
//! Callers bind the guard to a `_killer` local that lives as long as the
//! child-wait call:
//!
//! ```ignore
//! let (mut child, _killer) = platform_timeout::spawn_with_timeout(
//!     "claude",
//!     |cmd| cmd.arg("--print").stdin(Stdio::piped()),
//!     30,
//! )?;
//! let output = child.wait_with_output()?;
//! // _killer drops here; disarms the Windows kill-thread if still pending.
//! ```

use std::ffi::OsStr;
use std::io;
use std::process::{Child, Command};

#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

/// RAII guard that disarms the Windows kill-thread when dropped.
///
/// Zero-sized on Unix (there's no kill-thread — `timeout(1)` handles it).
pub struct TimeoutGuard {
    #[cfg(windows)]
    done: Arc<AtomicBool>,
}

#[cfg(windows)]
impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
    }
}

/// Spawn `program`, with the `Command` further configured by `configure`,
/// subject to a time budget of `timeout_secs`.
///
/// See the module-level docs for the semantics on each platform. The
/// returned guard must outlive the child-wait call; bind it to a `_killer`
/// local so it drops at the right point.
pub fn spawn_with_timeout<P, F>(
    program: P,
    configure: F,
    timeout_secs: u64,
) -> io::Result<(Child, TimeoutGuard)>
where
    P: AsRef<OsStr>,
    F: FnOnce(&mut Command) -> &mut Command,
{
    #[cfg(unix)]
    {
        let mut cmd = Command::new("timeout");
        cmd.arg(format!("{}s", timeout_secs)).arg(program);
        configure(&mut cmd);
        let child = cmd.spawn()?;
        Ok((child, TimeoutGuard {}))
    }
    #[cfg(windows)]
    {
        use std::thread;
        use std::time::Duration;

        let mut cmd = Command::new(program);
        configure(&mut cmd);
        let child = cmd.spawn()?;
        let pid = child.id();

        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(timeout_secs));
            if !done_clone.load(Ordering::Acquire) {
                // Best-effort: kill the process tree. /T walks children,
                // /F forces. We ignore the exit status — the child may
                // have just finished on its own, which is fine.
                let _ = Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .output();
            }
        });

        Ok((child, TimeoutGuard { done }))
    }
}

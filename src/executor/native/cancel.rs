//! Cancellation token for the native executor run loop.
//!
//! Single source of truth for "should this agent stop?" signals. Fed
//! by multiple sources — Ctrl-C (interactive), inbox messages
//! (workgraph IPC in later stages), external commands — and consulted
//! at every turn boundary and inside every cancellable await.
//!
//! Two levels:
//!
//! - **Cooperative**: let the current tool / LLM call finish, then
//!   return to prompt. Triggered by single Ctrl-C.
//! - **Hard**: SIGKILL our subprocess tree and return to prompt now.
//!   Triggered by double Ctrl-C within `DOUBLE_TAP_WINDOW`. Implies
//!   cooperative too, so consumers that only care about "should I
//!   stop" can check `is_cooperative()` without missing hard cancels.
//!
//! # Typical use
//!
//! ```ignore
//! let cancel = CancelToken::new();
//! // Spawn the signal listener once per session.
//! cancel.clone().spawn_ctrl_c_listener();
//!
//! loop {
//!     // Turn boundary — check cooperative cancel first.
//!     if cancel.take_cooperative() {
//!         eprintln!("cancelled");
//!         break;
//!     }
//!
//!     // Cancellable await.
//!     tokio::select! {
//!         biased;
//!         _ = cancel.cancelled() => continue,
//!         res = llm_call() => { /* ... */ }
//!     }
//! }
//! ```
//!
//! The `take_cooperative()` call atomically checks-and-clears the
//! flag, so subsequent loop iterations start fresh. `cancelled()`
//! returns a future that resolves on the next cancellation (it does
//! NOT consume the flag — callers are expected to `take_cooperative`
//! at the next boundary).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// Window in which a second Ctrl-C counts as a "double tap" that
/// escalates from cooperative to hard cancel. 500ms matches muscle
/// memory for terminal users familiar with Claude Code and most
/// other REPLs that use the same convention.
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

struct Inner {
    cooperative: AtomicBool,
    hard: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                cooperative: AtomicBool::new(false),
                hard: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    /// Request a cooperative cancel. Wakes any awaiters on `cancelled()`.
    /// Idempotent — safe to call from multiple sources.
    pub fn request_cooperative(&self) {
        self.inner.cooperative.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Request a hard cancel. Also sets the cooperative flag — a hard
    /// cancel IS a cooperative cancel plus subprocess tree-kill, so
    /// any code path that only consults `is_cooperative` still stops.
    pub fn request_hard(&self) {
        self.inner.hard.store(true, Ordering::SeqCst);
        self.inner.cooperative.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Is a cooperative cancel currently pending?
    pub fn is_cooperative(&self) -> bool {
        self.inner.cooperative.load(Ordering::SeqCst)
    }

    /// Is a hard cancel currently pending?
    pub fn is_hard(&self) -> bool {
        self.inner.hard.load(Ordering::SeqCst)
    }

    /// Atomically check-and-clear the cooperative flag. Returns true
    /// if a cancel was pending. Use this at the top of each turn
    /// boundary so subsequent iterations start clean.
    pub fn take_cooperative(&self) -> bool {
        self.inner.cooperative.swap(false, Ordering::SeqCst)
    }

    /// Atomically check-and-clear the hard flag. Returns true if a
    /// hard cancel was pending. Separate from the cooperative flag
    /// so the consumer can decide *when* to perform the tree-kill
    /// (e.g., after it has pulled together its "interrupted" state).
    pub fn take_hard(&self) -> bool {
        self.inner.hard.swap(false, Ordering::SeqCst)
    }

    /// Future that resolves when a cooperative cancel is requested.
    /// Does NOT consume the flag — pair with `take_cooperative` at
    /// the next turn boundary.
    ///
    /// Designed for use inside `tokio::select!` to cancel in-flight
    /// LLM calls and tool executions.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cooperative() {
                return;
            }
            // Register with Notify *before* re-checking the flag to
            // close the race where a signal arrives between check
            // and await.
            let fut = self.inner.notify.notified();
            if self.is_cooperative() {
                return;
            }
            fut.await;
        }
    }

    /// Spawn a background task that listens for Ctrl-C (SIGINT) for
    /// the lifetime of this process and maps signals onto the token:
    ///
    /// - First Ctrl-C → cooperative cancel.
    /// - Second Ctrl-C within `DOUBLE_TAP_WINDOW` → hard cancel.
    /// - A third Ctrl-C after hard already fired → cooperative again
    ///   (the run loop is expected to have returned to prompt and
    ///   reset both flags by then; a stray Ctrl-C just cancels the
    ///   next iteration's boundary check).
    /// - After `DOUBLE_TAP_WINDOW` elapses, the tap window resets so
    ///   slow re-taps don't accidentally escalate.
    ///
    /// Re-arms after each signal so any number of Ctrl-Cs in one
    /// session are each captured. Drops out silently if the signal
    /// handler install fails (pre-existing shell that stole SIGINT,
    /// test harness with its own handler, etc.).
    pub fn spawn_ctrl_c_listener(self) {
        tokio::spawn(async move {
            let mut last_tap: Option<Instant> = None;
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    break;
                }
                let now = Instant::now();
                let is_double_tap = last_tap
                    .map(|t| now.duration_since(t) <= DOUBLE_TAP_WINDOW)
                    .unwrap_or(false);

                if is_double_tap && !self.is_hard() {
                    self.request_hard();
                    // Consume the tap window so a third rapid Ctrl-C
                    // doesn't re-fire hard.
                    last_tap = None;
                } else {
                    self.request_cooperative();
                    last_tap = Some(now);
                }
            }
        });
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_future_resolves_after_request() {
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let handle = tokio::spawn(async move {
            c2.cancelled().await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.request_cooperative();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn take_cooperative_clears_the_flag() {
        let cancel = CancelToken::new();
        cancel.request_cooperative();
        assert!(cancel.is_cooperative());
        assert!(cancel.take_cooperative());
        assert!(!cancel.is_cooperative());
        assert!(!cancel.take_cooperative());
    }

    #[tokio::test]
    async fn cancelled_returns_immediately_if_already_set() {
        let cancel = CancelToken::new();
        cancel.request_cooperative();
        tokio::time::timeout(Duration::from_millis(50), cancel.cancelled())
            .await
            .expect("should resolve immediately");
    }

    #[tokio::test]
    async fn multiple_awaiters_all_wake() {
        let cancel = CancelToken::new();
        let mut handles = vec![];
        for _ in 0..4 {
            let c = cancel.clone();
            handles.push(tokio::spawn(async move { c.cancelled().await }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.request_cooperative();
        for h in handles {
            tokio::time::timeout(Duration::from_secs(1), h)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn hard_implies_cooperative() {
        let cancel = CancelToken::new();
        cancel.request_hard();
        assert!(cancel.is_hard());
        assert!(
            cancel.is_cooperative(),
            "hard cancel must also set cooperative"
        );
    }

    #[tokio::test]
    async fn take_hard_clears_only_hard_flag() {
        let cancel = CancelToken::new();
        cancel.request_hard();
        assert!(cancel.take_hard());
        assert!(!cancel.is_hard());
        // Cooperative flag is independent — callers usually take_cooperative
        // separately at the next boundary.
        assert!(cancel.is_cooperative());
        assert!(!cancel.take_hard());
    }

    #[tokio::test]
    async fn hard_wakes_cancelled_future() {
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let handle = tokio::spawn(async move { c2.cancelled().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.request_hard();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
    }
}

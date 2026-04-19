//! Tool-progress plumbing.
//!
//! Long-running tools (deep_research, reader, chunk_map, map,
//! summarize, research, web_fetch) emit human-readable progress
//! as they work — one line per sub-step. Until now those lines
//! only went to the process's stderr, which meant:
//!
//!   - visible on screen when running `wg nex` at a terminal
//!   - captured into the daemon log (not user-visible) when
//!     running inside a coordinator subprocess
//!   - invisible to `wg tui` chat and `wg session attach`, whose
//!     display reads from `chat/<ref>/.streaming`
//!
//! This module bridges: a tool's `execute_streaming` scopes a
//! `tokio::task_local` progress callback that routes to whatever
//! destination the caller plugged in (usually the chat-transcript
//! mirror in `agent.rs`). Any code path inside the tool can call
//! `progress!("anything")` — the macro writes to stderr *and*
//! forwards to the callback if one is set, so stderr behavior is
//! unchanged while TUI / attach gains the same live view.
//!
//! Task-local (not thread-local) because tokio can migrate tasks
//! across worker threads between `.await` points — a thread-local
//! would be seen inconsistently. `task_local!` survives those
//! migrations, so nested tools, spawned joins, and select branches
//! all see the correct callback.

use std::sync::Arc;

/// Callback invoked for each progress line a tool emits. Empty or
/// whitespace-only strings are filtered out by the caller of the
/// macro; the callback can assume non-empty content.
pub type ProgressCallback = Arc<dyn Fn(String) + Send + Sync>;

tokio::task_local! {
    /// The currently-active progress callback for this task. Set
    /// by a tool's `execute_streaming` wrapper, read by the
    /// `progress!` macro inside the tool body. Absent outside of
    /// an `execute_streaming` call — `emit_if_set` is a no-op in
    /// that case, so non-streaming callers don't pay any cost.
    pub static CURRENT: ProgressCallback;
}

/// Run `fut` with `callback` installed as the active progress
/// sink. Tools implementing `execute_streaming` wrap their body
/// in this; the `progress!` macro inside the body fires both
/// stderr and this callback.
///
/// `callback` takes ownership — usually built by adapting the
/// `ToolStreamCallback` the tool was handed.
pub async fn scope<F, T>(callback: ProgressCallback, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT.scope(callback, fut).await
}

/// Internal: forward `s` to the active progress callback if one
/// is set, otherwise drop it. Used by the `progress!` macro.
pub fn emit_if_set(s: &str) {
    let _ = CURRENT.try_with(|cb| cb(s.to_string()));
}

/// Emit a progress line. Writes to stderr (always, matching the
/// old `eprintln!` behavior) and forwards to the task-local
/// progress callback if one is set (so the TUI chat transcript
/// and `wg session attach` pick it up).
///
/// Format args just like `eprintln!`. Prefer one progress line
/// per meaningful sub-step — the TUI shows these as live output
/// inside the tool's box.
#[macro_export]
macro_rules! tool_progress {
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        eprintln!("{}", __line);
        $crate::executor::native::tools::progress::emit_if_set(&__line);
    }};
}

pub use crate::tool_progress;

/// Adapt a `ToolStreamCallback` (used by `execute_streaming`) into
/// the `ProgressCallback` shape `scope` expects. The stream
/// callback takes `String` by value; wrap in `Arc<dyn Fn>` so the
/// task_local can hold it cheaply.
pub fn from_tool_stream_callback(
    cb: super::ToolStreamCallback,
) -> ProgressCallback {
    // Wrap the box in a Mutex-free adapter: `ToolStreamCallback` is
    // `Fn(String)`, and we want `Fn(String)`, so it's a plain
    // re-wrap. The Arc lets us clone cheaply for nested scopes.
    Arc::new(move |s: String| cb(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_forwards_when_callback_scoped() {
        let received = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let received_cb = received.clone();
        let cb: ProgressCallback = Arc::new(move |s: String| {
            received_cb.lock().unwrap().push(s);
        });
        scope(cb, async {
            emit_if_set("line-1");
            emit_if_set("line-2");
        })
        .await;
        let got = received.lock().unwrap().clone();
        assert_eq!(got, vec!["line-1".to_string(), "line-2".to_string()]);
    }

    #[tokio::test]
    async fn emit_is_noop_without_scope() {
        // No callback scoped — emit_if_set must not panic.
        emit_if_set("orphan-progress-line");
    }

    #[tokio::test]
    async fn scope_survives_await_points() {
        let received = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let received_cb = received.clone();
        let cb: ProgressCallback = Arc::new(move |s: String| {
            received_cb.lock().unwrap().push(s);
        });
        scope(cb, async {
            emit_if_set("before yield");
            tokio::task::yield_now().await;
            emit_if_set("after yield");
        })
        .await;
        let got = received.lock().unwrap().clone();
        assert_eq!(got, vec!["before yield".to_string(), "after yield".to_string()]);
    }
}

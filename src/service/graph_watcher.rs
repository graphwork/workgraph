//! Filesystem watcher for the workgraph graph file.
//!
//! Watches `<dir>/graph.jsonl` (and any other coordinator-relevant files) using
//! [`notify-debouncer-mini`]. When a change is detected, fires a user-provided
//! callback. The debouncer collapses bursts of writes within a short window
//! into a single event, so that one logical change (which often triggers
//! multiple `write`/`fsync` syscalls) wakes the dispatcher exactly once.
//!
//! This is the *primary* trigger for the dispatcher's main loop; a slower
//! safety timer in the daemon catches anything the watcher misses (lost events,
//! NFS, time-based work).
//!
//! # Fallback behaviour
//!
//! [`GraphWatcher::start`] returns `Err(...)` if `notify` cannot initialise
//! (missing inotify support on some NFS mounts, certain WSL1 / sandbox
//! filesystems, etc.). The caller is expected to log a warning and fall back to
//! polling at a shorter interval.

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Minimum debounce window. Anything shorter and the OS may not have flushed
/// the burst yet.
pub const MIN_DEBOUNCE_MS: u64 = 10;

/// Default debounce window for graph watching.
pub const DEFAULT_DEBOUNCE_MS: u64 = 100;

/// A running graph file watcher.
///
/// While this value is alive, the underlying `notify` watcher thread is also
/// alive. Drop it to stop watching.
pub struct GraphWatcher {
    /// Held for its Drop impl. The underlying watcher thread is stopped when
    /// the debouncer is dropped.
    _debouncer: Debouncer<notify::RecommendedWatcher>,
    target: PathBuf,
}

impl GraphWatcher {
    /// Path that this watcher is targeting (the graph file).
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Start watching `graph_path`.
    ///
    /// `graph_path` does not need to exist at the time of the call: if the file
    /// hasn't been created yet (e.g. between `wg service start` and `wg init`),
    /// we watch the parent directory non-recursively, so a `Create` event for
    /// the graph file will be observed once it appears.
    ///
    /// `debounce` is the coalescing window. Multiple events arriving within
    /// this window are collapsed into a single callback. 50–200ms is a sane
    /// range; values below `MIN_DEBOUNCE_MS` are clamped up.
    ///
    /// `callback` is invoked from the watcher's internal thread whenever a
    /// debounced batch of events touches the graph file (or its parent dir, in
    /// case the file is being created/replaced). It should return quickly: do
    /// the actual work elsewhere (e.g. set an atomic flag, write to a self-pipe,
    /// or send an IPC request).
    pub fn start<F>(graph_path: &Path, debounce: Duration, mut callback: F) -> Result<Self>
    where
        F: FnMut() + Send + 'static,
    {
        let parent = graph_path.parent().unwrap_or_else(|| Path::new("."));
        // Make sure the parent exists so `watch()` doesn't fail.
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create watch dir {}", parent.display()))?;

        let debounce = debounce.max(Duration::from_millis(MIN_DEBOUNCE_MS));
        let target = graph_path.to_path_buf();
        let target_for_callback = target.clone();
        let target_filename = target.file_name().map(|f| f.to_os_string());

        let mut debouncer = new_debouncer(debounce, move |res: DebounceEventResult| {
            match res {
                Ok(events) => {
                    let relevant = events.iter().any(|e| {
                        // Direct file match (most common case once the file exists).
                        if e.path == target_for_callback {
                            return true;
                        }
                        // Path may be reported relative to the parent or with a
                        // different canonical form on some FS. Match by filename.
                        if let (Some(ev_name), Some(ref t_name)) =
                            (e.path.file_name(), target_filename.as_ref())
                            && ev_name == t_name.as_os_str()
                        {
                            return true;
                        }
                        false
                    });
                    if relevant {
                        callback();
                    }
                }
                Err(_errs) => {
                    // Errors from the underlying watcher (e.g. queue overflow).
                    // We deliberately don't surface them: the safety timer will
                    // still kick the loop on schedule.
                }
            }
        })
        .context("Failed to create notify debouncer")?;

        debouncer
            .watcher()
            .watch(parent, RecursiveMode::NonRecursive)
            .with_context(|| format!("Failed to watch directory {}", parent.display()))?;

        Ok(GraphWatcher {
            _debouncer: debouncer,
            target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// Wait up to `timeout` for `cond` to return true. Returns true if it did.
    fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    /// Append a single line to the graph file, creating it if missing.
    fn append(path: &Path, line: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open graph file");
        writeln!(f, "{}", line).expect("write");
        f.sync_all().ok();
    }

    #[test]
    fn fires_on_graph_write() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = tmp.path().join("graph.jsonl");
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let _w = GraphWatcher::start(&graph, Duration::from_millis(30), move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .expect("start watcher");

        // Give the watcher a moment to register the directory watch.
        std::thread::sleep(Duration::from_millis(50));

        append(&graph, r#"{"id":"t1"}"#);

        // Should fire within 500ms (30ms debounce + scheduling).
        let fired = wait_for(Duration::from_millis(500), || {
            counter.load(Ordering::SeqCst) >= 1
        });
        assert!(
            fired,
            "watcher did not fire within 500ms (count={})",
            counter.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn debounces_burst_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = tmp.path().join("graph.jsonl");
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();

        // Use a relatively long debounce to make the test deterministic across
        // FS / scheduler quirks.
        let _w = GraphWatcher::start(&graph, Duration::from_millis(150), move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .expect("start watcher");

        std::thread::sleep(Duration::from_millis(50));

        // Burst of 10 writes spaced by ~5ms each (well inside the debounce
        // window). Total burst duration ~50ms.
        for i in 0..10 {
            append(&graph, &format!(r#"{{"id":"t{}"}}"#, i));
            std::thread::sleep(Duration::from_millis(5));
        }

        // Wait long enough for the debounce window to expire and any followup
        // events to drain.
        std::thread::sleep(Duration::from_millis(500));

        let count = counter.load(Ordering::SeqCst);
        assert!(
            count >= 1,
            "watcher should fire at least once (count={})",
            count
        );
        // Without debounce we'd see ~10 callbacks. Debounced, we expect ≤ 2.
        // Allow a tiny bit of slack for FS implementations that emit a Create
        // event ahead of subsequent Modify events landing in a separate batch.
        assert!(
            count <= 2,
            "watcher should debounce burst writes (count={}, expected ≤ 2)",
            count
        );
    }

    #[test]
    fn handles_missing_graph_file_at_startup() {
        // Graph file does NOT exist when watcher starts. This simulates the
        // race between `wg service start` and `wg init`.
        let tmp = tempfile::tempdir().unwrap();
        let graph = tmp.path().join("graph.jsonl");
        assert!(!graph.exists());

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let watcher = GraphWatcher::start(&graph, Duration::from_millis(50), move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .expect("watcher must start even when graph file missing");

        std::thread::sleep(Duration::from_millis(50));

        // Now create the graph file.
        append(&graph, r#"{"id":"first"}"#);

        let fired = wait_for(Duration::from_millis(500), || {
            counter.load(Ordering::SeqCst) >= 1
        });
        assert!(
            fired,
            "watcher should fire when missing graph file is created (count={})",
            counter.load(Ordering::SeqCst)
        );
        assert_eq!(watcher.target(), &graph);
    }

    #[test]
    fn ignores_unrelated_files_in_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = tmp.path().join("graph.jsonl");
        let other = tmp.path().join("config.toml");

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let _w = GraphWatcher::start(&graph, Duration::from_millis(50), move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .expect("start watcher");

        std::thread::sleep(Duration::from_millis(50));

        // Write to an unrelated file in the same dir.
        append(&other, "executor = \"claude\"\n");

        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "watcher should not fire for unrelated files"
        );
    }
}

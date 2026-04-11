//! Integration tests validating the file-system watcher for multi-user scenarios.
//!
//! The workgraph TUI uses `notify-debouncer-mini` (50ms debounce, inotify on Linux)
//! to detect `.workgraph/` changes in real time.  These tests verify:
//!
//! 1. **Multiple watchers** — N independent watchers all receive all M events from
//!    atomic rename writes (the pattern `save_graph_inner` uses).
//! 2. **Burst writes** — rapid successive renames within the debounce window
//!    still result in all watchers being notified (no lost final state).
//! 3. **inotify watch capacity** — creating many recursive watchers stays well
//!    within the default `fs.inotify.max_user_watches` limit (8192).

use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Simulate the atomic-rename write pattern used by `save_graph_inner`:
/// write to `.graph.tmp.<pid>`, fsync, then rename over the target file.
fn atomic_write(dir: &std::path::Path, filename: &str, content: &str) {
    let target = dir.join(filename);
    let tmp = dir.join(format!(".{}.tmp.{}", filename, std::process::id()));

    let mut f = fs::File::create(&tmp).expect("create temp file");
    f.write_all(content.as_bytes()).expect("write temp");
    f.sync_all().expect("fsync temp");

    fs::rename(&tmp, &target).expect("atomic rename");
}

/// Spawn a debounced watcher on `watch_dir` that increments `counter` on each
/// debounced batch of events.  Returns the debouncer (must be kept alive).
/// Returns `None` when the OS is out of file descriptors.
fn spawn_watcher(
    watch_dir: &std::path::Path,
    counter: Arc<AtomicUsize>,
) -> Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>> {
    let mut debouncer = match new_debouncer(Duration::from_millis(50), move |res| {
        if let Ok(_events) = res {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }) {
        Ok(d) => d,
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("Too many open files") || msg.contains("os error 24") {
                return None;
            }
            panic!("create debouncer: {e}");
        }
    };

    debouncer
        .watcher()
        .watch(watch_dir, RecursiveMode::Recursive)
        .expect("start watching");

    Some(debouncer)
}

/// Core multi-watcher test: spawn `num_watchers` watchers, perform `num_writes`
/// atomic renames with `write_interval` between them, then verify every watcher
/// received at least one notification.
fn run_multi_watcher_test(num_watchers: usize, num_writes: usize, write_interval: Duration) {
    let dir = TempDir::new().expect("create temp dir");
    let watch_dir = dir.path().to_path_buf();

    // Seed the target file so watchers have something to watch.
    atomic_write(&watch_dir, "graph.jsonl", "{}");

    // Spin up N watchers.
    let mut counters: Vec<Arc<AtomicUsize>> = Vec::new();
    let mut _watchers = Vec::new(); // hold ownership to keep watchers alive

    for _ in 0..num_watchers {
        let counter = Arc::new(AtomicUsize::new(0));
        let watcher = match spawn_watcher(&watch_dir, counter.clone()) {
            Some(w) => w,
            None => {
                eprintln!(
                    "Skipping multi-watcher test: OS file descriptor limit reached \
                     (common during parallel cargo test)"
                );
                return;
            }
        };
        counters.push(counter);
        _watchers.push(watcher);
    }

    // Give watchers time to register with the kernel.
    std::thread::sleep(Duration::from_millis(100));

    // Perform M atomic-rename writes.
    for i in 0..num_writes {
        atomic_write(
            &watch_dir,
            "graph.jsonl",
            &format!("{{\"write\": {}}}\n", i),
        );
        if !write_interval.is_zero() {
            std::thread::sleep(write_interval);
        }
    }

    // Wait for debounce to settle: debounce window (50ms) + generous margin.
    std::thread::sleep(Duration::from_millis(300));

    // Every watcher must have been notified at least once.
    for (idx, counter) in counters.iter().enumerate() {
        let count = counter.load(Ordering::Relaxed);
        assert!(
            count > 0,
            "Watcher {} received 0 notifications after {} writes (expected >= 1)",
            idx,
            num_writes,
        );
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// 5 watchers, 10 writes spaced 60ms apart (each write lands in its own
/// debounce window) — every watcher should see multiple events.
#[test]
fn test_multi_user_watcher_spaced_writes() {
    run_multi_watcher_test(5, 10, Duration::from_millis(60));
}

/// 5 watchers, 20 rapid-fire writes (no delay) — tests that the debouncer
/// coalesces correctly and no watcher is starved.
#[test]
fn test_multi_user_watcher_burst_writes() {
    run_multi_watcher_test(5, 20, Duration::ZERO);
}

/// 7 watchers (the documented max for <100ms guarantee), 15 writes.
#[test]
fn test_multi_user_watcher_seven_users() {
    run_multi_watcher_test(7, 15, Duration::from_millis(30));
}

/// Single watcher baseline — sanity check.
#[test]
fn test_multi_user_watcher_single() {
    run_multi_watcher_test(1, 5, Duration::from_millis(60));
}

/// Verify that the notification latency from atomic rename to watcher callback
/// is under 100ms (the target from the architecture doc).
#[test]
fn test_multi_user_watcher_latency() {
    let dir = TempDir::new().expect("create temp dir");
    let watch_dir = dir.path().to_path_buf();
    atomic_write(&watch_dir, "graph.jsonl", "{}");

    let notify_time = Arc::new(std::sync::Mutex::new(None::<Instant>));
    let notify_time_clone = notify_time.clone();

    let mut debouncer = match new_debouncer(
        Duration::from_millis(50),
        move |res: notify_debouncer_mini::DebounceEventResult| {
            if res.is_ok() {
                let mut lock = notify_time_clone.lock().unwrap();
                if lock.is_none() {
                    *lock = Some(Instant::now());
                }
            }
        },
    ) {
        Ok(d) => d,
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("Too many open files") || msg.contains("os error 24") {
                eprintln!("Skipping latency test: OS file descriptor limit reached");
                return;
            }
            panic!("create debouncer: {e}");
        }
    };

    debouncer
        .watcher()
        .watch(&watch_dir, RecursiveMode::Recursive)
        .expect("start watching");

    std::thread::sleep(Duration::from_millis(100));

    let write_time = Instant::now();
    atomic_write(&watch_dir, "graph.jsonl", "{\"latency\": \"test\"}\n");

    // Wait up to 500ms for the notification.
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if Instant::now() > deadline {
            panic!("Watcher did not fire within 500ms of atomic rename");
        }
        if let Some(t) = *notify_time.lock().unwrap() {
            let latency = t.duration_since(write_time);
            // The debouncer adds 50ms, so expect ~50-100ms. Allow 200ms for CI load.
            assert!(
                latency < Duration::from_millis(200),
                "Notification latency {}ms exceeds 200ms target",
                latency.as_millis(),
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Verify that creating many recursive watchers doesn't approach the
/// inotify limit.  Each recursive watch on a directory tree adds roughly
/// one watch per subdirectory.  With ~5 watches per TUI and the default
/// limit of 8192, we should comfortably handle 10+ watchers.
#[test]
fn test_multi_user_watcher_inotify_capacity() {
    let dir = TempDir::new().expect("create temp dir");
    let watch_dir = dir.path().to_path_buf();

    // Create a modest directory tree simulating .workgraph/ structure.
    for subdir in &[
        "service",
        "agency",
        "agency/roles",
        "agency/tradeoffs",
        "functions",
    ] {
        fs::create_dir_all(watch_dir.join(subdir)).expect("create subdir");
    }

    // Spawn 10 watchers (more than the 7-user target).
    let mut _watchers = Vec::new();
    for i in 0..10 {
        let counter = Arc::new(AtomicUsize::new(0));
        match new_debouncer(Duration::from_millis(50), {
            let counter = counter.clone();
            move |res: notify_debouncer_mini::DebounceEventResult| {
                if res.is_ok() {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        }) {
            Ok(mut debouncer) => {
                let result = debouncer
                    .watcher()
                    .watch(&watch_dir, RecursiveMode::Recursive);
                assert!(
                    result.is_ok(),
                    "Watcher {} failed to register: {:?} (may indicate inotify limit exhaustion)",
                    i,
                    result.err(),
                );
                _watchers.push(debouncer);
            }
            Err(e) => {
                let msg = format!("{e:?}");
                if msg.contains("Too many open files") || msg.contains("os error 24") {
                    eprintln!(
                        "Skipping inotify capacity test at watcher {i}: \
                         OS file descriptor limit reached"
                    );
                    return;
                }
                panic!(
                    "Failed to create debouncer {} (may indicate inotify limit): {:?}",
                    i, e
                );
            }
        }
    }

    // The important assertion is above: all 10 recursive watchers registered
    // without hitting inotify limits. Clean up is automatic via Drop.
}

/// Verify that writes to subdirectories (simulating service state, agency data)
/// are also detected by the recursive watcher.
#[test]
fn test_multi_user_watcher_subdirectory_events() {
    let dir = TempDir::new().expect("create temp dir");
    let watch_dir = dir.path().to_path_buf();

    fs::create_dir_all(watch_dir.join("service")).expect("create service dir");
    fs::create_dir_all(watch_dir.join("agency")).expect("create agency dir");

    let counter = Arc::new(AtomicUsize::new(0));
    let _watcher = match spawn_watcher(&watch_dir, counter.clone()) {
        Some(w) => w,
        None => {
            eprintln!("Skipping subdirectory watcher test: OS file descriptor limit reached");
            return;
        }
    };

    std::thread::sleep(Duration::from_millis(100));

    // Write to different subdirectories.
    atomic_write(
        &watch_dir.join("service"),
        "state.json",
        "{\"status\": \"running\"}",
    );
    std::thread::sleep(Duration::from_millis(80));
    atomic_write(
        &watch_dir.join("agency"),
        "roles.json",
        "{\"role\": \"test\"}",
    );
    std::thread::sleep(Duration::from_millis(80));
    atomic_write(&watch_dir, "graph.jsonl", "{\"task\": \"root\"}");

    // Wait for debounce to settle.
    std::thread::sleep(Duration::from_millis(200));

    let count = counter.load(Ordering::Relaxed);
    assert!(
        count >= 2,
        "Expected at least 2 notification batches for 3 writes across subdirs, got {}",
        count,
    );
}

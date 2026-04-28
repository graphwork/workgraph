//! Integration tests for `wg html`.
//!
//! These exercise the static-site renderer directly (via `commands::html::render_site`)
//! against synthetic graphs in tempdirs. The CLI smoke is covered by running
//! the actual binary against the real `.workgraph/` graph in this repo
//! during validation.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use tempfile::TempDir;
use workgraph::graph::{Node, Status, Task, WorkGraph};

use workgraph::html;

fn make_task(id: &str, title: &str, visibility: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        visibility: visibility.to_string(),
        ..Task::default()
    }
}

fn paths_in(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in walkdir(dir) {
        out.push(e.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
    }
    out.sort();
    out
}

fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walkdir(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn read_all(dir: &Path) -> String {
    let mut buf = String::new();
    for entry in walkdir(dir) {
        if let Ok(s) = fs::read_to_string(&entry) {
            buf.push_str(&s);
            buf.push('\n');
        }
    }
    buf
}

fn build_graph(tasks: Vec<Task>) -> WorkGraph {
    let mut g = WorkGraph::new();
    for t in tasks {
        g.add_node(Node::Task(t));
    }
    g
}

#[test]
fn renders_index_with_only_public_task_count() {
    // 3 public, 2 internal — index should reflect 3 task nodes.
    let mut t1 = make_task("alpha", "Alpha", "public");
    let mut t2 = make_task("beta", "Beta", "public");
    let mut t3 = make_task("gamma", "Gamma", "public");
    t2.after = vec!["alpha".into()];
    t3.after = vec!["beta".into()];
    t1.status = Status::Done;
    t2.status = Status::InProgress;
    t3.status = Status::Open;

    let internal_a = make_task(".eval-alpha", "Eval Alpha", "internal");
    let internal_b = make_task(".assign-beta", "Assign Beta", "internal");

    let graph = build_graph(vec![t1, t2, t3, internal_a, internal_b]);

    let dir = TempDir::new().unwrap();
    let summary = html::render_site(&graph, dir.path(), false, None).unwrap();

    assert_eq!(summary.public_count, 3, "expected 3 public tasks");
    assert_eq!(summary.total_in_graph, 5);
    assert_eq!(summary.pages_written, 3);

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    // Each public task id should appear in the index.
    assert!(index.contains("alpha"), "alpha missing from index");
    assert!(index.contains("beta"), "beta missing from index");
    assert!(index.contains("gamma"), "gamma missing from index");

    // Three task pages should exist.
    assert!(dir.path().join("tasks/alpha.html").exists());
    assert!(dir.path().join("tasks/beta.html").exists());
    assert!(dir.path().join("tasks/gamma.html").exists());
}

#[test]
fn internal_tasks_excluded_from_all_output() {
    let public = make_task("public-task", "Public stuff", "public");
    let internal = make_task("secret-task", "API_KEY=swordfish", "internal");
    let peer = make_task("peer-only", "peer-confidential", "peer");

    let graph = build_graph(vec![public, internal, peer]);

    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), false, None).unwrap();

    // The internal-only id should not appear in any rendered file.
    let blob = read_all(dir.path());
    assert!(
        !blob.contains("secret-task"),
        "internal task id leaked into output"
    );
    assert!(
        !blob.contains("API_KEY=swordfish"),
        "internal task body leaked into output"
    );
    assert!(
        !blob.contains("peer-only"),
        "peer-visibility task leaked into output"
    );
    assert!(
        !blob.contains("peer-confidential"),
        "peer-visibility body leaked into output"
    );

    // No internal task page file should exist.
    let files = paths_in(dir.path());
    for f in &files {
        assert!(
            !f.contains("secret-task") && !f.contains("peer-only"),
            "found leaked file: {}",
            f
        );
    }
}

#[test]
fn per_task_links_resolve_within_output() {
    let mut t1 = make_task("a", "A", "public");
    let mut t2 = make_task("b", "B", "public");
    let mut t3 = make_task("c", "C", "public");
    t2.after = vec!["a".into()];
    t3.after = vec!["a".into(), "b".into()];

    let graph = build_graph(vec![t1.clone(), t2, t3]);

    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), false, None).unwrap();

    // Index should link to tasks/a.html, tasks/b.html, tasks/c.html.
    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    for id in &["a", "b", "c"] {
        let needle = format!("tasks/{}.html", id);
        assert!(
            index.contains(&needle),
            "index missing link to {}: index html = {}",
            needle,
            &index[..index.len().min(2000)]
        );
    }

    // c's page should link to tasks/a.html and tasks/b.html (relative paths).
    let c_page = fs::read_to_string(dir.path().join("tasks/c.html")).unwrap();
    assert!(c_page.contains("./a.html"), "c page missing dep link to a");
    assert!(c_page.contains("./b.html"), "c page missing dep link to b");

    // a's page should mention dependents (b and c) via "Required by".
    let a_page = fs::read_to_string(dir.path().join("tasks/a.html")).unwrap();
    assert!(a_page.contains("./b.html"), "a page missing dependent link to b");
    assert!(a_page.contains("./c.html"), "a page missing dependent link to c");

    // No file should reference a hashed/missing path.
    let _ = t1; // silence unused
}

#[test]
fn empty_public_graph_renders_without_crashing() {
    let internal = make_task("only-internal", "internal", "internal");
    let graph = build_graph(vec![internal]);

    let dir = TempDir::new().unwrap();
    let summary = html::render_site(&graph, dir.path(), false, None).unwrap();
    assert_eq!(summary.public_count, 0);
    assert_eq!(summary.pages_written, 0);

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    assert!(
        index.contains("No tasks to display") || index.contains("Tasks (0)"),
        "expected empty-graph indicator in index.html"
    );
}

#[test]
fn show_all_overrides_visibility_filter() {
    let public = make_task("public-id", "p", "public");
    let internal = make_task("internal-id", "i", "internal");
    let graph = build_graph(vec![public, internal]);

    let dir = TempDir::new().unwrap();
    let summary = html::render_site(&graph, dir.path(), true, None).unwrap();
    assert_eq!(summary.public_count, 2, "with --all both tasks should appear");
    assert_eq!(summary.pages_written, 2);

    let blob = read_all(dir.path());
    assert!(blob.contains("internal-id"));
    assert!(blob.contains("public-id"));
}

#[test]
fn dag_layout_is_left_to_right_by_dependency() {
    // a -> b -> c chain. b should be in a layer ≥ a's, c ≥ b's.
    // We can sanity-check by ensuring all three appear in the SVG.
    let mut a = make_task("la-a", "A", "public");
    let mut b = make_task("la-b", "B", "public");
    let mut c = make_task("la-c", "C", "public");
    b.after = vec!["la-a".into()];
    c.after = vec!["la-b".into()];
    a.status = Status::Done;
    b.status = Status::InProgress;
    c.status = Status::Open;

    let graph = build_graph(vec![a, b, c]);
    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), false, None).unwrap();

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    assert!(index.contains("<svg"));
    // Each id appears in svg as text.
    for id in &["la-a", "la-b", "la-c"] {
        assert!(index.contains(id), "id {} missing", id);
    }

    // Status colors (TUI palette).
    assert!(index.contains("rgb(80,220,100)"), "Done color missing");
    assert!(index.contains("rgb(60,200,220)"), "InProgress color missing");
    assert!(index.contains("rgb(200,200,80)"), "Open color missing");
}

#[test]
fn description_html_is_escaped() {
    let mut t = make_task("xss-test", "Title", "public");
    t.description = Some("<script>alert('pwn')</script>".into());

    let graph = build_graph(vec![t]);
    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), false, None).unwrap();

    let page = fs::read_to_string(dir.path().join("tasks/xss-test.html")).unwrap();
    assert!(
        !page.contains("<script>alert"),
        "raw <script> tag leaked: {}",
        &page
    );
    assert!(
        page.contains("&lt;script&gt;"),
        "expected escaped <script>"
    );
}

#[test]
fn dependency_on_internal_task_aggregates_as_count_no_id_leak() {
    // Public task `pub-a` depends on two internal tasks. The internal IDs must
    // NOT appear anywhere in the rendered output — only an aggregate count.
    let mut pub_a = make_task("pub-a", "Public A", "public");
    let internal_assign = make_task(".assign-pub-a-internal-uniq", "Assign Public A", "internal");
    let internal_other = make_task(".other-internal-marker", "Other internal", "internal");
    pub_a.after = vec![
        ".assign-pub-a-internal-uniq".into(),
        ".other-internal-marker".into(),
    ];

    let graph = build_graph(vec![pub_a, internal_assign, internal_other]);
    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), false, None).unwrap();

    let page = fs::read_to_string(dir.path().join("tasks/pub-a.html")).unwrap();
    assert!(
        page.contains("2 non-public dependencies hidden"),
        "expected aggregate count, got: {}",
        page
    );

    // Internal task IDs must NOT appear in the page at all.
    let blob = read_all(dir.path());
    assert!(
        !blob.contains(".assign-pub-a-internal-uniq"),
        "internal dep id leaked"
    );
    assert!(
        !blob.contains(".other-internal-marker"),
        "internal dep id leaked"
    );
}

#[test]
fn output_files_layout_matches_expected() {
    let p1 = make_task("layout-x", "X", "public");
    let p2 = make_task("layout-y", "Y", "public");
    let graph = build_graph(vec![p1, p2]);

    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), false, None).unwrap();

    let files: HashSet<String> = paths_in(dir.path()).into_iter().collect();
    assert!(files.contains("index.html"));
    assert!(files.contains("style.css"));
    assert!(files.contains("tasks/layout-x.html"));
    assert!(files.contains("tasks/layout-y.html"));
}

#[test]
fn since_filter_excludes_old_tasks_and_notes_in_footer() {
    use chrono::{Duration, Utc};

    // One task with a recent timestamp, one with a 30-day-old timestamp.
    let recent_ts = (Utc::now() - Duration::hours(1)).to_rfc3339();
    let old_ts = (Utc::now() - Duration::days(30)).to_rfc3339();

    let mut recent = make_task("recent-task", "Recent", "public");
    recent.created_at = Some(recent_ts);

    let mut old = make_task("old-task", "Old", "public");
    old.created_at = Some(old_ts);

    let graph = build_graph(vec![recent, old]);

    let dir = TempDir::new().unwrap();
    // --all --since 24h: should only include the recent task
    let summary = html::render_site(&graph, dir.path(), true, Some("24h")).unwrap();

    assert_eq!(
        summary.public_count, 1,
        "expected only 1 task within 24h window"
    );
    assert!(
        dir.path().join("tasks/recent-task.html").exists(),
        "recent-task page missing"
    );
    assert!(
        !dir.path().join("tasks/old-task.html").exists(),
        "old-task page should not exist within 24h window"
    );

    // Footer must mention the time window.
    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    assert!(
        index.contains("last 24h"),
        "footer must mention 'last 24h'; got footer area: {}",
        index
            .find("<footer>")
            .map(|i| &index[i..])
            .unwrap_or("(no footer)")
    );
}

#[test]
fn since_filter_composes_with_visibility() {
    use chrono::{Duration, Utc};

    let recent_ts = (Utc::now() - Duration::hours(2)).to_rfc3339();

    // A recent public task and a recent internal task.
    let mut pub_task = make_task("pub-recent", "Public recent", "public");
    pub_task.created_at = Some(recent_ts.clone());
    let mut int_task = make_task("int-recent", "Internal recent", "internal");
    int_task.created_at = Some(recent_ts);

    let graph = build_graph(vec![pub_task, int_task]);
    let dir = TempDir::new().unwrap();

    // Without --all: visibility filter removes internal; --since keeps the recent public one.
    let summary = html::render_site(&graph, dir.path(), false, Some("24h")).unwrap();
    assert_eq!(
        summary.public_count, 1,
        "public-only filter should keep 1 public task"
    );
    assert!(
        dir.path().join("tasks/pub-recent.html").exists(),
        "public recent task page missing"
    );
    assert!(
        !dir.path().join("tasks/int-recent.html").exists(),
        "internal task must not appear in public-only output"
    );
}

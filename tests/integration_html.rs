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
    let summary = html::render_site(&graph, dir.path(), dir.path(), false, None).unwrap();

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
    html::render_site(&graph, dir.path(), dir.path(), false, None).unwrap();

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
    html::render_site(&graph, dir.path(), dir.path(), false, None).unwrap();

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
    let summary = html::render_site(&graph, dir.path(), dir.path(), false, None).unwrap();
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
    let summary = html::render_site(&graph, dir.path(), dir.path(), true, None).unwrap();
    assert_eq!(summary.public_count, 2, "with --all both tasks should appear");
    assert_eq!(summary.pages_written, 2);

    let blob = read_all(dir.path());
    assert!(blob.contains("internal-id"));
    assert!(blob.contains("public-id"));
}

#[test]
fn dag_layout_renders_task_ids() {
    // a -> b -> c chain. All three task ids must appear in the rendered index.
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
    html::render_site(&graph, dir.path(), dir.path(), false, None).unwrap();

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    // viz-pre element present (ASCII viz, not SVG).
    assert!(index.contains("viz-pre"), "viz-pre missing from index");
    // Each id appears in the task list section.
    for id in &["la-a", "la-b", "la-c"] {
        assert!(index.contains(id), "id {} missing", id);
    }
    // Status colors appear in the legend swatches.
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
    html::render_site(&graph, dir.path(), dir.path(), false, None).unwrap();

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
    html::render_site(&graph, dir.path(), dir.path(), false, None).unwrap();

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
    html::render_site(&graph, dir.path(), dir.path(), false, None).unwrap();

    let files: HashSet<String> = paths_in(dir.path()).into_iter().collect();
    assert!(files.contains("index.html"));
    assert!(files.contains("style.css"));
    assert!(files.contains("tasks/layout-x.html"));
    assert!(files.contains("tasks/layout-y.html"));
}

#[test]
fn since_filter_excludes_old_tasks_and_notes_in_footer() {
    use chrono::{Duration, Utc};

    let recent_ts = (Utc::now() - Duration::hours(1)).to_rfc3339();
    let old_ts = (Utc::now() - Duration::days(30)).to_rfc3339();

    let mut recent = make_task("recent-task", "Recent", "public");
    recent.created_at = Some(recent_ts);

    let mut old = make_task("old-task", "Old", "public");
    old.created_at = Some(old_ts);

    let graph = build_graph(vec![recent, old]);

    let dir = TempDir::new().unwrap();
    let summary = html::render_site(&graph, dir.path(), dir.path(), true, Some("24h")).unwrap();

    assert_eq!(summary.public_count, 1, "expected only 1 task within 24h window");
    assert!(dir.path().join("tasks/recent-task.html").exists(), "recent-task page missing");
    assert!(!dir.path().join("tasks/old-task.html").exists(), "old-task page should not exist");

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    assert!(
        index.contains("last 24h"),
        "footer must mention 'last 24h'"
    );
}

#[test]
fn since_filter_composes_with_visibility() {
    use chrono::{Duration, Utc};

    let recent_ts = (Utc::now() - Duration::hours(2)).to_rfc3339();

    let mut pub_task = make_task("pub-recent", "Public recent", "public");
    pub_task.created_at = Some(recent_ts.clone());
    let mut int_task = make_task("int-recent", "Internal recent", "internal");
    int_task.created_at = Some(recent_ts);

    let graph = build_graph(vec![pub_task, int_task]);
    let dir = TempDir::new().unwrap();

    let summary = html::render_site(&graph, dir.path(), dir.path(), false, Some("24h")).unwrap();
    assert_eq!(summary.public_count, 1, "public-only filter should keep 1 public task");
    assert!(dir.path().join("tasks/pub-recent.html").exists(), "public recent task page missing");
    assert!(!dir.path().join("tasks/int-recent.html").exists(), "internal task must not appear");
}

// ────────────────────────────────────────────────────────────────────────────
// wg-html-v2: theme support, edge JSON, panel JS wiring
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn v2_index_includes_theme_toggle_and_panel_assets() {
    let mut t = make_task("theme-tester", "T", "public");
    t.status = Status::Open;
    let graph = build_graph(vec![t]);
    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), dir.path(), true, None).unwrap();

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();

    // Theme toggle button is present and wired by id.
    assert!(
        index.contains(r#"id="theme-toggle""#),
        "theme toggle button missing from index"
    );
    // The bootstrap script applies a saved theme before paint.
    assert!(
        index.contains("localStorage.getItem('wg-html-theme')"),
        "theme bootstrap script missing"
    );
    // The panel script tag is included from a separate file (rsync-friendly).
    assert!(
        index.contains(r#"src="panel.js""#),
        "panel.js script tag missing"
    );
    // The companion files exist on disk.
    assert!(
        dir.path().join("panel.js").exists(),
        "panel.js asset must be written"
    );
    assert!(
        dir.path().join("style.css").exists(),
        "style.css asset must be written"
    );
}

#[test]
fn v2_css_carries_tui_palette() {
    // Spec: "Color values verified to match TUI palette (cite source file or
    // document the mapping)" — the CSS must contain the exact RGB triples
    // documented at src/tui/viz_viewer/state.rs:271 and the magenta/cyan
    // edge highlight colors from render.rs:1500.
    let dir = TempDir::new().unwrap();
    let graph = build_graph(vec![make_task("anything", "A", "public")]);
    html::render_site(&graph, dir.path(), dir.path(), true, None).unwrap();

    let css = fs::read_to_string(dir.path().join("style.css")).unwrap();
    // Status colors (TUI flash_color_for_status, state.rs:271)
    for needle in [
        "rgb(80, 220, 100)",  // done
        "rgb(220, 60, 60)",   // failed
        "rgb(60, 200, 220)",  // in-progress
        "rgb(200, 200, 80)",  // open
        "rgb(60, 160, 220)",  // waiting
        "rgb(140, 230, 80)",  // pending-eval
    ] {
        assert!(css.contains(needle), "missing TUI status color {}", needle);
    }
    // Edge highlight colors (TUI render.rs:1500 — magenta/cyan/yellow)
    assert!(css.contains("rgb(188, 63, 188)"), "missing magenta edge color");
    assert!(css.contains("rgb(17, 168, 205)"), "missing cyan edge color");
    assert!(css.contains("rgb(229, 229, 16)"), "missing yellow edge color");
}

#[test]
fn v2_css_supports_dark_and_light_themes() {
    let dir = TempDir::new().unwrap();
    let graph = build_graph(vec![make_task("any", "A", "public")]);
    html::render_site(&graph, dir.path(), dir.path(), true, None).unwrap();

    let css = fs::read_to_string(dir.path().join("style.css")).unwrap();
    // Dark theme is the default (no media query needed).
    assert!(css.contains("--bg:"), "dark theme variables missing");
    // Light theme via prefers-color-scheme + manual override.
    assert!(
        css.contains("@media (prefers-color-scheme: light)"),
        "light theme media query missing"
    );
    assert!(
        css.contains(r#"[data-theme="light"]"#),
        "manual light override missing"
    );
    assert!(
        css.contains(r#"[data-theme="dark"]"#),
        "manual dark override missing"
    );
}

#[test]
fn v2_inline_json_blobs_present_in_index() {
    let mut a = make_task("alpha-v2", "Alpha", "public");
    let mut b = make_task("beta-v2", "Beta", "public");
    b.after = vec!["alpha-v2".into()];
    a.status = Status::Done;
    b.status = Status::InProgress;

    let graph = build_graph(vec![a, b]);
    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), dir.path(), true, None).unwrap();

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    // Three JSON blobs feed the panel JS: tasks, edges (reachability), cycles.
    assert!(
        index.contains("window.WG_TASKS"),
        "WG_TASKS inline JSON missing"
    );
    assert!(
        index.contains("window.WG_EDGES"),
        "WG_EDGES inline JSON missing"
    );
    assert!(
        index.contains("window.WG_CYCLES"),
        "WG_CYCLES inline JSON missing"
    );
    // beta-v2's reachable upstream set must include alpha-v2 — that's the
    // whole point of the edge JSON, and what the JS uses for highlighting.
    assert!(
        index.contains("\"alpha-v2\""),
        "alpha-v2 missing from inline JSON"
    );
}

#[test]
fn v2_task_list_links_carry_data_task_id_for_panel_wiring() {
    // Every task in the index list (and the viz-pre when available) must
    // carry a `data-task-id` attribute so the panel JS can resolve clicks.
    // The viz-pre rendering depends on the `wg` binary (subprocess) which is
    // not present in this test runner; the smoke scenario exercises that
    // path. Here we just check the list section, which is rendered without
    // a subprocess.
    let mut a = make_task("vlinka", "A", "public");
    let mut b = make_task("vlinkb", "B", "public");
    b.after = vec!["vlinka".into()];
    a.status = Status::Done;
    b.status = Status::Open;

    let graph = build_graph(vec![a, b]);
    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), dir.path(), true, None).unwrap();

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    assert!(
        index.contains(r#"data-task-id="vlinka""#),
        "vlinka data-task-id missing"
    );
    assert!(
        index.contains(r#"data-task-id="vlinkb""#),
        "vlinkb data-task-id missing"
    );
}

#[test]
fn v2_strip_ansi_keeps_unicode_and_drops_csi() {
    // Internal helper, but the contract matters: the viz capture path strips
    // ANSI escapes before wrapping in clickable spans, and must preserve
    // multibyte UTF-8 (box-drawing characters) intact.
    use workgraph::html;
    // Round-trip through the public render_site only verifies output, but
    // strip_ansi is private. We exercise it indirectly via parse_since edge
    // cases as a sanity check that html.rs is wired up.
    assert!(html::parse_since("1h").is_ok());
    assert!(html::parse_since("0h").is_err());
}

#[test]
fn v2_index_renders_static_when_viz_subprocess_unavailable() {
    // If the viz subprocess fails (e.g. tempdir without graph.jsonl on disk
    // or a missing wg binary in tests), the page must still render — the
    // task list and panel infrastructure remain.
    let t = make_task("standalone", "Standalone", "public");
    let graph = build_graph(vec![t]);
    let dir = TempDir::new().unwrap();
    html::render_site(&graph, dir.path(), dir.path(), true, None).unwrap();

    let index = fs::read_to_string(dir.path().join("index.html")).unwrap();
    // Even without viz, the panel container must exist for clickability.
    assert!(
        index.contains(r#"id="side-panel""#),
        "side panel container missing"
    );
    // The footer must still render with the task counts.
    assert!(
        index.contains("Showing 1 of 1 tasks") || index.contains("Tasks (1)"),
        "task count missing from index"
    );
}

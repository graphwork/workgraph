//! `wg html`: render the workgraph as a static, clickable HTML viewer.
//!
//! Design goals (v2):
//! - **TUI parity**: render `wg viz --all` verbatim in a monospace `<pre>`,
//!   with the same color palette as the TUI (status colors from
//!   `flash_color_for_status`, edge highlights from
//!   `tui::viz_viewer::render`'s upstream/downstream selection logic).
//! - **Universal clickability**: every task id and status indicator in the
//!   viz is wrapped in a `<span class="task-link" data-task-id>` that opens
//!   a side-panel detail overlay matching what `wg show <task>` displays.
//! - **Edge highlighting**: clicking a task highlights its `--after` (upstream)
//!   edges in magenta and its consumers (downstream) in cyan, with everything
//!   else dimmed so the selection's relationships pop. This uses the
//!   `char_edge_map` produced by the viz layer (per-character edge attribution).
//! - **Theme support**: dark theme by default, with `prefers-color-scheme`
//!   auto-detection on first load and a manual toggle persisted via
//!   localStorage.
//! - **Static-rsync friendly**: vanilla JS, inline JSON, no XHR, no server.
//!   Open `<out>/index.html` over `file://` and everything works.
//!
//! The structured viz data is captured by subprocessing the same `wg`
//! binary with `viz --all --no-tui --json`. This keeps the implementation
//! contained in the library crate without requiring it to depend on the
//! binary's `commands::viz` module directly.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::graph::{Status, Task, WorkGraph};
use crate::parser::load_graph;

// ────────────────────────────────────────────────────────────────────────────
// Public API
// ────────────────────────────────────────────────────────────────────────────

/// Parse a human-readable duration string (e.g. "1h", "24h", "7d", "30d", "2w") into a
/// chrono Duration. Returns an error with a clear message on invalid input.
pub fn parse_since(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("Empty duration string — use a value like 1h, 24h, 7d, 30d");
    }
    let (num_str, unit) = if let Some(n) = s.strip_suffix('h') {
        (n, 'h')
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 'd')
    } else if let Some(n) = s.strip_suffix('w') {
        (n, 'w')
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 'm')
    } else {
        anyhow::bail!(
            "Invalid --since value '{}': expected a number followed by h/d/w/m (e.g. 1h, 24h, 7d, 30d)",
            s
        );
    };

    let num: i64 = num_str.parse().map_err(|_| {
        anyhow::anyhow!(
            "Invalid --since value '{}': '{}' is not a valid number",
            s,
            num_str
        )
    })?;

    if num <= 0 {
        anyhow::bail!("--since value must be positive (got '{}')", s);
    }

    Ok(match unit {
        'h' => Duration::hours(num),
        'd' => Duration::days(num),
        'w' => Duration::weeks(num),
        'm' => Duration::minutes(num),
        _ => unreachable!(),
    })
}

#[derive(Debug, Clone)]
pub struct RenderSummary {
    pub out_dir: std::path::PathBuf,
    pub total_in_graph: usize,
    pub public_count: usize,
    pub pages_written: usize,
    pub show_all: bool,
    pub since: Option<String>,
}

/// Public render entry point. Builds the complete static site.
pub fn render_site(
    graph: &WorkGraph,
    workgraph_dir: &Path,
    out_dir: &Path,
    show_all: bool,
    since: Option<&str>,
) -> Result<RenderSummary> {
    let since_cutoff: Option<DateTime<Utc>> = since
        .map(|s| parse_since(s).map(|d| Utc::now() - d))
        .transpose()?;

    fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create output dir: {}", out_dir.display()))?;
    let tasks_dir = out_dir.join("tasks");
    fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("failed to create tasks dir: {}", tasks_dir.display()))?;

    let all_tasks: Vec<&Task> = graph.tasks().collect();

    // Visibility filter (only when --public-only).
    let visibility_filtered: Vec<&Task> = if show_all {
        all_tasks.clone()
    } else {
        all_tasks
            .iter()
            .filter(|t| t.visibility == "public")
            .copied()
            .collect()
    };

    // Time-window filter (applied on top of visibility filter).
    let included: Vec<&Task> = if let Some(cutoff) = since_cutoff {
        visibility_filtered
            .into_iter()
            .filter(|t| task_in_window(t, cutoff))
            .collect()
    } else {
        visibility_filtered
    };
    let included_ids: HashSet<&str> = included.iter().map(|t| t.id.as_str()).collect();

    // Capture structured viz output (text + node positions + char-level edge map).
    let viz = capture_viz_json(workgraph_dir, show_all);

    // Eval scores per task.
    let evals = load_eval_scores(workgraph_dir);

    // Compute reachable upstream + downstream sets per task. Used by the JS
    // layer to highlight the "before" / "after" pattern of edges on click.
    let edge_reach = compute_edge_reachability(graph, &included_ids);

    // Build the inline JSON blobs.
    let tasks_json = build_tasks_json(graph, &included, &evals, &included_ids);
    let edges_json = build_edges_json(&edge_reach);
    let cycles_json = build_cycles_json(&viz);

    // Write static assets.
    fs::write(out_dir.join("style.css"), STYLE_CSS).context("failed to write style.css")?;
    fs::write(out_dir.join("panel.js"), PANEL_JS).context("failed to write panel.js")?;

    // Render the index.
    let index_html = render_index(
        graph,
        &included,
        &included_ids,
        &viz,
        &tasks_json,
        &edges_json,
        &cycles_json,
        show_all,
        since,
    );
    fs::write(out_dir.join("index.html"), &index_html).context("failed to write index.html")?;

    // Per-task pages (deep-link targets — work with file:// URLs).
    let mut pages_written = 0usize;
    for task in &included {
        let eval = evals.get(&task.id);
        let html = render_task_page(task, graph, &included_ids, eval);
        let path = tasks_dir.join(format!("{}.html", url_encode_id(&task.id)));
        fs::write(&path, html).with_context(|| format!("failed to write {}", path.display()))?;
        pages_written += 1;
    }

    Ok(RenderSummary {
        out_dir: out_dir.to_path_buf(),
        total_in_graph: graph.tasks().count(),
        public_count: included.len(),
        pages_written,
        show_all,
        since: since.map(|s| s.to_string()),
    })
}

pub fn run(workgraph_dir: &Path, out: &Path, all: bool, since: Option<&str>, json: bool) -> Result<()> {
    let graph_path = workgraph_dir.join("graph.jsonl");
    if !graph_path.exists() {
        anyhow::bail!(
            "Workgraph not initialized at {}. Run `wg init` first.",
            workgraph_dir.display()
        );
    }
    let graph = load_graph(&graph_path).context("failed to load graph")?;

    let summary = render_site(&graph, workgraph_dir, out, all, since)?;

    if json {
        let payload = serde_json::json!({
            "out_dir": summary.out_dir.display().to_string(),
            "total_in_graph": summary.total_in_graph,
            "public_count": summary.public_count,
            "pages_written": summary.pages_written,
            "show_all": summary.show_all,
            "since": summary.since,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        let filter = if summary.show_all {
            format!("all tasks ({} included)", summary.public_count)
        } else {
            format!(
                "{} public of {} total",
                summary.public_count, summary.total_in_graph,
            )
        };
        println!(
            "Wrote {} pages to {} ({})",
            summary.pages_written + 1,
            summary.out_dir.display(),
            filter,
        );
        println!("Open {}/index.html in a browser.", summary.out_dir.display());
    }

    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Static assets (compiled in via include_str!)
// ────────────────────────────────────────────────────────────────────────────

const STYLE_CSS: &str = include_str!("html_assets/style.css");
const PANEL_JS: &str = include_str!("html_assets/panel.js");

// ────────────────────────────────────────────────────────────────────────────
// Status helpers
// ────────────────────────────────────────────────────────────────────────────

/// Return true if the task has any timestamp that falls within the window.
fn task_in_window(task: &Task, cutoff: DateTime<Utc>) -> bool {
    let timestamps: &[Option<&str>] = &[
        task.created_at.as_deref(),
        task.started_at.as_deref(),
        task.completed_at.as_deref(),
        task.last_iteration_completed_at.as_deref(),
    ];
    timestamps.iter().flatten().any(|ts| {
        DateTime::parse_from_rfc3339(ts)
            .map(|dt| dt.with_timezone(&Utc) >= cutoff)
            .unwrap_or(false)
    })
}

/// Status color — RGB triples mirror the TUI palette in
/// `tui::viz_viewer::state::flash_color_for_status` (state.rs:271).
fn status_color(status: Status) -> &'static str {
    match status {
        Status::Done => "rgb(80,220,100)",
        Status::Failed => "rgb(220,60,60)",
        Status::InProgress => "rgb(60,200,220)",
        Status::Open => "rgb(200,200,80)",
        Status::Blocked => "rgb(180,120,60)",
        Status::Abandoned => "rgb(140,100,160)",
        Status::Waiting | Status::PendingValidation => "rgb(60,160,220)",
        Status::PendingEval => "rgb(140,230,80)",
        Status::Incomplete => "rgb(255,165,0)",
    }
}

fn status_class(status: Status) -> &'static str {
    match status {
        Status::Done => "done",
        Status::Failed => "failed",
        Status::InProgress => "in-progress",
        Status::Open => "open",
        Status::Blocked => "blocked",
        Status::Abandoned => "abandoned",
        Status::Waiting => "waiting",
        Status::PendingValidation => "pending-validation",
        Status::PendingEval => "pending-eval",
        Status::Incomplete => "incomplete",
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn url_encode_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => out.push_str(&format!("%{:02X}", ch as u32)),
        }
    }
    out
}

fn task_filename(id: &str) -> String {
    format!("tasks/{}.html", url_encode_id(id))
}

// ────────────────────────────────────────────────────────────────────────────
// Structured viz capture (subprocess `wg viz --json`)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct VizJson {
    /// Rendered ASCII (may contain ANSI escapes).
    #[serde(default)]
    text: String,
    /// task_id → line index.
    #[serde(default)]
    node_lines: BTreeMap<String, usize>,
    /// per-character edge cells.
    #[serde(default)]
    char_edges: Vec<CharEdge>,
    /// task_id → list of cycle members (only for tasks in non-trivial SCCs).
    #[serde(default)]
    cycle_members: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CharEdge {
    line: usize,
    col: usize,
    from: String,
    to: String,
}

/// Capture viz output as structured JSON via subprocess. Falls back to an
/// empty viz if the subprocess fails (the page still renders, just without
/// the ASCII tree section).
fn capture_viz_json(workgraph_dir: &Path, show_all: bool) -> VizJson {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return VizJson::default(),
    };
    // The `--json` flag is the global one (clap-level); placing it before the
    // subcommand keeps clap from rejecting it as a subcommand-local option.
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--dir")
        .arg(workgraph_dir)
        .arg("--json")
        .arg("viz")
        .arg("--no-tui")
        .arg("--columns")
        .arg("140")
        .arg("--edge-color")
        .arg("gray");
    if show_all {
        cmd.arg("--all");
    }
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return VizJson::default(),
    };
    serde_json::from_slice(&out.stdout).unwrap_or_default()
}

// ────────────────────────────────────────────────────────────────────────────
// ANSI strip + viz HTML rendering
// ────────────────────────────────────────────────────────────────────────────

/// Remove ANSI CSI escape sequences (\x1b[...m) from text. We strip them
/// because we apply our own coloring via CSS classes per character.
fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // CSI: skip until a final byte in 0x40..=0x7e
            i += 2;
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&b) {
                    break;
                }
            }
        } else {
            // Push the next valid UTF-8 character (avoid splitting multibyte chars).
            let ch_start = i;
            let first = bytes[i];
            let len = if first < 0x80 {
                1
            } else if first < 0xc0 {
                1 // shouldn't happen for valid UTF-8 — treat as 1
            } else if first < 0xe0 {
                2
            } else if first < 0xf0 {
                3
            } else {
                4
            };
            let end = (ch_start + len).min(bytes.len());
            if let Ok(s) = std::str::from_utf8(&bytes[ch_start..end]) {
                out.push_str(s);
            }
            i = end;
        }
    }
    out
}

/// Render the captured viz text into clickable HTML.
///
/// Strategy:
/// 1. Strip ANSI escapes (we apply our own coloring).
/// 2. For each line, build a per-character "marker" map:
///    - `Marker::TaskLink(id, status)` for cells that fall inside a task-id
///      label (or the trailing status-indicator parens).
///    - `Marker::Edge(edges)` for cells in the `char_edge_map`.
///    - `Marker::Plain` otherwise.
/// 3. Walk character cells, opening/closing spans on marker transitions.
fn render_viz_html(
    viz: &VizJson,
    graph: &WorkGraph,
    included_ids: &HashSet<&str>,
) -> String {
    let plain = strip_ansi(&viz.text);
    if plain.trim().is_empty() {
        return "<pre class=\"viz-pre\">(no tasks to display)</pre>".to_string();
    }

    // Per-line cells of (column → list of edges). Note: char_edge_map columns
    // are visible-column indices (not byte offsets). We line up by chars.
    let mut edges_by_pos: HashMap<(usize, usize), Vec<(String, String)>> = HashMap::new();
    for e in &viz.char_edges {
        edges_by_pos
            .entry((e.line, e.col))
            .or_default()
            .push((e.from.clone(), e.to.clone()));
    }

    // node_lines maps task_id → line index. We'll also need its status.
    let task_status: HashMap<&str, Status> = graph
        .tasks()
        .map(|t| (t.id.as_str(), t.status))
        .collect();

    // For each line, identify task-id occurrences. The viz typically renders a
    // single task on its own line, but a task id may appear multiple times
    // (e.g., a header summary line). We mark every literal occurrence of any
    // included task id within the line.
    let mut task_id_strs: Vec<&str> = included_ids.iter().copied().collect();
    // Match longest ids first so 'foo-bar' doesn't mask 'foo'.
    task_id_strs.sort_by(|a, b| b.len().cmp(&a.len()));

    let mut html = String::with_capacity(plain.len() * 2);
    html.push_str("<pre class=\"viz-pre\">");

    for (line_idx, line) in plain.lines().enumerate() {
        // Collect `(start_char_idx, end_char_idx, task_id)` ranges where a
        // task id (and its trailing "  (status...)" decorator) lives. The
        // decorator is included so that clicking the status-glyph parens
        // opens the same task as clicking the id.
        let line_chars: Vec<char> = line.chars().collect();
        let line_str: String = line_chars.iter().collect();
        let mut task_ranges: Vec<(usize, usize, &str, Status)> = Vec::new();

        // Use byte-index find, then convert to char index.
        for &id in &task_id_strs {
            // Find every occurrence of the id in this line whose surrounding
            // characters are not identifier-y (so we don't match 'foo' inside
            // 'foo-bar').
            let mut byte_search_start = 0usize;
            while let Some(rel) = line[byte_search_start..].find(id) {
                let byte_pos = byte_search_start + rel;
                let byte_end = byte_pos + id.len();
                // Boundary check
                let prev_ok = byte_pos == 0
                    || line[..byte_pos]
                        .chars()
                        .next_back()
                        .map(|c| !is_id_char(c))
                        .unwrap_or(true);
                let next_ok = byte_end == line.len()
                    || line[byte_end..]
                        .chars()
                        .next()
                        .map(|c| !is_id_char(c))
                        .unwrap_or(true);
                if prev_ok && next_ok {
                    let char_start = line[..byte_pos].chars().count();
                    let mut char_end = char_start + id.chars().count();
                    // Extend the range across an immediately-following
                    // status decorator like "  (in-progress · ...)" so the
                    // whole label is clickable.
                    char_end = extend_through_status_decorator(&line_chars, char_end);
                    let st = task_status.get(id).copied().unwrap_or(Status::Open);
                    task_ranges.push((char_start, char_end, id, st));
                }
                byte_search_start = byte_pos + id.len();
            }
        }

        // Resolve overlaps: prefer earlier start, longer end. Sort and dedupe.
        task_ranges.sort_by(|a, b| (a.0, std::cmp::Reverse(a.1)).cmp(&(b.0, std::cmp::Reverse(b.1))));
        let mut nonoverlapping: Vec<(usize, usize, &str, Status)> = Vec::new();
        for r in task_ranges {
            if let Some(last) = nonoverlapping.last() {
                if r.0 < last.1 {
                    continue;
                }
            }
            nonoverlapping.push(r);
        }

        render_line(
            &mut html,
            line_idx,
            &line_chars,
            &nonoverlapping,
            &edges_by_pos,
            &line_str,
        );
        html.push('\n');
    }

    html.push_str("</pre>");
    html
}

/// Walk one line's character cells emitting plain text, edge spans, or
/// task-link spans as appropriate.
fn render_line(
    out: &mut String,
    line_idx: usize,
    line_chars: &[char],
    task_ranges: &[(usize, usize, &str, Status)],
    edges_by_pos: &HashMap<(usize, usize), Vec<(String, String)>>,
    _line_str: &str,
) {
    let mut col = 0usize;
    let mut range_iter = task_ranges.iter().peekable();

    while col < line_chars.len() {
        // Are we at the start of a task-link range?
        if let Some(&(start, end, id, status)) = range_iter.peek() {
            if col == *start {
                let s_class = status_class(*status);
                out.push_str("<span class=\"task-link\" data-task-id=\"");
                out.push_str(&escape_html(id));
                out.push_str("\" data-status=\"");
                out.push_str(s_class);
                out.push_str("\">");
                // Emit characters within the range (each cell may also be an
                // edge cell — e.g., a connector inside a label is rare but
                // possible — however, we prefer the task-link semantics here
                // so the whole label clicks as one task).
                let span_end = (*end).min(line_chars.len());
                for c in &line_chars[col..span_end] {
                    out.push_str(&escape_html(&c.to_string()));
                }
                out.push_str("</span>");
                col = span_end;
                range_iter.next();
                continue;
            }
        }

        // Otherwise emit a single character — wrapped in an edge span if a
        // char_edge_map entry exists at (line_idx, col).
        let c = line_chars[col];
        if let Some(edges) = edges_by_pos.get(&(line_idx, col)) {
            // Build the data-edges attribute as `from1>to1|from2>to2|…`.
            // We use `>` as the separator (not in task ids) and `|` for list.
            let mut data = String::new();
            for (i, (from, to)) in edges.iter().enumerate() {
                if i > 0 {
                    data.push('|');
                }
                data.push_str(&escape_html(from));
                data.push('>');
                data.push_str(&escape_html(to));
            }
            out.push_str("<span class=\"edge\" data-edges=\"");
            out.push_str(&data);
            out.push_str("\">");
            out.push_str(&escape_html(&c.to_string()));
            out.push_str("</span>");
        } else {
            // Plain text cell — wrapped in a `text-cell` span only when its
            // line contains a task-link (so the dim-others rule doesn't dim
            // structure-less header rows). For simplicity we always emit a
            // text-cell span — the dimming rule fires only with `body[data-
            // selected]` so unrelated pages aren't affected.
            out.push_str(&escape_html(&c.to_string()));
        }
        col += 1;
    }
}

/// True if `c` is part of a task identifier.
fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

/// If position `start_col` lies right after a task id, see whether the next
/// chars match `  (` (two spaces then a left paren). If so, extend the range
/// through the matching closing paren so the whole "(status · ...)" decorator
/// is part of the clickable region.
fn extend_through_status_decorator(line_chars: &[char], start_col: usize) -> usize {
    let mut i = start_col;
    // Skip spaces.
    let space_start = i;
    while i < line_chars.len() && line_chars[i] == ' ' {
        i += 1;
    }
    // If we didn't find at least one space + `(`, return the original start.
    if i == space_start || i >= line_chars.len() || line_chars[i] != '(' {
        return start_col;
    }
    // Walk to the matching `)` (no nesting in our format).
    let mut depth = 0;
    while i < line_chars.len() {
        match line_chars[i] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    // Unmatched paren — extend to end of line.
    line_chars.len()
}

// ────────────────────────────────────────────────────────────────────────────
// Eval scores
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct EvalSummary {
    score: f64,
    dimensions: Vec<(String, f64)>,
}

fn load_eval_scores(workgraph_dir: &Path) -> HashMap<String, EvalSummary> {
    let evals_dir = workgraph_dir.join("agency").join("evaluations");
    let mut latest: HashMap<String, (String, EvalSummary)> = HashMap::new();
    let entries = match fs::read_dir(&evals_dir) {
        Ok(e) => e,
        Err(_) => return HashMap::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let task_id = match v.get("task_id").and_then(|x| x.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let score = match v.get("score").and_then(|x| x.as_f64()) {
            Some(s) => s,
            None => continue,
        };
        let timestamp = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let dims: Vec<(String, f64)> = v
            .get("dimensions")
            .and_then(|d| d.as_object())
            .map(|obj| {
                let mut pairs: Vec<(String, f64)> = obj
                    .iter()
                    .filter_map(|(k, val)| val.as_f64().map(|f| (k.clone(), f)))
                    .collect();
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                pairs
            })
            .unwrap_or_default();

        let keep = match latest.get(&task_id) {
            None => true,
            Some((existing_ts, _)) => &timestamp > existing_ts,
        };
        if keep {
            latest.insert(
                task_id,
                (
                    timestamp,
                    EvalSummary {
                        score,
                        dimensions: dims,
                    },
                ),
            );
        }
    }
    latest
        .into_iter()
        .map(|(task_id, (_, summary))| (task_id, summary))
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Reachability (used for highlight on selection)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct EdgeReach {
    /// task_id → ancestor set (visible upstream tasks reachable via --after).
    upstream: HashMap<String, BTreeSet<String>>,
    /// task_id → descendant set (visible downstream tasks reachable via --before).
    downstream: HashMap<String, BTreeSet<String>>,
}

fn compute_edge_reachability(graph: &WorkGraph, included: &HashSet<&str>) -> EdgeReach {
    // Build forward + reverse adjacency limited to the included set.
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in graph.tasks() {
        if !included.contains(task.id.as_str()) {
            continue;
        }
        for blocker in &task.after {
            if included.contains(blocker.as_str()) {
                forward.entry(blocker).or_default().push(task.id.as_str());
                reverse.entry(task.id.as_str()).or_default().push(blocker);
            }
        }
    }

    let mut up: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut down: HashMap<String, BTreeSet<String>> = HashMap::new();

    for &id in included {
        // Upstream BFS via reverse adjacency.
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: Vec<&str> = reverse.get(id).cloned().unwrap_or_default();
        while let Some(n) = queue.pop() {
            if seen.insert(n.to_string()) {
                if let Some(parents) = reverse.get(n) {
                    for p in parents {
                        queue.push(p);
                    }
                }
            }
        }
        up.insert(id.to_string(), seen);

        // Downstream BFS via forward adjacency.
        let mut seen2: BTreeSet<String> = BTreeSet::new();
        let mut queue2: Vec<&str> = forward.get(id).cloned().unwrap_or_default();
        while let Some(n) = queue2.pop() {
            if seen2.insert(n.to_string()) {
                if let Some(children) = forward.get(n) {
                    for c in children {
                        queue2.push(c);
                    }
                }
            }
        }
        down.insert(id.to_string(), seen2);
    }

    EdgeReach {
        upstream: up,
        downstream: down,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Inline JSON builders
// ────────────────────────────────────────────────────────────────────────────

fn task_to_json(
    task: &Task,
    graph: &WorkGraph,
    eval: Option<&EvalSummary>,
    included_ids: &HashSet<&str>,
) -> serde_json::Value {
    let log_entries: Vec<serde_json::Value> = task
        .log
        .iter()
        .rev()
        .take(40)
        .rev()
        .map(|e| {
            serde_json::json!({
                "timestamp": e.timestamp,
                "message": e.message,
            })
        })
        .collect();

    let after_visible: Vec<&str> = task
        .after
        .iter()
        .map(|s| s.as_str())
        .filter(|id| included_ids.contains(id))
        .collect();
    let before_visible: Vec<&str> = graph
        .tasks()
        .filter(|t| t.after.iter().any(|a| a == &task.id))
        .filter(|t| included_ids.contains(t.id.as_str()))
        .map(|t| t.id.as_str())
        .collect();

    let mut obj = serde_json::json!({
        "id": task.id,
        "title": task.title,
        "status": task.status.to_string(),
        "after": after_visible,
        "before": before_visible,
        "tags": task.tags,
        "log": log_entries,
        "loop_iteration": task.loop_iteration,
        "detail_href": task_filename(&task.id),
    });

    if let Some(m) = &task.model {
        obj["model"] = serde_json::Value::String(m.clone());
    }
    if let Some(a) = &task.agent {
        obj["agent"] = serde_json::Value::String(a.clone());
    }
    if let Some(exec) = &task.exec {
        obj["exec"] = serde_json::Value::String(exec.clone());
    }
    if let Some(c) = &task.created_at {
        obj["created_at"] = serde_json::Value::String(c.clone());
    }
    if let Some(s) = &task.started_at {
        obj["started_at"] = serde_json::Value::String(s.clone());
    }
    if let Some(c) = &task.completed_at {
        obj["completed_at"] = serde_json::Value::String(c.clone());
    }
    if let Some(reason) = &task.failure_reason {
        obj["failure_reason"] = serde_json::Value::String(reason.clone());
    }
    if let Some(d) = &task.description {
        let truncated = if d.chars().count() > 8000 {
            let mut s = d.chars().take(8000).collect::<String>();
            s.push('…');
            s
        } else {
            d.clone()
        };
        obj["description"] = serde_json::Value::String(truncated);
    }
    if let Some(ev) = eval {
        obj["eval_score"] = serde_json::json!(ev.score);
        let dims: serde_json::Value = ev
            .dimensions
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        obj["eval_dims"] = dims;
    }
    obj
}

fn build_tasks_json(
    graph: &WorkGraph,
    included: &[&Task],
    evals: &HashMap<String, EvalSummary>,
    included_ids: &HashSet<&str>,
) -> String {
    let map: serde_json::Map<String, serde_json::Value> = included
        .iter()
        .map(|t| {
            let eval = evals.get(&t.id);
            (t.id.clone(), task_to_json(t, graph, eval, included_ids))
        })
        .collect();
    let json_str = serde_json::to_string(&serde_json::Value::Object(map))
        .unwrap_or_else(|_| "{}".to_string());
    json_str.replace("</script>", "<\\/script>")
}

fn build_edges_json(reach: &EdgeReach) -> String {
    let mut map = serde_json::Map::new();
    let mut keys: BTreeSet<&String> = BTreeSet::new();
    keys.extend(reach.upstream.keys());
    keys.extend(reach.downstream.keys());
    for k in keys {
        let up: Vec<&String> = reach
            .upstream
            .get(k)
            .map(|s| s.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let down: Vec<&String> = reach
            .downstream
            .get(k)
            .map(|s| s.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        map.insert(
            k.clone(),
            serde_json::json!({
                "up": up,
                "down": down,
            }),
        );
    }
    serde_json::to_string(&serde_json::Value::Object(map))
        .unwrap_or_else(|_| "{}".to_string())
        .replace("</script>", "<\\/script>")
}

fn build_cycles_json(viz: &VizJson) -> String {
    serde_json::to_string(&viz.cycle_members)
        .unwrap_or_else(|_| "{}".to_string())
        .replace("</script>", "<\\/script>")
}

// ────────────────────────────────────────────────────────────────────────────
// Page render
// ────────────────────────────────────────────────────────────────────────────

fn render_legend() -> String {
    let entries = [
        Status::Open,
        Status::InProgress,
        Status::Done,
        Status::Failed,
        Status::Blocked,
        Status::Waiting,
        Status::PendingValidation,
        Status::PendingEval,
        Status::Abandoned,
        Status::Incomplete,
    ];
    let mut s = String::new();
    s.push_str("<ul class=\"legend\">\n");
    for st in entries {
        s.push_str(&format!(
            "  <li><span class=\"swatch\" style=\"background:{color}\"></span>{name}</li>\n",
            color = status_color(st),
            name = st,
        ));
    }
    s.push_str("</ul>\n");
    s
}

fn render_footer(
    total_in_graph: usize,
    total_shown: usize,
    show_all: bool,
    since_label: Option<&str>,
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let filter_note = if show_all {
        format!(
            "Showing {} of {} tasks{}.",
            total_shown,
            total_in_graph,
            since_label
                .map(|s| format!(", last {}", s))
                .unwrap_or_default(),
        )
    } else {
        let hidden = total_in_graph.saturating_sub(total_shown);
        if let Some(label) = since_label {
            format!(
                "Showing {} of {} tasks: <strong>--public-only</strong>, last {}. {} non-public tasks hidden.",
                total_shown, total_in_graph, label, hidden,
            )
        } else {
            format!(
                "Visibility filter: <strong>--public-only</strong>. Showing {} of {} tasks; \
                 {} non-public tasks hidden.",
                total_shown, total_in_graph, hidden,
            )
        }
    };
    format!(
        "<p>{filter}</p>\n\
         <p class=\"meta\">Rendered by <code>wg html</code> at {now}.</p>\n",
        filter = filter_note,
        now = now,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_index(
    graph: &WorkGraph,
    included: &[&Task],
    included_ids: &HashSet<&str>,
    viz: &VizJson,
    tasks_json: &str,
    edges_json: &str,
    cycles_json: &str,
    show_all: bool,
    since_label: Option<&str>,
) -> String {
    let total_in_graph = graph.tasks().count();
    let total_shown = included.len();

    let viz_html = render_viz_html(viz, graph, included_ids);

    // Ordered task list (by status then id).
    let mut ordered: Vec<&&Task> = included.iter().collect();
    ordered.sort_by_key(|t| (t.status.to_string(), t.id.clone()));
    let mut list = String::new();
    list.push_str("<ul class=\"task-list\">\n");
    for t in &ordered {
        list.push_str(&format!(
            "  <li><a href=\"{href}\" data-task-id=\"{id_attr}\"><span class=\"badge {cls}\">{status}</span> \
             <code>{id}</code> — {title}</a></li>\n",
            href = task_filename(&t.id),
            id_attr = escape_html(&t.id),
            cls = status_class(t.status),
            status = t.status,
            id = escape_html(&t.id),
            title = escape_html(&t.title),
        ));
    }
    list.push_str("</ul>\n");

    let legend = render_legend();
    let footer = render_footer(total_in_graph, total_shown, show_all, since_label);

    let title_suffix = if show_all { "all tasks" } else { "public mirror" };

    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\" />\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
         <title>Workgraph — {title_suffix}</title>\n\
         <link rel=\"stylesheet\" href=\"style.css\" />\n\
         <script>\n\
         /* Theme bootstrap — runs before paint to avoid a flash. */\n\
         (function () {{\n\
             try {{\n\
                 var saved = localStorage.getItem('wg-html-theme');\n\
                 if (saved === 'dark' || saved === 'light') {{\n\
                     document.documentElement.setAttribute('data-theme', saved);\n\
                 }}\n\
             }} catch (_) {{}}\n\
         }})();\n\
         </script>\n\
         </head>\n\
         <body>\n\
         <header class=\"page-header\">\n\
         <div>\n\
         <h1>Workgraph</h1>\n\
         <p class=\"subtitle\">{n} tasks shown · click a task id to inspect</p>\n\
         </div>\n\
         <div class=\"header-controls\">\n\
         <button id=\"theme-toggle\" class=\"theme-toggle\" type=\"button\">Light theme</button>\n\
         </div>\n\
         </header>\n\
         <div class=\"page-layout\">\n\
         <main class=\"main-content\">\n\
         <section class=\"dag-section\">\n\
         <h2>Dependency graph <span class=\"viz-hint\">(click a task to inspect — magenta = upstream deps · cyan = downstream consumers)</span></h2>\n\
         <div class=\"viz-wrap\">{viz}</div>\n\
         </section>\n\
         <section class=\"legend-section\">\n\
         <h2>Legend</h2>\n\
         {legend}\n\
         </section>\n\
         <section class=\"list-section\">\n\
         <h2>Tasks ({total_shown})</h2>\n\
         {list}\n\
         </section>\n\
         </main>\n\
         <aside id=\"side-panel\" class=\"side-panel\" aria-label=\"Task detail\">\n\
         <button id=\"panel-close\" class=\"panel-close\" type=\"button\" aria-label=\"Close detail panel\">×</button>\n\
         <div id=\"panel-content\"></div>\n\
         </aside>\n\
         </div>\n\
         <footer>{footer}</footer>\n\
         <script id=\"wg-tasks-json\">window.WG_TASKS = {tasks_json};</script>\n\
         <script id=\"wg-edges-json\">window.WG_EDGES = {edges_json};</script>\n\
         <script id=\"wg-cycles-json\">window.WG_CYCLES = {cycles_json};</script>\n\
         <script src=\"panel.js\"></script>\n\
         </body>\n\
         </html>\n",
        title_suffix = title_suffix,
        n = total_shown,
        viz = viz_html,
        legend = legend,
        list = list,
        total_shown = total_shown,
        footer = footer,
        tasks_json = tasks_json,
        edges_json = edges_json,
        cycles_json = cycles_json,
    )
}

// ────────────────────────────────────────────────────────────────────────────
// Per-task page (deep-link target)
// ────────────────────────────────────────────────────────────────────────────

fn render_task_page(
    task: &Task,
    graph: &WorkGraph,
    included_ids: &HashSet<&str>,
    eval: Option<&EvalSummary>,
) -> String {
    let title = escape_html(&task.title);
    let id = escape_html(&task.id);
    let status_str = task.status.to_string();
    let status_cls = status_class(task.status);

    let mut deps_html = String::from("<ul class=\"deps\">");
    let mut hidden_dep_count = 0usize;
    if task.after.is_empty() {
        deps_html.push_str("<li class=\"none\">(none)</li>");
    } else {
        for dep in &task.after {
            if included_ids.contains(dep.as_str()) {
                let dep_status = graph
                    .get_task(dep)
                    .map(|t| t.status)
                    .unwrap_or(Status::Open);
                deps_html.push_str(&format!(
                    "<li><a href=\"{href}\"><span class=\"badge {cls}\">{st}</span> <code>{id}</code></a></li>",
                    href = format!("./{}.html", url_encode_id(dep)),
                    cls = status_class(dep_status),
                    st = dep_status,
                    id = escape_html(dep),
                ));
            } else {
                hidden_dep_count += 1;
            }
        }
        if hidden_dep_count > 0 {
            deps_html.push_str(&format!(
                "<li class=\"hidden-dep\"><span class=\"note\">{} non-public dependenc{} hidden</span></li>",
                hidden_dep_count,
                if hidden_dep_count == 1 { "y" } else { "ies" },
            ));
        }
    }
    deps_html.push_str("</ul>");

    let dependents: Vec<&str> = {
        let mut v: Vec<&str> = graph
            .tasks()
            .filter(|t| t.after.iter().any(|a| a == &task.id))
            .map(|t| t.id.as_str())
            .collect();
        v.sort();
        v
    };

    let mut dependents_html = String::from("<ul class=\"deps\">");
    let mut hidden_dependents_count = 0usize;
    if dependents.is_empty() {
        dependents_html.push_str("<li class=\"none\">(none)</li>");
    } else {
        for d in dependents {
            if included_ids.contains(d) {
                let dep_status = graph.get_task(d).map(|t| t.status).unwrap_or(Status::Open);
                dependents_html.push_str(&format!(
                    "<li><a href=\"{href}\"><span class=\"badge {cls}\">{st}</span> <code>{id}</code></a></li>",
                    href = format!("./{}.html", url_encode_id(d)),
                    cls = status_class(dep_status),
                    st = dep_status,
                    id = escape_html(d),
                ));
            } else {
                hidden_dependents_count += 1;
            }
        }
        if hidden_dependents_count > 0 {
            dependents_html.push_str(&format!(
                "<li class=\"hidden-dep\"><span class=\"note\">{} non-public dependent{} hidden</span></li>",
                hidden_dependents_count,
                if hidden_dependents_count == 1 { "" } else { "s" },
            ));
        }
    }
    dependents_html.push_str("</ul>");

    let description_html = match &task.description {
        Some(d) if !d.trim().is_empty() => {
            format!("<pre class=\"description\">{}</pre>", escape_html(d))
        }
        _ => "<p class=\"none\">(no description)</p>".to_string(),
    };

    let mut meta_rows: Vec<(String, String)> = Vec::new();
    meta_rows.push((
        "Status".into(),
        format!("<span class=\"badge {}\">{}</span>", status_cls, status_str),
    ));
    if let Some(a) = &task.assigned {
        meta_rows.push(("Assigned".into(), format!("<code>{}</code>", escape_html(a))));
    }
    if let Some(agent) = &task.agent {
        meta_rows.push((
            "Agent identity".into(),
            format!("<code>{}</code>", escape_html(agent)),
        ));
    }
    if let Some(model) = &task.model {
        meta_rows.push((
            "Model".into(),
            format!("<code>{}</code>", escape_html(model)),
        ));
    }
    if let Some(c) = &task.created_at {
        meta_rows.push(("Created".into(), escape_html(c)));
    }
    if let Some(s) = &task.started_at {
        meta_rows.push(("Started".into(), escape_html(s)));
    }
    if let Some(c) = &task.completed_at {
        meta_rows.push(("Completed".into(), escape_html(c)));
    }
    if !task.tags.is_empty() {
        let tags = task
            .tags
            .iter()
            .map(|t| format!("<code>{}</code>", escape_html(t)))
            .collect::<Vec<_>>()
            .join(", ");
        meta_rows.push(("Tags".into(), tags));
    }
    if let Some(usage) = &task.token_usage {
        meta_rows.push((
            "Tokens".into(),
            format!("{} in / {} out", usage.total_input(), usage.output_tokens),
        ));
    }
    if let Some(reason) = &task.failure_reason {
        meta_rows.push(("Failure reason".into(), escape_html(reason)));
    }
    if let Some(ev) = eval {
        meta_rows.push(("Eval score".into(), format!("{:.2}", ev.score)));
        for (dim, val) in &ev.dimensions {
            meta_rows.push((
                format!("  └ {}", dim.replace('_', " ")),
                format!("{:.2}", val),
            ));
        }
    }

    let mut meta_html = String::from("<table class=\"meta-table\"><tbody>");
    for (k, v) in meta_rows {
        meta_html.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", k, v));
    }
    meta_html.push_str("</tbody></table>");

    // Log entries (last 50 to give more context than v1's 30)
    let log_html = if task.log.is_empty() {
        "<p class=\"none\">(no log entries)</p>".to_string()
    } else {
        let mut s = String::from("<ul class=\"task-log\">");
        for entry in task.log.iter().rev().take(50).rev() {
            let ts = escape_html(&entry.timestamp);
            let msg = escape_html(&entry.message);
            s.push_str(&format!(
                "<li><span class=\"log-ts\">{ts}</span> {msg}</li>"
            ));
        }
        s.push_str("</ul>");
        s
    };

    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\" />\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
         <title>{id} — Workgraph</title>\n\
         <link rel=\"stylesheet\" href=\"../style.css\" />\n\
         <script>\n\
         (function () {{\n\
             try {{\n\
                 var saved = localStorage.getItem('wg-html-theme');\n\
                 if (saved === 'dark' || saved === 'light') {{\n\
                     document.documentElement.setAttribute('data-theme', saved);\n\
                 }}\n\
             }} catch (_) {{}}\n\
         }})();\n\
         </script>\n\
         </head>\n\
         <body class=\"task-page\">\n\
         <header>\n\
         <p class=\"breadcrumb\"><a href=\"../index.html\">← graph</a></p>\n\
         <h1><code>{id}</code></h1>\n\
         <p class=\"task-title\">{title}</p>\n\
         </header>\n\
         <main>\n\
         <section><h2>Metadata</h2>{meta}</section>\n\
         <section><h2>Description</h2>{desc}</section>\n\
         <section><h2>Depends on</h2>{deps}</section>\n\
         <section><h2>Required by</h2>{revdeps}</section>\n\
         <section><h2>Log</h2>{log}</section>\n\
         </main>\n\
         <footer><p class=\"meta\">Visibility = <code>{vis}</code></p></footer>\n\
         </body>\n\
         </html>\n",
        id = id,
        title = title,
        meta = meta_html,
        desc = description_html,
        deps = deps_html,
        revdeps = dependents_html,
        log = log_html,
        vis = escape_html(&task.visibility),
    )
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi() {
        let s = "\x1b[31mhello\x1b[0m world";
        assert_eq!(strip_ansi(s), "hello world");
    }

    #[test]
    fn strip_ansi_preserves_unicode() {
        let s = "\x1b[36m├→\x1b[0m foo";
        assert_eq!(strip_ansi(s), "├→ foo");
    }

    #[test]
    fn extend_through_status_decorator_works() {
        let line: Vec<char> = "task-x  (in-progress · 5m) more".chars().collect();
        let after_id = "task-x".chars().count();
        let end = extend_through_status_decorator(&line, after_id);
        let consumed: String = line[after_id..end].iter().collect();
        assert!(consumed.starts_with("  ("));
        assert!(consumed.ends_with(')'));
    }

    #[test]
    fn extend_through_status_decorator_no_match_returns_original() {
        let line: Vec<char> = "task-x more text".chars().collect();
        let after_id = "task-x".chars().count();
        let end = extend_through_status_decorator(&line, after_id);
        assert_eq!(end, after_id);
    }

    #[test]
    fn parse_since_basic() {
        assert_eq!(parse_since("1h").unwrap(), Duration::hours(1));
        assert_eq!(parse_since("24h").unwrap(), Duration::hours(24));
        assert_eq!(parse_since("7d").unwrap(), Duration::days(7));
        assert_eq!(parse_since("2w").unwrap(), Duration::weeks(2));
        assert!(parse_since("0h").is_err());
        assert!(parse_since("abc").is_err());
    }
}

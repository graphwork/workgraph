//! `wg html`: render the workgraph as a static HTML site (public tasks only).
//!
//! Emits a directory of plain HTML/CSS/SVG files: an index page with a
//! layered SVG DAG view and one detail page per public task. The output is
//! rsync-friendly (no runtime requirements, no JavaScript framework).
//!
//! Visibility filter: only tasks with `visibility = "public"` are included.
//! Internal/peer tasks are excluded — including from edges (a public task
//! that depends on an internal task does not show that edge in the rendered
//! site, but the dependency annotation is preserved on its detail page).
//!
//! Layout: a simple longest-path layered layout (left-to-right). No external
//! tools (no graphviz dot dependency); the layout is computed in Rust and
//! rendered as inline SVG.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::graph::{Status, Task, WorkGraph};
use crate::parser::load_graph;

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

/// Return true if the task has any timestamp that falls within the window.
/// Uses created_at, started_at, completed_at, and last_iteration_completed_at.
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

/// Status palette mirrored from `src/tui/viz_viewer/state.rs`.
/// Returned as CSS-formatted `rgb(r,g,b)` strings for direct embedding.
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

/// CSS class name for a status, used for legend / per-task badge styling.
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

/// Escape a string for safe inclusion in HTML/SVG text or attribute contexts.
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

/// Layered placement of one node in the DAG.
struct LaidOut {
    layer: usize,
    pos_in_layer: usize,
    layer_size: usize,
}

/// Compute longest-path layering: each node's layer is `1 + max(layer of after-deps)`,
/// considering only after-deps that are themselves in the included set.
/// Within each layer, nodes are sorted by id for deterministic output.
fn compute_layers(included_ids: &HashSet<&str>, tasks_by_id: &HashMap<&str, &Task>) -> HashMap<String, LaidOut> {
    let mut layer_of: HashMap<String, usize> = HashMap::new();

    // Iterative longest-path layering. Repeat until fixed point. Worst-case
    // O(V*E) but the graph is small enough that this is fine.
    let mut changed = true;
    while changed {
        changed = false;
        for &id in included_ids {
            let task = match tasks_by_id.get(id) {
                Some(t) => t,
                None => continue,
            };
            let mut max_dep_layer: Option<usize> = None;
            for after in &task.after {
                if included_ids.contains(after.as_str()) {
                    if let Some(&l) = layer_of.get(after) {
                        max_dep_layer = Some(max_dep_layer.map_or(l, |m| m.max(l)));
                    } else {
                        // dep hasn't been assigned yet — defer
                        max_dep_layer = Some(usize::MAX);
                        break;
                    }
                }
            }
            let new_layer = match max_dep_layer {
                None => 0,
                Some(usize::MAX) => continue, // wait for next pass
                Some(l) => l + 1,
            };
            match layer_of.get(id) {
                Some(&existing) if existing >= new_layer => {}
                _ => {
                    layer_of.insert(id.to_string(), new_layer);
                    changed = true;
                }
            }
        }
    }

    // Any remaining nodes (in a cycle) — assign to layer 0 as fallback.
    for &id in included_ids {
        layer_of.entry(id.to_string()).or_insert(0);
    }

    // Group by layer, sort within layer by id.
    let mut by_layer: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (id, layer) in &layer_of {
        by_layer.entry(*layer).or_default().push(id.clone());
    }
    for v in by_layer.values_mut() {
        v.sort();
    }

    let mut out: HashMap<String, LaidOut> = HashMap::new();
    for (layer, ids) in by_layer {
        let layer_size = ids.len();
        for (pos, id) in ids.into_iter().enumerate() {
            out.insert(
                id,
                LaidOut {
                    layer,
                    pos_in_layer: pos,
                    layer_size,
                },
            );
        }
    }
    out
}

const NODE_W: usize = 220;
const NODE_H: usize = 60;
const LAYER_GAP_X: usize = 80;
const NODE_GAP_Y: usize = 30;
const MARGIN: usize = 40;

/// Compute (x, y) of a node's top-left corner.
fn node_xy(laid: &LaidOut, max_layer_size: usize) -> (usize, usize) {
    let x = MARGIN + laid.layer * (NODE_W + LAYER_GAP_X);
    // Center this layer's nodes vertically within the canvas.
    let layer_height = laid.layer_size * NODE_H + laid.layer_size.saturating_sub(1) * NODE_GAP_Y;
    let canvas_height = max_layer_size * NODE_H + max_layer_size.saturating_sub(1) * NODE_GAP_Y;
    let layer_top = MARGIN + (canvas_height.saturating_sub(layer_height)) / 2;
    let y = layer_top + laid.pos_in_layer * (NODE_H + NODE_GAP_Y);
    (x, y)
}

/// Render the DAG as an inline SVG fragment.
fn render_svg(
    laid: &HashMap<String, LaidOut>,
    tasks_by_id: &HashMap<&str, &Task>,
    included_ids: &HashSet<&str>,
) -> String {
    let max_layer_size = laid.values().map(|l| l.layer_size).max().unwrap_or(1);
    let max_layer = laid.values().map(|l| l.layer).max().unwrap_or(0);
    let width = MARGIN * 2 + (max_layer + 1) * NODE_W + max_layer * LAYER_GAP_X;
    let height = MARGIN * 2 + max_layer_size * NODE_H + max_layer_size.saturating_sub(1) * NODE_GAP_Y;

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg class=\"dag\" xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\">\n",
        width, height, width, height
    ));

    // Arrowhead marker definition.
    svg.push_str(
        "<defs>\n  <marker id=\"arrow\" viewBox=\"0 0 10 10\" refX=\"10\" refY=\"5\" \
         markerWidth=\"6\" markerHeight=\"6\" orient=\"auto-start-reverse\">\n    \
         <path d=\"M0,0 L10,5 L0,10 z\" fill=\"#888\" />\n  </marker>\n</defs>\n",
    );

    // Render edges first so nodes draw over them.
    for (id, _laid_to) in laid {
        let task = match tasks_by_id.get(id.as_str()) {
            Some(t) => t,
            None => continue,
        };
        for after in &task.after {
            if !included_ids.contains(after.as_str()) {
                continue;
            }
            let from = match laid.get(after) {
                Some(l) => l,
                None => continue,
            };
            let to = match laid.get(id) {
                Some(l) => l,
                None => continue,
            };
            let (fx, fy) = node_xy(from, max_layer_size);
            let (tx, ty) = node_xy(to, max_layer_size);
            // Edge from right-mid of "from" to left-mid of "to".
            let x1 = fx + NODE_W;
            let y1 = fy + NODE_H / 2;
            let x2 = tx;
            let y2 = ty + NODE_H / 2;
            // Bezier curve with horizontal handles.
            let cx = (x1 + x2) / 2;
            svg.push_str(&format!(
                "<path d=\"M{x1},{y1} C{cx},{y1} {cx},{y2} {x2},{y2}\" \
                 fill=\"none\" stroke=\"#888\" stroke-width=\"1.5\" marker-end=\"url(#arrow)\" />\n",
                x1 = x1, y1 = y1, cx = cx, x2 = x2, y2 = y2,
            ));
        }
    }

    // Render nodes.
    for (id, laid_node) in laid {
        let task = match tasks_by_id.get(id.as_str()) {
            Some(t) => t,
            None => continue,
        };
        let (x, y) = node_xy(laid_node, max_layer_size);
        let fill = status_color(task.status);
        let label_id = escape_html(&task.id);
        let label_title = escape_html(&task.title);
        let truncated_title = if task.title.chars().count() > 28 {
            let mut s: String = task.title.chars().take(28).collect();
            s.push('…');
            escape_html(&s)
        } else {
            label_title.clone()
        };
        let href = format!("tasks/{}.html", url_encode_id(&task.id));
        svg.push_str(&format!(
            "<a xlink:href=\"{href}\" href=\"{href}\">\n  \
             <title>{label_id} — {label_title} ({status})</title>\n  \
             <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"6\" ry=\"6\" \
             fill=\"{fill}\" stroke=\"#222\" stroke-width=\"1\" />\n  \
             <text x=\"{tx}\" y=\"{ty1}\" text-anchor=\"middle\" font-size=\"12\" \
             font-family=\"monospace\" font-weight=\"bold\" fill=\"#000\">{label_id}</text>\n  \
             <text x=\"{tx}\" y=\"{ty2}\" text-anchor=\"middle\" font-size=\"11\" \
             font-family=\"sans-serif\" fill=\"#000\">{truncated}</text>\n\
             </a>\n",
            href = href,
            label_id = label_id,
            label_title = label_title,
            status = task.status,
            x = x,
            y = y,
            w = NODE_W,
            h = NODE_H,
            fill = fill,
            tx = x + NODE_W / 2,
            ty1 = y + 24,
            ty2 = y + 44,
            truncated = truncated_title,
        ));
    }

    svg.push_str("</svg>\n");
    svg
}

/// Percent-encode characters in a task id that aren't safe for use as a
/// path segment in a URL. Task ids are generally kebab-case + dots, so the
/// only character we typically need to handle is leading-dot system ids
/// (no encoding actually needed) — but we still escape conservatively.
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

/// Filename for a task's detail page (relative to output dir).
fn task_filename(id: &str) -> String {
    format!("tasks/{}.html", url_encode_id(id))
}

const STYLE_CSS: &str = include_str!("html_assets/style.css");

/// Generate the index page HTML.
fn render_index(
    graph: &WorkGraph,
    included: &[&Task],
    tasks_by_id: &HashMap<&str, &Task>,
    included_ids: &HashSet<&str>,
    laid: &HashMap<String, LaidOut>,
    show_all: bool,
    since_label: Option<&str>,
) -> String {
    // Status counts.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for t in included {
        *counts.entry(t.status.to_string()).or_insert(0) += 1;
    }
    let total_in_graph = graph.tasks().count();
    let total_public = included.len();

    let svg = if included.is_empty() {
        "<p class=\"empty\">No tasks to display.</p>".to_string()
    } else {
        render_svg(laid, tasks_by_id, included_ids)
    };

    // Ordered task list (by layer then id).
    let mut ordered: Vec<&&Task> = included.iter().collect();
    ordered.sort_by_key(|t| {
        (
            laid.get(&t.id).map(|l| l.layer).unwrap_or(0),
            laid.get(&t.id).map(|l| l.pos_in_layer).unwrap_or(0),
            t.id.clone(),
        )
    });

    let mut list = String::new();
    list.push_str("<ul class=\"task-list\">\n");
    for t in ordered {
        list.push_str(&format!(
            "  <li><a href=\"{href}\"><span class=\"badge {cls}\">{status}</span> \
             <code>{id}</code> — {title}</a></li>\n",
            href = task_filename(&t.id),
            cls = status_class(t.status),
            status = t.status,
            id = escape_html(&t.id),
            title = escape_html(&t.title),
        ));
    }
    list.push_str("</ul>\n");

    let legend = render_legend();
    let footer = render_footer(total_in_graph, total_public, show_all, since_label);

    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\" />\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
         <title>Workgraph — Public Mirror</title>\n\
         <link rel=\"stylesheet\" href=\"style.css\" />\n\
         </head>\n\
         <body>\n\
         <header><h1>Workgraph</h1>\n\
         <p class=\"subtitle\">Public mirror of the project's task graph.</p>\n\
         </header>\n\
         <main>\n\
         <section class=\"dag-section\">\n\
         <h2>Dependency graph</h2>\n\
         <div class=\"dag-wrap\">{svg}</div>\n\
         </section>\n\
         <section class=\"legend-section\">\n\
         <h2>Legend</h2>\n\
         {legend}\n\
         </section>\n\
         <section class=\"list-section\">\n\
         <h2>Tasks ({total_public})</h2>\n\
         {list}\n\
         </section>\n\
         </main>\n\
         <footer>{footer}</footer>\n\
         </body>\n\
         </html>\n",
        svg = svg,
        legend = legend,
        list = list,
        total_public = total_public,
        footer = footer,
    )
}

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
    total_public: usize,
    show_all: bool,
    since_label: Option<&str>,
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let visibility_str = if show_all { "all tasks" } else { "public-only" };
    let filter_note = if show_all {
        format!(
            "Visibility filter: <strong>OFF</strong> (--all). Showing {} of {} tasks{}.",
            total_public,
            total_in_graph,
            since_label
                .map(|s| format!(", last {}", s))
                .unwrap_or_default(),
        )
    } else {
        let hidden = total_in_graph.saturating_sub(total_public);
        if let Some(label) = since_label {
            format!(
                "Showing {} of {} tasks: {}, last {}. {} internal/peer tasks are hidden.",
                total_public, total_in_graph, visibility_str, label, hidden,
            )
        } else {
            format!(
                "Visibility filter: <strong>public-only</strong>. Showing {} of {} tasks; \
                 {} internal/peer tasks are hidden.",
                total_public, total_in_graph, hidden,
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

fn render_task_page(
    task: &Task,
    graph: &WorkGraph,
    included_ids: &HashSet<&str>,
) -> String {
    let title = escape_html(&task.title);
    let id = escape_html(&task.id);
    let status_str = task.status.to_string();
    let status_cls = status_class(task.status);

    // Dependencies (after) — link to public deps; aggregate hidden ones into a count
    // so internal task ids never leak into the rendered output.
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

    // Reverse deps: who depends on this task?
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

    // Metadata table.
    let mut meta_rows: Vec<(String, String)> = Vec::new();
    meta_rows.push(("Status".into(), format!("<span class=\"badge {}\">{}</span>", status_cls, status_str)));
    if let Some(a) = &task.assigned {
        meta_rows.push(("Assigned".into(), format!("<code>{}</code>", escape_html(a))));
    }
    if let Some(agent) = &task.agent {
        meta_rows.push(("Agent identity".into(), format!("<code>{}</code>", escape_html(agent))));
    }
    if let Some(model) = &task.model {
        meta_rows.push(("Model".into(), format!("<code>{}</code>", escape_html(model))));
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

    let mut meta_html = String::from("<table class=\"meta-table\"><tbody>");
    for (k, v) in meta_rows {
        meta_html.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", k, v));
    }
    meta_html.push_str("</tbody></table>");

    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\" />\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
         <title>{id} — Workgraph</title>\n\
         <link rel=\"stylesheet\" href=\"../style.css\" />\n\
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
         </main>\n\
         <footer><p class=\"meta\">Public mirror — visibility = <code>{vis}</code></p></footer>\n\
         </body>\n\
         </html>\n",
        id = id,
        title = title,
        meta = meta_html,
        desc = description_html,
        deps = deps_html,
        revdeps = dependents_html,
        vis = escape_html(&task.visibility),
    )
}

/// Render the entire HTML site to a directory. Returns the count of pages emitted.
pub fn render_site(
    graph: &WorkGraph,
    out_dir: &Path,
    show_all: bool,
    since: Option<&str>,
) -> Result<RenderSummary> {
    // Parse --since into a cutoff timestamp.
    let since_cutoff: Option<DateTime<Utc>> = since
        .map(|s| parse_since(s).map(|d| Utc::now() - d))
        .transpose()?;

    fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create output dir: {}", out_dir.display()))?;
    let tasks_dir = out_dir.join("tasks");
    fs::create_dir_all(&tasks_dir)
        .with_context(|| format!("failed to create tasks dir: {}", tasks_dir.display()))?;

    let all_tasks: Vec<&Task> = graph.tasks().collect();

    // Visibility filter.
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
    let tasks_by_id: HashMap<&str, &Task> = included.iter().map(|t| (t.id.as_str(), *t)).collect();

    let laid = compute_layers(&included_ids, &tasks_by_id);

    // Write style.css
    let css_path = out_dir.join("style.css");
    fs::write(&css_path, STYLE_CSS).context("failed to write style.css")?;

    // Write index.html
    let index_html = render_index(
        graph,
        &included,
        &tasks_by_id,
        &included_ids,
        &laid,
        show_all,
        since,
    );
    let index_path = out_dir.join("index.html");
    fs::write(&index_path, &index_html).context("failed to write index.html")?;

    // Write per-task pages
    let mut pages_written = 0usize;
    for task in &included {
        let html = render_task_page(task, graph, &included_ids);
        let path = tasks_dir.join(format!("{}.html", url_encode_id(&task.id)));
        fs::write(&path, html)
            .with_context(|| format!("failed to write {}", path.display()))?;
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

#[derive(Debug, Clone)]
pub struct RenderSummary {
    pub out_dir: std::path::PathBuf,
    pub total_in_graph: usize,
    pub public_count: usize,
    pub pages_written: usize,
    pub show_all: bool,
    pub since: Option<String>,
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

    let summary = render_site(&graph, out, all, since)?;

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
            let since_str = summary
                .since
                .as_deref()
                .map(|s| format!(", last {}", s))
                .unwrap_or_default();
            format!("all tasks (visibility filter OFF{})", since_str)
        } else {
            let since_str = summary
                .since
                .as_deref()
                .map(|s| format!(", last {}", s))
                .unwrap_or_default();
            format!(
                "{} public of {} total{}",
                summary.public_count, summary.total_in_graph, since_str,
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_since tests ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_since_hours() {
        let d = parse_since("1h").unwrap();
        assert_eq!(d.num_seconds(), 3600);
        let d24 = parse_since("24h").unwrap();
        assert_eq!(d24.num_seconds(), 24 * 3600);
    }

    #[test]
    fn test_parse_since_days() {
        let d = parse_since("7d").unwrap();
        assert_eq!(d.num_seconds(), 7 * 86400);
        let d30 = parse_since("30d").unwrap();
        assert_eq!(d30.num_seconds(), 30 * 86400);
    }

    #[test]
    fn test_parse_since_weeks() {
        let d = parse_since("2w").unwrap();
        assert_eq!(d.num_seconds(), 2 * 7 * 86400);
    }

    #[test]
    fn test_parse_since_minutes() {
        let d = parse_since("30m").unwrap();
        assert_eq!(d.num_seconds(), 30 * 60);
    }

    #[test]
    fn test_parse_since_rejects_garbage() {
        assert!(parse_since("").is_err(), "empty string should fail");
        assert!(parse_since("abc").is_err(), "no unit and non-numeric should fail");
        assert!(parse_since("7x").is_err(), "unknown unit 'x' should fail");
        assert!(parse_since("0d").is_err(), "zero is not positive");
        assert!(parse_since("-1d").is_err(), "negative should fail");
    }

    #[test]
    fn test_parse_since_rejects_no_unit() {
        // No unit → error (unlike archive.rs which defaults to days)
        assert!(parse_since("7").is_err());
    }

    // ── task_in_window tests ───────────────────────────────────────────────────

    #[test]
    fn test_task_in_window_created_at_matches() {
        use crate::graph::Task;
        let now = Utc::now();
        let recent = (now - Duration::hours(1)).to_rfc3339();
        let mut task = Task::default();
        task.created_at = Some(recent);
        let cutoff = now - Duration::hours(2);
        assert!(task_in_window(&task, cutoff));
    }

    #[test]
    fn test_task_in_window_old_task_excluded() {
        use crate::graph::Task;
        let now = Utc::now();
        let old = (now - Duration::days(10)).to_rfc3339();
        let mut task = Task::default();
        task.created_at = Some(old);
        let cutoff = now - Duration::hours(24);
        assert!(!task_in_window(&task, cutoff));
    }

    #[test]
    fn test_task_in_window_completed_at_matches() {
        use crate::graph::Task;
        let now = Utc::now();
        let old_created = (now - Duration::days(30)).to_rfc3339();
        let recent_completed = (now - Duration::hours(2)).to_rfc3339();
        let mut task = Task::default();
        task.created_at = Some(old_created);
        task.completed_at = Some(recent_completed);
        let cutoff = now - Duration::hours(24);
        assert!(task_in_window(&task, cutoff));
    }

    #[test]
    fn test_task_in_window_no_timestamps_excluded() {
        use crate::graph::Task;
        let now = Utc::now();
        let task = Task::default();
        let cutoff = now - Duration::hours(24);
        assert!(!task_in_window(&task, cutoff));
    }
}

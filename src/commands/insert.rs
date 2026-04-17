//! `wg insert` — graph-surgery primitive.
//!
//! Inserts a new task at a position relative to an existing target:
//!
//! - `before <target>` — new task is a prerequisite of target
//! - `after <target>`  — new task runs after target
//! - `parallel <target>` — new task sits at the same graph slot as
//!   target, inheriting both its predecessors and successors
//!
//! All three positions support an "additive" default (original edges
//! preserved) and a "splice" / "replace-edges" variant that rewires
//! the original edges through the new node exclusively.
//!
//! This is the foundation for `wg rescue`, which is a thin wrapper
//! over `insert parallel <target> --replace-edges` with metadata
//! bookkeeping.
//!
//! Edge-rewrite semantics are applied atomically inside a single
//! `modify_graph` closure so the graph is never observed in a
//! half-rewired state.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;

use workgraph::graph::{Node, Status, Task};
use workgraph::parser::modify_graph;

/// Which position in the graph to insert the new task at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// New task becomes a prerequisite of target.
    Before,
    /// New task runs after target.
    After,
    /// New task sits at target's graph slot, inheriting both its
    /// predecessors and its successors.
    Parallel,
}

impl std::str::FromStr for Position {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "before" => Ok(Position::Before),
            "after" => Ok(Position::After),
            "parallel" => Ok(Position::Parallel),
            other => Err(format!(
                "invalid position '{}' — must be one of: before, after, parallel",
                other
            )),
        }
    }
}

/// Options that modify default additive behavior.
#[derive(Debug, Default)]
pub struct InsertOptions {
    /// For `before`/`after`: remove the direct old edge so the flow goes
    /// THROUGH the new node exclusively. For `parallel`, has no effect
    /// (use `replace_edges` instead).
    pub splice: bool,
    /// For `parallel`: remove target from successors' `after` lists so
    /// they wait on the new node ONLY, not target. Used by `wg rescue`
    /// to route around a failed node. No effect for `before`/`after`.
    pub replace_edges: bool,
}

/// Run `wg insert`. Atomic edge rewrite — target + all neighbors are
/// updated in one graph modification.
pub fn run(
    dir: &Path,
    position: Position,
    target_id: &str,
    title: &str,
    description: Option<&str>,
    new_id: Option<&str>,
    opts: InsertOptions,
) -> Result<String> {
    if title.trim().is_empty() {
        anyhow::bail!("Task title must not be empty");
    }

    let path = crate::commands::graph_path(dir);

    // If the caller didn't supply an ID, derive one from the title.
    // Keeps the slug short + readable without colliding with existing IDs.
    let derived_id = match new_id {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => derive_id_from_title(title),
    };

    // Shared slot so the closure can tell the caller what ID it actually
    // used (may differ from the derived/explicit input if there was a
    // collision and a -N suffix got appended).
    let assigned_slot = std::sync::Arc::new(std::sync::Mutex::new(derived_id.clone()));
    let assigned_for_closure = assigned_slot.clone();

    let new_node_id = derived_id.clone();
    modify_graph(&path, move |graph| {
        // Verify target exists before we touch anything
        if graph.get_task(target_id).is_none() {
            eprintln!(
                "\x1b[31merror:\x1b[0m target task '{}' not found",
                target_id
            );
            return false;
        }

        // If the derived/explicit id collides, append -2, -3, ...
        let final_id = unique_id(graph, &new_node_id);
        if let Ok(mut s) = assigned_for_closure.lock() {
            *s = final_id.clone();
        }

        // Snapshot target's current edges BEFORE any mutation.
        let (target_after, target_before): (Vec<String>, Vec<String>) = {
            let t = graph.get_task(target_id).unwrap();
            (t.after.clone(), t.before.clone())
        };

        // Build the new task with edges appropriate to the position.
        let mut new_task = Task {
            id: final_id.clone(),
            title: title.to_string(),
            description: description.map(String::from),
            status: Status::Open,
            created_at: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        };

        match position {
            Position::Before => {
                // N is a prerequisite of target.
                if opts.splice {
                    // Splice: N inherits target's old predecessors.
                    // Target now depends ONLY on N.
                    new_task.after = target_after.clone();
                    new_task.before = vec![target_id.to_string()];
                    // Rewrite predecessor.before lists: they had target,
                    // now they also have N (target still exists but will
                    // depend only on N).
                    for pred_id in &target_after {
                        if let Some(p) = graph.get_task_mut(pred_id) {
                            // Add N to p.before if not present
                            if !p.before.contains(&final_id) {
                                p.before.push(final_id.clone());
                            }
                            // Remove target from p.before
                            p.before.retain(|s| s != target_id);
                        }
                    }
                    // Target: after = [N] only
                    if let Some(t) = graph.get_task_mut(target_id) {
                        t.after = vec![final_id.clone()];
                    }
                } else {
                    // Additive: N is a new prerequisite alongside the old ones.
                    new_task.before = vec![target_id.to_string()];
                    if let Some(t) = graph.get_task_mut(target_id) {
                        if !t.after.contains(&final_id) {
                            t.after.push(final_id.clone());
                        }
                    }
                }
            }
            Position::After => {
                // N runs after target.
                if opts.splice {
                    // Splice: N inherits target's old successors.
                    // Successors now depend ONLY on N.
                    new_task.after = vec![target_id.to_string()];
                    new_task.before = target_before.clone();
                    for succ_id in &target_before {
                        if let Some(s) = graph.get_task_mut(succ_id) {
                            if !s.after.contains(&final_id) {
                                s.after.push(final_id.clone());
                            }
                            s.after.retain(|p| p != target_id);
                        }
                    }
                    if let Some(t) = graph.get_task_mut(target_id) {
                        t.before = vec![final_id.clone()];
                    }
                } else {
                    // Additive: N is a new successor alongside the old ones.
                    new_task.after = vec![target_id.to_string()];
                    if let Some(t) = graph.get_task_mut(target_id) {
                        if !t.before.contains(&final_id) {
                            t.before.push(final_id.clone());
                        }
                    }
                }
            }
            Position::Parallel => {
                // N sits at target's slot — inherits both edge sets.
                new_task.after = target_after.clone();
                new_task.before = target_before.clone();

                // Predecessors: add N to their before list (parallel to target).
                for pred_id in &target_after {
                    if let Some(p) = graph.get_task_mut(pred_id)
                        && !p.before.contains(&final_id)
                    {
                        p.before.push(final_id.clone());
                    }
                }

                // Successors: always add N to their after list.
                // If replace_edges, also remove target from their after list
                // so they unblock from N only (rescue semantics).
                for succ_id in &target_before {
                    if let Some(s) = graph.get_task_mut(succ_id) {
                        if !s.after.contains(&final_id) {
                            s.after.push(final_id.clone());
                        }
                        if opts.replace_edges {
                            s.after.retain(|p| p != target_id);
                        }
                    }
                }

                // If replace_edges, target's outgoing edges are superseded
                // by N's — clear them from target so it becomes a dead node
                // (still present for history but no longer a live dependency).
                if opts.replace_edges {
                    if let Some(t) = graph.get_task_mut(target_id) {
                        t.before.clear();
                    }
                }
            }
        }

        graph.add_node(Node::Task(new_task));
        true
    })
    .context("failed to modify graph")?;

    crate::commands::notify_graph_changed(dir);
    let assigned_id = assigned_slot
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| derived_id.clone());
    Ok(assigned_id)
}

/// Derive a task ID from a title: lowercase, kebab-case, trim to ~40
/// chars. Matches the convention used by `wg add`.
fn derive_id_from_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.len() > 40 {
        trimmed[..40].trim_end_matches('-').to_string()
    } else {
        trimmed
    }
}

/// If `candidate` already exists in `graph`, append `-2`, `-3`, ... until
/// a unique ID is found. Preserves the caller's intent for the base name.
fn unique_id(graph: &workgraph::graph::WorkGraph, candidate: &str) -> String {
    if graph.get_task(candidate).is_none() {
        return candidate.to_string();
    }
    for n in 2..=999u32 {
        let tried = format!("{}-{}", candidate, n);
        if graph.get_task(&tried).is_none() {
            return tried;
        }
    }
    // Absurd fallback — just tack on a timestamp.
    format!("{}-{}", candidate, Utc::now().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::test_helpers::{make_task_with_status, setup_workgraph};
    use workgraph::parser::load_graph;
    use tempfile::tempdir;

    fn make(id: &str) -> Task {
        make_task_with_status(id, id, Status::Open)
    }

    // ── before ─────────────────────────────────────────────────────

    #[test]
    fn before_additive_keeps_old_predecessor_edge() {
        let dir = tempdir().unwrap();
        let mut p = make("p");
        let mut t = make("t");
        p.before = vec!["t".into()];
        t.after = vec!["p".into()];
        setup_workgraph(dir.path(), vec![p, t]);

        let new_id = run(
            dir.path(),
            Position::Before,
            "t",
            "new prereq",
            None,
            Some("n"),
            InsertOptions::default(),
        )
        .unwrap();
        assert_eq!(new_id, "n");

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();
        let t2 = g.get_task("t").unwrap();
        assert!(t2.after.contains(&"p".to_string()), "old pred preserved");
        assert!(t2.after.contains(&"n".to_string()), "new pred added");
        let n = g.get_task("n").unwrap();
        assert_eq!(n.before, vec!["t".to_string()]);
        assert!(n.after.is_empty(), "additive: no inherited edges");
    }

    #[test]
    fn before_splice_redirects_old_predecessor_through_new_node() {
        let dir = tempdir().unwrap();
        let mut p = make("p");
        let mut t = make("t");
        p.before = vec!["t".into()];
        t.after = vec!["p".into()];
        setup_workgraph(dir.path(), vec![p, t]);

        run(
            dir.path(),
            Position::Before,
            "t",
            "n",
            None,
            Some("n"),
            InsertOptions {
                splice: true,
                ..Default::default()
            },
        )
        .unwrap();

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();
        let t2 = g.get_task("t").unwrap();
        assert_eq!(t2.after, vec!["n".to_string()], "splice: t depends only on n");
        let n = g.get_task("n").unwrap();
        assert_eq!(n.after, vec!["p".to_string()], "splice: n inherited p");
        let p2 = g.get_task("p").unwrap();
        assert!(p2.before.contains(&"n".to_string()), "p.before has n");
        assert!(!p2.before.contains(&"t".to_string()), "p.before no longer has t");
    }

    // ── after ──────────────────────────────────────────────────────

    #[test]
    fn after_additive_appends_successor() {
        let dir = tempdir().unwrap();
        let mut t = make("t");
        let mut s = make("s");
        t.before = vec!["s".into()];
        s.after = vec!["t".into()];
        setup_workgraph(dir.path(), vec![t, s]);

        run(
            dir.path(),
            Position::After,
            "t",
            "follow",
            None,
            Some("f"),
            InsertOptions::default(),
        )
        .unwrap();

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();
        let t2 = g.get_task("t").unwrap();
        assert!(t2.before.contains(&"s".to_string()));
        assert!(t2.before.contains(&"f".to_string()));
        let f = g.get_task("f").unwrap();
        assert_eq!(f.after, vec!["t".to_string()]);
        assert!(f.before.is_empty());
    }

    #[test]
    fn after_splice_redirects_old_successor() {
        let dir = tempdir().unwrap();
        let mut t = make("t");
        let mut s = make("s");
        t.before = vec!["s".into()];
        s.after = vec!["t".into()];
        setup_workgraph(dir.path(), vec![t, s]);

        run(
            dir.path(),
            Position::After,
            "t",
            "f",
            None,
            Some("f"),
            InsertOptions {
                splice: true,
                ..Default::default()
            },
        )
        .unwrap();

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();
        let t2 = g.get_task("t").unwrap();
        assert_eq!(t2.before, vec!["f".to_string()], "splice: t feeds only f");
        let f = g.get_task("f").unwrap();
        assert_eq!(f.before, vec!["s".to_string()], "splice: f inherited s");
        let s2 = g.get_task("s").unwrap();
        assert!(s2.after.contains(&"f".to_string()));
        assert!(!s2.after.contains(&"t".to_string()));
    }

    // ── parallel ───────────────────────────────────────────────────

    #[test]
    fn parallel_additive_inherits_both_edge_sets_leaves_target_intact() {
        let dir = tempdir().unwrap();
        let mut p = make("p");
        let mut t = make("t");
        let mut s = make("s");
        p.before = vec!["t".into()];
        t.after = vec!["p".into()];
        t.before = vec!["s".into()];
        s.after = vec!["t".into()];
        setup_workgraph(dir.path(), vec![p, t, s]);

        run(
            dir.path(),
            Position::Parallel,
            "t",
            "alt",
            None,
            Some("alt"),
            InsertOptions::default(),
        )
        .unwrap();

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();
        let n = g.get_task("alt").unwrap();
        assert_eq!(n.after, vec!["p".to_string()]);
        assert_eq!(n.before, vec!["s".to_string()]);
        let t2 = g.get_task("t").unwrap();
        // Target still has its edges
        assert_eq!(t2.after, vec!["p".to_string()]);
        assert_eq!(t2.before, vec!["s".to_string()]);
        // Successor now depends on both
        let s2 = g.get_task("s").unwrap();
        assert!(s2.after.contains(&"t".to_string()));
        assert!(s2.after.contains(&"alt".to_string()));
    }

    #[test]
    fn parallel_replace_edges_routes_successors_to_new_only() {
        let dir = tempdir().unwrap();
        let mut p = make("p");
        let mut t = make("t");
        let mut s = make("s");
        p.before = vec!["t".into()];
        t.after = vec!["p".into()];
        t.before = vec!["s".into()];
        s.after = vec!["t".into()];
        setup_workgraph(dir.path(), vec![p, t, s]);

        run(
            dir.path(),
            Position::Parallel,
            "t",
            "rescue",
            None,
            Some("rescue"),
            InsertOptions {
                replace_edges: true,
                ..Default::default()
            },
        )
        .unwrap();

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();
        let rescue = g.get_task("rescue").unwrap();
        assert_eq!(rescue.after, vec!["p".to_string()]);
        assert_eq!(rescue.before, vec!["s".to_string()]);
        // Successor now waits ONLY on rescue
        let s2 = g.get_task("s").unwrap();
        assert_eq!(s2.after, vec!["rescue".to_string()]);
        assert!(!s2.after.contains(&"t".to_string()));
        // Target is dead — cleared outgoing edges
        let t2 = g.get_task("t").unwrap();
        assert!(t2.before.is_empty(), "target's outgoing cleared");
        // Target's predecessors unchanged (t still depends on p, for history)
        assert_eq!(t2.after, vec!["p".to_string()]);
    }

    // ── misc ───────────────────────────────────────────────────────

    #[test]
    fn nonexistent_target_does_not_mutate_graph() {
        let dir = tempdir().unwrap();
        let t = make("t");
        setup_workgraph(dir.path(), vec![t]);

        let _ = run(
            dir.path(),
            Position::Before,
            "nonexistent",
            "x",
            None,
            Some("x"),
            InsertOptions::default(),
        );

        let g = load_graph(&crate::commands::graph_path(dir.path())).unwrap();
        assert!(g.get_task("x").is_none(), "no task created");
    }

    #[test]
    fn id_collision_gets_suffix() {
        let dir = tempdir().unwrap();
        let t = make("t");
        let n = make("n");
        setup_workgraph(dir.path(), vec![t, n]);

        let assigned = run(
            dir.path(),
            Position::Before,
            "t",
            "x",
            None,
            Some("n"),
            InsertOptions::default(),
        )
        .unwrap();
        assert_eq!(assigned, "n-2", "colliding id gets numeric suffix");
    }

    #[test]
    fn derive_id_from_title_slugifies_correctly() {
        assert_eq!(
            derive_id_from_title("Do the thing, please!"),
            "do-the-thing-please"
        );
        assert_eq!(derive_id_from_title("  leading space"), "leading-space");
        let long = "a".repeat(60);
        assert!(derive_id_from_title(&long).len() <= 40);
    }
}

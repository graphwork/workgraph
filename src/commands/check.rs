use anyhow::{Context, Result};
use std::path::Path;
use workgraph::check::{check_all, LoopEdgeIssueKind};
use workgraph::parser::load_graph;

use super::graph_path;

pub fn run(dir: &Path) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let result = check_all(&graph);

    let mut warnings = 0;
    let mut errors = 0;

    // Cycles are warnings (allowed for recurring tasks)
    if !result.cycles.is_empty() {
        println!("Warning: Cycles detected (this is OK for recurring tasks):");
        for cycle in &result.cycles {
            println!("  {}", cycle.join(" -> "));
            warnings += 1;
        }
    }

    // Orphan references are errors
    if !result.orphan_refs.is_empty() {
        println!("Error: Orphan references:");
        for orphan in &result.orphan_refs {
            println!("  {} --[{}]--> {} (not found)", orphan.from, orphan.relation, orphan.to);
            errors += 1;
        }
    }

    // Loop edge issues are errors
    if !result.loop_edge_issues.is_empty() {
        println!("Error: Loop edge issues:");
        for issue in &result.loop_edge_issues {
            let desc = match &issue.kind {
                LoopEdgeIssueKind::TargetNotFound => {
                    format!("{} --[loops_to]--> {} (target not found)", issue.from, issue.target)
                }
                LoopEdgeIssueKind::ZeroMaxIterations => {
                    format!("{} --[loops_to]--> {} (max_iterations is 0, loop will never fire)", issue.from, issue.target)
                }
                LoopEdgeIssueKind::GuardTaskNotFound(guard_task) => {
                    format!("{} --[loops_to]--> {} (guard references non-existent task '{}')", issue.from, issue.target, guard_task)
                }
                LoopEdgeIssueKind::SelfLoop => {
                    format!("{} --[loops_to]--> {} (self-loop: task would immediately re-open on completion)", issue.from, issue.target)
                }
            };
            println!("  {}", desc);
            errors += 1;
        }
    }

    // Count loop edges for info
    let loop_edge_count: usize = graph.tasks().map(|t| t.loops_to.len()).sum();
    if loop_edge_count > 0 && result.loop_edge_issues.is_empty() {
        println!("Loop edges: {} edge(s), all valid", loop_edge_count);
    }

    if errors > 0 {
        anyhow::bail!("Found {} error(s) and {} warning(s)", errors, warnings);
    } else if warnings > 0 {
        println!("Graph OK: {} nodes, {} warning(s)", graph.len(), warnings);
    } else {
        println!("Graph OK: {} nodes, no issues found", graph.len());
    }

    Ok(())
}

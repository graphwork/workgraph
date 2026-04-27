//! `wg recover` — batch recovery for credit-exhaustion / mass-failure scenarios.
//!
//! Surveys failed tasks and resets them in one operation: retries user-tasks,
//! abandons agency followups (`.evaluate-*` / `.flip-*` / `.assign-*` / `.verify-*`)
//! so they regenerate fresh from their parents.
//!
//! Defaults to dry-run; pass `--yes` to execute.
//!
//! See task `wg-recover-clean` for the spec.

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status, Task};
use workgraph::parser::modify_graph;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

/// Options for `wg recover`.
#[derive(Debug, Clone, Default)]
pub struct RecoverOptions {
    /// Execute the plan. If false, print the plan only.
    pub yes: bool,
    /// Filter expression(s) (e.g., `status=failed`, `tag=foo`,
    /// `id-prefix=tui-`, `attempts<=2`, `error~timeout`). AND-combined.
    pub filter: Vec<String>,
    /// Override model on each user-task before retry (provider:model format).
    pub set_model: Option<String>,
    /// Override endpoint on each user-task before retry.
    pub set_endpoint: Option<String>,
    /// Keep agency followups instead of abandoning them.
    pub keep_agency: bool,
    /// Skip tasks whose retry_count >= max_attempts.
    pub max_attempts: u32,
    /// Recovery reason — recorded as a log entry on each retried task.
    pub reason: Option<String>,
}

impl RecoverOptions {
    pub fn dry_run() -> Self {
        Self {
            yes: false,
            max_attempts: 5,
            ..Default::default()
        }
    }
}

/// One row in the recovery plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub id: String,
    pub action: PlanAction,
    pub attempt_after: u32,
    pub model_change: Option<String>,
    pub endpoint_change: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    Retry,
    AbandonFollowup,
    Skip(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    MaxAttemptsExceeded,
    FilteredOut,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub user_retries: Vec<PlanEntry>,
    pub agency_abandons: Vec<PlanEntry>,
    pub skipped: Vec<PlanEntry>,
}

impl Plan {
    pub fn total_failed(&self) -> usize {
        self.user_retries.len() + self.agency_abandons.len() + self.skipped.len()
    }
}

/// Build the recovery plan from a graph snapshot. Pure function — no I/O.
pub fn build_plan(tasks: &[Task], opts: &RecoverOptions) -> Result<Plan> {
    let filters = parse_filters(&opts.filter)?;

    let mut plan = Plan::default();

    for task in tasks {
        if !task_matches_filters(task, &filters) {
            continue;
        }

        let entry = PlanEntry {
            id: task.id.clone(),
            action: PlanAction::Skip(SkipReason::FilteredOut),
            attempt_after: task.retry_count + 1,
            model_change: opts.set_model.clone(),
            endpoint_change: opts.set_endpoint.clone(),
        };

        if workgraph::graph::is_system_task(&task.id) {
            // Agency followups: abandon (unless --keep-agency).
            if is_agency_followup(&task.id) && !opts.keep_agency {
                plan.agency_abandons.push(PlanEntry {
                    action: PlanAction::AbandonFollowup,
                    ..entry
                });
            }
            // Other system tasks (e.g., `.coordinator-*`, `.compact-*`) we
            // leave alone — they don't belong in a credit-exhaustion sweep.
        } else if task.retry_count >= opts.max_attempts {
            plan.skipped.push(PlanEntry {
                action: PlanAction::Skip(SkipReason::MaxAttemptsExceeded),
                ..entry
            });
        } else {
            plan.user_retries.push(PlanEntry {
                action: PlanAction::Retry,
                ..entry
            });
        }
    }

    plan.user_retries.sort_by(|a, b| a.id.cmp(&b.id));
    plan.agency_abandons.sort_by(|a, b| a.id.cmp(&b.id));
    plan.skipped.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(plan)
}

fn is_agency_followup(id: &str) -> bool {
    id.starts_with(".evaluate-")
        || id.starts_with(".flip-")
        || id.starts_with(".assign-")
        || id.starts_with(".verify-")
        || id.starts_with(".verify-deferred-")
}

#[derive(Debug, Clone)]
enum Filter {
    Status(Status),
    Tag(String),
    IdPrefix(String),
    AttemptsCmp(Cmp, u32),
    ErrorContains(String),
}

#[derive(Debug, Clone, Copy)]
enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

fn parse_filters(raw: &[String]) -> Result<Vec<Filter>> {
    // Default: status=failed. Apply any --filter args on top (overriding status if specified).
    let mut filters: Vec<Filter> = vec![Filter::Status(Status::Failed)];

    for r in raw {
        for clause in r.split(',') {
            let clause = clause.trim();
            if clause.is_empty() {
                continue;
            }
            let f = parse_one_filter(clause)
                .with_context(|| format!("Invalid --filter clause: {}", clause))?;
            // If this clause is a Status filter, drop the default Status filter.
            if matches!(f, Filter::Status(_)) {
                filters.retain(|x| !matches!(x, Filter::Status(_)));
            }
            filters.push(f);
        }
    }

    Ok(filters)
}

fn parse_one_filter(clause: &str) -> Result<Filter> {
    if let Some(v) = clause.strip_prefix("status=") {
        let s = parse_status(v)?;
        return Ok(Filter::Status(s));
    }
    if let Some(v) = clause.strip_prefix("tag=") {
        return Ok(Filter::Tag(v.trim().to_string()));
    }
    if let Some(v) = clause.strip_prefix("id-prefix=") {
        return Ok(Filter::IdPrefix(v.trim().to_string()));
    }
    if let Some(v) = clause.strip_prefix("error~") {
        return Ok(Filter::ErrorContains(v.trim().to_string()));
    }
    if let Some((cmp, n)) = parse_attempts_cmp(clause) {
        return Ok(Filter::AttemptsCmp(cmp, n));
    }
    anyhow::bail!(
        "unknown filter clause '{}' (expected status=X, tag=X, id-prefix=X, attempts<=N, error~X)",
        clause
    );
}

fn parse_attempts_cmp(clause: &str) -> Option<(Cmp, u32)> {
    let prefixes = [
        ("attempts<=", Cmp::Le),
        ("attempts>=", Cmp::Ge),
        ("attempts==", Cmp::Eq),
        ("attempts<", Cmp::Lt),
        ("attempts>", Cmp::Gt),
        ("attempts=", Cmp::Eq),
    ];
    for (p, cmp) in prefixes {
        if let Some(v) = clause.strip_prefix(p) {
            return v.trim().parse::<u32>().ok().map(|n| (cmp, n));
        }
    }
    None
}

fn parse_status(s: &str) -> Result<Status> {
    let s = s.trim().to_lowercase();
    let st = match s.as_str() {
        "open" => Status::Open,
        "in-progress" | "inprogress" => Status::InProgress,
        "done" => Status::Done,
        "failed" => Status::Failed,
        "abandoned" => Status::Abandoned,
        "blocked" => Status::Blocked,
        "waiting" => Status::Waiting,
        "incomplete" => Status::Incomplete,
        "pending-validation" | "pendingvalidation" => Status::PendingValidation,
        _ => anyhow::bail!("unknown status '{}'", s),
    };
    Ok(st)
}

fn task_matches_filters(task: &Task, filters: &[Filter]) -> bool {
    for f in filters {
        match f {
            Filter::Status(s) => {
                if &task.status != s {
                    return false;
                }
            }
            Filter::Tag(t) => {
                if !task.tags.iter().any(|tag| tag == t) {
                    return false;
                }
            }
            Filter::IdPrefix(p) => {
                if !task.id.starts_with(p) {
                    return false;
                }
            }
            Filter::AttemptsCmp(cmp, n) => {
                let a = task.retry_count;
                let ok = match cmp {
                    Cmp::Lt => a < *n,
                    Cmp::Le => a <= *n,
                    Cmp::Eq => a == *n,
                    Cmp::Ge => a >= *n,
                    Cmp::Gt => a > *n,
                };
                if !ok {
                    return false;
                }
            }
            Filter::ErrorContains(needle) => {
                let hay = task.failure_reason.as_deref().unwrap_or("");
                if !hay.contains(needle.as_str()) {
                    return false;
                }
            }
        }
    }
    true
}

/// Apply the plan to `graph.jsonl`. Returns the executed plan.
fn apply_plan(dir: &Path, plan: &Plan, opts: &RecoverOptions) -> Result<()> {
    let path = super::graph_path(dir);
    let now = Utc::now().to_rfc3339();
    let user = workgraph::current_user();
    let recover_msg = format!(
        "Reset by `wg recover`{}",
        opts.reason
            .as_deref()
            .map(|r| format!(" — reason: {}", r))
            .unwrap_or_default()
    );

    modify_graph(&path, |graph| {
        // Retry user tasks
        for entry in &plan.user_retries {
            if let Some(task) = graph.get_task_mut(&entry.id) {
                task.status = Status::Open;
                task.failure_reason = None;
                task.assigned = None;
                task.ready_after = None;
                task.session_id = None;
                task.checkpoint = None;
                task.tags.retain(|t| t != "converged");

                if let Some(m) = &opts.set_model {
                    task.model = Some(m.clone());
                }
                if let Some(e) = &opts.set_endpoint {
                    task.endpoint = Some(e.clone());
                }

                task.log.push(LogEntry {
                    timestamp: now.clone(),
                    actor: None,
                    user: Some(user.clone()),
                    message: recover_msg.clone(),
                });
            }
        }

        // Abandon agency followups. Don't skip Failed — that's the whole point
        // of recover: failed followups need to be cleared so the agency pipeline
        // regenerates them from the freshly-retried parent. But preserve Done
        // and already-Abandoned status (no work to do).
        for entry in &plan.agency_abandons {
            if let Some(task) = graph.get_task_mut(&entry.id) {
                if task.status != Status::Done && task.status != Status::Abandoned {
                    task.status = Status::Abandoned;
                    task.failure_reason = Some(
                        "Auto-abandoned by `wg recover` so it regenerates from parent"
                            .to_string(),
                    );
                    task.log.push(LogEntry {
                        timestamp: now.clone(),
                        actor: None,
                        user: Some(user.clone()),
                        message: format!(
                            "Abandoned by `wg recover`{}",
                            opts.reason
                                .as_deref()
                                .map(|r| format!(" — reason: {}", r))
                                .unwrap_or_default()
                        ),
                    });
                }
            }
        }
        true
    })
    .context("Failed to modify graph during recovery")?;

    super::notify_graph_changed(dir);

    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "recover",
        None,
        None,
        serde_json::json!({
            "retried": plan.user_retries.iter().map(|e| &e.id).collect::<Vec<_>>(),
            "abandoned": plan.agency_abandons.iter().map(|e| &e.id).collect::<Vec<_>>(),
            "skipped": plan.skipped.iter().map(|e| &e.id).collect::<Vec<_>>(),
            "set_model": opts.set_model,
            "set_endpoint": opts.set_endpoint,
            "reason": opts.reason,
        }),
        config.log.rotation_threshold,
    );

    Ok(())
}

pub fn print_plan(plan: &Plan, opts: &RecoverOptions) {
    println!("=== wg recover: {} matching tasks ===", plan.total_failed());
    println!("User tasks (will retry): {}", plan.user_retries.len());
    println!(
        "Agency followups ({}): {}",
        if opts.keep_agency {
            "kept"
        } else {
            "will abandon, auto-recreate"
        },
        plan.agency_abandons.len()
    );
    println!("Skipped (max-attempts exceeded): {}", plan.skipped.len());

    if !plan.user_retries.is_empty() {
        println!("\nUser-task changes:");
        for e in &plan.user_retries {
            let model_note = e
                .model_change
                .as_deref()
                .map(|m| format!("  model→ {}", m))
                .unwrap_or_default();
            let endpoint_note = e
                .endpoint_change
                .as_deref()
                .map(|x| format!("  endpoint→ {}", x))
                .unwrap_or_default();
            println!(
                "  {:<32} attempt #{}{}{}",
                e.id, e.attempt_after, model_note, endpoint_note
            );
        }
    }

    if !plan.agency_abandons.is_empty() {
        println!("\nAgency followups:");
        for e in &plan.agency_abandons {
            println!("  {}", e.id);
        }
    }

    if !plan.skipped.is_empty() {
        println!("\nSkipped:");
        for e in &plan.skipped {
            let why = match &e.action {
                PlanAction::Skip(SkipReason::MaxAttemptsExceeded) => {
                    format!("max-attempts ({}) exceeded", opts.max_attempts)
                }
                PlanAction::Skip(SkipReason::FilteredOut) => "filtered out".to_string(),
                _ => "?".to_string(),
            };
            println!("  {:<32} attempt #{} — {}", e.id, e.attempt_after, why);
        }
    }

    if !opts.yes {
        println!("\nApply with --yes");
    }
}

/// Top-level CLI entry point.
pub fn run(dir: &Path, opts: RecoverOptions) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    if let Some(m) = &opts.set_model
        && let Err(e) = workgraph::config::parse_model_spec_strict(m)
    {
        anyhow::bail!("Invalid --set-model format: {}", e);
    }

    let graph = workgraph::parser::load_graph(&path).context("Failed to load graph")?;
    let tasks: Vec<Task> = graph.tasks().cloned().collect();
    let plan = build_plan(&tasks, &opts)?;
    print_plan(&plan, &opts);

    if opts.yes {
        if plan.user_retries.is_empty() && plan.agency_abandons.is_empty() {
            println!("\nNothing to do.");
            return Ok(());
        }
        apply_plan(dir, &plan, &opts)?;
        println!(
            "\nApplied: {} retried, {} abandoned",
            plan.user_retries.len(),
            plan.agency_abandons.len()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, WorkGraph};
    use workgraph::parser::save_graph;

    fn task(id: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            status,
            ..Task::default()
        }
    }

    fn setup(dir: &Path, tasks: Vec<Task>) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut g = WorkGraph::new();
        for t in tasks {
            g.add_node(Node::Task(t));
        }
        save_graph(&g, &path).unwrap();
        path
    }

    #[test]
    fn test_recover_dry_run_lists_failures() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let mut t1 = task("user-a", Status::Failed);
        t1.retry_count = 1;
        let mut e1 = task(".evaluate-user-a", Status::Failed);
        e1.retry_count = 1;
        let ok = task("user-b", Status::Done);
        setup(dp, vec![t1, e1, ok]);

        let opts = RecoverOptions::dry_run();
        // Just call run() — confirm no error and no state change.
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        // Nothing should have changed
        assert_eq!(g.get_task("user-a").unwrap().status, Status::Failed);
        assert_eq!(g.get_task(".evaluate-user-a").unwrap().status, Status::Failed);
    }

    #[test]
    fn test_build_plan_separates_user_and_agency() {
        let mut t1 = task("user-a", Status::Failed);
        t1.retry_count = 1;
        let e1 = task(".evaluate-user-a", Status::Failed);
        let f1 = task(".flip-user-a", Status::Failed);
        let a1 = task(".assign-user-a", Status::Failed);
        let v1 = task(".verify-user-a", Status::Failed);
        let other_sys = task(".coordinator-0", Status::Failed);
        let done = task("done-task", Status::Done);

        let opts = RecoverOptions::dry_run();
        let plan = build_plan(&[t1, e1, f1, a1, v1, other_sys, done], &opts).unwrap();
        assert_eq!(plan.user_retries.len(), 1);
        assert_eq!(plan.user_retries[0].id, "user-a");
        // Should pick up .evaluate, .flip, .assign, .verify but NOT .coordinator-0
        assert_eq!(plan.agency_abandons.len(), 4);
        for e in &plan.agency_abandons {
            assert!(
                e.id.starts_with(".evaluate-")
                    || e.id.starts_with(".flip-")
                    || e.id.starts_with(".assign-")
                    || e.id.starts_with(".verify-")
            );
        }
    }

    #[test]
    fn test_recover_yes_resets_user_tasks() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let mut t1 = task("user-a", Status::Failed);
        t1.retry_count = 2;
        t1.failure_reason = Some("credit exhaustion".to_string());
        t1.assigned = Some("agent-x".to_string());
        setup(dp, vec![t1]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            ..Default::default()
        };
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        let t = g.get_task("user-a").unwrap();
        assert_eq!(t.status, Status::Open);
        assert_eq!(t.failure_reason, None);
        assert_eq!(t.assigned, None);
        assert_eq!(t.retry_count, 2, "retry_count preserved");
    }

    #[test]
    fn test_recover_yes_abandons_agency_followups() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let t1 = task("user-a", Status::Failed);
        let mut e1 = task(".evaluate-user-a", Status::Failed);
        e1.after = vec!["user-a".to_string()];
        let mut f1 = task(".flip-user-a", Status::Failed);
        f1.after = vec!["user-a".to_string()];
        setup(dp, vec![t1, e1, f1]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            ..Default::default()
        };
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        assert_eq!(g.get_task("user-a").unwrap().status, Status::Open);
        assert_eq!(g.get_task(".evaluate-user-a").unwrap().status, Status::Abandoned);
        assert_eq!(g.get_task(".flip-user-a").unwrap().status, Status::Abandoned);
    }

    #[test]
    fn test_recover_keep_agency_preserves_followups() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let t1 = task("user-a", Status::Failed);
        let e1 = task(".evaluate-user-a", Status::Failed);
        setup(dp, vec![t1, e1]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            keep_agency: true,
            ..Default::default()
        };
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        assert_eq!(g.get_task("user-a").unwrap().status, Status::Open);
        assert_eq!(
            g.get_task(".evaluate-user-a").unwrap().status,
            Status::Failed,
            "--keep-agency should leave followups untouched"
        );
    }

    #[test]
    fn test_recover_set_model_edits_before_retry() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let mut t1 = task("user-a", Status::Failed);
        t1.model = Some("claude:sonnet-4-6".to_string());
        setup(dp, vec![t1]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            set_model: Some("openrouter:anthropic/claude-sonnet-4-6".to_string()),
            ..Default::default()
        };
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        let t = g.get_task("user-a").unwrap();
        assert_eq!(t.status, Status::Open);
        assert_eq!(
            t.model.as_deref(),
            Some("openrouter:anthropic/claude-sonnet-4-6")
        );
    }

    #[test]
    fn test_recover_max_attempts_skips_exhausted() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let mut t1 = task("user-a", Status::Failed);
        t1.retry_count = 5;
        let mut t2 = task("user-b", Status::Failed);
        t2.retry_count = 1;
        setup(dp, vec![t1, t2]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            ..Default::default()
        };
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        // user-a was at attempt 5 → skipped (NOT reset)
        assert_eq!(g.get_task("user-a").unwrap().status, Status::Failed);
        // user-b had retry_count 1 < 5 → retried
        assert_eq!(g.get_task("user-b").unwrap().status, Status::Open);
    }

    #[test]
    fn test_recover_filter_status_open_excludes_failed() {
        // Filter switches the default status filter
        let mut t1 = task("user-a", Status::Failed);
        t1.retry_count = 0;
        let t2 = task("user-b", Status::Open);
        let opts = RecoverOptions {
            filter: vec!["status=open".to_string()],
            max_attempts: 5,
            ..Default::default()
        };
        let plan = build_plan(&[t1, t2], &opts).unwrap();
        // status=open filter means user-b is the only candidate
        assert_eq!(plan.user_retries.len(), 1);
        assert_eq!(plan.user_retries[0].id, "user-b");
    }

    #[test]
    fn test_recover_filter_id_prefix() {
        let t1 = task("tui-a", Status::Failed);
        let t2 = task("tui-b", Status::Failed);
        let t3 = task("nex-c", Status::Failed);
        let opts = RecoverOptions {
            filter: vec!["id-prefix=tui-".to_string()],
            max_attempts: 5,
            ..Default::default()
        };
        let plan = build_plan(&[t1, t2, t3], &opts).unwrap();
        let ids: Vec<&str> = plan.user_retries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["tui-a", "tui-b"]);
    }

    #[test]
    fn test_recover_filter_attempts_le() {
        let mut t1 = task("user-a", Status::Failed);
        t1.retry_count = 1;
        let mut t2 = task("user-b", Status::Failed);
        t2.retry_count = 4;
        let opts = RecoverOptions {
            filter: vec!["attempts<=2".to_string()],
            max_attempts: 99,
            ..Default::default()
        };
        let plan = build_plan(&[t1, t2], &opts).unwrap();
        let ids: Vec<&str> = plan.user_retries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["user-a"]);
    }

    #[test]
    fn test_recover_filter_error_contains() {
        let mut t1 = task("user-a", Status::Failed);
        t1.failure_reason = Some("hit credit limit".to_string());
        let mut t2 = task("user-b", Status::Failed);
        t2.failure_reason = Some("compile error".to_string());
        let opts = RecoverOptions {
            filter: vec!["error~credit".to_string()],
            max_attempts: 5,
            ..Default::default()
        };
        let plan = build_plan(&[t1, t2], &opts).unwrap();
        let ids: Vec<&str> = plan.user_retries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["user-a"]);
    }

    #[test]
    fn test_recover_reason_logged_on_each_retry() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let t1 = task("user-a", Status::Failed);
        setup(dp, vec![t1]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            reason: Some("credit-exhaustion-2026-04-26".to_string()),
            ..Default::default()
        };
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        let t = g.get_task("user-a").unwrap();
        let last = t.log.last().unwrap();
        assert!(
            last.message.contains("credit-exhaustion-2026-04-26"),
            "log should include reason: {}",
            last.message
        );
    }

    #[test]
    fn test_recover_set_endpoint_edits_before_retry() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let t1 = task("user-a", Status::Failed);
        setup(dp, vec![t1]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            set_endpoint: Some("openrouter".to_string()),
            ..Default::default()
        };
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        assert_eq!(g.get_task("user-a").unwrap().endpoint.as_deref(), Some("openrouter"));
    }

    #[test]
    fn test_recover_does_not_touch_non_failed_user_tasks() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        let t1 = task("user-a", Status::Failed);
        let t2 = task("user-b", Status::InProgress);
        let t3 = task("user-c", Status::Done);
        setup(dp, vec![t1, t2, t3]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            ..Default::default()
        };
        run(dp, opts).unwrap();

        let g = load_graph(graph_path(dp)).unwrap();
        assert_eq!(g.get_task("user-a").unwrap().status, Status::Open);
        assert_eq!(g.get_task("user-b").unwrap().status, Status::InProgress);
        assert_eq!(g.get_task("user-c").unwrap().status, Status::Done);
    }

    #[test]
    fn test_recover_invalid_model_format_errors() {
        let dir = tempdir().unwrap();
        let dp = dir.path();
        setup(dp, vec![task("user-a", Status::Failed)]);

        let opts = RecoverOptions {
            yes: true,
            max_attempts: 5,
            set_model: Some("not-a-valid-spec".to_string()),
            ..Default::default()
        };
        let err = run(dp, opts).unwrap_err();
        assert!(err.to_string().contains("Invalid --set-model"));
    }
}

//! Context assembly for spawned agents.
//!
//! Gathers dependency artifacts, logs, scope-based context (downstream awareness,
//! graph summaries, CLAUDE.md), and resolves the effective context scope.

use std::fs;
use std::path::Path;

use workgraph::config::Config;
use workgraph::context_scope::ContextScope;
use workgraph::graph::{LogEntry, Status};
use workgraph::notify::config::NotifyConfig;
use workgraph::notify::telegram::TelegramConfig;

/// Knowledge tiers for model-specific context injection
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum KnowledgeTier {
    Essential, // 8KB for Minimax M2.7, 32K context models
    Core,      // 16KB for DeepSeek V3, 64K context models
    Full,      // 40KB for Llama 3.1+, 128K context models
}

/// Build context string from dependency artifacts and logs.
///
/// When scope >= Task, includes upstream task titles alongside artifacts (R5).
pub(crate) fn build_task_context(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
) -> String {
    let mut context_parts = Vec::new();

    for dep_id in &task.after {
        if let Some(dep_task) = graph.get_task(dep_id) {
            // R5: Include upstream task title alongside artifacts
            if !dep_task.artifacts.is_empty() {
                context_parts.push(format!(
                    "From {} ({}): artifacts: {}",
                    dep_id,
                    dep_task.title,
                    dep_task.artifacts.join(", ")
                ));
            }

            if dep_task.status == Status::Done && !dep_task.log.is_empty() {
                let logs: Vec<&LogEntry> = dep_task.log.iter().rev().take(5).collect();
                for entry in logs.iter().rev() {
                    context_parts.push(format!(
                        "From {} logs: {} {}",
                        dep_id, entry.timestamp, entry.message
                    ));
                }
            }

            // Include context for Failed dependencies (triage support)
            if dep_task.status == Status::Failed {
                let reason = dep_task.failure_reason.as_deref().unwrap_or("unknown");
                context_parts.push(format!("From {} (FAILED): reason: {}", dep_id, reason));
                if !dep_task.log.is_empty() {
                    let logs: Vec<&LogEntry> = dep_task.log.iter().rev().take(3).collect();
                    for entry in logs.iter().rev() {
                        context_parts.push(format!(
                            "From {} logs: {} {}",
                            dep_id, entry.timestamp, entry.message
                        ));
                    }
                }
            }
        }
    }

    // Inject cycle metadata if this task has cycle_config
    if let Some(ref cc) = task.cycle_config {
        context_parts.push(format!(
            "Cycle status: iteration {} of this task (max {})",
            task.loop_iteration, cc.max_iterations
        ));
        if let Some(ref delay) = cc.delay {
            context_parts.push(format!("  cycle delay: {}", delay));
        }
    }

    // Inject resume context from checkpoint (set by coordinator when waking a Waiting task)
    if let Some(ref checkpoint) = task.checkpoint {
        context_parts.push(checkpoint.clone());
    }

    if context_parts.is_empty() {
        "No context from dependencies".to_string()
    } else {
        context_parts.join("\n")
    }
}

/// Build the ScopeContext for scope-based prompt assembly.
///
/// Gathers R1 (downstream awareness), R4 (tags/skills), project description,
/// graph summaries, and CLAUDE.md content based on the resolved scope.
pub(crate) fn build_scope_context(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
    scope: ContextScope,
    config: &Config,
    workgraph_dir: &Path,
) -> workgraph::service::executor::ScopeContext {
    let mut ctx = workgraph::service::executor::ScopeContext::default();

    // R1: Downstream awareness (task+ scope)
    if scope >= ContextScope::Task {
        let task_id = &task.id;
        let downstream: Vec<_> = graph
            .tasks()
            .filter(|t| t.after.contains(task_id))
            .collect();
        if !downstream.is_empty() {
            let mut lines =
                vec!["## Downstream Consumers\n\nTasks that depend on your work:".to_string()];
            for dt in &downstream {
                lines.push(format!("- **{}**: \"{}\"", dt.id, dt.title));
            }
            ctx.downstream_info = lines.join("\n");
        }
    }

    // R4: Tags and skills (task+ scope)
    if scope >= ContextScope::Task {
        let mut info_parts = Vec::new();
        if !task.tags.is_empty() {
            info_parts.push(format!("- **Tags:** {}", task.tags.join(", ")));
        }
        if !task.skills.is_empty() {
            info_parts.push(format!("- **Skills:** {}", task.skills.join(", ")));
        }
        if !info_parts.is_empty() {
            ctx.tags_skills_info = info_parts.join("\n");
        }
    }

    // Graph+ scope: project description
    if scope >= ContextScope::Graph
        && let Some(ref desc) = config.project.description
        && !desc.is_empty()
    {
        ctx.project_description = desc.clone();
    }

    // Graph+ scope: 1-hop neighborhood subgraph summary
    if scope >= ContextScope::Graph {
        ctx.graph_summary = build_graph_summary(graph, task, workgraph_dir);
    }

    // Full scope: full graph summary
    if scope >= ContextScope::Full {
        ctx.full_graph_summary = build_full_graph_summary(graph);
    }

    // Full scope: CLAUDE.md content
    if scope >= ContextScope::Full {
        ctx.claude_md_content = read_claude_md(workgraph_dir);
    }

    // Task+ scope: queued messages
    if scope >= ContextScope::Task {
        ctx.queued_messages = workgraph::messages::format_queued_messages(workgraph_dir, &task.id);
    }

    // Note: cursor advancement happens after spawn in execution.rs,
    // where the agent_id is known.

    // Adaptive decomposition guidance toggle (from config)
    ctx.decomp_guidance = config.guardrails.decomp_guidance;

    // Task+ scope: Telegram escalation availability
    if scope >= ContextScope::Task {
        ctx.telegram_available = is_telegram_configured(workgraph_dir);
    }

    ctx
}

/// Test file glob patterns recognized by the pre-spawn scanner.
///
/// Each entry is `(glob_pattern, verify_command_template)`.
/// The template uses `{file}` as a placeholder for the matched path.
const TEST_FILE_PATTERNS: &[(&str, &str)] = &[
    // Python
    ("test_*.py", "python -m pytest {file}"),
    ("*_test.py", "python -m pytest {file}"),
    // Rust (Rust tests are typically inline; but test binaries live in tests/)
    ("test_*.rs", "cargo test"),
    ("*_test.rs", "cargo test"),
    // Go
    ("*_test.go", "go test ./..."),
    // JavaScript / TypeScript
    ("*.test.js", "npx jest {file}"),
    ("*.test.ts", "npx jest {file}"),
    ("*.test.tsx", "npx jest {file}"),
    ("*.test.mjs", "npx jest {file}"),
    ("*.spec.js", "npx jest {file}"),
    ("*.spec.ts", "npx jest {file}"),
    ("*.spec.tsx", "npx jest {file}"),
];

/// Directories to scan for test files (relative to project root).
const TEST_DIRS: &[&str] = &["tests", "test", "spec", "src/tests", "src/test"];

/// Discover test files in the project directory before spawning an agent.
///
/// Returns a list of test file paths (relative to `project_root`).
/// Scans both well-known test directories and the project root for
/// files matching common test naming conventions.
pub(crate) fn discover_test_files(project_root: &Path) -> Vec<String> {
    let mut found = Vec::new();

    // Collect directories to scan: known test dirs + project root
    let mut scan_dirs: Vec<std::path::PathBuf> = TEST_DIRS
        .iter()
        .map(|d| project_root.join(d))
        .filter(|d| d.is_dir())
        .collect();
    // Also scan project root itself (for top-level test files)
    scan_dirs.push(project_root.to_path_buf());

    for dir in &scan_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            for (pattern, _) in TEST_FILE_PATTERNS {
                if glob_match(pattern, &file_name) {
                    // Make path relative to project root
                    if let Ok(rel) = path.strip_prefix(project_root) {
                        let rel_str = rel.to_string_lossy().to_string();
                        if !found.contains(&rel_str) {
                            found.push(rel_str);
                        }
                    }
                    break; // matched — no need to check other patterns
                }
            }
        }
    }

    found.sort();
    found
}

/// Build a verify command from discovered test files.
///
/// Selects the most appropriate test runner based on the file types found.
/// Returns `None` if no test files were discovered.
#[allow(dead_code)]
pub(crate) fn build_auto_verify_command(test_files: &[String]) -> Option<String> {
    if test_files.is_empty() {
        return None;
    }

    // Detect project type from test file extensions
    let has_rust = test_files.iter().any(|f| f.ends_with(".rs"));
    let has_python = test_files.iter().any(|f| f.ends_with(".py"));
    let has_go = test_files.iter().any(|f| f.ends_with(".go"));
    let has_js = test_files.iter().any(|f| {
        f.ends_with(".js") || f.ends_with(".ts") || f.ends_with(".tsx") || f.ends_with(".mjs")
    });

    // For mixed projects or Rust projects, use cargo test
    if has_rust {
        return Some("cargo test".to_string());
    }
    if has_go {
        return Some("go test ./...".to_string());
    }
    if has_python {
        // Use the specific test files for targeted verification
        let py_files: Vec<&str> = test_files
            .iter()
            .filter(|f| f.ends_with(".py"))
            .map(|s| s.as_str())
            .collect();
        return Some(format!("python -m pytest {}", py_files.join(" ")));
    }
    if has_js {
        return Some("npx jest".to_string());
    }

    None
}

/// Format discovered test files for injection into the agent prompt.
pub(crate) fn format_test_discovery_context(test_files: &[String]) -> String {
    if test_files.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "## Discovered Test Files\n".to_string(),
        "The following test files exist in this project and will be used to verify your work:"
            .to_string(),
    ];
    for f in test_files {
        lines.push(format!("- `{}`", f));
    }
    lines.push(String::new());
    lines.push(
        "**You MUST run these tests before calling `wg done`.** \
         If any test fails, fix the issue before marking the task complete."
            .to_string(),
    );

    lines.join("\n")
}

/// Simple glob matching for test file patterns.
/// Supports only `*` (matches any sequence of non-dot chars within a segment)
/// and leading/trailing wildcards.
fn glob_match(pattern: &str, name: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        // *_test.py → name ends with "_test.py"
        name.ends_with(suffix)
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        // test_* → name starts with "test_"
        name.starts_with(prefix)
    } else if pattern.contains('*') {
        // e.g. *.test.js — split on first *
        let (before, after) = pattern.split_once('*').unwrap();
        name.starts_with(before) && name.ends_with(after)
    } else {
        name == pattern
    }
}

/// Inline artifact content for graph+ scopes.
///
/// - Files under 500 bytes: inline full content
/// - Larger files: first 3 lines + byte count
/// - Non-existent files: note that file was not found
fn inline_artifact_content(artifacts: &[String], workgraph_dir: &Path) -> String {
    if artifacts.is_empty() {
        return String::new();
    }

    let project_root = workgraph_dir
        .canonicalize()
        .ok()
        .and_then(|abs| abs.parent().map(std::path::Path::to_path_buf));

    let project_root = match project_root {
        Some(r) => r,
        None => return String::new(),
    };

    let mut lines = Vec::new();
    for artifact in artifacts {
        let path = project_root.join(artifact);
        match fs::metadata(&path) {
            Ok(meta) => {
                let size = meta.len();
                if size <= 500 {
                    match fs::read_to_string(&path) {
                        Ok(content) => {
                            lines.push(format!(
                                "  {} ({} bytes):\n  ```\n{}\n  ```",
                                artifact, size, content
                            ));
                        }
                        Err(_) => {
                            lines.push(format!("  {} ({} bytes, binary)", artifact, size));
                        }
                    }
                } else {
                    // Large file: first 3 lines + byte count
                    match fs::read_to_string(&path) {
                        Ok(content) => {
                            let preview: String =
                                content.lines().take(3).collect::<Vec<_>>().join("\n");
                            lines.push(format!(
                                "  {} ({} bytes):\n  ```\n{}\n  ...\n  ```",
                                artifact, size, preview
                            ));
                        }
                        Err(_) => {
                            lines.push(format!("  {} ({} bytes, binary)", artifact, size));
                        }
                    }
                }
            }
            Err(_) => {
                lines.push(format!("  {} (not found)", artifact));
            }
        }
    }
    lines.join("\n")
}

/// Build a 1-hop neighborhood graph summary for graph+ scopes.
///
/// Includes: status counts, upstream tasks, downstream tasks, and siblings.
/// Neighbor content is wrapped in XML fencing for prompt injection protection.
/// Hard cap at 4000 chars.
pub(crate) fn build_graph_summary(
    graph: &workgraph::WorkGraph,
    task: &workgraph::graph::Task,
    workgraph_dir: &Path,
) -> String {
    let mut parts = Vec::new();

    // Status counts
    let mut open = 0u32;
    let mut in_progress = 0u32;
    let mut done = 0u32;
    let mut failed = 0u32;
    let mut blocked = 0u32;
    let total = graph.tasks().count() as u32;
    for t in graph.tasks() {
        match t.status {
            Status::Open => open += 1,
            Status::InProgress => in_progress += 1,
            Status::Done => done += 1,
            Status::Failed => failed += 1,
            Status::Blocked => blocked += 1,
            Status::Abandoned | Status::Waiting | Status::PendingValidation => {}
        }
    }
    parts.push(format!(
        "## Graph Status\n\n{} tasks \u{2014} {} done, {} in-progress, {} open, {} blocked, {} failed",
        total, done, in_progress, open, blocked, failed
    ));

    // Upstream tasks (direct dependencies) — XML fenced
    if !task.after.is_empty() {
        let mut lines = vec!["### Upstream (dependencies)".to_string()];
        for dep_id in &task.after {
            if let Some(dep) = graph.get_task(dep_id) {
                let desc_preview = dep
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect::<String>();
                let mut entry = format!(
                    "<neighbor-context source=\"{}\">\n- **{}** [{}]: {} \u{2014} {}",
                    dep.id, dep.id, dep.status, dep.title, desc_preview
                );
                // Inline artifact content for neighbors
                let artifact_content = inline_artifact_content(&dep.artifacts, workgraph_dir);
                if !artifact_content.is_empty() {
                    entry.push_str(&format!("\n  Artifacts:\n{}", artifact_content));
                }
                entry.push_str("\n</neighbor-context>");
                lines.push(entry);
            }
        }
        parts.push(lines.join("\n"));
    }

    // Downstream tasks (tasks that depend on this one) — XML fenced
    let task_id = &task.id;
    let downstream: Vec<_> = graph
        .tasks()
        .filter(|t| t.after.contains(task_id))
        .collect();
    if !downstream.is_empty() {
        let mut lines = vec!["### Downstream (dependents)".to_string()];
        for dt in &downstream {
            let desc_preview = dt
                .description
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>();
            let mut entry = format!(
                "<neighbor-context source=\"{}\">\n- **{}** [{}]: {} \u{2014} {}",
                dt.id, dt.id, dt.status, dt.title, desc_preview
            );
            let artifact_content = inline_artifact_content(&dt.artifacts, workgraph_dir);
            if !artifact_content.is_empty() {
                entry.push_str(&format!("\n  Artifacts:\n{}", artifact_content));
            }
            entry.push_str("\n</neighbor-context>");
            lines.push(entry);
        }
        parts.push(lines.join("\n"));
    }

    // Siblings (tasks sharing the same upstream dependencies)
    if !task.after.is_empty() {
        let siblings: Vec<_> = graph
            .tasks()
            .filter(|t| {
                t.id != task.id
                    && !t.after.is_empty()
                    && t.after.iter().any(|dep| task.after.contains(dep))
            })
            .collect();
        if !siblings.is_empty() {
            let mut lines = vec!["### Siblings (share upstream dependencies)".to_string()];
            for sib in siblings.iter().take(10) {
                lines.push(format!("- **{}** [{}]: {}", sib.id, sib.status, sib.title));
            }
            if siblings.len() > 10 {
                lines.push(format!("- ... and {} more", siblings.len() - 10));
            }
            parts.push(lines.join("\n"));
        }
    }

    let summary = parts.join("\n\n");
    // Hard cap at 4000 chars
    if summary.len() > 4000 {
        let end = summary.floor_char_boundary(3950);
        let mut truncated = summary[..end].to_string();
        truncated.push_str("\n\n... (graph summary truncated)");
        truncated
    } else {
        summary
    }
}

/// Build a full graph summary for full scope.
///
/// Lists all tasks with statuses and dependency edges, with 4000-char budget.
pub(crate) fn build_full_graph_summary(graph: &workgraph::WorkGraph) -> String {
    let mut parts = vec!["## Full Graph Summary\n".to_string()];
    let mut budget = 4000i32;
    let total = graph.tasks().count();

    for (task_count, t) in graph.tasks().enumerate() {
        let deps = if t.after.is_empty() {
            String::new()
        } else {
            format!(" (after: {})", t.after.join(", "))
        };
        let line = format!("- **{}** [{}]: {}{}\n", t.id, t.status, t.title, deps);
        budget -= line.len() as i32;
        if budget < 0 {
            let remaining = total - task_count;
            parts.push(format!("... and {} more tasks", remaining));
            break;
        }
        parts.push(line);
    }

    parts.join("")
}

/// Read CLAUDE.md content from the project root (parent of .workgraph/).
fn read_claude_md(workgraph_dir: &Path) -> String {
    let project_root = workgraph_dir
        .canonicalize()
        .ok()
        .and_then(|abs| abs.parent().map(std::path::Path::to_path_buf));

    let project_root = match project_root {
        Some(r) => r,
        None => return String::new(),
    };

    let claude_md_path = project_root.join("CLAUDE.md");
    std::fs::read_to_string(&claude_md_path).unwrap_or_default()
}

/// Read the workgraph usage guide for non-Claude models.
///
/// Checks for a user-customizable guide at `.workgraph/wg-guide.md`. If that file
/// exists, its content is used. Otherwise falls back to the built-in default guide
/// embedded in the binary.
#[allow(dead_code)]
pub(crate) fn read_wg_guide(workgraph_dir: &Path) -> String {
    let custom_path = workgraph_dir.join("wg-guide.md");
    if custom_path.exists()
        && let Ok(content) = std::fs::read_to_string(&custom_path)
        && !content.trim().is_empty()
    {
        return content;
    }
    workgraph::service::executor::DEFAULT_WG_GUIDE.to_string()
}

/// Classify model into knowledge tier based on context window and capabilities
pub(crate) fn classify_model_tier(model: &str) -> KnowledgeTier {
    let model_lower = model.to_lowercase();

    // Tier 1: Essential (8KB) - 32K context window models
    if model_lower.contains("minimax")
        || model_lower.contains("qwen-2.5")
        || model_lower.contains("qwen2.5")
    {
        KnowledgeTier::Essential
    }
    // Tier 2: Core (16KB) - 64K context window models
    else if model_lower.contains("deepseek") || model_lower.contains("claude-haiku") {
        KnowledgeTier::Core
    }
    // Tier 3: Full (40KB) - 128K+ context window models
    else if model_lower.contains("llama-3.1")
        || model_lower.contains("llama3.1")
        || model_lower.contains("claude-sonnet")
        || model_lower.contains("claude-opus")
    {
        KnowledgeTier::Full
    }
    // Conservative default for unknown models
    else {
        KnowledgeTier::Essential
    }
}

/// Check if Telegram escalation is configured and available.
///
/// Looks for a valid Telegram configuration in either the project-local
/// `.workgraph/notify.toml` or global `~/.config/workgraph/notify.toml`.
/// Returns true if Telegram bot token and chat ID are configured.
fn is_telegram_configured(workgraph_dir: &Path) -> bool {
    // Try project-local config first
    let project_config_path = workgraph_dir.join("notify.toml");
    if let Ok(config) = NotifyConfig::load_from(&project_config_path)
        && TelegramConfig::from_notify_config(&config).is_ok()
    {
        return true;
    }

    // Try global config
    if let Some(global_config_path) = dirs::config_dir() {
        let global_config_path = global_config_path.join("workgraph").join("notify.toml");
        if let Ok(config) = NotifyConfig::load_from(&global_config_path)
            && TelegramConfig::from_notify_config(&config).is_ok()
        {
            return true;
        }
    }

    false
}

/// Build tiered workgraph knowledge guide based on model capabilities
pub(crate) fn build_tiered_guide(
    workgraph_dir: &Path,
    tier: KnowledgeTier,
    _model: &str,
) -> String {
    // Check for custom override first
    let custom_path = workgraph_dir.join("wg-guide.md");
    if custom_path.exists()
        && let Ok(content) = std::fs::read_to_string(&custom_path)
        && !content.trim().is_empty()
    {
        return content;
    }

    match tier {
        KnowledgeTier::Essential => build_essential_guide(workgraph_dir),
        KnowledgeTier::Core => build_core_guide(workgraph_dir),
        KnowledgeTier::Full => build_full_guide(workgraph_dir),
    }
}

/// Build essential guide (8KB) for Tier 1 models like Minimax M2.7
fn build_essential_guide(workgraph_dir: &Path) -> String {
    let claude_md = read_claude_md_content(workgraph_dir);
    let memory_md = read_memory_md(workgraph_dir);

    format!(
        r#"# Workgraph Agent Guide (Essential)

**You are an AI agent working on one task in a workgraph project.** Other agents work on other tasks concurrently.

## CRITICAL: Attempt Work Before Failing or Decomposing

**Core Principle:** Always attempt the work before concluding it cannot be done. Difficulty is not impossibility.

**The Graph is Alive.** You are one node in a living system. Your job is not just to complete your task, but to grow the graph where it needs growing:

- **Task too large?** → Fan out independent parts as parallel subtasks
- **Prerequisite missing?** → `wg add "Prereq: ..." && wg add "$WG_TASK_ID" --after prereq-id`
- **Follow-up needed?** → `wg add "Verify: ..." --after $WG_TASK_ID`
- **Found a bug/missing doc?** → `wg add "Fix: ..." --after $WG_TASK_ID`

**Anti-pattern — Explain-and-Bail:** DO NOT read a task, write a long explanation of why it's hard, and then fail. Attempt the work first. A failed attempt with partial progress is more valuable than an explanation of why you didn't try.

## Decision Framework: When to Decompose vs Implement

Fanout is a tool, not a default. Assess complexity first, then decide.

### Stay inline (default) when:
- Task is straightforward, even if it touches multiple files sequentially
- Each step depends on the previous (sequential work doesn't parallelize)
- Simple fixes, config changes, small features
- Task seems hard but is single-scope — difficulty alone is NOT a reason to decompose

### Fan out when:
- 3+ independent files/components need changes that can genuinely run in parallel
- You hit context pressure (re-reading files, losing track of changes)
- Natural parallelism exists (e.g., 3 separate test files, N independent modules)
- Discovered bugs or missing prereqs outside your scope

### If you decompose:
- Each subtask must list its file scope — NO two subtasks may modify the same file
- Include "Implement directly — do not decompose further" in subtask descriptions
- Always include an integrator task at join points

## Decomposition Pattern Templates

### Pipeline (Sequential Steps)
When work must proceed in order:
```bash
wg add 'Step 1: Parse input' --after $WG_TASK_ID
wg add 'Step 2: Transform data' --after step-1-parse-input
wg add 'Step 3: Write output' --after step-2-transform-data
```

### Fan-Out-Merge (Parallel + Integration)
When work has independent parts that converge:
```bash
wg add 'Part A: Module X' --after $WG_TASK_ID
wg add 'Part B: Module Y' --after $WG_TASK_ID
wg add 'Part C: Module Z' --after $WG_TASK_ID
wg add 'Integrate modules' --after part-a-module-x,part-b-module-y,part-c-module-z
```

**CRITICAL:** Always include an integrator task at join points. Never leave parallel work unmerged.

### Iterate-Until-Pass (Refinement Loop)
When work requires multiple passes:
```bash
wg add 'Refine implementation' --after $WG_TASK_ID --max-iterations 3
```
Use `wg done --converged` when work has stabilized.

## Task Description Requirements

Every **code task** description MUST include:

```markdown
## Validation
- [ ] Failing test written first: test_feature_x_<scenario>
- [ ] Implementation makes test pass
- [ ] cargo build + cargo test pass with no regressions
- [ ] <any additional acceptance criteria>
```

Verification is handled by the FLIP/eval pipeline.

## Core Commands

| Command | Purpose |
|---------|---------|
| `wg add "title" -d "desc"` | Create a new task |
| `wg add "title" --after task-id` | Create task with dependency |
| `wg show <id>` | View task details, status, deps, logs |
| `wg log <id> "msg"` | Log progress (recoverable breadcrumbs) |
| `wg done <id>` | Mark your task complete |
| `wg fail <id> --reason "..."` | Mark failed (only after genuine attempt) |
| `wg list` | List all tasks |
| `wg ready` | List tasks ready to be worked on |

## Dependencies with `--after`

Use `--after` to express that one task depends on another. This is CRITICAL for correct execution order.

```bash
# Task B depends on Task A completing first
wg add "Task B" --after task-a

# Task C depends on multiple predecessors
wg add "Task C" --after task-a,task-b

# Subtask that depends on current task
wg add "Subtask" --after $WG_TASK_ID
```

**Always use `--after` when creating subtasks.** Without it, tasks form a flat unordered list.

## Environment Variables
- `$WG_TASK_ID` — the task you are working on
- `$WG_AGENT_ID` — your unique agent identifier
- `$WG_EXECUTOR_TYPE` — executor type (native, claude, etc.)
- `$WG_MODEL` — the model you are running as

{}

{}"#,
        extract_project_instructions(&claude_md),
        extract_project_context(&memory_md)
    )
}

/// Build core guide (16KB) for Tier 2 models like DeepSeek V3
fn build_core_guide(workgraph_dir: &Path) -> String {
    // For now, build on essential guide with additional content
    let essential = build_essential_guide(workgraph_dir);

    format!(
        "{}\n\n{}\n\n{}",
        essential,
        build_agent_communication_section(),
        build_graph_patterns_section()
    )
}

/// Build full guide (40KB) for Tier 3 models like Llama 3.1+
fn build_full_guide(workgraph_dir: &Path) -> String {
    // For now, build on core guide with additional content
    let core = build_core_guide(workgraph_dir);

    format!(
        "{}\n\n{}\n\n{}",
        core,
        build_agency_system_section(),
        build_advanced_patterns_section()
    )
}

/// Read CLAUDE.md content from the project root
fn read_claude_md_content(workgraph_dir: &Path) -> String {
    let project_root = workgraph_dir.parent().unwrap_or(workgraph_dir);
    let claude_md_path = project_root.join("CLAUDE.md");
    std::fs::read_to_string(&claude_md_path).unwrap_or_default()
}

/// Read memory context from user's memory directory
fn read_memory_md(workgraph_dir: &Path) -> String {
    // Try to find the memory directory - it could be in ~/.claude/ or similar
    let project_root = workgraph_dir.parent().unwrap_or(workgraph_dir);
    if let Some(home_dir) = dirs::home_dir() {
        let memory_path = home_dir
            .join(".claude")
            .join("projects")
            .join("-home-erik-workgraph")
            .join("memory")
            .join("MEMORY.md");

        if let Ok(content) = std::fs::read_to_string(&memory_path) {
            return content;
        }
    }

    // Fallback - try relative to workgraph dir
    let memory_path = project_root
        .join(".claude")
        .join("memory")
        .join("MEMORY.md");
    std::fs::read_to_string(&memory_path).unwrap_or_default()
}

/// Extract critical project instructions from CLAUDE.md
fn extract_project_instructions(claude_md: &str) -> String {
    if claude_md.trim().is_empty() {
        return String::new();
    }

    // Look for orchestrator role and critical patterns
    let mut instructions = String::new();

    if claude_md.contains("orchestrating agent") || claude_md.contains("Orchestrating agent") {
        instructions.push_str("\n## Project Role (from CLAUDE.md)\n");
        instructions.push_str("**You are a distributed agent** in a workgraph system. Other agents handle other tasks.\n");
        instructions.push_str("**CRITICAL:** Use `wg add` for task creation. Do NOT attempt monolithic implementations.\n");
    }

    if claude_md.contains("CRITICAL") {
        instructions.push_str("\n**Project-specific critical constraints apply** - see task description for details.\n");
    }

    instructions
}

/// Extract project context from memory
fn extract_project_context(memory_md: &str) -> String {
    if memory_md.trim().is_empty() {
        return String::new();
    }

    let mut context = String::new();
    context.push_str("\n## Project Context\n");

    // Extract key project facts - limit to essential info for Tier 1
    if memory_md.contains("Workgraph") {
        context.push_str(
            "**Project:** Workgraph - task coordination graph for humans and AI agents\n",
        );
    }

    if memory_md.contains("Rust") {
        context
            .push_str("**Language:** Rust (use `cargo build` and `cargo test` for validation)\n");
    }

    if memory_md.contains("graph.jsonl") {
        context.push_str(
            "**Core files:** `.workgraph/graph.jsonl` (task storage), `src/` (implementation)\n",
        );
    }

    context
}

/// Build agent communication section for Tier 2+
fn build_agent_communication_section() -> String {
    r#"## Agent Communication

### Messages
Check for messages from other agents:
```bash
wg msg read $WG_TASK_ID --agent $WG_AGENT_ID
wg msg send $WG_TASK_ID "Acknowledged - implementing your suggestion"
```

### Coordination Patterns
- **Sequential handoff:** Use `--after` to pass work from one agent to another
- **Parallel collaboration:** Multiple agents work on parts, integrator combines results
- **Iterative refinement:** Use cycles with `--max-iterations` for review/improve loops"#
        .to_string()
}

/// Build graph patterns section for Tier 2+
fn build_graph_patterns_section() -> String {
    r#"## Advanced Graph Patterns

### Cycles and Loops
Workgraph supports cycles for recurring work:
```bash
wg add 'Review code' --after implement-feature --max-iterations 3
wg add 'Fix issues' --after review-code
wg add 'Final check' --after fix-issues
# Creates a review→fix→check cycle that can repeat
```

### Conditional Dependencies
Use status-based dependencies:
```bash
wg add 'Deploy to staging' --after tests-pass
wg add 'Deploy to prod' --after deploy-to-staging
```

### Complex Fan-Out
For multi-dimensional work:
```bash
# By component
wg add 'Frontend tests' --after $WG_TASK_ID
wg add 'Backend tests' --after $WG_TASK_ID
wg add 'Integration tests' --after $WG_TASK_ID

# By environment
wg add 'Test on Linux' --after $WG_TASK_ID
wg add 'Test on MacOS' --after $WG_TASK_ID
wg add 'Test on Windows' --after $WG_TASK_ID

# Integration
wg add 'Merge results' --after frontend-tests,backend-tests,integration-tests,test-on-linux,test-on-macos,test-on-windows
```"#.to_string()
}

/// Build agency system section for Tier 3+
fn build_agency_system_section() -> String {
    r#"## Agency System (Advanced)

### Roles and Specialization
Tasks can be assigned to specialized agents:
```bash
wg agent create security-expert --role programmer --tradeoff careful
wg assign security-audit security-expert
```

### Evaluation and Feedback
Use FLIP scoring for quality assessment:
```bash
wg evaluate run $WG_TASK_ID --criteria "correctness,completeness,efficiency"
```

### Agent Evolution
The system learns from performance:
```bash
wg evolve run  # Analyzes task outcomes and improves agent assignments
```"#
        .to_string()
}

/// Build advanced patterns section for Tier 3+
fn build_advanced_patterns_section() -> String {
    r#"## Advanced Coordination Patterns

### Federation and Sharing
Share successful patterns across projects:
```bash
wg func save "testing-pipeline" --pattern "test→review→merge"
wg func apply "testing-pipeline" --input target=new-feature
```

### Complex Lifecycle Management
Handle long-running processes:
```bash
wg add 'Monitor deployment' --after deploy --max-iterations 24 \
  --cycle-delay 3600  # Check hourly
```

### Error Recovery Patterns
Build resilient workflows:
```bash
wg add 'Backup strategy' --after main-task
wg add 'Fallback implementation' --after main-task
wg add 'Choose best result' --after backup-strategy,fallback-implementation
```"#
        .to_string()
}

/// Resolve the effective exec_mode for a task using the priority hierarchy:
/// task.exec_mode > role.default_exec_mode > "full".
pub(crate) fn resolve_task_exec_mode(
    task: &workgraph::graph::Task,
    workgraph_dir: &Path,
) -> String {
    if let Some(ref mode) = task.exec_mode {
        return mode.clone();
    }

    // Check role's default_exec_mode if task has an agent
    if let Some(ref agent_hash) = task.agent {
        let agency_dir = workgraph_dir.join("agency");
        let agents_dir = agency_dir.join("cache/agents");
        let roles_dir = agency_dir.join("cache/roles");
        if let Ok(agent) = workgraph::agency::find_agent_by_prefix(&agents_dir, agent_hash)
            && let Ok(role) = workgraph::agency::find_role_by_prefix(&roles_dir, &agent.role_id)
            && let Some(mode) = role.default_exec_mode
        {
            return mode;
        }
    }

    "full".to_string()
}

/// Resolve the context scope for a task using the priority hierarchy:
/// task > role > coordinator config > default ("task").
pub(crate) fn resolve_task_scope(
    task: &workgraph::graph::Task,
    config: &Config,
    workgraph_dir: &Path,
) -> ContextScope {
    // Get role's default_context_scope if task has an agent
    let role_scope = task.agent.as_ref().and_then(|agent_hash| {
        let agency_dir = workgraph_dir.join("agency");
        let agents_dir = agency_dir.join("cache/agents");
        let roles_dir = agency_dir.join("cache/roles");
        let agent = workgraph::agency::find_agent_by_prefix(&agents_dir, agent_hash).ok()?;
        let role = workgraph::agency::find_role_by_prefix(&roles_dir, &agent.role_id).ok()?;
        role.default_context_scope
    });

    workgraph::context_scope::resolve_context_scope(
        task.context_scope.as_deref(),
        role_scope.as_deref(),
        config.coordinator.default_context_scope.as_deref(),
    )
}

/// Build previous attempt context for retry injection.
///
/// When a task has `retry_count > 0`, looks for the most recent archived agent
/// attempt and extracts context in priority order:
/// 1. Checkpoint summary (auto or explicit)
/// 2. Truncated output.log tail
/// 3. Task log entries
///
/// Returns empty string if no previous attempt context is found or retry_count is 0.
pub(crate) fn build_previous_attempt_context(
    task: &workgraph::graph::Task,
    workgraph_dir: &Path,
    max_tokens: u32,
) -> String {
    if task.retry_count == 0 || max_tokens == 0 {
        return String::new();
    }

    // Find the most recent archived agent for this task
    let archive_base = workgraph_dir.join("log").join("agents").join(&task.id);

    if !archive_base.exists() {
        return String::new();
    }

    // Get the most recent archive directory (sorted by timestamp)
    let mut archives: Vec<_> = match fs::read_dir(&archive_base) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect(),
        Err(_) => return String::new(),
    };

    if archives.is_empty() {
        return String::new();
    }

    // Sort by directory name (ISO timestamps sort lexicographically)
    archives.sort_by_key(|e| e.file_name());
    let latest_archive = archives.last().unwrap().path();
    let archive_timestamp = archives
        .last()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .to_string();

    // Estimate max bytes (~4 chars per token as rough heuristic)
    let max_bytes = (max_tokens as usize) * 4;

    // Priority 1: Look for checkpoint summary from the previous agent
    let checkpoint_context = find_checkpoint_for_task(task, workgraph_dir);
    if let Some(summary) = checkpoint_context
        && !summary.is_empty()
    {
        return format_previous_context(&archive_timestamp, &summary, max_bytes);
    }

    // Priority 2: Truncated output.log from the archive
    let output_path = latest_archive.join("output.txt");
    if output_path.exists()
        && let Ok(content) = fs::read_to_string(&output_path)
        && !content.trim().is_empty()
    {
        let tail = truncate_to_tail(&content, max_bytes);
        return format_previous_context(&archive_timestamp, &tail, max_bytes);
    }

    // Priority 3: Task log entries
    if !task.log.is_empty() {
        let log_context = task
            .log
            .iter()
            .map(|entry| {
                format!(
                    "[{}] {}: {}",
                    entry.timestamp,
                    entry.actor.as_deref().unwrap_or("system"),
                    entry.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        if !log_context.is_empty() {
            let truncated = truncate_to_tail(&log_context, max_bytes);
            return format_previous_context(&archive_timestamp, &truncated, max_bytes);
        }
    }

    String::new()
}

/// Find the most recent checkpoint for a task from any previously assigned agent.
fn find_checkpoint_for_task(task: &workgraph::graph::Task, workgraph_dir: &Path) -> Option<String> {
    let mut prev_agents: Vec<String> = Vec::new();
    for entry in &task.log {
        if let Some(ref actor) = entry.actor
            && actor.starts_with("agent-")
            && !prev_agents.contains(actor)
        {
            prev_agents.push(actor.clone());
        }
    }

    for agent_id in prev_agents.iter().rev() {
        if let Ok(Some(checkpoint)) =
            crate::commands::checkpoint::load_latest(workgraph_dir, agent_id)
        {
            return Some(format!(
                "Checkpoint ({:?}, agent {}): {}",
                checkpoint.checkpoint_type, checkpoint.agent_id, checkpoint.summary
            ));
        }
    }

    None
}

/// Truncate a string to its last `max_bytes` bytes, preserving valid UTF-8 boundaries.
fn truncate_to_tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    let start = s.len() - max_bytes;
    let start = s.ceil_char_boundary(start);
    format!("... (truncated)\n{}", &s[start..])
}

/// Format the previous attempt context section for injection into the prompt.
fn format_previous_context(timestamp: &str, content: &str, max_bytes: usize) -> String {
    let truncated_content = if content.len() > max_bytes {
        truncate_to_tail(content, max_bytes)
    } else {
        content.to_string()
    };

    format!(
        "## Previous Attempt Context\n\
         This task was previously attempted (archived at {}).\n\
         Here is context from that attempt:\n\n\
         {}\n\n\
         Continue from where they left off. Do not repeat work already done.",
        timestamp, truncated_content
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn test_build_task_context() {
        let mut graph = WorkGraph::new();

        // Create a dependency task with artifacts and logs
        let mut dep_task = make_task("dep-1", "Dependency");
        dep_task.status = Status::Done;
        dep_task.artifacts = vec!["output.txt".to_string(), "data.json".to_string()];
        dep_task.log = vec![
            LogEntry {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                user: Some(workgraph::current_user()),
                message: "Started work".to_string(),
            },
            LogEntry {
                timestamp: "2026-01-01T00:01:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                user: Some(workgraph::current_user()),
                message: "Found important result".to_string(),
            },
            LogEntry {
                timestamp: "2026-01-01T00:02:00Z".to_string(),
                actor: Some("agent-1".to_string()),
                user: Some(workgraph::current_user()),
                message: "Completed successfully".to_string(),
            },
        ];
        graph.add_node(Node::Task(dep_task));

        // Create main task blocked by dependency
        let mut main_task = make_task("main", "Main Task");
        main_task.after = vec!["dep-1".to_string()];
        graph.add_node(Node::Task(main_task.clone()));

        let context = build_task_context(&graph, &main_task);
        assert!(context.contains("dep-1"));
        // R5: Upstream title included
        assert!(context.contains("(Dependency)"));
        assert!(context.contains("output.txt"));
        assert!(context.contains("data.json"));
        // Verify log entries are included
        assert!(context.contains("From dep-1 logs:"));
        assert!(context.contains("Started work"));
        assert!(context.contains("Found important result"));
        assert!(context.contains("Completed successfully"));
    }

    #[test]
    fn test_build_task_context_no_deps() {
        let graph = WorkGraph::new();
        let task = make_task("t1", "Test Task");

        let context = build_task_context(&graph, &task);
        assert_eq!(context, "No context from dependencies");
        assert!(!context.contains("logs:"));
    }

    #[test]
    fn test_build_task_context_no_loop_metadata_for_normal_tasks() {
        let graph = WorkGraph::new();
        let task = make_task("t1", "Normal Task");
        let context = build_task_context(&graph, &task);
        assert!(!context.contains("Loop status"));
    }

    #[test]
    fn test_build_graph_summary_includes_status_counts() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Done task");
        t1.status = Status::Done;
        graph.add_node(Node::Task(t1));

        let mut t2 = make_task("t2", "Open task");
        t2.status = Status::Open;
        graph.add_node(Node::Task(t2));

        let mut t3 = make_task("t3", "In progress");
        t3.status = Status::InProgress;
        graph.add_node(Node::Task(t3));

        let main = make_task("main", "Main task");
        graph.add_node(Node::Task(main.clone()));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(
            summary.contains("## Graph Status"),
            "Should have status header"
        );
        assert!(summary.contains("4 tasks"), "Should count all tasks");
        assert!(summary.contains("1 done"), "Should count done tasks");
        assert!(
            summary.contains("1 in-progress"),
            "Should count in-progress tasks"
        );
        assert!(
            summary.contains("2 open"),
            "Should count open tasks (main + t2)"
        );
    }

    #[test]
    fn test_build_graph_summary_includes_upstream_and_downstream() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let mut upstream = make_task("upstream", "Upstream task");
        upstream.status = Status::Done;
        upstream.description = Some("Does upstream work".to_string());
        graph.add_node(Node::Task(upstream));

        let mut main = make_task("main", "Main task");
        main.after = vec!["upstream".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let mut downstream = make_task("downstream", "Downstream task");
        downstream.after = vec!["main".to_string()];
        downstream.description = Some("Consumes main output".to_string());
        graph.add_node(Node::Task(downstream));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(
            summary.contains("### Upstream"),
            "Should have upstream section"
        );
        assert!(summary.contains("upstream"), "Should list upstream task");
        assert!(
            summary.contains("### Downstream"),
            "Should have downstream section"
        );
        assert!(
            summary.contains("downstream"),
            "Should list downstream task"
        );
        assert!(
            summary.contains("Consumes main output"),
            "Should include description preview"
        );
    }

    #[test]
    fn test_build_graph_summary_includes_siblings() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let parent = make_task("parent", "Parent task");
        graph.add_node(Node::Task(parent));

        let mut main = make_task("main", "Main task");
        main.after = vec!["parent".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let mut sibling = make_task("sibling", "Sibling task");
        sibling.after = vec!["parent".to_string()];
        graph.add_node(Node::Task(sibling));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(
            summary.contains("### Siblings"),
            "Should have siblings section"
        );
        assert!(summary.contains("sibling"), "Should list sibling task");
    }

    #[test]
    fn test_build_graph_summary_xml_fencing() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        let upstream = make_task("dep", "Dependency");
        graph.add_node(Node::Task(upstream));

        let mut main = make_task("main", "Main task");
        main.after = vec!["dep".to_string()];
        graph.add_node(Node::Task(main.clone()));

        let summary = build_graph_summary(&graph, &main, wg_dir);
        assert!(
            summary.contains("<neighbor-context source=\"dep\">"),
            "Upstream should be XML fenced"
        );
        assert!(
            summary.contains("</neighbor-context>"),
            "Should close XML fence"
        );
    }

    #[test]
    fn test_build_graph_summary_truncates_at_4000_chars() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();

        // Create many tasks to exceed 4000 chars
        for i in 0..200 {
            let mut t = make_task(
                &format!("task-{:03}", i),
                &format!(
                    "A task with a long title to inflate the summary for task number {}",
                    i
                ),
            );
            t.description = Some(format!(
                "Description for task {} with extra words to pad length",
                i
            ));
            if i > 0 {
                t.after = vec!["task-000".to_string()];
            }
            graph.add_node(Node::Task(t));
        }

        let main_task = graph.get_task("task-000").unwrap().clone();
        let summary = build_graph_summary(&graph, &main_task, wg_dir);
        assert!(
            summary.len() <= 4100,
            "Summary should be capped near 4000 chars, got {}",
            summary.len()
        );
        if summary.len() > 3950 {
            assert!(summary.contains("truncated"), "Should indicate truncation");
        }
    }

    #[test]
    fn test_build_full_graph_summary_lists_tasks() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "First task");
        t1.status = Status::Done;
        graph.add_node(Node::Task(t1));

        let mut t2 = make_task("t2", "Second task");
        t2.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(t2));

        let summary = build_full_graph_summary(&graph);
        assert!(
            summary.contains("## Full Graph Summary"),
            "Should have header"
        );
        assert!(summary.contains("t1"), "Should list first task");
        assert!(summary.contains("[done]"), "Should show status");
        assert!(summary.contains("t2"), "Should list second task");
        assert!(summary.contains("(after: t1)"), "Should show dependencies");
    }

    #[test]
    fn test_build_full_graph_summary_truncates_at_budget() {
        let mut graph = WorkGraph::new();
        // Create enough tasks to exceed the 4000-char budget
        for i in 0..200 {
            let t = make_task(
                &format!("task-with-long-id-{:04}", i),
                &format!("A task with a somewhat long title for padding number {}", i),
            );
            graph.add_node(Node::Task(t));
        }

        let summary = build_full_graph_summary(&graph);
        assert!(
            summary.len() <= 4200,
            "Should be bounded by budget, got {}",
            summary.len()
        );
        assert!(summary.contains("more tasks"), "Should indicate truncation");
    }

    #[test]
    fn test_build_scope_context_clean_scope_empty() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let graph = WorkGraph::new();
        let task = make_task("t1", "Test task");
        let config = Config::default();

        let ctx = build_scope_context(&graph, &task, ContextScope::Clean, &config, wg_dir);
        assert!(
            ctx.downstream_info.is_empty(),
            "Clean scope should have no downstream info"
        );
        assert!(
            ctx.tags_skills_info.is_empty(),
            "Clean scope should have no tags info"
        );
        assert!(
            ctx.project_description.is_empty(),
            "Clean scope should have no project description"
        );
        assert!(
            ctx.graph_summary.is_empty(),
            "Clean scope should have no graph summary"
        );
        assert!(
            ctx.full_graph_summary.is_empty(),
            "Clean scope should have no full graph summary"
        );
        assert!(
            ctx.claude_md_content.is_empty(),
            "Clean scope should have no CLAUDE.md content"
        );
    }

    #[test]
    fn test_build_scope_context_task_scope_includes_downstream() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Main task");
        graph.add_node(Node::Task(task.clone()));

        let mut downstream = make_task("d1", "Dependent task");
        downstream.after = vec!["t1".to_string()];
        graph.add_node(Node::Task(downstream));

        let config = Config::default();
        let ctx = build_scope_context(&graph, &task, ContextScope::Task, &config, wg_dir);
        assert!(
            ctx.downstream_info.contains("d1"),
            "Task scope should include downstream"
        );
        assert!(
            ctx.downstream_info.contains("Dependent task"),
            "Should include downstream title"
        );
        // Should NOT include graph-level stuff
        assert!(
            ctx.graph_summary.is_empty(),
            "Task scope should not have graph summary"
        );
    }

    #[test]
    fn test_build_scope_context_task_scope_includes_tags_skills() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let graph = WorkGraph::new();
        let mut task = make_task("t1", "Tagged task");
        task.tags = vec!["rust".to_string(), "backend".to_string()];
        task.skills = vec!["implementation".to_string()];

        let config = Config::default();
        let ctx = build_scope_context(&graph, &task, ContextScope::Task, &config, wg_dir);
        assert!(ctx.tags_skills_info.contains("rust"), "Should include tags");
        assert!(
            ctx.tags_skills_info.contains("implementation"),
            "Should include skills"
        );
    }

    #[test]
    fn test_build_scope_context_graph_scope_includes_summary() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Graph task");
        graph.add_node(Node::Task(task.clone()));

        let mut config = Config::default();
        config.project.description = Some("A test project".to_string());

        let ctx = build_scope_context(&graph, &task, ContextScope::Graph, &config, wg_dir);
        assert!(
            ctx.project_description.contains("A test project"),
            "Graph scope should include project description"
        );
        assert!(
            !ctx.graph_summary.is_empty(),
            "Graph scope should have graph summary"
        );
        // Should NOT include full-scope stuff
        assert!(
            ctx.full_graph_summary.is_empty(),
            "Graph scope should not have full graph summary"
        );
        assert!(
            ctx.claude_md_content.is_empty(),
            "Graph scope should not have CLAUDE.md"
        );
    }

    #[test]
    fn test_build_scope_context_full_scope_includes_everything() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Full task");
        graph.add_node(Node::Task(task.clone()));

        let mut config = Config::default();
        config.project.description = Some("Test project".to_string());

        let ctx = build_scope_context(&graph, &task, ContextScope::Full, &config, wg_dir);
        assert!(
            !ctx.graph_summary.is_empty(),
            "Full scope should have graph summary"
        );
        assert!(
            !ctx.full_graph_summary.is_empty(),
            "Full scope should have full graph summary"
        );
        assert!(
            ctx.full_graph_summary.contains("Full Graph Summary"),
            "Should include full graph summary header"
        );
    }

    #[test]
    fn test_resolve_task_scope_defaults_to_task() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let task = make_task("t1", "Test");
        let config = Config::default();
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(scope, ContextScope::Task, "Default scope should be Task");
    }

    #[test]
    fn test_resolve_task_scope_task_overrides() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut task = make_task("t1", "Test");
        task.context_scope = Some("clean".to_string());
        let mut config = Config::default();
        config.coordinator.default_context_scope = Some("full".to_string());
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(
            scope,
            ContextScope::Clean,
            "Task scope should override config"
        );
    }

    #[test]
    fn test_resolve_task_scope_config_fallback() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let task = make_task("t1", "Test");
        let mut config = Config::default();
        config.coordinator.default_context_scope = Some("graph".to_string());
        let scope = resolve_task_scope(&task, &config, wg_dir);
        assert_eq!(
            scope,
            ContextScope::Graph,
            "Config scope should be used as fallback"
        );
    }

    // =========================================================================
    // Previous attempt context tests
    // =========================================================================

    #[test]
    fn test_build_previous_attempt_context_zero_retry_count() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let task = make_task("t1", "Test task");
        // retry_count is 0 by default
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(result.is_empty(), "Should return empty for retry_count 0");
    }

    #[test]
    fn test_build_previous_attempt_context_disabled_by_zero_tokens() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        let result = build_previous_attempt_context(&task, wg_dir, 0);
        assert!(
            result.is_empty(),
            "Should return empty when max_tokens is 0"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_no_archive() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.is_empty(),
            "Should return empty when no archive exists"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_with_archive_output() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create an archive with output
        let archive_dir = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(
            archive_dir.join("output.txt"),
            "Agent started working on task t1\nCompleted analysis of requirements\nFound 3 issues",
        )
        .unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.contains("Previous Attempt Context"),
            "Should contain header"
        );
        assert!(
            result.contains("2026-03-07T10:00:00Z"),
            "Should contain archive timestamp"
        );
        assert!(
            result.contains("Found 3 issues"),
            "Should contain output content"
        );
        assert!(
            result.contains("Continue from where they left off"),
            "Should contain continuation instruction"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_empty_output_skipped() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create an archive with empty output
        let archive_dir = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(archive_dir.join("output.txt"), "   \n\n  ").unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        // No checkpoint, empty output, no logs => empty result
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.is_empty(),
            "Should return empty for whitespace-only output"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_uses_most_recent_archive() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create two archives (older and newer)
        let old_archive = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-06T10:00:00Z");
        std::fs::create_dir_all(&old_archive).unwrap();
        std::fs::write(old_archive.join("output.txt"), "Old agent output").unwrap();

        let new_archive = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&new_archive).unwrap();
        std::fs::write(new_archive.join("output.txt"), "New agent output").unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 2;
        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.contains("New agent output"),
            "Should use most recent archive"
        );
        assert!(
            !result.contains("Old agent output"),
            "Should not use old archive"
        );
    }

    #[test]
    fn test_build_previous_attempt_context_with_checkpoint() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create an archive
        let archive_dir = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&archive_dir).unwrap();
        std::fs::write(archive_dir.join("output.txt"), "Some output").unwrap();

        // Create a checkpoint for the agent
        let cp_dir = wg_dir.join("agents").join("agent-99").join("checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        let checkpoint = serde_json::json!({
            "task_id": "t1",
            "agent_id": "agent-99",
            "timestamp": "2026-03-07T10:30:00Z",
            "type": "auto",
            "summary": "Completed web search, found 5 relevant docs",
            "files_modified": [],
            "artifacts_registered": []
        });
        std::fs::write(
            cp_dir.join("2026-03-07T10-30-00.000Z.json"),
            serde_json::to_string(&checkpoint).unwrap(),
        )
        .unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        task.log = vec![LogEntry {
            timestamp: "2026-03-07T09:00:00Z".to_string(),
            actor: Some("agent-99".to_string()),
            user: Some(workgraph::current_user()),
            message: "Spawned by coordinator".to_string(),
        }];

        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.contains("Completed web search"),
            "Should use checkpoint summary. Got: {}",
            result
        );
    }

    #[test]
    fn test_build_previous_attempt_context_falls_back_to_logs() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        std::fs::create_dir_all(wg_dir).unwrap();

        // Create an archive directory but NO output.txt
        let archive_dir = wg_dir
            .join("log")
            .join("agents")
            .join("t1")
            .join("2026-03-07T10:00:00Z");
        std::fs::create_dir_all(&archive_dir).unwrap();

        let mut task = make_task("t1", "Test task");
        task.retry_count = 1;
        task.log = vec![
            LogEntry {
                timestamp: "2026-03-07T09:00:00Z".to_string(),
                actor: Some("agent-50".to_string()),
                user: Some(workgraph::current_user()),
                message: "Started research".to_string(),
            },
            LogEntry {
                timestamp: "2026-03-07T09:30:00Z".to_string(),
                actor: Some("agent-50".to_string()),
                user: Some(workgraph::current_user()),
                message: "Found key insight about X".to_string(),
            },
        ];

        let result = build_previous_attempt_context(&task, wg_dir, 2000);
        assert!(
            result.contains("Found key insight"),
            "Should fall back to task log entries. Got: {}",
            result
        );
    }

    #[test]
    fn test_truncate_to_tail() {
        let short = "Hello";
        assert_eq!(truncate_to_tail(short, 100), "Hello");

        let long = "A".repeat(1000);
        let truncated = truncate_to_tail(&long, 500);
        assert!(truncated.len() <= 520, "Should be roughly max_bytes");
        assert!(truncated.starts_with("... (truncated)"));
    }

    #[test]
    fn test_format_previous_context_structure() {
        let result = format_previous_context("2026-03-07T10:00:00Z", "Some work done", 8000);
        assert!(result.starts_with("## Previous Attempt Context"));
        assert!(result.contains("2026-03-07T10:00:00Z"));
        assert!(result.contains("Some work done"));
        assert!(result.contains("Continue from where they left off"));
    }

    #[test]
    fn test_build_task_context_includes_failed_dep_info() {
        let mut graph = WorkGraph::new();

        let mut dep_task = make_task("dep-a", "Build parser");
        dep_task.status = Status::Failed;
        dep_task.failure_reason = Some("cargo test test_parse_config failed".to_string());
        dep_task.log = vec![LogEntry {
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            actor: Some("agent-1".to_string()),
            user: None,
            message: "Parser fails on nested keys".to_string(),
        }];
        graph.add_node(Node::Task(dep_task));

        let mut task = make_task("task-b", "Use parser");
        task.after = vec!["dep-a".to_string()];
        graph.add_node(Node::Task(task.clone()));

        let context = build_task_context(&graph, &task);
        assert!(
            context.contains("(FAILED)"),
            "Context should mention FAILED status, got: {}",
            context
        );
        assert!(
            context.contains("cargo test test_parse_config failed"),
            "Context should include failure reason, got: {}",
            context
        );
        assert!(
            context.contains("Parser fails on nested keys"),
            "Context should include recent log, got: {}",
            context
        );
    }

    #[test]
    fn test_build_task_context_failed_dep_unknown_reason() {
        let mut graph = WorkGraph::new();

        let mut dep_task = make_task("dep-x", "Failing task");
        dep_task.status = Status::Failed;
        // No failure_reason set
        graph.add_node(Node::Task(dep_task));

        let mut task = make_task("task-y", "Downstream");
        task.after = vec!["dep-x".to_string()];
        graph.add_node(Node::Task(task.clone()));

        let context = build_task_context(&graph, &task);
        assert!(context.contains("(FAILED)"));
        assert!(context.contains("unknown"));
    }

    #[test]
    fn test_build_task_context_mixed_done_and_failed_deps() {
        let mut graph = WorkGraph::new();

        let mut done_dep = make_task("dep-ok", "Done task");
        done_dep.status = Status::Done;
        done_dep.artifacts = vec!["result.txt".to_string()];
        done_dep.log = vec![LogEntry {
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            actor: None,
            user: None,
            message: "Completed".to_string(),
        }];
        graph.add_node(Node::Task(done_dep));

        let mut failed_dep = make_task("dep-fail", "Failed task");
        failed_dep.status = Status::Failed;
        failed_dep.failure_reason = Some("OOM".to_string());
        graph.add_node(Node::Task(failed_dep));

        let mut task = make_task("task-z", "Multi-dep task");
        task.after = vec!["dep-ok".to_string(), "dep-fail".to_string()];
        graph.add_node(Node::Task(task.clone()));

        let context = build_task_context(&graph, &task);
        // Should have Done dep's artifacts
        assert!(context.contains("result.txt"));
        // Should have Done dep's log
        assert!(context.contains("Completed"));
        // Should have Failed dep's info
        assert!(context.contains("(FAILED)"));
        assert!(context.contains("OOM"));
    }

    #[test]
    fn test_glob_match_prefix() {
        assert!(glob_match("test_*.py", "test_foo.py"));
        assert!(glob_match("test_*.py", "test_outputs.py"));
        assert!(!glob_match("test_*.py", "foo_test.py"));
        assert!(!glob_match("test_*.py", "test_foo.rs"));
    }

    #[test]
    fn test_glob_match_suffix() {
        assert!(glob_match("*_test.py", "foo_test.py"));
        assert!(glob_match("*_test.go", "widget_test.go"));
        assert!(!glob_match("*_test.py", "test_foo.py"));
    }

    #[test]
    fn test_glob_match_middle() {
        assert!(glob_match("*.test.js", "app.test.js"));
        assert!(glob_match("*.test.ts", "widget.test.ts"));
        assert!(glob_match("*.spec.js", "app.spec.js"));
        assert!(!glob_match("*.test.js", "test_app.js"));
    }

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("Makefile", "Makefile"));
        assert!(!glob_match("Makefile", "makefile"));
    }

    #[test]
    fn test_discover_test_files_finds_python_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create test directory with test files
        let tests_dir = root.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        fs::write(tests_dir.join("test_outputs.py"), "# test").unwrap();
        fs::write(tests_dir.join("widget_test.py"), "# test").unwrap();
        fs::write(tests_dir.join("helper.py"), "# not a test").unwrap();

        let found = discover_test_files(root);
        assert!(found.contains(&"tests/test_outputs.py".to_string()));
        assert!(found.contains(&"tests/widget_test.py".to_string()));
        assert!(!found.iter().any(|f| f.contains("helper.py")));
    }

    #[test]
    fn test_discover_test_files_finds_rust_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let tests_dir = root.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        fs::write(tests_dir.join("test_integration.rs"), "// test").unwrap();

        let found = discover_test_files(root);
        assert!(found.contains(&"tests/test_integration.rs".to_string()));
    }

    #[test]
    fn test_discover_test_files_finds_js_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        fs::write(root.join("app.test.js"), "// test").unwrap();
        fs::write(root.join("widget.spec.ts"), "// test").unwrap();

        let found = discover_test_files(root);
        assert!(found.contains(&"app.test.js".to_string()));
        assert!(found.contains(&"widget.spec.ts".to_string()));
    }

    #[test]
    fn test_discover_test_files_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let found = discover_test_files(tmp.path());
        assert!(found.is_empty());
    }

    #[test]
    fn test_build_auto_verify_command_rust() {
        let files = vec!["tests/test_integration.rs".to_string()];
        assert_eq!(
            build_auto_verify_command(&files),
            Some("cargo test".to_string())
        );
    }

    #[test]
    fn test_build_auto_verify_command_python() {
        let files = vec![
            "tests/test_outputs.py".to_string(),
            "tests/test_parser.py".to_string(),
        ];
        assert_eq!(
            build_auto_verify_command(&files),
            Some("python -m pytest tests/test_outputs.py tests/test_parser.py".to_string())
        );
    }

    #[test]
    fn test_build_auto_verify_command_go() {
        let files = vec!["widget_test.go".to_string()];
        assert_eq!(
            build_auto_verify_command(&files),
            Some("go test ./...".to_string())
        );
    }

    #[test]
    fn test_build_auto_verify_command_js() {
        let files = vec!["app.test.js".to_string()];
        assert_eq!(
            build_auto_verify_command(&files),
            Some("npx jest".to_string())
        );
    }

    #[test]
    fn test_build_auto_verify_command_empty() {
        let files: Vec<String> = vec![];
        assert_eq!(build_auto_verify_command(&files), None);
    }

    #[test]
    fn test_format_test_discovery_context() {
        let files = vec![
            "tests/test_outputs.py".to_string(),
            "tests/test_parser.py".to_string(),
        ];
        let ctx = format_test_discovery_context(&files);
        assert!(ctx.contains("## Discovered Test Files"));
        assert!(ctx.contains("tests/test_outputs.py"));
        assert!(ctx.contains("tests/test_parser.py"));
        assert!(ctx.contains("MUST run these tests"));
    }

    #[test]
    fn test_format_test_discovery_context_empty() {
        let ctx = format_test_discovery_context(&[]);
        assert!(ctx.is_empty());
    }

    #[test]
    fn test_read_wg_guide_returns_default_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let guide = read_wg_guide(&wg_dir);
        assert_eq!(guide, workgraph::service::executor::DEFAULT_WG_GUIDE);
    }

    #[test]
    fn test_read_wg_guide_reads_custom_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let custom_guide = "Custom wg guide for this project.";
        std::fs::write(wg_dir.join("wg-guide.md"), custom_guide).unwrap();

        let guide = read_wg_guide(&wg_dir);
        assert_eq!(guide, custom_guide);
    }

    #[test]
    fn test_read_wg_guide_falls_back_on_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        std::fs::write(wg_dir.join("wg-guide.md"), "  \n  ").unwrap();

        let guide = read_wg_guide(&wg_dir);
        assert_eq!(guide, workgraph::service::executor::DEFAULT_WG_GUIDE);
    }
}

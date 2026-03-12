//! Lightweight LLM-based assignment: replaces full Claude Code sessions with a single API call.
//!
//! Pattern follows `triage.rs`: build prompt → call `run_lightweight_llm_call` → parse JSON → apply.

use anyhow::{Context, Result};

use workgraph::agency::{self, Agent, short_hash};
use workgraph::config::{Config, DispatchRole};
use workgraph::graph::{Task, TokenUsage};

/// Parsed assignment decision from the LLM.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct AssignmentVerdict {
    /// Hash (or prefix) of the agent to assign. Use "new:<role_id>:<tradeoff_id>" to create.
    pub agent_hash: String,
    /// Execution weight: "shell", "bare", "light", or "full".
    #[serde(default)]
    pub exec_mode: Option<String>,
    /// Context scope: "clean", "task", "graph", or "full".
    #[serde(default)]
    pub context_scope: Option<String>,
    /// Brief explanation of the decision.
    #[serde(default)]
    pub reason: String,
    /// When true, the assigner signals that no good match was found and the
    /// primitive store should be expanded via the creator agent.
    #[serde(default)]
    pub create_needed: bool,
}

/// Pre-gathered agent catalog entry for prompt rendering.
struct AgentEntry {
    hash: String,
    name: String,
    role_name: String,
    role_skills: Vec<String>,
    tradeoff_name: String,
    avg_score: Option<f64>,
    task_count: u32,
    capabilities: Vec<String>,
    _staleness_flags: Vec<String>,
}

/// Build the agent catalog for the assignment prompt.
fn build_agent_catalog(
    agents: &[Agent],
    roles_dir: &std::path::Path,
    tradeoffs_dir: &std::path::Path,
) -> Vec<AgentEntry> {
    agents
        .iter()
        .filter(|a| !a.is_human() && a.staleness_flags.is_empty())
        .map(|a| {
            let role = agency::find_role_by_prefix(roles_dir, &a.role_id).ok();
            let tradeoff = agency::find_tradeoff_by_prefix(tradeoffs_dir, &a.tradeoff_id).ok();
            let role_skills = role
                .as_ref()
                .map(|r| r.component_ids.to_vec())
                .unwrap_or_default();
            AgentEntry {
                hash: short_hash(&a.id).to_string(),
                name: a.name.clone(),
                role_name: role.as_ref().map(|r| r.name.clone()).unwrap_or_default(),
                role_skills,
                tradeoff_name: tradeoff.map(|t| t.name.clone()).unwrap_or_default(),
                avg_score: a.performance.avg_score,
                task_count: a.performance.task_count,
                capabilities: a.capabilities.clone(),
                _staleness_flags: a
                    .staleness_flags
                    .iter()
                    .map(|f| format!("{:?}", f))
                    .collect(),
            }
        })
        .collect()
}

/// Render the agent catalog as a compact text block for the prompt.
fn render_agent_catalog(entries: &[AgentEntry]) -> String {
    if entries.is_empty() {
        return "No agents available.\n".to_string();
    }
    let mut out = String::new();
    for e in entries {
        out.push_str(&format!(
            "- **{}** (hash: {}): role={}, tradeoff={}, score={}, tasks={}{}{}\n",
            e.name,
            e.hash,
            e.role_name,
            e.tradeoff_name,
            e.avg_score
                .map(|s| format!("{:.2}", s))
                .unwrap_or_else(|| "none".to_string()),
            e.task_count,
            if e.capabilities.is_empty() {
                String::new()
            } else {
                format!(", capabilities=[{}]", e.capabilities.join(", "))
            },
            if e.role_skills.is_empty() {
                String::new()
            } else {
                format!(", role_components=[{}]", e.role_skills.join(", "))
            },
        ));
    }
    out
}

/// Build the full assignment prompt for the lightweight LLM call.
pub(crate) fn build_assignment_prompt(
    task: &Task,
    mode_context: &str,
    agent_catalog: &str,
    underspec_warning: Option<&str>,
) -> String {
    let task_id = &task.id;
    let task_title = &task.title;
    let task_desc = task.description.as_deref().unwrap_or("(no description)");
    let task_skills = if task.skills.is_empty() {
        "(none)".to_string()
    } else {
        task.skills.join(", ")
    };
    let task_tags = if task.tags.is_empty() {
        "(none)".to_string()
    } else {
        task.tags.join(", ")
    };
    let task_deps = if task.after.is_empty() {
        "(none)".to_string()
    } else {
        task.after.join(", ")
    };
    let context_scope_note = task
        .context_scope
        .as_ref()
        .map(|s| format!("\n- **Pre-set context scope:** {}", s))
        .unwrap_or_default();

    let underspec = underspec_warning.unwrap_or("");

    format!(
        r#"You are an agent assignment system. Given a task and available agents, select the best agent and configure execution parameters.

## Task
- **ID:** {task_id}
- **Title:** {task_title}
- **Description:** {task_desc}
- **Skills:** {task_skills}
- **Tags:** {task_tags}
- **Dependencies:** {task_deps}{context_scope_note}
{underspec}
{mode_context}
## Available Agents

{agent_catalog}
## Decision Criteria

1. **Role fit**: Agent's role skills should overlap with task requirements.
2. **Tradeoff fit**: Agent's operational style should match task nature (Careful for correctness-critical, Fast for routine, Thorough for complex).
3. **Performance**: Prefer agents with higher avg_score on similar tasks.
4. **Capabilities**: Match agent capabilities to task tags/skills.
5. **Cold start**: When agents have no scores, match on role and spread work across untested agents.

## exec_mode Selection
- **shell**: Task has exec command, no LLM needed.
- **bare**: Pure reasoning, synthesis, no file access needed.
- **light**: Read-only file access (research, review, exploration).
- **full**: Modifies files (implementation, debugging, refactoring, test writing). Default if unsure.

## context_scope Selection
- **clean**: Self-contained computation/writing, no workgraph interaction needed.
- **task**: Standard implementation (default if unsure).
- **graph**: Integration tasks spanning multiple components (3+ dependencies).
- **full**: Meta-tasks about workgraph itself.

## Response

Respond with ONLY a JSON object (no markdown fences, no commentary):

{{
  "agent_hash": "<hash prefix of selected agent>",
  "exec_mode": "<shell|bare|light|full>",
  "context_scope": "<clean|task|graph|full>",
  "reason": "<one-sentence explanation>",
  "create_needed": false
}}

Always pick the closest match — never fail to assign. If no agent is a good fit
(the task requires capabilities not represented by any existing agent), still assign
the best available but set `"create_needed": true` to signal that new agent types
should be created for future tasks like this."#
    )
}

/// Run the lightweight assignment LLM call and parse the verdict.
/// Returns the assignment verdict and any token usage from the LLM call.
pub(crate) fn run_lightweight_assignment(
    config: &Config,
    task: &Task,
    agents: &[Agent],
    roles_dir: &std::path::Path,
    tradeoffs_dir: &std::path::Path,
    mode_context: &str,
    underspec_warning: Option<&str>,
) -> Result<(AssignmentVerdict, Option<TokenUsage>)> {
    let timeout_secs = config.agency.triage_timeout.unwrap_or(30);

    let catalog_entries = build_agent_catalog(agents, roles_dir, tradeoffs_dir);
    let catalog_text = render_agent_catalog(&catalog_entries);

    let prompt = build_assignment_prompt(task, mode_context, &catalog_text, underspec_warning);

    let result = workgraph::service::llm::run_lightweight_llm_call(
        config,
        DispatchRole::Assigner,
        &prompt,
        timeout_secs,
    )
    .context("Assignment LLM call failed")?;

    let token_usage = result.token_usage;

    // Parse JSON verdict from output (reuse triage JSON extraction logic)
    let json_str = extract_assignment_json(&result.text).ok_or_else(|| {
        anyhow::anyhow!(
            "No valid JSON found in assignment output: {}",
            &result.text[..result.text.len().min(200)]
        )
    })?;

    let verdict: AssignmentVerdict = serde_json::from_str(&json_str)
        .with_context(|| format!("Failed to parse assignment JSON: {}", json_str))?;

    // Validate exec_mode
    if let Some(ref mode) = verdict.exec_mode {
        match mode.as_str() {
            "shell" | "bare" | "light" | "full" => {}
            other => {
                eprintln!(
                    "[assignment] Warning: invalid exec_mode '{}', defaulting to 'full'",
                    other
                );
            }
        }
    }

    // Validate context_scope
    if let Some(ref scope) = verdict.context_scope {
        match scope.as_str() {
            "clean" | "task" | "graph" | "full" => {}
            other => {
                eprintln!(
                    "[assignment] Warning: invalid context_scope '{}', defaulting to 'task'",
                    other
                );
            }
        }
    }

    Ok((verdict, token_usage))
}

/// Extract a JSON object from potentially noisy LLM output.
fn extract_assignment_json(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    // Strip markdown code fences
    if trimmed.starts_with("```") {
        let inner = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
            return Some(inner.to_string());
        }
    }

    // Find first { to last }
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
        && start <= end
    {
        let candidate = &trimmed[start..=end];
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Status, Task};

    #[test]
    fn test_extract_assignment_json_plain() {
        let input = r#"{"agent_hash": "abc123", "exec_mode": "full", "context_scope": "task", "reason": "best match"}"#;
        let result = extract_assignment_json(input).unwrap();
        let parsed: AssignmentVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.agent_hash, "abc123");
        assert_eq!(parsed.exec_mode.as_deref(), Some("full"));
    }

    #[test]
    fn test_extract_assignment_json_with_fences() {
        let input = "```json\n{\"agent_hash\": \"abc\", \"reason\": \"ok\"}\n```";
        let result = extract_assignment_json(input).unwrap();
        let parsed: AssignmentVerdict = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.agent_hash, "abc");
    }

    #[test]
    fn test_extract_assignment_json_garbage() {
        assert!(extract_assignment_json("no json here").is_none());
    }

    #[test]
    fn test_build_assignment_prompt_contains_task_info() {
        let task = Task {
            id: "test-task".to_string(),
            title: "Fix the bug".to_string(),
            description: Some("There is a bug in foo.rs".to_string()),
            status: Status::Open,
            skills: vec!["rust".to_string()],
            tags: vec!["implementation".to_string()],
            ..Default::default()
        };
        let prompt = build_assignment_prompt(
            &task,
            "## Mode\nPerformance",
            "- Agent1 (hash: abc)\n",
            None,
        );
        assert!(prompt.contains("test-task"));
        assert!(prompt.contains("Fix the bug"));
        assert!(prompt.contains("rust"));
        assert!(prompt.contains("Agent1"));
        assert!(prompt.contains("Performance"));
    }

    #[test]
    fn test_render_agent_catalog_empty() {
        let result = render_agent_catalog(&[]);
        assert!(result.contains("No agents"));
    }

    #[test]
    fn test_render_agent_catalog_entries() {
        let entries = vec![AgentEntry {
            hash: "abc12345".to_string(),
            name: "TestAgent".to_string(),
            role_name: "Programmer".to_string(),
            role_skills: vec!["coding".to_string()],
            tradeoff_name: "Careful".to_string(),
            avg_score: Some(0.85),
            task_count: 10,
            capabilities: vec!["rust".to_string()],
            _staleness_flags: vec![],
        }];
        let result = render_agent_catalog(&entries);
        assert!(result.contains("TestAgent"));
        assert!(result.contains("abc12345"));
        assert!(result.contains("0.85"));
        assert!(result.contains("rust"));
    }
}

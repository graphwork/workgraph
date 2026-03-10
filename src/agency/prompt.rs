use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use super::hash::short_hash;
use super::run_mode::AssignmentPath;
use super::store;
use super::types::*;

/// Expand `~` at the start of a path to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Resolve a single skill reference to its content.
///
/// - `Name`: returns the name string as-is (tag only).
/// - `File`: reads the file, expanding `~` and resolving relative paths from `workgraph_root`.
/// - `Url`: fetches the URL content (requires `matrix-lite` feature for reqwest).
/// - `Inline`: returns the content directly.
///
/// `workgraph_root` is the project root directory (parent of `.workgraph/`).
pub fn resolve_skill(skill: &ContentRef, workgraph_root: &Path) -> Result<ResolvedSkill, String> {
    match skill {
        ContentRef::Name(name) => {
            if let Some(content) = name.strip_prefix("inline:") {
                Ok(ResolvedSkill {
                    name: "inline".to_string(),
                    content: content.to_string(),
                })
            } else {
                Ok(ResolvedSkill {
                    name: name.clone(),
                    content: name.clone(),
                })
            }
        }
        ContentRef::File(path) => {
            let expanded = expand_tilde(path);
            let resolved = if expanded.is_absolute() {
                expanded
            } else {
                workgraph_root.join(&expanded)
            };
            let content = fs::read_to_string(&resolved)
                .map_err(|e| format!("Failed to read skill file {}: {}", resolved.display(), e))?;
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            Ok(ResolvedSkill { name, content })
        }
        ContentRef::Url(url) => resolve_url(url),
        ContentRef::Inline(content) => Ok(ResolvedSkill {
            name: "inline".to_string(),
            content: content.clone(),
        }),
    }
}

#[cfg(feature = "matrix-lite")]
fn resolve_url(url: &str) -> Result<ResolvedSkill, String> {
    // Use a blocking reqwest call since skill resolution happens at setup time.
    let body = reqwest::blocking::get(url)
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::text)
        .map_err(|e| format!("Failed to fetch skill URL {}: {}", url, e))?;
    Ok(ResolvedSkill {
        name: url.to_string(),
        content: body,
    })
}

#[cfg(not(feature = "matrix-lite"))]
fn resolve_url(url: &str) -> Result<ResolvedSkill, String> {
    Err(format!(
        "Cannot fetch skill URL {} (built without HTTP support; enable matrix-lite feature)",
        url
    ))
}

/// Resolve all skills in a role, returning successfully resolved skills.
///
/// Skills that fail to resolve produce a warning on stderr but do not abort.
pub fn resolve_all_skills(role: &Role, workgraph_root: &Path) -> Vec<ResolvedSkill> {
    role.component_ids
        .iter()
        .filter_map(|id| {
            let content_ref = parse_component_id(id);
            match resolve_skill(&content_ref, workgraph_root) {
                Ok(resolved) => Some(resolved),
                Err(warning) => {
                    eprintln!("Warning: {}", warning);
                    None
                }
            }
        })
        .collect()
}

/// Parse a component ID string into a ContentRef, detecting prefixes like
/// `inline:`, `file:///`, `https://`, `http://`.
fn parse_component_id(id: &str) -> ContentRef {
    if let Some(content) = id.strip_prefix("inline:") {
        ContentRef::Inline(content.to_string())
    } else if let Some(rest) = id.strip_prefix("file:///") {
        ContentRef::File(PathBuf::from(format!("/{}", rest)))
    } else if id.starts_with("https://") || id.starts_with("http://") {
        ContentRef::Url(id.to_string())
    } else {
        ContentRef::Name(id.to_string())
    }
}

/// Returns true if a component ID looks like a content-ref (inline:, file:///, http(s)://)
/// rather than a hash-based primitives store ID.
fn is_content_ref_id(id: &str) -> bool {
    id.starts_with("inline:")
        || id.starts_with("file:///")
        || id.starts_with("https://")
        || id.starts_with("http://")
}

/// Resolve all component IDs in a role, loading rich `RoleComponent` data from the
/// primitives store when available. Falls back to `ContentRef` resolution for IDs
/// that are content refs (inline:, file:///, http(s)://) or not found in the store.
///
/// `agency_dir` is the `.workgraph/agency/` directory containing `primitives/components/`.
pub fn resolve_all_components(
    role: &Role,
    workgraph_root: &Path,
    agency_dir: &Path,
) -> Vec<ResolvedSkill> {
    let components_dir = agency_dir.join("primitives/components");
    role.component_ids
        .iter()
        .filter_map(|id| {
            // First: if it's a content-ref prefix, use existing resolution
            if is_content_ref_id(id) {
                let content_ref = parse_component_id(id);
                return match resolve_skill(&content_ref, workgraph_root) {
                    Ok(resolved) => Some(resolved),
                    Err(warning) => {
                        eprintln!("Warning: {}", warning);
                        None
                    }
                };
            }

            // Second: try loading from the primitives store as a RoleComponent
            let component_path = components_dir.join(format!("{}.yaml", id));
            if let Ok(comp) = store::load_component(&component_path) {
                // Resolve the component's inner content ref to get actual content
                let content = match resolve_skill(&comp.content, workgraph_root) {
                    Ok(resolved) => resolved.content,
                    Err(_) => comp.description.clone(),
                };
                let category_label = match comp.category {
                    ComponentCategory::Translated => "Translated",
                    ComponentCategory::Enhanced => "Enhanced",
                    ComponentCategory::Novel => "Novel",
                };
                return Some(ResolvedSkill {
                    name: comp.name,
                    content: format!("[{}] {}\n{}", category_label, comp.description, content),
                });
            }

            // Third: try prefix match in the store
            if let Ok(comp) = store::find_component_by_prefix(&components_dir, id) {
                let content = match resolve_skill(&comp.content, workgraph_root) {
                    Ok(resolved) => resolved.content,
                    Err(_) => comp.description.clone(),
                };
                let category_label = match comp.category {
                    ComponentCategory::Translated => "Translated",
                    ComponentCategory::Enhanced => "Enhanced",
                    ComponentCategory::Novel => "Novel",
                };
                return Some(ResolvedSkill {
                    name: comp.name,
                    content: format!("[{}] {}\n{}", category_label, comp.description, content),
                });
            }

            // Fourth: fall back to ContentRef-based resolution (name/inline)
            let content_ref = parse_component_id(id);
            match resolve_skill(&content_ref, workgraph_root) {
                Ok(resolved) => Some(resolved),
                Err(warning) => {
                    eprintln!("Warning: {}", warning);
                    None
                }
            }
        })
        .collect()
}

/// Resolve an outcome_id to a `DesiredOutcome` from the primitives store.
///
/// Returns `None` if the outcome cannot be found (graceful fallback).
pub fn resolve_outcome(outcome_id: &str, agency_dir: &Path) -> Option<DesiredOutcome> {
    if outcome_id.is_empty() {
        return None;
    }
    let outcomes_dir = agency_dir.join("primitives/outcomes");

    // Try exact match first
    let outcome_path = outcomes_dir.join(format!("{}.yaml", outcome_id));
    if let Ok(outcome) = store::load_outcome(&outcome_path) {
        return Some(outcome);
    }

    // Try prefix match
    store::find_outcome_by_prefix(&outcomes_dir, outcome_id).ok()
}

// ---------------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------------

/// Render the identity section to inject into agent prompts.
///
/// The output is placed between system context and task description in the prompt.
///
/// When `outcome` is provided, its `success_criteria` are rendered under the
/// Desired Outcome section. Otherwise, `role.outcome_id` is rendered as raw text.
pub fn render_identity_prompt(
    role: &Role,
    tradeoff: &TradeoffConfig,
    resolved_skills: &[ResolvedSkill],
) -> String {
    render_identity_prompt_rich(role, tradeoff, resolved_skills, None)
}

/// Render the identity section with optional resolved outcome for richer output.
///
/// This is the full-featured version that renders `DesiredOutcome.success_criteria`
/// when the outcome primitive is available.
pub fn render_identity_prompt_rich(
    role: &Role,
    tradeoff: &TradeoffConfig,
    resolved_skills: &[ResolvedSkill],
    outcome: Option<&DesiredOutcome>,
) -> String {
    let mut out = String::new();

    out.push_str("## Agent Identity\n\n");
    let _ = writeln!(out, "### Role: {}", role.name);
    let _ = writeln!(out, "{}\n", role.description);

    if !resolved_skills.is_empty() {
        out.push_str("#### Skills\n");
        for skill in resolved_skills {
            if skill.content == skill.name {
                let _ = writeln!(out, "- {}", skill.name);
            } else {
                let _ = writeln!(out, "- **{}**\n{}", skill.name, skill.content);
            }
        }
        out.push('\n');
    }

    out.push_str("#### Desired Outcome\n");
    if let Some(resolved_outcome) = outcome {
        let _ = writeln!(out, "**{}**", resolved_outcome.name);
        let _ = writeln!(out, "{}\n", resolved_outcome.description);
        if !resolved_outcome.success_criteria.is_empty() {
            out.push_str("**Success Criteria:**\n");
            for criterion in &resolved_outcome.success_criteria {
                let _ = writeln!(out, "- {}", criterion);
            }
            out.push('\n');
        }
    } else {
        let _ = writeln!(out, "{}\n", role.outcome_id);
    }

    let has_tradeoffs = !tradeoff.acceptable_tradeoffs.is_empty();
    let has_constraints = !tradeoff.unacceptable_tradeoffs.is_empty();

    if has_tradeoffs || has_constraints {
        out.push_str("### Operational Parameters\n");

        if has_tradeoffs {
            out.push_str("#### Acceptable Trade-offs\n");
            for t in &tradeoff.acceptable_tradeoffs {
                let _ = writeln!(out, "- {}", t);
            }
            out.push('\n');
        }

        if has_constraints {
            out.push_str("#### Non-negotiable Constraints\n");
            for constraint in &tradeoff.unacceptable_tradeoffs {
                let _ = writeln!(out, "- {}", constraint);
            }
            out.push('\n');
        }
    }

    out.push_str("---");

    out
}

/// Input data for the evaluator prompt renderer.
pub struct EvaluatorInput<'a> {
    /// Task title
    pub task_title: &'a str,
    /// Task description (may be None)
    pub task_description: Option<&'a str>,
    /// Task skills required
    pub task_skills: &'a [String],
    /// Verification criteria (if any)
    pub verify: Option<&'a str>,
    /// Agent that worked on the task (if assigned)
    pub agent: Option<&'a Agent>,
    /// Role used by the agent (if identity was assigned)
    pub role: Option<&'a Role>,
    /// Tradeoff config used by the agent (if identity was assigned)
    pub tradeoff: Option<&'a TradeoffConfig>,
    /// Produced artifacts (file paths / references)
    pub artifacts: &'a [String],
    /// Progress log entries
    pub log_entries: &'a [crate::graph::LogEntry],
    /// Time the task started (ISO 8601, if available)
    pub started_at: Option<&'a str>,
    /// Time the task completed (ISO 8601, if available)
    pub completed_at: Option<&'a str>,
    /// Git diff of artifact files at completion time (ground truth for evaluator)
    pub artifact_diff: Option<&'a str>,
    /// Pre-rendered identity prompt for the evaluator agent itself (if configured).
    /// When present, replaces the generic system instruction with the evaluator's
    /// own role components and tradeoff configuration.
    pub evaluator_identity: Option<&'a str>,
    /// Downstream task context for organizational impact scoring.
    /// Each entry: (task_title, status_str, description_snippet).
    pub downstream_tasks: &'a [(String, String, Option<String>)],
    /// FLIP score for the source task (from source: "flip" evaluation), if available.
    pub flip_score: Option<f64>,
    /// Verification status from .verify-flip-<task>: "passed" or "failed", if available.
    pub verify_status: Option<&'a str>,
    /// Log entries from the .verify-flip-<task> task, if available.
    pub verify_findings: Option<&'a str>,
}

/// Render the evaluator prompt that an LLM evaluator will receive.
///
/// The output is a self-contained prompt instructing the evaluator to assess
/// the agent's work and return structured JSON.
pub fn render_evaluator_prompt(input: &EvaluatorInput) -> String {
    let mut out = String::new();

    // -- System instructions / evaluator identity --
    if let Some(identity) = input.evaluator_identity {
        out.push_str(identity);
        out.push_str("\n\n");
        out.push_str(
            "Review the task definition, the agent identity that was used, the produced artifacts,\n\
             and the task log. Then produce a JSON evaluation.\n\n",
        );
    } else {
        out.push_str("# Evaluator Instructions\n\n");
        out.push_str(
            "You are an evaluator assessing the quality of work performed by an AI agent.\n\
             Review the task definition, the agent identity that was used, the produced artifacts,\n\
             and the task log. Then produce a JSON evaluation.\n\n",
        );
    }

    // -- Task definition --
    out.push_str("## Task Definition\n\n");
    let _ = writeln!(out, "**Title:** {}\n", input.task_title);
    if let Some(desc) = input.task_description {
        let _ = writeln!(out, "**Description:**\n{}\n", desc);
    }
    if !input.task_skills.is_empty() {
        out.push_str("**Required Skills:**\n");
        for skill in input.task_skills {
            let _ = writeln!(out, "- {}", skill);
        }
        out.push('\n');
    }
    if let Some(verify) = input.verify {
        let _ = writeln!(out, "**Verification Criteria:**\n{}\n", verify);
    }

    // -- Agent identity --
    out.push_str("## Agent Identity\n\n");
    if let Some(agent) = input.agent {
        let _ = writeln!(
            out,
            "**Agent:** {} ({})\n",
            agent.name,
            short_hash(&agent.id)
        );
    }
    if let Some(role) = input.role {
        let _ = writeln!(out, "**Role:** {} ({})", role.name, role.id);
        let _ = writeln!(out, "{}\n", role.description);
        let _ = writeln!(out, "**Desired Outcome:** {}\n", role.outcome_id);
    } else {
        out.push_str("*No role was assigned.*\n\n");
    }
    if let Some(tradeoff) = input.tradeoff {
        let _ = writeln!(out, "**Motivation:** {} ({})", tradeoff.name, tradeoff.id);
        let _ = writeln!(out, "{}\n", tradeoff.description);
        if !tradeoff.acceptable_tradeoffs.is_empty() {
            out.push_str("**Acceptable Trade-offs:**\n");
            for t in &tradeoff.acceptable_tradeoffs {
                let _ = writeln!(out, "- {}", t);
            }
            out.push('\n');
        }
        if !tradeoff.unacceptable_tradeoffs.is_empty() {
            out.push_str("**Non-negotiable Constraints:**\n");
            for c in &tradeoff.unacceptable_tradeoffs {
                let _ = writeln!(out, "- {}", c);
            }
            out.push('\n');
        }
    } else {
        out.push_str("*No motivation was assigned.*\n\n");
    }

    // -- Artifacts --
    out.push_str("## Task Artifacts\n\n");
    if input.artifacts.is_empty() {
        out.push_str("*No artifacts were recorded.*\n\n");
    } else {
        for artifact in input.artifacts {
            let _ = writeln!(out, "- `{}`", artifact);
        }
        out.push('\n');
    }

    // -- Log --
    out.push_str("## Task Log\n\n");
    if input.log_entries.is_empty() {
        out.push_str("*No log entries.*\n\n");
    } else {
        for entry in input.log_entries {
            let actor = entry.actor.as_deref().unwrap_or("system");
            let _ = writeln!(
                out,
                "- [{}] ({}): {}",
                entry.timestamp, actor, entry.message
            );
        }
        out.push('\n');
    }

    // -- Timing --
    if input.started_at.is_some() || input.completed_at.is_some() {
        out.push_str("## Timing\n\n");
        if let Some(started) = input.started_at {
            let _ = writeln!(out, "- Started: {}", started);
        }
        if let Some(completed) = input.completed_at {
            let _ = writeln!(out, "- Completed: {}", completed);
        }
        out.push('\n');
    }

    // -- Downstream context (for organizational impact scoring) --
    if !input.downstream_tasks.is_empty() {
        out.push_str("## Downstream Tasks\n\n");
        out.push_str(
            "These tasks depend on this task's output. Consider whether the output\n\
             is structured, documented, and usable enough for downstream consumers.\n\n",
        );
        for (title, status, desc) in input.downstream_tasks {
            let _ = write!(out, "- **{}** ({})", title, status);
            if let Some(d) = desc {
                // Truncate long descriptions to save tokens (char-boundary safe)
                let snippet = if d.len() > 120 {
                    let mut end = 120;
                    while !d.is_char_boundary(end) && end > 0 {
                        end -= 1;
                    }
                    &d[..end]
                } else {
                    d.as_str()
                };
                let _ = write!(out, " — {}", snippet);
            }
            out.push('\n');
        }
        out.push('\n');
    }

    // -- FLIP Verification Results (when available) --
    if input.flip_score.is_some() || input.verify_status.is_some() {
        out.push_str("## FLIP Verification Results\n\n");
        if let Some(score) = input.flip_score {
            let threshold = 0.70;
            let relation = if score >= threshold {
                "at or above"
            } else {
                "below"
            };
            let _ = writeln!(
                out,
                "FLIP Score: {:.2} ({} threshold {:.2})",
                score, relation, threshold
            );
        }
        if let Some(status) = input.verify_status {
            let _ = writeln!(out, "Verification Status: {}", status.to_uppercase());
        }
        if let Some(findings) = input.verify_findings {
            out.push_str("Verification Findings:\n");
            out.push_str(findings);
            out.push('\n');
        }
        out.push('\n');
        out.push_str(
            "NOTE: Verification is a strong signal. If verification failed, significantly\n\
             reduce the overall score. If verification passed despite low FLIP, the FLIP\n\
             may have been a false alarm.\n\n",
        );
    }

    // -- Evaluation rubric & output format --
    out.push_str("## Evaluation Criteria\n\n");
    out.push_str(
        "Assess the agent's work on these dimensions (each scored 0.0 to 1.0):\n\n\
         ### Individual Quality (70% of overall score)\n\n\
         1. **correctness** — Does the output match the desired outcome? Are verification\n\
            criteria satisfied? Is the implementation functionally correct?\n\
         2. **completeness** — Were all aspects of the task addressed? Are there missing\n\
            pieces, unhandled edge cases, or incomplete deliverables?\n\
         3. **efficiency** — Was the work done efficiently within the allowed parameters?\n\
            Minimal unnecessary steps, no wasted effort, appropriate scope.\n\
         4. **style_adherence** — Does the output follow project conventions, coding\n\
            standards, and the constraints set by the motivation (trade-offs respected,\n\
            non-negotiable constraints honoured)?\n\n\
         ### Organizational Impact (30% of overall score)\n\n\
         5. **downstream_usability** — Is the output structured, documented, and formatted\n\
            so that downstream tasks can consume it without rework? Are interfaces clean,\n\
            artifacts well-named, and context properly handed off?\n\
         6. **coordination_overhead** — Did the agent work autonomously without creating\n\
            unnecessary work for others? Low overhead = high score. Deduct for: leaving\n\
            ambiguous state, requiring manual cleanup, creating confusion in the task log.\n\
         7. **blocking_impact** — Was the work completed in a timely manner relative to\n\
            the task scope? Did it avoid unnecessarily blocking downstream work? Consider\n\
            the task complexity and whether the time taken was proportionate.\n\n",
    );

    out.push_str(
        "Compute an overall **score** as a weighted average:\n\
         - correctness: 30%\n\
         - completeness: 20%\n\
         - efficiency: 10%\n\
         - style_adherence: 10%\n\
         - downstream_usability: 15%\n\
         - coordination_overhead: 10%\n\
         - blocking_impact: 5%\n\n\
         Note: `intent_fidelity` is mechanically injected from the FLIP score and does not \
         need to be scored by the evaluator. Do not include it in your output dimensions.\n\n",
    );

    // -- Rubric spectrum --
    out.push_str(
        "### Rubric Spectrum\n\n\
         Map your overall score to one of these levels:\n\n\
         | Score Range | Level | Meaning |\n\
         |------------|-------|--------|\n\
         | 0.0–0.2 | Failing | Fundamental failures; output unusable |\n\
         | 0.2–0.4 | Below Expectations | Significant deficiencies; major rework needed |\n\
         | 0.4–0.6 | Meets Expectations | Acceptable but unremarkable |\n\
         | 0.6–0.8 | Exceeds Expectations | Solid, reliable work |\n\
         | 0.8–1.0 | Exceptional | Best-in-class output |\n\n\
         Calibrate your scores against this spectrum. Most competent work falls in 0.6–0.8.\n\n",
    );

    if input.downstream_tasks.is_empty() {
        out.push_str(
            "Note: No downstream tasks are listed. Score organizational dimensions based\n\
             on general output quality: is the work self-contained, well-documented, and\n\
             ready for potential future consumers? Default to 0.7 if insufficient signal.\n\n",
        );
    }

    out.push_str("## Required Output\n\n");
    out.push_str(
        "Respond with **only** a JSON object (no markdown fences, no commentary):\n\n\
         ```\n\
         {\n  \
           \"score\": <0.0-1.0>,\n  \
           \"dimensions\": {\n    \
             \"correctness\": <0.0-1.0>,\n    \
             \"completeness\": <0.0-1.0>,\n    \
             \"efficiency\": <0.0-1.0>,\n    \
             \"style_adherence\": <0.0-1.0>,\n    \
             \"downstream_usability\": <0.0-1.0>,\n    \
             \"coordination_overhead\": <0.0-1.0>,\n    \
             \"blocking_impact\": <0.0-1.0>\n  \
           },\n  \
           \"notes\": \"<brief explanation of strengths, weaknesses, and suggestions>\"\n\
         }\n\
         ```\n",
    );

    out
}

// ---------------------------------------------------------------------------
// FLIP (Fidelity via Latent Intent Probing) evaluation prompts
// ---------------------------------------------------------------------------

/// Input for the FLIP inference phase — contains task output but NOT the task description.
/// The inference evaluator must reconstruct what the task was from the output alone.
pub struct FlipInferenceInput<'a> {
    /// Agent that worked on the task (if assigned)
    pub agent: Option<&'a Agent>,
    /// Role used by the agent (if identity was assigned)
    pub role: Option<&'a Role>,
    /// Tradeoff config used by the agent (if identity was assigned)
    pub tradeoff: Option<&'a TradeoffConfig>,
    /// Produced artifacts (file paths / references)
    pub artifacts: &'a [String],
    /// Progress log entries (task description/title should be redacted from messages)
    pub log_entries: &'a [crate::graph::LogEntry],
    /// Time the task started (ISO 8601, if available)
    pub started_at: Option<&'a str>,
    /// Time the task completed (ISO 8601, if available)
    pub completed_at: Option<&'a str>,
    /// Git diff of artifact files at completion time
    pub artifact_diff: Option<&'a str>,
}

/// Render the FLIP inference prompt: given only the task output, reconstruct
/// what the original task description must have been.
pub fn render_flip_inference_prompt(input: &FlipInferenceInput) -> String {
    let mut out = String::new();

    out.push_str("# Prompt Reconstruction Task (FLIP Inference)\n\n");
    out.push_str(
        "You are performing a roundtrip intent fidelity evaluation. An AI agent completed a task, \
         and you can see the agent's output below. Your job is to **reconstruct** what the \
         original task description must have been.\n\n\
         You do NOT have access to the original task description. Based solely on the artifacts, \
         code changes, and log entries below, infer:\n\
         1. What was the goal of this task?\n\
         2. What specific requirements or acceptance criteria were given?\n\
         3. What constraints or context was provided?\n\n\
         Write your reconstruction as a task description that could have produced this output.\n\n",
    );

    // -- Agent identity (describes HOW to work, not WHAT to do) --
    if input.agent.is_some() || input.role.is_some() || input.tradeoff.is_some() {
        out.push_str("## Agent Context\n\n");
        out.push_str("(This describes the agent's working style, not the task itself.)\n\n");
        if let Some(role) = input.role {
            let _ = writeln!(out, "**Role:** {}", role.name);
            let _ = writeln!(out, "{}\n", role.description);
        }
        if let Some(tradeoff) = input.tradeoff {
            let _ = writeln!(out, "**Working style:** {}\n", tradeoff.description);
        }
    }

    // -- Artifacts --
    out.push_str("## Task Output — Artifacts\n\n");
    if input.artifacts.is_empty() {
        out.push_str("*No artifacts were recorded.*\n\n");
    } else {
        for artifact in input.artifacts {
            let _ = writeln!(out, "- `{}`", artifact);
        }
        out.push('\n');
    }

    // -- Git diff (the primary signal) --
    if let Some(diff) = input.artifact_diff {
        out.push_str("## Task Output — Code Changes\n\n");
        out.push_str("```diff\n");
        out.push_str(diff);
        if !diff.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    // -- Log entries (redacted: strip task ID references from messages) --
    out.push_str("## Task Output — Log Entries\n\n");
    if input.log_entries.is_empty() {
        out.push_str("*No log entries.*\n\n");
    } else {
        for entry in input.log_entries {
            let actor = entry.actor.as_deref().unwrap_or("system");
            // Redact common task-identifying patterns from log messages
            let msg = redact_task_hints(&entry.message);
            let _ = writeln!(out, "- [{}] ({}): {}", entry.timestamp, actor, msg);
        }
        out.push('\n');
    }

    // -- Timing --
    if input.started_at.is_some() || input.completed_at.is_some() {
        out.push_str("## Timing\n\n");
        if let Some(started) = input.started_at {
            let _ = writeln!(out, "- Started: {}", started);
        }
        if let Some(completed) = input.completed_at {
            let _ = writeln!(out, "- Completed: {}", completed);
        }
        out.push('\n');
    }

    // -- Output format --
    out.push_str("## Required Output\n\n");
    out.push_str(
        "Respond with **only** a JSON object (no markdown fences, no commentary):\n\n\
         ```\n\
         {\n  \
           \"inferred_prompt\": \"<your reconstruction of the original task description, \
         including goal, requirements, and acceptance criteria>\"\n\
         }\n\
         ```\n",
    );

    out
}

/// Redact task-identifying hints from log messages to prevent information leakage
/// during FLIP inference. Strips patterns like task IDs, "Task: ...", etc.
fn redact_task_hints(msg: &str) -> String {
    use std::sync::LazyLock;

    // Redact task ID patterns (prefixed-kebab-case IDs like implement-foo-bar)
    static TASK_ID_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"\b(implement|research|fix|add|update|refactor|evaluate|test|verify|integration|assign)-[a-z0-9]([a-z0-9-]*[a-z0-9])?\b"
        ).unwrap()
    });

    TASK_ID_RE.replace_all(msg, "[TASK-ID]").to_string()
}

/// Input for the FLIP comparison phase — receives both actual and inferred prompts.
pub struct FlipComparisonInput<'a> {
    /// The original task title
    pub actual_title: &'a str,
    /// The original task description
    pub actual_description: Option<&'a str>,
    /// The inferred prompt from phase 1
    pub inferred_prompt: &'a str,
}

/// Render the FLIP comparison prompt: compare the actual and inferred prompts
/// and produce similarity scores.
pub fn render_flip_comparison_prompt(input: &FlipComparisonInput) -> String {
    let mut out = String::new();

    out.push_str("# Prompt Similarity Scoring (FLIP Comparison)\n\n");
    out.push_str(
        "You are performing a roundtrip intent fidelity evaluation. An agent completed a task, \
         and a separate evaluator reconstructed what the task must have been by examining only \
         the output. Your job is to compare the ACTUAL task description with the INFERRED \
         reconstruction and score their similarity.\n\n",
    );

    // -- Actual prompt --
    out.push_str("## ACTUAL Task Description (ground truth)\n\n");
    let _ = writeln!(out, "**Title:** {}\n", input.actual_title);
    if let Some(desc) = input.actual_description {
        let _ = writeln!(out, "{}\n", desc);
    }

    // -- Inferred prompt --
    out.push_str("## INFERRED Task Description (reconstructed from output)\n\n");
    let _ = writeln!(out, "{}\n", input.inferred_prompt);

    // -- Scoring instructions --
    out.push_str("## Scoring Dimensions\n\n");
    out.push_str(
        "Score on these dimensions (each 0.0 to 1.0):\n\n\
         1. **semantic_match** — Do they describe the same core task? Same goal, same domain?\n\
            - 1.0: Clearly the same task\n\
            - 0.5: Related but different emphasis\n\
            - 0.0: Completely different tasks\n\n\
         2. **requirement_coverage** — What fraction of the actual requirements appear in \
         the inferred version? (recall)\n\
            - 1.0: All requirements captured\n\
            - 0.5: About half captured\n\
            - 0.0: None captured\n\n\
         3. **specificity_match** — Is the inferred version as specific as the original?\n\
            - 1.0: Equally specific\n\
            - 0.5: Inferred is vaguer or more generic\n\
            - 0.0: Inferred is completely non-specific\n\n\
         4. **hallucination_rate** — What fraction of inferred requirements are NOT in the \
         original? (false positive rate, inverted: 0.0 = no hallucination = good)\n\
            - 0.0: No hallucinated requirements (best)\n\
            - 0.5: Half the inferred requirements are fabricated\n\
            - 1.0: All inferred requirements are fabricated\n\n\
         Compute a **flip_score** as:\n\
         `flip_score = 0.4 * semantic_match + 0.3 * requirement_coverage + \
         0.2 * specificity_match + 0.1 * (1.0 - hallucination_rate)`\n\n",
    );

    // -- Output format --
    out.push_str("## Required Output\n\n");
    out.push_str(
        "Respond with **only** a JSON object (no markdown fences, no commentary):\n\n\
         ```\n\
         {\n  \
           \"flip_score\": <0.0-1.0>,\n  \
           \"dimensions\": {\n    \
             \"semantic_match\": <0.0-1.0>,\n    \
             \"requirement_coverage\": <0.0-1.0>,\n    \
             \"specificity_match\": <0.0-1.0>,\n    \
             \"hallucination_rate\": <0.0-1.0>\n  \
           },\n  \
           \"notes\": \"<brief explanation of key similarities and differences>\"\n\
         }\n\
         ```\n",
    );

    out
}

// ---------------------------------------------------------------------------
// Run mode context for assigner prompts
// ---------------------------------------------------------------------------

/// Input data for assigner mode context.
pub struct AssignerModeContext<'a> {
    /// Current run_mode value.
    pub run_mode: f64,
    /// Effective exploration rate (max of run_mode and min_exploration_rate).
    pub effective_exploration_rate: f64,
    /// Which assignment path was selected.
    pub assignment_path: AssignmentPath,
    /// For learning mode: the experiment specification.
    pub experiment: Option<&'a AssignmentExperiment>,
    /// For performance mode: top cached agents with scores.
    pub cached_agents: &'a [(String, f64)],
    /// Total assignment count so far.
    pub total_assignments: u32,
}

/// Render mode context for the assigner prompt.
///
/// Extends the assigner prompt with:
/// 1. Mode context (run_mode, effective rate, selected path).
/// 2. Experiment specification (learning mode).
/// 3. Cache contents (performance mode).
pub fn render_assigner_mode_context(ctx: &AssignerModeContext) -> String {
    let mut out = String::new();

    out.push_str("## Assignment Mode Context\n\n");
    let _ = writeln!(out, "- **Run mode:** {:.2}", ctx.run_mode);
    let _ = writeln!(
        out,
        "- **Effective exploration rate:** {:.2}",
        ctx.effective_exploration_rate
    );
    let _ = writeln!(
        out,
        "- **Assignment path:** {}",
        match ctx.assignment_path {
            AssignmentPath::Performance => "Performance (cache-first)",
            AssignmentPath::Learning => "Learning (structured experiment)",
            AssignmentPath::ForcedExploration => "Forced Exploration (interval trigger)",
        }
    );
    let _ = writeln!(out, "- **Total assignments:** {}\n", ctx.total_assignments);

    match ctx.assignment_path {
        AssignmentPath::Performance => {
            if ctx.cached_agents.is_empty() {
                out.push_str(
                    "### Cache Status\n\n\
                     No cached agents available. Use best-guess composition.\n\n",
                );
            } else {
                out.push_str("### Cached Agents (ranked by fit)\n\n");
                for (name, score) in ctx.cached_agents {
                    let _ = writeln!(out, "- {} (score: {:.2})", name, score);
                }
                out.push('\n');
                out.push_str(
                    "Deploy the highest-scoring cached agent if its score meets the threshold.\n\
                     Do NOT vary composition dimensions — deterministic selection only.\n\n",
                );
            }
        }
        AssignmentPath::Learning | AssignmentPath::ForcedExploration => {
            if let Some(exp) = ctx.experiment {
                out.push_str("### Experiment Specification\n\n");
                if exp.bizarre_ideation {
                    out.push_str(
                        "**Bizarre ideation mode:** Compose from random primitives with no\n\
                         attractor guidance. Maximise novelty.\n\n",
                    );
                } else {
                    match &exp.dimension {
                        ExperimentDimension::RoleComponent {
                            replaced,
                            introduced,
                        } => {
                            let _ = writeln!(out, "**Experiment type:** ComponentSwap");
                            if let Some(r) = replaced {
                                let _ = writeln!(out, "- Replace component: `{}`", r);
                            } else {
                                out.push_str("- Add new component (no replacement)\n");
                            }
                            let _ = writeln!(out, "- Introduce component: `{}`", introduced);
                        }
                        ExperimentDimension::TradeoffConfig {
                            replaced,
                            introduced,
                        } => {
                            let _ = writeln!(out, "**Experiment type:** ConfigSwap");
                            if let Some(r) = replaced {
                                let _ = writeln!(out, "- Replace tradeoff: `{}`", r);
                            }
                            let _ = writeln!(out, "- Introduce tradeoff: `{}`", introduced);
                        }
                        ExperimentDimension::NovelComposition => {
                            out.push_str(
                                "**Experiment type:** NovelComposition\n\
                                 Compose entirely from primitives. No base composition.\n",
                            );
                        }
                    }
                    if let Some(base) = &exp.base_composition {
                        let _ = writeln!(out, "\n**Base composition:** `{}`", base);
                    }
                }
                out.push('\n');
                out.push_str(
                    "Your role is to construct a coherent agent from the specified primitives.\n\
                     The experiment design (what to vary) is algorithmic — do NOT override it.\n\n",
                );
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use tempfile::TempDir;

    use super::super::starters::{build_role, build_tradeoff};

    #[test]
    fn resolve_name_returns_name_as_content() {
        let skill = ContentRef::Name("my-skill".to_string());
        let resolved = resolve_skill(&skill, Path::new("/tmp")).unwrap();
        assert_eq!(resolved.name, "my-skill");
        assert_eq!(resolved.content, "my-skill");
    }

    #[test]
    fn resolve_inline_returns_content_directly() {
        let skill = ContentRef::Inline("do the thing well".to_string());
        let resolved = resolve_skill(&skill, Path::new("/tmp")).unwrap();
        assert_eq!(resolved.name, "inline");
        assert_eq!(resolved.content, "do the thing well");
    }

    #[test]
    fn resolve_file_absolute_path() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("skill.md");
        let mut f = fs::File::create(&file_path).unwrap();
        write!(f, "# Skill\nDo stuff").unwrap();

        let skill = ContentRef::File(file_path.clone());
        let resolved = resolve_skill(&skill, Path::new("/nonexistent")).unwrap();
        assert_eq!(resolved.name, "skill");
        assert_eq!(resolved.content, "# Skill\nDo stuff");
    }

    #[test]
    fn resolve_file_relative_path() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("skills").join("coding.txt");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, "Write good code").unwrap();

        let skill = ContentRef::File(PathBuf::from("skills/coding.txt"));
        let resolved = resolve_skill(&skill, dir.path()).unwrap();
        assert_eq!(resolved.name, "coding");
        assert_eq!(resolved.content, "Write good code");
    }

    #[test]
    fn resolve_file_missing_returns_error() {
        let skill = ContentRef::File(PathBuf::from("/no/such/file.md"));
        let result = resolve_skill(&skill, Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read skill file"));
    }

    #[test]
    fn resolve_all_returns_component_ids_as_names() {
        let role = build_role(
            "Test Role",
            "A test role",
            vec!["comp-1".to_string(), "comp-2".to_string()],
            "Testing",
        );

        let resolved = resolve_all_skills(&role, Path::new("/tmp"));
        // Each component_id is resolved as ContentRef::Name, returning it as-is
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "comp-1");
        assert_eq!(resolved[0].content, "comp-1");
        assert_eq!(resolved[1].name, "comp-2");
        assert_eq!(resolved[1].content, "comp-2");
    }

    #[test]
    fn expand_tilde_with_home() {
        let path = Path::new("~/some/file.txt");
        let expanded = expand_tilde(path);
        // Should not start with ~ anymore
        assert!(!expanded.starts_with("~"));
        assert!(expanded.ends_with("some/file.txt"));
    }

    #[test]
    fn expand_tilde_without_tilde() {
        let path = Path::new("/absolute/path.txt");
        let expanded = expand_tilde(path);
        assert_eq!(expanded, PathBuf::from("/absolute/path.txt"));
    }

    // -- Identity prompt rendering tests ------------------------------------

    fn sample_role() -> Role {
        build_role(
            "Implementer",
            "Writes code to fulfil task requirements.",
            vec!["rust".to_string(), "inline:fn main() {}".to_string()],
            "Working, tested code merged to main.",
        )
    }

    fn sample_tradeoff() -> TradeoffConfig {
        build_tradeoff(
            "Quality First",
            "Prioritise correctness and maintainability.",
            vec!["Slower delivery for higher quality".into()],
            vec!["Skipping tests".into()],
        )
    }

    #[test]
    fn test_render_identity_prompt_full() {
        let role = build_role(
            "Implementer",
            "Writes code to fulfil task requirements.",
            vec![],
            "Working, tested code merged to main.",
        );
        let tradeoff = build_tradeoff(
            "Quality First",
            "Prioritise correctness and maintainability.",
            vec![
                "Slower delivery for higher quality".into(),
                "More verbose code for clarity".into(),
            ],
            vec!["Skipping tests".into(), "Ignoring error handling".into()],
        );
        let skills = vec![
            ResolvedSkill {
                name: "Rust".into(),
                content: "Write idiomatic Rust code.".into(),
            },
            ResolvedSkill {
                name: "Testing".into(),
                content: "Write comprehensive tests.".into(),
            },
        ];

        let output = render_identity_prompt(&role, &tradeoff, &skills);

        // Verify structure
        assert!(output.starts_with("## Agent Identity\n"));
        assert!(output.contains("### Role: Implementer\n"));
        assert!(output.contains("Writes code to fulfil task requirements.\n"));
        assert!(output.contains("#### Skills\n"));
        assert!(output.contains("- **Rust**\nWrite idiomatic Rust code.\n"));
        assert!(output.contains("- **Testing**\nWrite comprehensive tests.\n"));
        assert!(output.contains("#### Desired Outcome\n"));
        assert!(output.contains("Working, tested code merged to main.\n"));
        assert!(output.contains("### Operational Parameters\n"));
        assert!(output.contains("#### Acceptable Trade-offs\n"));
        assert!(output.contains("- Slower delivery for higher quality\n"));
        assert!(output.contains("- More verbose code for clarity\n"));
        assert!(output.contains("#### Non-negotiable Constraints\n"));
        assert!(output.contains("- Skipping tests\n"));
        assert!(output.contains("- Ignoring error handling\n"));
        assert!(output.ends_with("---"));
    }

    #[test]
    fn test_render_identity_prompt_no_skills() {
        let role = build_role(
            "Reviewer",
            "Reviews code for quality.",
            vec![],
            "All code reviewed.",
        );
        let tradeoff = build_tradeoff(
            "Fast",
            "Be fast.",
            vec!["Less thorough reviews".into()],
            vec!["Missing security issues".into()],
        );

        let output = render_identity_prompt(&role, &tradeoff, &[]);

        // No Skills header when empty
        assert!(!output.contains("#### Skills\n"));
        // But everything else is present
        assert!(output.contains("### Role: Reviewer\n"));
        assert!(output.contains("#### Desired Outcome\n"));
        assert!(output.contains("#### Acceptable Trade-offs\n"));
        assert!(output.contains("- Less thorough reviews\n"));
        assert!(output.contains("#### Non-negotiable Constraints\n"));
        assert!(output.contains("- Missing security issues\n"));
    }

    #[test]
    fn test_render_identity_prompt_empty_tradeoffs() {
        let role = build_role("Minimal", "A minimal role.", vec![], "Done.");
        let tradeoff = build_tradeoff("Minimal Motivation", "Minimal.", vec![], vec![]);

        let output = render_identity_prompt(&role, &tradeoff, &[]);

        // Empty sections should be omitted entirely to save tokens
        assert!(!output.contains("### Operational Parameters\n"));
        assert!(!output.contains("#### Acceptable Trade-offs\n"));
        assert!(!output.contains("#### Non-negotiable Constraints\n"));
        assert!(output.ends_with("---"));
    }

    #[test]
    fn test_render_identity_prompt_section_order() {
        let role = sample_role();
        let tradeoff = sample_tradeoff();
        let skills = vec![ResolvedSkill {
            name: "Coding".into(),
            content: "Write code.".into(),
        }];

        let output = render_identity_prompt(&role, &tradeoff, &skills);

        // Verify sections appear in the correct order
        let agent_identity_pos = output.find("## Agent Identity").unwrap();
        let role_pos = output.find("### Role:").unwrap();
        let skills_pos = output.find("#### Skills").unwrap();
        let desired_outcome_pos = output.find("#### Desired Outcome").unwrap();
        let operational_pos = output.find("### Operational Parameters").unwrap();
        let acceptable_pos = output.find("#### Acceptable Trade-offs").unwrap();
        let constraints_pos = output.find("#### Non-negotiable Constraints").unwrap();
        let separator_pos = output.find("---").unwrap();

        assert!(agent_identity_pos < role_pos);
        assert!(role_pos < skills_pos);
        assert!(skills_pos < desired_outcome_pos);
        assert!(desired_outcome_pos < operational_pos);
        assert!(operational_pos < acceptable_pos);
        assert!(acceptable_pos < constraints_pos);
        assert!(constraints_pos < separator_pos);
    }

    #[test]
    fn test_render_identity_prompt_name_only_skills() {
        let role = build_role("Worker", "Does work.", vec![], "Work done.");
        let tradeoff = build_tradeoff("Fast", "Be fast.", vec!["Skip docs".into()], vec![]);
        let skills = vec![
            ResolvedSkill {
                name: "rust".into(),
                content: "rust".into(),
            },
            ResolvedSkill {
                name: "testing".into(),
                content: "testing".into(),
            },
        ];

        let output = render_identity_prompt(&role, &tradeoff, &skills);

        // Name-only skills should render as simple bullet items
        assert!(output.contains("- rust\n"));
        assert!(output.contains("- testing\n"));
        // Should NOT use bold or H3 formatting for name-only skills
        assert!(!output.contains("### rust"));
        assert!(!output.contains("**rust**"));
    }

    #[test]
    fn test_render_identity_prompt_partial_tradeoffs() {
        let role = build_role("Worker", "Does work.", vec![], "Work done.");
        // Only acceptable tradeoffs, no constraints
        let tradeoff = build_tradeoff("Fast", "Be fast.", vec!["Skip docs".into()], vec![]);

        let output = render_identity_prompt(&role, &tradeoff, &[]);

        assert!(output.contains("### Operational Parameters\n"));
        assert!(output.contains("#### Acceptable Trade-offs\n"));
        assert!(output.contains("- Skip docs\n"));
        // No constraints section
        assert!(!output.contains("#### Non-negotiable Constraints\n"));
    }

    // -- Evaluator prompt rendering tests -----------------------------------

    fn sample_log_entries() -> Vec<crate::graph::LogEntry> {
        vec![
            crate::graph::LogEntry {
                timestamp: "2025-05-01T10:00:00Z".into(),
                actor: Some("agent-1".into()),
                message: "Starting implementation".into(),
            },
            crate::graph::LogEntry {
                timestamp: "2025-05-01T10:30:00Z".into(),
                actor: None,
                message: "Completed core logic".into(),
            },
        ]
    }

    #[test]
    fn test_render_evaluator_prompt_full() {
        let role = sample_role();
        let tradeoff = sample_tradeoff();
        let artifacts = vec!["src/main.rs".to_string(), "tests/test_main.rs".to_string()];
        let log = sample_log_entries();

        let input = EvaluatorInput {
            task_title: "Implement feature X",
            task_description: Some("Build feature X with full test coverage."),
            task_skills: &["rust".to_string(), "testing".to_string()],
            verify: Some("All tests pass and code compiles without warnings."),
            agent: None,
            role: Some(&role),
            tradeoff: Some(&tradeoff),
            artifacts: &artifacts,
            log_entries: &log,
            started_at: Some("2025-05-01T10:00:00Z"),
            completed_at: Some("2025-05-01T11:00:00Z"),
            artifact_diff: None,
            evaluator_identity: None,
            downstream_tasks: &[],
            flip_score: None,
            verify_status: None,
            verify_findings: None,
        };

        let output = render_evaluator_prompt(&input);

        // System instructions
        assert!(output.starts_with("# Evaluator Instructions\n"));
        assert!(output.contains("You are an evaluator"));

        // Task definition
        assert!(output.contains("## Task Definition"));
        assert!(output.contains("**Title:** Implement feature X"));
        assert!(output.contains("Build feature X with full test coverage."));
        assert!(output.contains("- rust\n"));
        assert!(output.contains("- testing\n"));
        assert!(output.contains("**Verification Criteria:**"));
        assert!(output.contains("All tests pass and code compiles without warnings."));

        // Agent identity — IDs are content hashes
        assert!(output.contains("## Agent Identity"));
        assert!(output.contains(&format!("**Role:** Implementer ({})", role.id)));
        assert!(output.contains("**Desired Outcome:** Working, tested code merged to main."));
        assert!(output.contains(&format!("**Motivation:** Quality First ({})", tradeoff.id)));
        assert!(output.contains("**Acceptable Trade-offs:**"));
        assert!(output.contains("- Slower delivery for higher quality"));
        assert!(output.contains("**Non-negotiable Constraints:**"));
        assert!(output.contains("- Skipping tests"));

        // Artifacts
        assert!(output.contains("## Task Artifacts"));
        assert!(output.contains("- `src/main.rs`"));
        assert!(output.contains("- `tests/test_main.rs`"));

        // Log
        assert!(output.contains("## Task Log"));
        assert!(output.contains("(agent-1): Starting implementation"));
        assert!(output.contains("(system): Completed core logic"));

        // Timing
        assert!(output.contains("## Timing"));
        assert!(output.contains("- Started: 2025-05-01T10:00:00Z"));
        assert!(output.contains("- Completed: 2025-05-01T11:00:00Z"));

        // Evaluation criteria — individual quality
        assert!(output.contains("## Evaluation Criteria"));
        assert!(output.contains("### Individual Quality"));
        assert!(output.contains("**correctness**"));
        assert!(output.contains("**completeness**"));
        assert!(output.contains("**efficiency**"));
        assert!(output.contains("**style_adherence**"));

        // Evaluation criteria — organizational impact
        assert!(output.contains("### Organizational Impact"));
        assert!(output.contains("**downstream_usability**"));
        assert!(output.contains("**coordination_overhead**"));
        assert!(output.contains("**blocking_impact**"));

        // Weights
        assert!(output.contains("correctness: 30%"));
        assert!(output.contains("completeness: 20%"));
        assert!(output.contains("efficiency: 10%"));
        assert!(output.contains("downstream_usability: 15%"));
        assert!(output.contains("coordination_overhead: 10%"));
        assert!(output.contains("blocking_impact: 5%"));

        // Output format
        assert!(output.contains("## Required Output"));
        assert!(output.contains("\"score\""));
        assert!(output.contains("\"dimensions\""));
        assert!(output.contains("\"notes\""));
        assert!(output.contains("\"downstream_usability\""));
        assert!(output.contains("\"coordination_overhead\""));
        assert!(output.contains("\"blocking_impact\""));
    }

    #[test]
    fn test_render_evaluator_prompt_minimal() {
        let input = EvaluatorInput {
            task_title: "Simple task",
            task_description: None,
            task_skills: &[],
            verify: None,
            agent: None,
            role: None,
            tradeoff: None,
            artifacts: &[],
            log_entries: &[],
            started_at: None,
            completed_at: None,
            artifact_diff: None,
            evaluator_identity: None,
            downstream_tasks: &[],
            flip_score: None,
            verify_status: None,
            verify_findings: None,
        };

        let output = render_evaluator_prompt(&input);

        assert!(output.contains("**Title:** Simple task"));
        assert!(!output.contains("**Description:**"));
        assert!(!output.contains("**Required Skills:**"));
        assert!(!output.contains("**Verification Criteria:**"));
        assert!(output.contains("*No role was assigned.*"));
        assert!(output.contains("*No motivation was assigned.*"));
        assert!(output.contains("*No artifacts were recorded.*"));
        assert!(output.contains("*No log entries.*"));
        assert!(!output.contains("## Timing"));
        // Evaluation sections should always be present
        assert!(output.contains("## Evaluation Criteria"));
        assert!(output.contains("## Required Output"));
    }

    #[test]
    fn test_render_evaluator_prompt_section_order() {
        let role = sample_role();
        let tradeoff = sample_tradeoff();
        let log = sample_log_entries();

        let input = EvaluatorInput {
            task_title: "Test order",
            task_description: Some("desc"),
            task_skills: &["rust".to_string()],
            verify: Some("verify"),
            agent: None,
            role: Some(&role),
            tradeoff: Some(&tradeoff),
            artifacts: &["file.rs".to_string()],
            log_entries: &log,
            started_at: Some("2025-01-01T00:00:00Z"),
            completed_at: Some("2025-01-01T01:00:00Z"),
            artifact_diff: None,
            evaluator_identity: None,
            downstream_tasks: &[],
            flip_score: None,
            verify_status: None,
            verify_findings: None,
        };

        let output = render_evaluator_prompt(&input);

        let instructions_pos = output.find("# Evaluator Instructions").unwrap();
        let task_def_pos = output.find("## Task Definition").unwrap();
        let identity_pos = output.find("## Agent Identity").unwrap();
        let artifacts_pos = output.find("## Task Artifacts").unwrap();
        let log_pos = output.find("## Task Log").unwrap();
        let timing_pos = output.find("## Timing").unwrap();
        let criteria_pos = output.find("## Evaluation Criteria").unwrap();
        let required_pos = output.find("## Required Output").unwrap();

        assert!(instructions_pos < task_def_pos);
        assert!(task_def_pos < identity_pos);
        assert!(identity_pos < artifacts_pos);
        assert!(artifacts_pos < log_pos);
        assert!(log_pos < timing_pos);
        assert!(timing_pos < criteria_pos);
        assert!(criteria_pos < required_pos);
    }

    #[test]
    fn test_render_evaluator_prompt_with_downstream_tasks() {
        let downstream = vec![
            (
                "Deploy to staging".to_string(),
                "Open".to_string(),
                Some("Deploy the built artifacts to the staging environment.".to_string()),
            ),
            (
                "Run integration tests".to_string(),
                "InProgress".to_string(),
                None,
            ),
        ];

        let input = EvaluatorInput {
            task_title: "Build release package",
            task_description: Some("Create the release package."),
            task_skills: &[],
            verify: None,
            agent: None,
            role: None,
            tradeoff: None,
            artifacts: &[],
            log_entries: &[],
            started_at: None,
            completed_at: None,
            artifact_diff: None,
            evaluator_identity: None,
            downstream_tasks: &downstream,
            flip_score: None,
            verify_status: None,
            verify_findings: None,
        };

        let output = render_evaluator_prompt(&input);

        // Downstream tasks section should appear
        assert!(output.contains("## Downstream Tasks"));
        assert!(
            output
                .contains("These tasks depend on this task's output. Consider whether the output")
        );
        assert!(output.contains("**Deploy to staging** (Open)"));
        assert!(output.contains("Deploy the built artifacts to the staging environment."));
        assert!(output.contains("**Run integration tests** (InProgress)"));

        // Should NOT show the "no downstream tasks" note
        assert!(!output.contains("No downstream tasks are listed"));

        // Organizational impact dimensions should still be present
        assert!(output.contains("### Organizational Impact"));
        assert!(output.contains("**downstream_usability**"));
        assert!(output.contains("**coordination_overhead**"));
        assert!(output.contains("**blocking_impact**"));
    }

    #[test]
    fn test_render_evaluator_prompt_with_flip_verify() {
        let input = EvaluatorInput {
            task_title: "Task with verify",
            task_description: Some("A task that was verified."),
            task_skills: &[],
            verify: None,
            agent: None,
            role: None,
            tradeoff: None,
            artifacts: &[],
            log_entries: &[],
            started_at: None,
            completed_at: None,
            artifact_diff: None,
            evaluator_identity: None,
            downstream_tasks: &[],
            flip_score: Some(0.45),
            verify_status: Some("passed"),
            verify_findings: Some("[2025-01-01] (agent-1): Tests pass\n[2025-01-01] (agent-1): Artifacts verified"),
        };

        let output = render_evaluator_prompt(&input);

        // FLIP Verification Results section should appear
        assert!(output.contains("## FLIP Verification Results"));
        assert!(output.contains("FLIP Score: 0.45 (below threshold 0.70)"));
        assert!(output.contains("Verification Status: PASSED"));
        assert!(output.contains("Verification Findings:"));
        assert!(output.contains("Tests pass"));
        assert!(output.contains("Artifacts verified"));
        assert!(output.contains("NOTE: Verification is a strong signal"));
    }

    #[test]
    fn test_render_evaluator_prompt_no_flip_verify() {
        let input = EvaluatorInput {
            task_title: "Task without verify",
            task_description: None,
            task_skills: &[],
            verify: None,
            agent: None,
            role: None,
            tradeoff: None,
            artifacts: &[],
            log_entries: &[],
            started_at: None,
            completed_at: None,
            artifact_diff: None,
            evaluator_identity: None,
            downstream_tasks: &[],
            flip_score: None,
            verify_status: None,
            verify_findings: None,
        };

        let output = render_evaluator_prompt(&input);

        // FLIP Verification Results section should NOT appear
        assert!(!output.contains("## FLIP Verification Results"));
        assert!(!output.contains("FLIP Score"));
        assert!(!output.contains("Verification Status"));
        assert!(!output.contains("NOTE: Verification is a strong signal"));
    }

    #[test]
    fn test_render_evaluator_prompt_flip_above_threshold() {
        let input = EvaluatorInput {
            task_title: "High FLIP",
            task_description: None,
            task_skills: &[],
            verify: None,
            agent: None,
            role: None,
            tradeoff: None,
            artifacts: &[],
            log_entries: &[],
            started_at: None,
            completed_at: None,
            artifact_diff: None,
            evaluator_identity: None,
            downstream_tasks: &[],
            flip_score: Some(0.85),
            verify_status: None,
            verify_findings: None,
        };

        let output = render_evaluator_prompt(&input);

        assert!(output.contains("## FLIP Verification Results"));
        assert!(output.contains("FLIP Score: 0.85 (at or above threshold 0.70)"));
        assert!(!output.contains("Verification Status"));
    }

    // -- Rich component resolution tests ------------------------------------

    use super::super::starters::{build_component, build_outcome};
    use super::super::store::{save_component, save_outcome};

    fn setup_agency_dir(dir: &TempDir) -> PathBuf {
        let agency_dir = dir.path().join("agency");
        fs::create_dir_all(agency_dir.join("primitives/components")).unwrap();
        fs::create_dir_all(agency_dir.join("primitives/outcomes")).unwrap();
        agency_dir
    }

    #[test]
    fn resolve_all_components_loads_from_store() {
        let dir = TempDir::new().unwrap();
        let agency_dir = setup_agency_dir(&dir);

        // Create and save a component to the primitives store
        let comp = build_component(
            "Rust Expert",
            "Deep knowledge of Rust idioms and patterns.",
            ComponentCategory::Translated,
            ContentRef::Name("rust-expertise".into()),
        );
        save_component(&comp, &agency_dir.join("primitives/components")).unwrap();

        // Build a role that references this component by its hash ID
        let role = build_role(
            "Coder",
            "Writes code.",
            vec![comp.id.clone()],
            "Code works.",
        );

        let resolved = resolve_all_components(&role, dir.path(), &agency_dir);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "Rust Expert");
        assert!(resolved[0].content.contains("[Translated]"));
        assert!(
            resolved[0]
                .content
                .contains("Deep knowledge of Rust idioms and patterns.")
        );
    }

    #[test]
    fn resolve_all_components_falls_back_to_content_ref() {
        let dir = TempDir::new().unwrap();
        let agency_dir = setup_agency_dir(&dir);

        // Use inline: and plain name IDs — no store entries needed
        let role = build_role(
            "Worker",
            "Does work.",
            vec![
                "inline:write clean code".to_string(),
                "plain-tag".to_string(),
            ],
            "Done.",
        );

        let resolved = resolve_all_components(&role, dir.path(), &agency_dir);
        assert_eq!(resolved.len(), 2);
        // inline: gets resolved to its content
        assert_eq!(resolved[0].name, "inline");
        assert_eq!(resolved[0].content, "write clean code");
        // plain name falls back to name-as-content
        assert_eq!(resolved[1].name, "plain-tag");
        assert_eq!(resolved[1].content, "plain-tag");
    }

    #[test]
    fn resolve_all_components_mixed_store_and_fallback() {
        let dir = TempDir::new().unwrap();
        let agency_dir = setup_agency_dir(&dir);

        // One component in store
        let comp = build_component(
            "Testing",
            "Write comprehensive tests.",
            ComponentCategory::Enhanced,
            ContentRef::Inline("Always test edge cases.".into()),
        );
        save_component(&comp, &agency_dir.join("primitives/components")).unwrap();

        // Role with one store component and one inline content ref
        let role = build_role(
            "Test Worker",
            "Tests things.",
            vec![comp.id.clone(), "inline:extra skill".to_string()],
            "All tested.",
        );

        let resolved = resolve_all_components(&role, dir.path(), &agency_dir);
        assert_eq!(resolved.len(), 2);
        // Store component
        assert_eq!(resolved[0].name, "Testing");
        assert!(resolved[0].content.contains("[Enhanced]"));
        assert!(resolved[0].content.contains("Always test edge cases."));
        // Inline fallback
        assert_eq!(resolved[1].name, "inline");
        assert_eq!(resolved[1].content, "extra skill");
    }

    #[test]
    fn resolve_outcome_loads_from_store() {
        let dir = TempDir::new().unwrap();
        let agency_dir = setup_agency_dir(&dir);

        let outcome = build_outcome(
            "Production Ready",
            "Code is ready for production deployment.",
            vec![
                "All tests pass".into(),
                "No compiler warnings".into(),
                "Documentation updated".into(),
            ],
        );
        save_outcome(&outcome, &agency_dir.join("primitives/outcomes")).unwrap();

        let resolved = resolve_outcome(&outcome.id, &agency_dir);
        assert!(resolved.is_some());
        let resolved = resolved.unwrap();
        assert_eq!(resolved.name, "Production Ready");
        assert_eq!(resolved.success_criteria.len(), 3);
        assert_eq!(resolved.success_criteria[0], "All tests pass");
    }

    #[test]
    fn resolve_outcome_returns_none_for_missing() {
        let dir = TempDir::new().unwrap();
        let agency_dir = setup_agency_dir(&dir);

        let resolved = resolve_outcome("nonexistent-hash", &agency_dir);
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_outcome_returns_none_for_empty_id() {
        let dir = TempDir::new().unwrap();
        let agency_dir = setup_agency_dir(&dir);

        let resolved = resolve_outcome("", &agency_dir);
        assert!(resolved.is_none());
    }

    // -- render_identity_prompt_rich tests -----------------------------------

    #[test]
    fn test_render_identity_prompt_rich_with_outcome() {
        let role = build_role("Coder", "Writes code.", vec![], "some-hash-id");
        let tradeoff = build_tradeoff("Balanced", "Balance quality and speed.", vec![], vec![]);
        let outcome = build_outcome(
            "Working Software",
            "Deliver working, tested software.",
            vec!["All tests pass".into(), "No regressions".into()],
        );

        let output = render_identity_prompt_rich(&role, &tradeoff, &[], Some(&outcome));

        assert!(output.contains("#### Desired Outcome\n"));
        assert!(output.contains("**Working Software**"));
        assert!(output.contains("Deliver working, tested software."));
        assert!(output.contains("**Success Criteria:**"));
        assert!(output.contains("- All tests pass"));
        assert!(output.contains("- No regressions"));
        // Should NOT contain the raw hash ID
        assert!(!output.contains("some-hash-id"));
    }

    #[test]
    fn test_render_identity_prompt_rich_without_outcome_matches_original() {
        let role = sample_role();
        let tradeoff = sample_tradeoff();
        let skills = vec![ResolvedSkill {
            name: "Coding".into(),
            content: "Write code.".into(),
        }];

        let original = render_identity_prompt(&role, &tradeoff, &skills);
        let rich = render_identity_prompt_rich(&role, &tradeoff, &skills, None);

        assert_eq!(original, rich);
    }

    #[test]
    fn test_render_identity_prompt_rich_outcome_no_criteria() {
        let role = build_role("Worker", "Does work.", vec![], "hash-id");
        let tradeoff = build_tradeoff("Fast", "Be fast.", vec![], vec![]);
        let outcome = build_outcome(
            "Work Complete",
            "All work is complete.",
            vec![], // no success criteria
        );

        let output = render_identity_prompt_rich(&role, &tradeoff, &[], Some(&outcome));

        assert!(output.contains("**Work Complete**"));
        assert!(output.contains("All work is complete."));
        // No criteria section when empty
        assert!(!output.contains("**Success Criteria:**"));
    }
}

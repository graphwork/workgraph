use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use super::hash::short_hash;
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
pub fn resolve_skill(skill: &SkillRef, workgraph_root: &Path) -> Result<ResolvedSkill, String> {
    match skill {
        SkillRef::Name(name) => Ok(ResolvedSkill {
            name: name.clone(),
            content: name.clone(),
        }),
        SkillRef::File(path) => {
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
        SkillRef::Url(url) => resolve_url(url),
        SkillRef::Inline(content) => Ok(ResolvedSkill {
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
    role.skills
        .iter()
        .filter_map(|skill_ref| match resolve_skill(skill_ref, workgraph_root) {
            Ok(resolved) => Some(resolved),
            Err(warning) => {
                eprintln!("Warning: {}", warning);
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------------

/// Render the identity section to inject into agent prompts.
///
/// The output is placed between system context and task description in the prompt.
pub fn render_identity_prompt(
    role: &Role,
    motivation: &Motivation,
    resolved_skills: &[ResolvedSkill],
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
    let _ = writeln!(out, "{}\n", role.desired_outcome);

    let has_tradeoffs = !motivation.acceptable_tradeoffs.is_empty();
    let has_constraints = !motivation.unacceptable_tradeoffs.is_empty();

    if has_tradeoffs || has_constraints {
        out.push_str("### Operational Parameters\n");

        if has_tradeoffs {
            out.push_str("#### Acceptable Trade-offs\n");
            for tradeoff in &motivation.acceptable_tradeoffs {
                let _ = writeln!(out, "- {}", tradeoff);
            }
            out.push('\n');
        }

        if has_constraints {
            out.push_str("#### Non-negotiable Constraints\n");
            for constraint in &motivation.unacceptable_tradeoffs {
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
    /// Motivation used by the agent (if identity was assigned)
    pub motivation: Option<&'a Motivation>,
    /// Produced artifacts (file paths / references)
    pub artifacts: &'a [String],
    /// Progress log entries
    pub log_entries: &'a [crate::graph::LogEntry],
    /// Time the task started (ISO 8601, if available)
    pub started_at: Option<&'a str>,
    /// Time the task completed (ISO 8601, if available)
    pub completed_at: Option<&'a str>,
}

/// Render the evaluator prompt that an LLM evaluator will receive.
///
/// The output is a self-contained prompt instructing the evaluator to assess
/// the agent's work and return structured JSON.
pub fn render_evaluator_prompt(input: &EvaluatorInput) -> String {
    let mut out = String::new();

    // -- System instructions --
    out.push_str("# Evaluator Instructions\n\n");
    out.push_str(
        "You are an evaluator assessing the quality of work performed by an AI agent.\n\
         Review the task definition, the agent identity that was used, the produced artifacts,\n\
         and the task log. Then produce a JSON evaluation.\n\n",
    );

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
        let _ = writeln!(out, "**Desired Outcome:** {}\n", role.desired_outcome);
    } else {
        out.push_str("*No role was assigned.*\n\n");
    }
    if let Some(motivation) = input.motivation {
        let _ = writeln!(
            out,
            "**Motivation:** {} ({})",
            motivation.name, motivation.id
        );
        let _ = writeln!(out, "{}\n", motivation.description);
        if !motivation.acceptable_tradeoffs.is_empty() {
            out.push_str("**Acceptable Trade-offs:**\n");
            for t in &motivation.acceptable_tradeoffs {
                let _ = writeln!(out, "- {}", t);
            }
            out.push('\n');
        }
        if !motivation.unacceptable_tradeoffs.is_empty() {
            out.push_str("**Non-negotiable Constraints:**\n");
            for c in &motivation.unacceptable_tradeoffs {
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

    // -- Evaluation rubric & output format --
    out.push_str("## Evaluation Criteria\n\n");
    out.push_str(
        "Assess the agent's work on these dimensions (each scored 0.0 to 1.0):\n\n\
         1. **correctness** — Does the output match the desired outcome? Are verification\n\
            criteria satisfied? Is the implementation functionally correct?\n\
         2. **completeness** — Were all aspects of the task addressed? Are there missing\n\
            pieces, unhandled edge cases, or incomplete deliverables?\n\
         3. **efficiency** — Was the work done efficiently within the allowed parameters?\n\
            Minimal unnecessary steps, no wasted effort, appropriate scope.\n\
         4. **style_adherence** — Does the output follow project conventions, coding\n\
            standards, and the constraints set by the motivation (trade-offs respected,\n\
            non-negotiable constraints honoured)?\n\n",
    );

    out.push_str(
        "Compute an overall **score** as a weighted average:\n\
         - correctness: 40%\n\
         - completeness: 30%\n\
         - efficiency: 15%\n\
         - style_adherence: 15%\n\n",
    );

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
             \"style_adherence\": <0.0-1.0>\n  \
           },\n  \
           \"notes\": \"<brief explanation of strengths, weaknesses, and suggestions>\"\n\
         }\n\
         ```\n",
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use tempfile::TempDir;

    use super::super::starters::{build_motivation, build_role};

    fn test_role(skills: Vec<SkillRef>) -> Role {
        build_role("Test Role", "A test role", skills, "Testing")
    }

    #[test]
    fn resolve_name_returns_name_as_content() {
        let skill = SkillRef::Name("my-skill".to_string());
        let resolved = resolve_skill(&skill, Path::new("/tmp")).unwrap();
        assert_eq!(resolved.name, "my-skill");
        assert_eq!(resolved.content, "my-skill");
    }

    #[test]
    fn resolve_inline_returns_content_directly() {
        let skill = SkillRef::Inline("do the thing well".to_string());
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

        let skill = SkillRef::File(file_path.clone());
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

        let skill = SkillRef::File(PathBuf::from("skills/coding.txt"));
        let resolved = resolve_skill(&skill, dir.path()).unwrap();
        assert_eq!(resolved.name, "coding");
        assert_eq!(resolved.content, "Write good code");
    }

    #[test]
    fn resolve_file_missing_returns_error() {
        let skill = SkillRef::File(PathBuf::from("/no/such/file.md"));
        let result = resolve_skill(&skill, Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read skill file"));
    }

    #[test]
    fn resolve_all_skips_failures_gracefully() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("good.md");
        fs::write(&file_path, "good content").unwrap();

        let role = test_role(vec![
            SkillRef::Name("tag-only".to_string()),
            SkillRef::File(PathBuf::from("/no/such/file.md")),
            SkillRef::File(file_path),
            SkillRef::Inline("inline content".to_string()),
        ]);

        let resolved = resolve_all_skills(&role, dir.path());
        // The missing file should be skipped, leaving 3 resolved skills
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].name, "tag-only");
        assert_eq!(resolved[1].name, "good");
        assert_eq!(resolved[1].content, "good content");
        assert_eq!(resolved[2].name, "inline");
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
            vec![
                SkillRef::Name("rust".into()),
                SkillRef::Inline("fn main() {}".into()),
            ],
            "Working, tested code merged to main.",
        )
    }

    fn sample_motivation() -> Motivation {
        build_motivation(
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
        let motivation = build_motivation(
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

        let output = render_identity_prompt(&role, &motivation, &skills);

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
        let motivation = build_motivation(
            "Fast",
            "Be fast.",
            vec!["Less thorough reviews".into()],
            vec!["Missing security issues".into()],
        );

        let output = render_identity_prompt(&role, &motivation, &[]);

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
        let motivation = build_motivation("Minimal Motivation", "Minimal.", vec![], vec![]);

        let output = render_identity_prompt(&role, &motivation, &[]);

        // Empty sections should be omitted entirely to save tokens
        assert!(!output.contains("### Operational Parameters\n"));
        assert!(!output.contains("#### Acceptable Trade-offs\n"));
        assert!(!output.contains("#### Non-negotiable Constraints\n"));
        assert!(output.ends_with("---"));
    }

    #[test]
    fn test_render_identity_prompt_section_order() {
        let role = sample_role();
        let motivation = sample_motivation();
        let skills = vec![ResolvedSkill {
            name: "Coding".into(),
            content: "Write code.".into(),
        }];

        let output = render_identity_prompt(&role, &motivation, &skills);

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
        let motivation = build_motivation("Fast", "Be fast.", vec!["Skip docs".into()], vec![]);
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

        let output = render_identity_prompt(&role, &motivation, &skills);

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
        let motivation = build_motivation("Fast", "Be fast.", vec!["Skip docs".into()], vec![]);

        let output = render_identity_prompt(&role, &motivation, &[]);

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
        let motivation = sample_motivation();
        let artifacts = vec!["src/main.rs".to_string(), "tests/test_main.rs".to_string()];
        let log = sample_log_entries();

        let input = EvaluatorInput {
            task_title: "Implement feature X",
            task_description: Some("Build feature X with full test coverage."),
            task_skills: &["rust".to_string(), "testing".to_string()],
            verify: Some("All tests pass and code compiles without warnings."),
            agent: None,
            role: Some(&role),
            motivation: Some(&motivation),
            artifacts: &artifacts,
            log_entries: &log,
            started_at: Some("2025-05-01T10:00:00Z"),
            completed_at: Some("2025-05-01T11:00:00Z"),
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
        assert!(output.contains(&format!(
            "**Motivation:** Quality First ({})",
            motivation.id
        )));
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

        // Evaluation criteria
        assert!(output.contains("## Evaluation Criteria"));
        assert!(output.contains("**correctness**"));
        assert!(output.contains("**completeness**"));
        assert!(output.contains("**efficiency**"));
        assert!(output.contains("**style_adherence**"));

        // Weights
        assert!(output.contains("correctness: 40%"));
        assert!(output.contains("completeness: 30%"));
        assert!(output.contains("efficiency: 15%"));

        // Output format
        assert!(output.contains("## Required Output"));
        assert!(output.contains("\"score\""));
        assert!(output.contains("\"dimensions\""));
        assert!(output.contains("\"notes\""));
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
            motivation: None,
            artifacts: &[],
            log_entries: &[],
            started_at: None,
            completed_at: None,
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
        let motivation = sample_motivation();
        let log = sample_log_entries();

        let input = EvaluatorInput {
            task_title: "Test order",
            task_description: Some("desc"),
            task_skills: &["rust".to_string()],
            verify: Some("verify"),
            agent: None,
            role: Some(&role),
            motivation: Some(&motivation),
            artifacts: &["file.rs".to_string()],
            log_entries: &log,
            started_at: Some("2025-01-01T00:00:00Z"),
            completed_at: Some("2025-01-01T01:00:00Z"),
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
}

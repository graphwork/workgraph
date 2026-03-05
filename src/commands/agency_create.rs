//! `wg agency create` — invoke the creator agent to expand the primitive store.
//!
//! Searches outside the agency for new role components, desired outcomes, and
//! tradeoff configs by examining project files (CLAUDE.md, docs/, README, etc.)
//! and proposing primitives that aren't yet in the store.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

use workgraph::agency::{
    self, AgencyStore, LocalStore, StoreCounts, load_role, load_tradeoff,
    render_identity_prompt_rich, resolve_all_components, resolve_outcome,
};
use workgraph::config::Config;

/// Build the creator prompt that instructs the LLM to propose new primitives.
fn build_creator_prompt(
    agency_dir: &Path,
    project_context: &str,
    existing_counts: &StoreCounts,
    config: &Config,
) -> String {
    let store = LocalStore::new(agency_dir);

    // Collect existing primitive names to avoid duplicates
    let mut existing_components = Vec::new();
    if let Ok(comps) = store.load_components() {
        for c in &comps {
            existing_components.push(format!(
                "  - {} ({}): {}",
                c.name,
                agency::short_hash(&c.id),
                c.description
            ));
        }
    }

    let mut existing_outcomes = Vec::new();
    if let Ok(outs) = store.load_outcomes() {
        for o in &outs {
            existing_outcomes.push(format!(
                "  - {} ({}): {}",
                o.name,
                agency::short_hash(&o.id),
                o.description
            ));
        }
    }

    let mut existing_tradeoffs = Vec::new();
    if let Ok(tradeoffs) = store.load_tradeoffs() {
        for t in &tradeoffs {
            existing_tradeoffs.push(format!(
                "  - {} ({}): {}",
                t.name,
                agency::short_hash(&t.id),
                t.description
            ));
        }
    }

    let comps_section = if existing_components.is_empty() {
        "  (none)".to_string()
    } else {
        existing_components.join("\n")
    };
    let outs_section = if existing_outcomes.is_empty() {
        "  (none)".to_string()
    } else {
        existing_outcomes.join("\n")
    };
    let tradeoffs_section = if existing_tradeoffs.is_empty() {
        "  (none)".to_string()
    } else {
        existing_tradeoffs.join("\n")
    };

    // Creator identity: use component-based rendering when configured, else hardcoded fallback
    let creator_intro = if let Some(ref creator_hash) = config.agency.creator_agent {
        let agents_dir = agency_dir.join("cache/agents");
        let agent_path = agents_dir.join(format!("{}.yaml", creator_hash));
        if let Ok(agent) = agency::load_agent(&agent_path) {
            let roles_dir = agency_dir.join("cache/roles");
            let role_path = roles_dir.join(format!("{}.yaml", agent.role_id));
            let tradeoffs_dir = agency_dir.join("primitives/tradeoffs");
            let tradeoff_path = tradeoffs_dir.join(format!("{}.yaml", agent.tradeoff_id));
            if let (Ok(role), Ok(tradeoff)) = (load_role(&role_path), load_tradeoff(&tradeoff_path))
            {
                let workgraph_root = agency_dir.parent().unwrap_or(agency_dir);
                let resolved_skills = resolve_all_components(&role, workgraph_root, agency_dir);
                let outcome = resolve_outcome(&role.outcome_id, agency_dir);
                let identity = render_identity_prompt_rich(
                    &role,
                    &tradeoff,
                    &resolved_skills,
                    outcome.as_ref(),
                );
                format!(
                    "{}\n\nYour job is to expand the primitive store by discovering new role \
                     components, desired outcomes, and tradeoff configurations that are implied \
                     by the project but not yet captured in the agency.",
                    identity
                )
            } else {
                "You are the Agency Creator agent. Your job is to expand the primitive store by\n\
                 discovering new role components, desired outcomes, and tradeoff configurations\n\
                 that are implied by the project but not yet captured in the agency."
                    .to_string()
            }
        } else {
            "You are the Agency Creator agent. Your job is to expand the primitive store by\n\
             discovering new role components, desired outcomes, and tradeoff configurations\n\
             that are implied by the project but not yet captured in the agency."
                .to_string()
        }
    } else {
        "You are the Agency Creator agent. Your job is to expand the primitive store by\n\
         discovering new role components, desired outcomes, and tradeoff configurations\n\
         that are implied by the project but not yet captured in the agency."
            .to_string()
    };

    format!(
        r#"{creator_intro}

## Current Primitive Store

Components ({comp_count}):
{comps}

Desired Outcomes ({out_count}):
{outs}

Tradeoff Configs ({tradeoff_count}):
{tradeoffs}

## Project Context

{context}

## Instructions

1. Read the project context above carefully.
2. Identify capabilities, outcomes, and tradeoff considerations that the project
   needs but that are NOT already represented in the primitive store.
3. Propose new primitives by outputting a JSON object with this schema:

```json
{{
  "components": [
    {{
      "name": "string",
      "description": "string — what capability this represents",
      "category": "translated|enhanced|novel"
    }}
  ],
  "outcomes": [
    {{
      "name": "string",
      "description": "string — what success looks like",
      "success_criteria": ["criterion1", "criterion2"]
    }}
  ],
  "tradeoffs": [
    {{
      "name": "string",
      "description": "string — what this tradeoff governs",
      "acceptable": ["acceptable tradeoff 1"],
      "unacceptable": ["unacceptable tradeoff 1"]
    }}
  ]
}}
```

Rules:
- Do NOT duplicate existing primitives (check names and descriptions above).
- Only propose primitives that are genuinely needed by the project.
- Keep descriptions concise (1-2 sentences).
- Propose 1-5 primitives total — quality over quantity.
- Output ONLY the JSON object, no markdown fencing, no explanation.
"#,
        comp_count = existing_counts.components,
        comps = comps_section,
        out_count = existing_counts.outcomes,
        outs = outs_section,
        tradeoff_count = existing_counts.tradeoffs,
        tradeoffs = tradeoffs_section,
        context = project_context,
    )
}

/// Gather project context from standard files (CLAUDE.md, README, docs/).
fn gather_project_context(workgraph_dir: &Path) -> String {
    let project_root = workgraph_dir.parent().unwrap_or(workgraph_dir);

    let mut context_parts = Vec::new();

    // Try CLAUDE.md
    let claude_md = project_root.join("CLAUDE.md");
    if claude_md.exists()
        && let Ok(content) = std::fs::read_to_string(&claude_md)
    {
        let truncated = if content.len() > 3000 {
            &content[..3000]
        } else {
            &content
        };
        context_parts.push(format!("### CLAUDE.md\n{}", truncated));
    }

    // Try README.md or README
    for name in &["README.md", "README", "README.txt"] {
        let readme = project_root.join(name);
        if readme.exists() {
            if let Ok(content) = std::fs::read_to_string(&readme) {
                let truncated = if content.len() > 3000 {
                    &content[..3000]
                } else {
                    &content
                };
                context_parts.push(format!("### {}\n{}", name, truncated));
            }
            break;
        }
    }

    // Try Cargo.toml or package.json for project description
    let cargo_toml = project_root.join("Cargo.toml");
    if cargo_toml.exists()
        && let Ok(content) = std::fs::read_to_string(&cargo_toml)
    {
        // Just the [package] section
        if let Some(pkg_start) = content.find("[package]") {
            let section_end = content[pkg_start + 9..]
                .find("\n[")
                .map(|i| pkg_start + 9 + i)
                .unwrap_or(content.len().min(pkg_start + 500));
            context_parts.push(format!(
                "### Cargo.toml [package]\n{}",
                &content[pkg_start..section_end]
            ));
        }
    }

    if context_parts.is_empty() {
        "No project context files found.".to_string()
    } else {
        context_parts.join("\n\n")
    }
}

/// Structured output from the creator agent.
#[derive(Debug, serde::Deserialize)]
struct CreatorOutput {
    #[serde(default)]
    components: Vec<ProposedComponent>,
    #[serde(default)]
    outcomes: Vec<ProposedOutcome>,
    #[serde(default)]
    tradeoffs: Vec<ProposedTradeoff>,
}

#[derive(Debug, serde::Deserialize)]
struct ProposedComponent {
    name: String,
    description: String,
    #[serde(default = "default_category")]
    category: String,
}

fn default_category() -> String {
    "skill".to_string()
}

#[derive(Debug, serde::Deserialize)]
struct ProposedOutcome {
    name: String,
    description: String,
    #[serde(default)]
    success_criteria: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ProposedTradeoff {
    name: String,
    description: String,
    #[serde(default)]
    acceptable: Vec<String>,
    #[serde(default)]
    unacceptable: Vec<String>,
}

/// Parse category string to ComponentCategory enum.
fn parse_category(s: &str) -> agency::ComponentCategory {
    match s.to_lowercase().as_str() {
        "translated" | "skill" => agency::ComponentCategory::Translated,
        "enhanced" | "knowledge" => agency::ComponentCategory::Enhanced,
        "novel" | "behaviour" | "behavior" | "communication" | "meta" => {
            agency::ComponentCategory::Novel
        }
        _ => agency::ComponentCategory::Translated,
    }
}

/// Parse the creator output JSON from raw LLM output.
fn parse_creator_output(raw: &str) -> Result<CreatorOutput> {
    // Try to find JSON in the output (may have markdown fencing)
    let trimmed = raw.trim();

    // Strip markdown code fences if present
    let json_str = if trimmed.starts_with("```") {
        let start = trimmed.find('\n').map(|i| i + 1).unwrap_or(0);
        let end = trimmed.rfind("```").unwrap_or(trimmed.len());
        &trimmed[start..end]
    } else {
        trimmed
    };

    // Try to find the JSON object
    let json_start = json_str.find('{');
    let json_end = json_str.rfind('}');

    match (json_start, json_end) {
        (Some(start), Some(end)) if end > start => {
            let json_slice = &json_str[start..=end];
            serde_json::from_str(json_slice)
                .with_context(|| "Failed to parse creator JSON output".to_string())
        }
        _ => bail!("No JSON object found in creator output"),
    }
}

/// Run `wg agency create` — invoke the creator agent.
pub fn run(dir: &Path, model: Option<&str>, dry_run: bool, json: bool) -> Result<()> {
    let agency_dir = dir.join("agency");
    let store = LocalStore::new(&agency_dir);
    let counts = store.entity_counts();

    // Validate agency exists
    if !store.is_valid() {
        bail!("Agency not initialized. Run `wg agency init` first.");
    }

    // Pre-flight: check that claude CLI is available
    if Command::new("claude")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDECODE")
        .arg("--version")
        .output()
        .is_err()
    {
        bail!(
            "The 'claude' CLI is required for agency create but was not found in PATH.\n\
             Install it from https://docs.anthropic.com/en/docs/claude-code and ensure it is on your PATH."
        );
    }

    // Load config for model resolution
    let config = Config::load_or_default(dir);

    // Determine model: CLI flag > model routing > legacy config > agent.model
    let model = model
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| {
            config
                .resolve_model_for_role(workgraph::config::DispatchRole::Creator)
                .model
        });

    // Gather project context
    let project_context = gather_project_context(dir);

    // Build the creator prompt
    let prompt = build_creator_prompt(&agency_dir, &project_context, &counts, &config);

    if dry_run {
        if json {
            let out = serde_json::json!({
                "mode": "dry_run",
                "model": model,
                "existing_counts": {
                    "components": counts.components,
                    "outcomes": counts.outcomes,
                    "tradeoffs": counts.tradeoffs,
                },
                "prompt_length": prompt.len(),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("=== Dry Run: wg agency create ===\n");
            println!("Model:           {}", model);
            println!("Components:      {}", counts.components);
            println!("Outcomes:        {}", counts.outcomes);
            println!("Tradeoffs:       {}", counts.tradeoffs);
            println!("Prompt length:   {} chars", prompt.len());
            println!("\n--- Creator Prompt ---\n");
            println!("{}", prompt);
        }
        return Ok(());
    }

    // Spawn the creator agent
    eprintln!("Running creator agent (model: {})...", model);

    let output = Command::new("claude")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDECODE")
        .arg("--model")
        .arg(&model)
        .arg("--print")
        .arg("--dangerously-skip-permissions")
        .arg(&prompt)
        .output()
        .context("Failed to run claude CLI — is it installed and in PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Creator agent failed (exit code {:?}):\n{}",
            output.status.code(),
            stderr
        );
    }

    let raw_output = String::from_utf8_lossy(&output.stdout);

    // Parse the structured output
    let creator_output =
        parse_creator_output(&raw_output).context("Failed to parse creator output")?;

    let total_proposed = creator_output.components.len()
        + creator_output.outcomes.len()
        + creator_output.tradeoffs.len();

    if total_proposed == 0 {
        if json {
            println!(
                "{}",
                serde_json::json!({"created": 0, "message": "No new primitives proposed"})
            );
        } else {
            println!("No new primitives proposed by the creator.");
        }
        return Ok(());
    }

    // Apply proposed primitives
    let mut results = Vec::new();

    // Create components
    let components_dir = store.components_dir();
    std::fs::create_dir_all(&components_dir)?;
    for comp in &creator_output.components {
        let category = parse_category(&comp.category);
        let component = agency::build_component(
            &comp.name,
            &comp.description,
            category,
            agency::ContentRef::Inline(comp.description.clone()),
        );

        // Skip duplicates
        if components_dir
            .join(format!("{}.yaml", component.id))
            .exists()
        {
            eprintln!("  Skipping duplicate component: {}", comp.name);
            continue;
        }

        let path = agency::save_component(&component, &components_dir)?;
        results.push(serde_json::json!({
            "type": "component",
            "name": comp.name,
            "id": agency::short_hash(&component.id),
            "path": path.display().to_string(),
        }));
        eprintln!(
            "  Created component: {} ({})",
            comp.name,
            agency::short_hash(&component.id)
        );
    }

    // Create outcomes
    let outcomes_dir = store.outcomes_dir();
    std::fs::create_dir_all(&outcomes_dir)?;
    for out in &creator_output.outcomes {
        let outcome =
            agency::build_outcome(&out.name, &out.description, out.success_criteria.clone());

        if outcomes_dir.join(format!("{}.yaml", outcome.id)).exists() {
            eprintln!("  Skipping duplicate outcome: {}", out.name);
            continue;
        }

        let path = agency::save_outcome(&outcome, &outcomes_dir)?;
        results.push(serde_json::json!({
            "type": "outcome",
            "name": out.name,
            "id": agency::short_hash(&outcome.id),
            "path": path.display().to_string(),
        }));
        eprintln!(
            "  Created outcome: {} ({})",
            out.name,
            agency::short_hash(&outcome.id)
        );
    }

    // Create tradeoffs
    let tradeoffs_dir = store.tradeoffs_dir();
    std::fs::create_dir_all(&tradeoffs_dir)?;
    for tc in &creator_output.tradeoffs {
        let tradeoff = agency::build_tradeoff(
            &tc.name,
            &tc.description,
            tc.acceptable.clone(),
            tc.unacceptable.clone(),
        );

        if tradeoffs_dir.join(format!("{}.yaml", tradeoff.id)).exists() {
            eprintln!("  Skipping duplicate tradeoff: {}", tc.name);
            continue;
        }

        let path = agency::save_tradeoff(&tradeoff, &tradeoffs_dir)?;
        results.push(serde_json::json!({
            "type": "tradeoff",
            "name": tc.name,
            "id": agency::short_hash(&tradeoff.id),
            "path": path.display().to_string(),
        }));
        eprintln!(
            "  Created tradeoff: {} ({})",
            tc.name,
            agency::short_hash(&tradeoff.id)
        );
    }

    if json {
        let out = serde_json::json!({
            "created": results.len(),
            "primitives": results,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("\nCreated {} new primitive(s).", results.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_creator_output_valid() {
        let raw = r#"{"components": [{"name": "Test", "description": "A test", "category": "skill"}], "outcomes": [], "tradeoffs": []}"#;
        let output = parse_creator_output(raw).unwrap();
        assert_eq!(output.components.len(), 1);
        assert_eq!(output.components[0].name, "Test");
    }

    #[test]
    fn test_parse_creator_output_with_fences() {
        let raw = "```json\n{\"components\": [], \"outcomes\": [{\"name\": \"O1\", \"description\": \"desc\", \"success_criteria\": []}], \"tradeoffs\": []}\n```";
        let output = parse_creator_output(raw).unwrap();
        assert_eq!(output.outcomes.len(), 1);
    }

    #[test]
    fn test_parse_creator_output_empty() {
        let raw = r#"{"components": [], "outcomes": [], "tradeoffs": []}"#;
        let output = parse_creator_output(raw).unwrap();
        assert_eq!(output.components.len(), 0);
        assert_eq!(output.outcomes.len(), 0);
        assert_eq!(output.tradeoffs.len(), 0);
    }

    #[test]
    fn test_parse_creator_output_no_json() {
        let raw = "I couldn't find any new primitives to propose.";
        assert!(parse_creator_output(raw).is_err());
    }

    #[test]
    fn test_parse_category() {
        assert!(matches!(
            parse_category("translated"),
            agency::ComponentCategory::Translated
        ));
        assert!(matches!(
            parse_category("skill"),
            agency::ComponentCategory::Translated
        ));
        assert!(matches!(
            parse_category("Enhanced"),
            agency::ComponentCategory::Enhanced
        ));
        assert!(matches!(
            parse_category("novel"),
            agency::ComponentCategory::Novel
        ));
        assert!(matches!(
            parse_category("behaviour"),
            agency::ComponentCategory::Novel
        ));
        assert!(matches!(
            parse_category("unknown"),
            agency::ComponentCategory::Translated
        ));
    }

    #[test]
    fn test_build_creator_prompt_includes_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let agency_dir = tmp.path().join("agency");
        std::fs::create_dir_all(agency_dir.join("primitives/components")).unwrap();
        std::fs::create_dir_all(agency_dir.join("primitives/outcomes")).unwrap();
        std::fs::create_dir_all(agency_dir.join("primitives/tradeoffs")).unwrap();
        std::fs::create_dir_all(agency_dir.join("cache/roles")).unwrap();
        std::fs::create_dir_all(agency_dir.join("cache/agents")).unwrap();
        std::fs::create_dir_all(agency_dir.join("evaluations")).unwrap();

        let store = LocalStore::new(&agency_dir);
        let counts = store.entity_counts();

        let config = Config::default();
        let prompt = build_creator_prompt(&agency_dir, "Test project", &counts, &config);
        assert!(prompt.contains("Test project"));
        assert!(prompt.contains("Components (0)"));
    }
}

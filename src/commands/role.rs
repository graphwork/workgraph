use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use workgraph::agency::{self, SkillRef};

/// JSON output for role listing
#[derive(Debug, Serialize)]
struct RoleSummary {
    id: String,
    name: String,
    skill_count: usize,
    avg_score: Option<f64>,
}

/// Parse a skill specification string into a SkillRef.
///
/// Formats:
///   "name:file:///path"      -> SkillRef::File
///   "name:https://url"       -> SkillRef::Url
///   "name:http://url"        -> SkillRef::Url
///   "name:inline:content"    -> SkillRef::Inline
///   "name"                   -> SkillRef::Name (tag-only)
fn parse_skill_ref(spec: &str) -> SkillRef {
    // Try "name:file:///path"
    if let Some(rest) = spec.strip_prefix("file:///") {
        return SkillRef::File(std::path::PathBuf::from(format!("/{}", rest)));
    }
    if let Some(idx) = spec.find(":file:///") {
        let path = &spec[idx + ":file://".len()..];
        return SkillRef::File(std::path::PathBuf::from(path));
    }

    // Try "name:https://url" or "name:http://url"
    if let Some(idx) = spec.find(":https://") {
        let url = &spec[idx + 1..];
        return SkillRef::Url(url.to_string());
    }
    if let Some(idx) = spec.find(":http://") {
        let url = &spec[idx + 1..];
        return SkillRef::Url(url.to_string());
    }

    // Try "name:inline:content"
    if let Some(idx) = spec.find(":inline:") {
        let content = &spec[idx + ":inline:".len()..];
        return SkillRef::Inline(content.to_string());
    }

    // Tag-only
    SkillRef::Name(spec.to_string())
}

/// wg role add <name> --outcome <desired_outcome> [--skill <spec>...] [--description <desc>]
pub fn run_add(
    dir: &Path,
    name: &str,
    outcome: &str,
    skills: &[String],
    description: Option<&str>,
) -> Result<()> {
    let agency_dir = dir.join("agency");
    agency::init(&agency_dir).context("Failed to initialize agency directory")?;

    let roles_dir = agency_dir.join("roles");

    let skill_refs: Vec<SkillRef> = skills.iter().map(|s| parse_skill_ref(s)).collect();
    let desc = description.unwrap_or("");

    let role = agency::build_role(name, desc, skill_refs, outcome);

    // Check if role with identical content already exists
    let role_path = roles_dir.join(format!("{}.yaml", role.id));
    if role_path.exists() {
        anyhow::bail!(
            "Role with identical content already exists ({})",
            agency::short_hash(&role.id)
        );
    }

    let path = agency::save_role(&role, &roles_dir).context("Failed to save role")?;

    println!(
        "Created role '{}' ({}) at {}",
        name,
        agency::short_hash(&role.id),
        path.display()
    );
    Ok(())
}

/// wg role list [--json]
pub fn run_list(dir: &Path, json: bool) -> Result<()> {
    let roles_dir = dir.join("agency").join("roles");
    let roles = agency::load_all_roles(&roles_dir).context("Failed to load roles")?;

    if json {
        let output: Vec<RoleSummary> = roles
            .iter()
            .map(|r| RoleSummary {
                id: r.id.clone(),
                name: r.name.clone(),
                skill_count: r.skills.len(),
                avg_score: r.performance.avg_score,
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if roles.is_empty() {
        println!("No roles defined. Use 'wg role add' to create one.");
    } else {
        println!("Roles:\n");
        for role in &roles {
            let score_str = role
                .performance
                .avg_score
                .map(|s| format!("{:.2}", s))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "  {}  {:20} skills: {}  avg_score: {}",
                agency::short_hash(&role.id),
                role.name,
                role.skills.len(),
                score_str,
            );
        }
    }

    Ok(())
}

/// Format a SkillRef for display
fn format_skill_ref(skill: &SkillRef) -> String {
    match skill {
        SkillRef::Name(name) => format!("{} (tag)", name),
        SkillRef::File(path) => format!("file: {}", path.display()),
        SkillRef::Url(url) => format!("url: {}", url),
        SkillRef::Inline(content) => {
            let preview: String = content.chars().take(60).collect();
            if content.len() > 60 {
                format!("inline: {}...", preview)
            } else {
                format!("inline: {}", preview)
            }
        }
    }
}

/// wg role show <id> [--json]
pub fn run_show(dir: &Path, id: &str, json: bool) -> Result<()> {
    let roles_dir = dir.join("agency").join("roles");
    let role = agency::find_role_by_prefix(&roles_dir, id)
        .with_context(|| format!("Failed to find role '{}'", id))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&role)?);
    } else {
        println!("Role: {} ({})", role.name, agency::short_hash(&role.id));
        println!("ID: {}", role.id);
        println!(
            "Description: {}",
            if role.description.is_empty() {
                "(none)"
            } else {
                &role.description
            }
        );
        println!("Desired outcome: {}", role.desired_outcome);
        println!();

        if role.skills.is_empty() {
            println!("Skills: (none)");
        } else {
            println!("Skills:");
            for skill in &role.skills {
                println!("  - {}", format_skill_ref(skill));
            }
        }

        println!();
        println!("Performance:");
        println!("  Tasks: {}", role.performance.task_count);
        let score_str = role
            .performance
            .avg_score
            .map(|s| format!("{:.2}", s))
            .unwrap_or_else(|| "-".to_string());
        println!("  Avg score: {}", score_str);
        if !role.performance.evaluations.is_empty() {
            println!("  Evaluations: {}", role.performance.evaluations.len());
        }
    }

    Ok(())
}

/// wg role lineage <id> [--json]
pub fn run_lineage(dir: &Path, id: &str, json: bool) -> Result<()> {
    let agency_dir = dir.join("agency");
    let roles_dir = agency_dir.join("roles");

    if !roles_dir.exists() {
        anyhow::bail!("No agency/roles directory found. Run 'wg init' first.");
    }

    // Resolve prefix to full ID first
    let role = agency::find_role_by_prefix(&roles_dir, id)
        .with_context(|| format!("Failed to find role '{}'", id))?;

    let ancestry = agency::role_ancestry(&role.id, &roles_dir)?;

    if ancestry.is_empty() {
        anyhow::bail!("Role '{}' not found", id);
    }

    if json {
        let json_nodes: Vec<serde_json::Value> = ancestry
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "name": n.name,
                    "generation": n.generation,
                    "created_by": n.created_by,
                    "created_at": n.created_at.to_rfc3339(),
                    "parent_ids": n.parent_ids,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_nodes)?);
        return Ok(());
    }

    let target = &ancestry[0];
    println!(
        "Lineage for role: {} ({})",
        agency::short_hash(&target.id),
        target.name
    );
    println!();

    for node in &ancestry {
        let indent = "  ".repeat(node.generation as usize);
        let gen_label = if node.generation == 0 {
            "gen 0 (root)".to_string()
        } else {
            format!("gen {}", node.generation)
        };

        let parents = if node.parent_ids.is_empty() {
            String::new()
        } else {
            let short_parents: Vec<&str> = node
                .parent_ids
                .iter()
                .map(|p| agency::short_hash(p))
                .collect();
            format!(" <- [{}]", short_parents.join(", "))
        };

        println!(
            "{}{} ({}) [{}] created by: {}{}",
            indent,
            agency::short_hash(&node.id),
            node.name,
            gen_label,
            node.created_by,
            parents
        );
    }

    if ancestry.len() == 1 && ancestry[0].parent_ids.is_empty() {
        println!();
        println!("This role has no evolutionary history (manually created).");
    }

    Ok(())
}

/// wg role edit <id>
///
/// After editing, the role is re-hashed. If the content changed, the file is
/// renamed to the new hash and the old file is removed.
pub fn run_edit(dir: &Path, id: &str) -> Result<()> {
    let roles_dir = dir.join("agency").join("roles");
    let role = agency::find_role_by_prefix(&roles_dir, id)
        .with_context(|| format!("Failed to find role '{}'", id))?;

    let role_path = roles_dir.join(format!("{}.yaml", role.id));

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    let status = std::process::Command::new(&editor)
        .arg(&role_path)
        .status()
        .with_context(|| format!("Failed to launch editor '{}'", editor))?;

    if !status.success() {
        anyhow::bail!("Editor exited with non-zero status");
    }

    // Validate and re-hash
    let mut edited = agency::load_role(&role_path).with_context(|| {
        format!(
            "Edited file is not valid role YAML. File saved at: {}",
            role_path.display()
        )
    })?;

    let new_id =
        agency::content_hash_role(&edited.skills, &edited.desired_outcome, &edited.description);
    if new_id != edited.id {
        // Content changed — rename to new hash
        let old_path = role_path;
        edited.id = new_id;
        agency::save_role(&edited, &roles_dir)?;
        std::fs::remove_file(&old_path).ok();
        println!(
            "Role content changed, new ID: {}",
            agency::short_hash(&edited.id)
        );
    } else {
        // Mutable fields (name, etc.) may have changed; re-save in place
        agency::save_role(&edited, &roles_dir)?;
        println!("Role '{}' updated", agency::short_hash(&edited.id));
    }

    Ok(())
}

/// wg role rm <id>
pub fn run_rm(dir: &Path, id: &str) -> Result<()> {
    let roles_dir = dir.join("agency").join("roles");
    let role = agency::find_role_by_prefix(&roles_dir, id)
        .with_context(|| format!("Failed to find role '{}'", id))?;

    let role_path = roles_dir.join(format!("{}.yaml", role.id));
    std::fs::remove_file(&role_path)
        .with_context(|| format!("Failed to remove role file: {}", role_path.display()))?;

    println!(
        "Removed role '{}' ({})",
        role.name,
        agency::short_hash(&role.id)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill_ref_name_only() {
        let skill = parse_skill_ref("rust");
        assert!(matches!(skill, SkillRef::Name(ref n) if n == "rust"));
    }

    #[test]
    fn test_parse_skill_ref_file() {
        let skill = parse_skill_ref("coding:file:///home/user/skill.md");
        assert!(matches!(skill, SkillRef::File(ref p) if p.to_str().unwrap().contains("skill.md")));
    }

    #[test]
    fn test_parse_skill_ref_url() {
        let skill = parse_skill_ref("review:https://example.com/skill.md");
        assert!(matches!(skill, SkillRef::Url(ref u) if u == "https://example.com/skill.md"));
    }

    #[test]
    fn test_parse_skill_ref_inline() {
        let skill = parse_skill_ref("code:inline:Write good Rust code");
        assert!(matches!(skill, SkillRef::Inline(ref c) if c == "Write good Rust code"));
    }

    #[test]
    fn test_content_hash_deterministic() {
        let skills = vec![SkillRef::Name("rust".into())];
        let h1 = agency::content_hash_role(&skills, "Working code", "A programmer");
        let h2 = agency::content_hash_role(&skills, "Working code", "A programmer");
        assert_eq!(h1, h2);
        // Different content produces different hash
        let h3 = agency::content_hash_role(&skills, "Different outcome", "A programmer");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_short_hash() {
        let hash = "a3f7c21deadbeef1234567890abcdef";
        assert_eq!(agency::short_hash(hash), "a3f7c21d");
    }
}

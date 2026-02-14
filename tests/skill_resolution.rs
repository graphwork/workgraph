//! Integration tests for skill resolution (`resolve_skill`, `resolve_all_skills`)
//! and their appearance in `render_identity_prompt`.

use std::path::{Path, PathBuf};
use tempfile::TempDir;

use workgraph::agency::{self, SkillRef};

// ---------------------------------------------------------------------------
// resolve_skill – individual variants
// ---------------------------------------------------------------------------

#[test]
fn resolve_skill_name_returns_tag() {
    let skill = SkillRef::Name("rust-expert".to_string());
    let resolved = agency::resolve_skill(&skill, Path::new("/tmp")).unwrap();
    assert_eq!(resolved.name, "rust-expert");
    assert_eq!(resolved.content, "rust-expert");
}

#[test]
fn resolve_skill_inline_returns_content() {
    let skill = SkillRef::Inline("Always write doc-comments".to_string());
    let resolved = agency::resolve_skill(&skill, Path::new("/tmp")).unwrap();
    assert_eq!(resolved.name, "inline");
    assert_eq!(resolved.content, "Always write doc-comments");
}

#[test]
fn resolve_skill_file_relative_path() {
    let tmp = TempDir::new().unwrap();
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join("coding.md"), "# Coding\nWrite clean code").unwrap();

    let skill = SkillRef::File(PathBuf::from("skills/coding.md"));
    let resolved = agency::resolve_skill(&skill, tmp.path()).unwrap();
    assert_eq!(resolved.name, "coding");
    assert_eq!(resolved.content, "# Coding\nWrite clean code");
}

#[test]
fn resolve_skill_file_absolute_path() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("absolute-skill.txt");
    std::fs::write(&file, "Absolute skill content").unwrap();

    let skill = SkillRef::File(file.clone());
    // workgraph_root should be ignored for absolute paths
    let resolved = agency::resolve_skill(&skill, Path::new("/nonexistent")).unwrap();
    assert_eq!(resolved.name, "absolute-skill");
    assert_eq!(resolved.content, "Absolute skill content");
}

#[test]
fn resolve_skill_file_tilde_expansion() {
    // Write a file under $HOME and resolve it via tilde path.
    // If HOME is not set (unlikely in test), skip gracefully.
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => {
            eprintln!("Skipping tilde test: HOME not set");
            return;
        }
    };

    let test_file = home.join(".workgraph-test-tilde-skill.md");
    std::fs::write(&test_file, "tilde resolved content").unwrap();

    // Clean up on panic via drop guard
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = Cleanup(test_file.clone());

    let skill = SkillRef::File(PathBuf::from("~/.workgraph-test-tilde-skill.md"));
    let resolved = agency::resolve_skill(&skill, Path::new("/tmp")).unwrap();
    assert_eq!(resolved.name, ".workgraph-test-tilde-skill");
    assert_eq!(resolved.content, "tilde resolved content");
}

#[test]
fn resolve_skill_file_nonexistent_returns_error() {
    let skill = SkillRef::File(PathBuf::from("/no/such/dir/skill.md"));
    let err = agency::resolve_skill(&skill, Path::new("/tmp")).unwrap_err();
    assert!(
        err.contains("Failed to read skill file"),
        "Expected 'Failed to read skill file' in error, got: {}",
        err
    );
}

#[test]
fn resolve_skill_url_without_http_feature() {
    // Default build does not enable matrix-lite, so URL resolution
    // should return a feature-gate error.
    let skill = SkillRef::Url("https://example.com/skill.md".to_string());
    let result = agency::resolve_skill(&skill, Path::new("/tmp"));
    // With matrix-lite it would succeed (or fail with a network error);
    // without it we get a clear feature-gate message.
    if let Err(e) = &result {
        assert!(
            e.contains("matrix-lite") || e.contains("HTTP"),
            "URL error should mention feature gate, got: {}",
            e
        );
    }
    // If it somehow succeeded (matrix-lite enabled), just verify structure
    if let Ok(resolved) = result {
        assert_eq!(resolved.name, "https://example.com/skill.md");
    }
}

// ---------------------------------------------------------------------------
// resolve_all_skills – mixed skill types
// ---------------------------------------------------------------------------

#[test]
fn resolve_all_skills_mixed_types() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("file-skill.md");
    std::fs::write(&file, "File skill body").unwrap();

    let role = agency::build_role(
        "Mixed",
        "A role with mixed skills",
        vec![
            SkillRef::Name("tag-skill".to_string()),
            SkillRef::File(file), // absolute, valid
            SkillRef::File(PathBuf::from("/missing/skill.md")), // will fail
            SkillRef::Inline("Inline skill body".to_string()),
        ],
        "Test outcome",
    );

    let resolved = agency::resolve_all_skills(&role, tmp.path());
    // The missing file should be skipped, giving us 3 resolved skills
    assert_eq!(resolved.len(), 3);
    assert_eq!(resolved[0].name, "tag-skill");
    assert_eq!(resolved[0].content, "tag-skill");
    assert_eq!(resolved[1].name, "file-skill");
    assert_eq!(resolved[1].content, "File skill body");
    assert_eq!(resolved[2].name, "inline");
    assert_eq!(resolved[2].content, "Inline skill body");
}

#[test]
fn resolve_all_skills_empty() {
    let role = agency::build_role("No Skills", "desc", vec![], "outcome");
    let resolved = agency::resolve_all_skills(&role, Path::new("/tmp"));
    assert!(resolved.is_empty());
}

#[test]
fn resolve_all_skills_all_failures() {
    let role = agency::build_role(
        "All Fail",
        "desc",
        vec![
            SkillRef::File(PathBuf::from("/no/a.md")),
            SkillRef::File(PathBuf::from("/no/b.md")),
        ],
        "outcome",
    );
    let resolved = agency::resolve_all_skills(&role, Path::new("/tmp"));
    assert!(resolved.is_empty());
}

// ---------------------------------------------------------------------------
// render_identity_prompt – resolved skills appear correctly
// ---------------------------------------------------------------------------

#[test]
fn render_identity_prompt_includes_resolved_skills() {
    let tmp = TempDir::new().unwrap();

    // Create a file-based skill
    let skill_file = tmp.path().join("debugging.md");
    std::fs::write(&skill_file, "Use systematic debugging with bisection").unwrap();

    let role = agency::build_role(
        "Debugger",
        "Finds and fixes bugs quickly.",
        vec![
            SkillRef::Name("debugging".to_string()),
            SkillRef::File(skill_file),
            SkillRef::Inline("Always add regression tests".to_string()),
        ],
        "All bugs fixed with regression tests",
    );

    let motivation = agency::build_motivation(
        "Thorough",
        "Leaves no stone unturned.",
        vec!["Takes longer".to_string()],
        vec!["Ignoring root cause".to_string()],
    );

    let resolved = agency::resolve_all_skills(&role, tmp.path());
    assert_eq!(resolved.len(), 3);

    let prompt = agency::render_identity_prompt(&role, &motivation, &resolved);

    // Role header
    assert!(prompt.contains("### Role: Debugger"), "Missing role header");
    assert!(
        prompt.contains("Finds and fixes bugs quickly."),
        "Missing role description"
    );

    // Skills section present
    assert!(prompt.contains("#### Skills"), "Missing Skills header");

    // Name-based skill
    assert!(
        prompt.contains("### debugging"),
        "Missing Name skill heading"
    );

    // File-based skill name (file stem) and content
    assert!(
        prompt.contains("### debugging"),
        "Missing file skill heading"
    );
    assert!(
        prompt.contains("Use systematic debugging with bisection"),
        "Missing file skill content"
    );

    // Inline skill
    assert!(
        prompt.contains("### inline"),
        "Missing inline skill heading"
    );
    assert!(
        prompt.contains("Always add regression tests"),
        "Missing inline skill content"
    );

    // Desired outcome
    assert!(
        prompt.contains("All bugs fixed with regression tests"),
        "Missing desired outcome"
    );

    // Motivation tradeoffs
    assert!(
        prompt.contains("- Takes longer"),
        "Missing acceptable tradeoff"
    );
    assert!(
        prompt.contains("- Ignoring root cause"),
        "Missing non-negotiable constraint"
    );
}

#[test]
fn render_identity_prompt_no_skills_omits_section() {
    let role = agency::build_role("Bare", "A barebones role.", vec![], "Some outcome");
    let motivation = agency::build_motivation("Simple", "Keep it simple.", vec![], vec![]);

    let prompt = agency::render_identity_prompt(&role, &motivation, &[]);

    assert!(prompt.contains("### Role: Bare"), "Missing role header");
    assert!(
        !prompt.contains("#### Skills"),
        "Skills section should be omitted when empty"
    );
    assert!(
        prompt.contains("#### Desired Outcome"),
        "Missing desired outcome header"
    );
}

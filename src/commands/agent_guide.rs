use anyhow::Result;

/// Universal agent / chat-agent role contract, bundled into the wg binary.
///
/// This text is project-independent: it describes how agents behave in ANY
/// workgraph project. Project-specific rules live in that project's
/// `CLAUDE.md` / `AGENTS.md`. Workgraph contributor docs live in
/// `docs/designs/` and `docs/research/` of the workgraph source repo.
pub const AGENT_GUIDE_TEXT: &str = include_str!("../text/agent_guide.md");

pub fn run() -> Result<()> {
    print!("{}", AGENT_GUIDE_TEXT);
    if !AGENT_GUIDE_TEXT.ends_with('\n') {
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_text_is_non_empty() {
        assert!(!AGENT_GUIDE_TEXT.trim().is_empty());
    }

    #[test]
    fn guide_text_covers_three_roles() {
        assert!(AGENT_GUIDE_TEXT.contains("dispatcher"));
        assert!(AGENT_GUIDE_TEXT.contains("chat agent"));
        assert!(AGENT_GUIDE_TEXT.contains("worker agent"));
    }

    #[test]
    fn guide_text_covers_chat_agent_contract() {
        assert!(AGENT_GUIDE_TEXT.contains("Chat Agent Contract"));
        assert!(AGENT_GUIDE_TEXT.contains("thin task-creator"));
        assert!(AGENT_GUIDE_TEXT.contains("NEVER"));
    }

    #[test]
    fn guide_text_warns_off_builtin_task_tools() {
        assert!(AGENT_GUIDE_TEXT.contains("TaskCreate"));
        assert!(AGENT_GUIDE_TEXT.contains("Task tool"));
    }

    #[test]
    fn guide_text_documents_validation_section() {
        assert!(AGENT_GUIDE_TEXT.contains("## Validation"));
    }

    #[test]
    fn guide_text_documents_smoke_gate() {
        assert!(AGENT_GUIDE_TEXT.contains("Smoke Gate"));
        assert!(AGENT_GUIDE_TEXT.contains("manifest.toml"));
    }

    #[test]
    fn guide_text_documents_quality_pass() {
        assert!(AGENT_GUIDE_TEXT.contains("quality pass") || AGENT_GUIDE_TEXT.contains("Quality pass"));
    }

    #[test]
    fn guide_text_documents_paused_task_convention() {
        assert!(AGENT_GUIDE_TEXT.contains("Paused-task") || AGENT_GUIDE_TEXT.contains("paused"));
    }
}

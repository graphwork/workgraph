//! Sanitize environment variables for spawned subprocesses.
//!
//! Claude Code injects env vars (CLAUDECODE, CLAUDE_CODE_ENTRYPOINT, etc.)
//! that prevent nested `claude` invocations. Rather than hardcoding specific
//! names — which breaks every time Anthropic adds a new one — we strip all
//! env vars matching `CLAUDE*` and log what was removed.

use std::process::Command;

/// Remove all `CLAUDE*` environment variables from a `Command`, logging each removal.
pub fn sanitize_command(cmd: &mut Command) -> Vec<String> {
    let removed: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CLAUDE"))
        .map(|(k, _)| k)
        .collect();

    for key in &removed {
        cmd.env_remove(key);
    }

    if !removed.is_empty() {
        eprintln!(
            "[env_sanitize] Removed {} CLAUDE* env var(s) from subprocess: {}",
            removed.len(),
            removed.join(", ")
        );
    }

    removed
}

/// Generate shell commands to unset all `CLAUDE*` environment variables.
/// Returns a string like `unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT\n`.
/// Returns empty string if no matching vars are found.
pub fn shell_unset_clause() -> String {
    let keys: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CLAUDE"))
        .map(|(k, _)| k)
        .collect();

    if keys.is_empty() {
        return String::new();
    }

    // Log to stderr so callers can see what was stripped
    eprintln!(
        "[env_sanitize] Generating unset for {} CLAUDE* env var(s): {}",
        keys.len(),
        keys.join(", ")
    );

    format!("unset {}\n", keys.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_unset_clause_format() {
        // We can't control env in unit tests easily, but we can verify
        // the function doesn't panic and returns a string.
        let clause = shell_unset_clause();
        // If CLAUDE* vars exist, it should start with "unset "
        if !clause.is_empty() {
            assert!(clause.starts_with("unset "));
            assert!(clause.ends_with('\n'));
        }
    }

    #[test]
    fn test_sanitize_command_does_not_panic() {
        let mut cmd = Command::new("echo");
        let removed = sanitize_command(&mut cmd);
        // Just verify it doesn't panic; actual removal depends on env
        assert!(removed.iter().all(|k| k.starts_with("CLAUDE")));
    }
}
